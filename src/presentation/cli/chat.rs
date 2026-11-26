//! Inline chat and print workflows. Keyboard events stay on one thread.

use super::{
    auth::finish_from_input,
    commands::ChatOptions,
    terminal::{self, Editor, RawTerminal, Surface},
};
use crate::{
    core::{
        settings,
        terminal_text::{TerminalText, clean},
    },
    dal::{
        local::{session::OsSessionStore, telemetry_consent::OsTelemetryConsentStore},
        remote::{
            annotations::AnnotationsClient, chat::CortexClient, gateway::GatewayClient,
            login_context,
        },
    },
    domain::{
        models::{
            chat::{ChatFailure, ChatPage, StreamEvent},
            native_auth::{AuthFailure, Session},
        },
        services::{
            annotations::{selected_context, with_selected_context},
            auth::AuthService,
            chat::{ChatApi, ChatState, follow, latest_chat, owned_history, running_output},
            telemetry::{Telemetry, TrackingApi, establish_consent},
        },
    },
};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::{
    io::{self, IsTerminal, Write},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

fn failure(error: impl ToString) -> io::Error {
    io::Error::other(error.to_string())
}

fn session() -> io::Result<Arc<Session>> {
    let gateway = GatewayClient::new(settings::GATEWAY_URL).map_err(failure)?;
    let mut service = AuthService::new(gateway, OsSessionStore);
    if !service.restore().unwrap_or(false) {
        if !io::stdin().is_terminal() {
            return Err(failure(
                "Sign in first with cortex login in an interactive terminal.",
            ));
        }
        let pending = service.start().map_err(failure)?;
        let link = login_context::browser_url(pending).map_err(failure)?;
        let mut out = io::stderr().lock();
        writeln!(
            out,
            "Sign in to Cortex. Match code: {}\nContinue only if the browser shows this code.\n{link}",
            pending.match_code
        )?;
        finish_from_input(&mut service, &mut io::stdin().lock(), &mut out)?;
    }
    service
        .session
        .take()
        .map(Arc::new)
        .ok_or_else(|| failure(AuthFailure::Cancelled))
}

pub fn execute(options: ChatOptions, out: &mut impl Write) -> io::Result<()> {
    execute_with_note_gate(options, out, settings::ANNOTATIONS_SOURCE_ENABLED)
}

fn execute_with_note_gate(
    options: ChatOptions,
    out: &mut impl Write,
    notes_enabled: bool,
) -> io::Result<()> {
    if options.prompt.is_none() && (!io::stdin().is_terminal() || !io::stdout().is_terminal()) {
        return out.write_all(super::formatters::help().as_bytes());
    }
    if !options.note_ids.is_empty() && !notes_enabled {
        return Err(failure(ChatFailure::SourceUnavailable));
    }
    let auth = session()?;
    let context = if options.note_ids.is_empty() {
        String::new()
    } else {
        let notes = AnnotationsClient::new(auth.clone()).map_err(failure)?;
        selected_context(&notes, &options.note_ids).map_err(failure)?
    };
    let telemetry = telemetry_for(&auth);
    let api = Arc::new(CortexClient::new(auth).map_err(failure)?);
    let chat = if options.continue_latest {
        latest_chat(api.as_ref()).map_err(failure)?
    } else {
        options.conversation
    };
    let history = chat
        .as_deref()
        .map(|id| owned_history(api.as_ref(), id))
        .transpose()
        .map_err(failure)?;
    if options.continue_latest && chat.is_none() {
        return Err(failure("No Cortex conversation is available to continue."));
    }
    if let Some(prompt) = options.prompt {
        if history
            .as_ref()
            .map(running_output)
            .transpose()
            .map_err(failure)?
            .flatten()
            .is_some()
        {
            return Err(failure(ChatFailure::RunningOutput));
        }
        let mut state = ChatState {
            chat_id: chat.clone(),
            ..Default::default()
        };
        let mut text = TerminalText::default();
        let request = with_selected_context(&prompt, &context);
        let result = api.send(chat.as_deref(), &request).and_then(|stream| {
            follow(api.as_ref(), stream, &mut state, |event| {
                if event.name == "token" {
                    out.write_all(
                        text.push(event.data["text"].as_str().unwrap_or(""))
                            .as_bytes(),
                    )
                    .map_err(|_| ChatFailure::Connection)?;
                    out.flush().map_err(|_| ChatFailure::Connection)?;
                }
                Ok(())
            })
        });
        if let Some(telemetry) = telemetry {
            telemetry.chat_result(result);
        }
        result.map_err(failure)?;
        return writeln!(out);
    }
    interactive(api, chat, history, &context, telemetry, out)
}

fn telemetry_for(session: &Arc<Session>) -> Option<Arc<Telemetry>> {
    use sha2::Digest;

    let api = Arc::new(GatewayClient::new(settings::GATEWAY_URL).ok()?);
    let mut digest = sha2::Sha256::digest(session.session_id.as_bytes());
    let fingerprint = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    digest.fill(0);
    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    let record = establish_consent(
        api.as_ref(),
        &OsTelemetryConsentStore,
        session,
        &fingerprint,
        interactive,
        ask_telemetry_consent,
    );
    record.as_ref()?;
    let tracking: Arc<dyn TrackingApi> = api;
    Telemetry::new(tracking, session.clone()).map(Arc::new)
}

fn ask_telemetry_consent(version: &str) -> Option<bool> {
    let mut output = io::stderr().lock();
    writeln!(
        output,
        "\nCortex can send limited, optional usage telemetry (policy version {version}). This choice is separate from accepting legal documents."
    )
    .ok()?;
    writeln!(output, "It includes chat request counts and success or failure categories. It never includes prompts, responses, reasoning, file contents, or credentials.").ok()?;
    writeln!(output, "Read: https://asodya.com/privacy-policy").ok()?;
    loop {
        write!(output, "Allow this telemetry? [y/N] ").ok()?;
        output.flush().ok()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).ok()?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Some(true),
            "" | "n" | "no" => return Some(false),
            _ => writeln!(output, "Enter yes or no.").ok()?,
        }
    }
}

fn interactive<G: ChatApi + 'static>(
    api: Arc<G>,
    mut chat: Option<String>,
    history: Option<ChatPage>,
    context: &str,
    telemetry: Option<Arc<Telemetry>>,
    out: &mut impl Write,
) -> io::Result<()> {
    let _raw = RawTerminal::enter(out)?;
    let colour = std::env::var_os("NO_COLOR").is_none();
    terminal::logo(out, colour)?;
    let (width, height) = crossterm::terminal::size()?;
    let mut surface = Surface::new(width, height, colour);
    let running = history
        .as_ref()
        .map(running_output)
        .transpose()
        .map_err(failure)?
        .flatten();
    if let Some(page) = history {
        for input in page.inputs.iter().rev() {
            surface.text(out, &format!("\n> {}\n", clean(&input.text)))?;
            for output in page
                .outputs
                .iter()
                .filter(|o| o.input_id == input.input_id && o.status != "running")
            {
                surface.text(out, &clean(&output.text))?;
                surface.text(out, "\n")?;
            }
        }
    }
    let mut state = ChatState {
        chat_id: chat.clone(),
        output_id: running.clone(),
        ..Default::default()
    };
    let mut editor = Editor::default();
    let mut expanded = false;
    let mut receiver: Option<mpsc::Receiver<Result<StreamEvent, ChatFailure>>> = None;
    let mut started: Option<Instant> = None;
    let mut cleaner = TerminalText::default();
    let mut stop_requested = false;
    let mut dirty = true;
    if let (Some(chat_id), Some(output_id)) = (chat.clone(), running) {
        let (tx, rx) = mpsc::channel();
        receiver = Some(rx);
        started = Some(Instant::now());
        let api = api.clone();
        let telemetry = telemetry.clone();
        thread::spawn(move || {
            let mut state = ChatState {
                chat_id: Some(chat_id.clone()),
                output_id: Some(output_id.clone()),
                ..Default::default()
            };
            let result = api.reconnect(&chat_id, &output_id, 0).and_then(|stream| {
                follow(api.as_ref(), stream, &mut state, |event| {
                    tx.send(Ok(event)).map_err(|_| ChatFailure::Connection)
                })
            });
            if let Some(telemetry) = telemetry {
                telemetry.chat_result(result);
            }
            if let Err(error) = result {
                let _ = tx.send(Err(error));
            }
        });
    }
    loop {
        if let Some(rx) = receiver.as_ref() {
            let mut finished = false;
            loop {
                let result = match rx.try_recv() {
                    Ok(value) => value,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        surface.text(out, &format!("\n{}\n", ChatFailure::IncompleteStream))?;
                        finished = true;
                        break;
                    }
                };
                match result {
                    Ok(event) => match state.apply(&event) {
                        Ok(true) => {
                            if event.name == "token" {
                                surface.text(
                                    out,
                                    &cleaner.push(event.data["text"].as_str().unwrap_or("")),
                                )?;
                            }
                            if state.complete {
                                chat = state.chat_id.clone();
                                finished = true;
                                break;
                            }
                        }
                        Ok(false) => {}
                        Err(error) => {
                            surface.text(out, &format!("\n{error}\n"))?;
                            finished = true;
                            break;
                        }
                    },
                    Err(error) => {
                        surface.text(out, &format!("\n{error}\n"))?;
                        finished = true;
                        break;
                    }
                }
            }
            if finished {
                receiver = None;
                started = None;
                stop_requested = false;
                dirty = true;
            }
        }
        if dirty || receiver.is_some() {
            surface.paint(
                out,
                &state,
                expanded,
                started.map(|t| t.elapsed()),
                if receiver.is_none() {
                    Some(&editor)
                } else {
                    None
                },
            )?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        dirty = true;
        match event::read()? {
            Event::Resize(w, h) => surface.resize(out, w, h)?,
            Event::Paste(text) if receiver.is_none() => editor.insert(&text),
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                if control && matches!(key.code, KeyCode::Char('o' | 'O')) {
                    expanded = !expanded;
                    #[cfg(test)]
                    tests::DETAIL_STATES.lock().unwrap().push(expanded);
                    continue;
                }
                if control && matches!(key.code, KeyCode::Char('c' | 'C')) {
                    if receiver.is_some() {
                        if let (Some(chat), Some(output)) =
                            (state.chat_id.as_deref(), state.output_id.as_deref())
                        {
                            if !stop_requested {
                                api.cancel(chat, output).map_err(failure)?;
                                stop_requested = true;
                                surface.text(out, "\nCancellation requested.\n")?;
                            }
                        } else {
                            surface.text(out, "\nDisconnected. The remote output continues.\n")?;
                            return Ok(());
                        }
                    } else {
                        editor.take();
                    }
                    continue;
                }
                if receiver.is_some() {
                    continue;
                }
                match key.code {
                    KeyCode::Char('d' | 'D') if control && editor.text.is_empty() => {
                        surface.clear(out)?;
                        return surface.text(out, "\n");
                    }
                    KeyCode::Enter => {
                        if editor.text.trim().is_empty() {
                            continue;
                        }
                        let text = editor.take();
                        surface.text(out, &format!("\n> {}\n", clean(&text)))?;
                        state = ChatState {
                            chat_id: chat.clone(),
                            ..Default::default()
                        };
                        cleaner = TerminalText::default();
                        started = Some(Instant::now());
                        let (tx, rx) = mpsc::channel();
                        receiver = Some(rx);
                        let api = api.clone();
                        let telemetry = telemetry.clone();
                        let id = chat.clone();
                        let context = context.to_owned();
                        thread::spawn(move || {
                            let mut state = ChatState {
                                chat_id: id.clone(),
                                ..Default::default()
                            };
                            let request = with_selected_context(&text, &context);
                            let result = api.send(id.as_deref(), &request).and_then(|stream| {
                                follow(api.as_ref(), stream, &mut state, |event| {
                                    tx.send(Ok(event)).map_err(|_| ChatFailure::Connection)
                                })
                            });
                            if let Some(telemetry) = telemetry {
                                telemetry.chat_result(result);
                            }
                            if let Err(error) = result {
                                let _ = tx.send(Err(error));
                            }
                        });
                    }
                    KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                        editor.insert(&c.to_string())
                    }
                    KeyCode::Tab => editor.insert("\t"),
                    KeyCode::Backspace => editor.backspace(),
                    KeyCode::Delete => editor.delete(),
                    KeyCode::Left => editor.left(),
                    KeyCode::Right => editor.right(),
                    KeyCode::Home => editor.cursor = 0,
                    KeyCode::End => editor.cursor = editor.text.len(),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::chat::ChatSummary;
    use serde_json::json;
    use std::{collections::VecDeque, io::Read};
    pub(super) static DETAIL_STATES: std::sync::Mutex<Vec<bool>> =
        std::sync::Mutex::new(Vec::new());
    static CANCELLATIONS: std::sync::Mutex<Vec<(String, String)>> =
        std::sync::Mutex::new(Vec::new());
    struct Fixture;
    struct SlowFrames {
        frames: VecDeque<Vec<u8>>,
        offset: usize,
    }
    impl Read for SlowFrames {
        fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
            let Some(frame) = self.frames.front() else {
                return Ok(0);
            };
            if self.offset == 0 {
                thread::sleep(Duration::from_millis(400));
            }
            let count = target.len().min(frame.len() - self.offset);
            target[..count].copy_from_slice(&frame[self.offset..self.offset + count]);
            self.offset += count;
            if self.offset == frame.len() {
                self.frames.pop_front();
                self.offset = 0;
            }
            Ok(count)
        }
    }
    impl ChatApi for Fixture {
        fn send(&self, _: Option<&str>, _: &str) -> Result<Box<dyn Read + Send>, ChatFailure> {
            let steps = [
                (
                    "stage",
                    json!({"stage":"accept","status":"started","expected_ms":null,"session_id":"10000000-0000-4000-8000-000000000001","output_id":"20000000-0000-4000-8000-000000000002"}),
                ),
                (
                    "step",
                    json!({"stage":"gather","step":"file.read.page","params":{"file":"example.txt"},"progress":[3,10],"elapsed_ms":400}),
                ),
                ("stage", json!({"stage":"answer","status":"started"})),
                (
                    "step",
                    json!({"stage":"answer","step":"model.ready","params":{"model":"fixture-model"},"elapsed_ms":800}),
                ),
                (
                    "token",
                    json!({"text":"A clear explanation from the supplied source.\nคำอธิบายจากแหล่งข้อมูลที่เลือก"}),
                ),
                (
                    "done",
                    json!({"session_id":"10000000-0000-4000-8000-000000000001","output_id":"20000000-0000-4000-8000-000000000002","total_ms":2400}),
                ),
            ];
            let frames = steps
                .into_iter()
                .enumerate()
                .map(|(index, (name, data))| {
                    format!(
                        "id: {}\nevent: {name}\ndata: {}\n\n",
                        index + 1,
                        json!({"message":"Reading the supplied source","data":data})
                    )
                    .into_bytes()
                })
                .collect();
            Ok(Box::new(SlowFrames { frames, offset: 0 }))
        }
        fn chats(&self) -> Result<Vec<ChatSummary>, ChatFailure> {
            Ok(Vec::new())
        }
        fn history(&self, _: &str) -> Result<ChatPage, ChatFailure> {
            Err(ChatFailure::InvalidInput)
        }
        fn reconnect(&self, _: &str, _: &str, _: u64) -> Result<Box<dyn Read + Send>, ChatFailure> {
            Err(ChatFailure::InvalidInput)
        }
        fn cancel(&self, chat: &str, output: &str) -> Result<(), ChatFailure> {
            CANCELLATIONS
                .lock()
                .unwrap()
                .push((chat.into(), output.into()));
            Ok(())
        }
    }

    #[test]
    #[ignore = "requires an interactive terminal; uses only in-memory fixture events"]
    fn terminal_review_fixture() {
        assert!(io::stdin().is_terminal());
        let mut out = io::stdout().lock();
        write!(out, "\x1b[2J\x1b[H").unwrap();
        interactive(Arc::new(Fixture), None, None, "", None, &mut out).unwrap();
        assert_eq!(*DETAIL_STATES.lock().unwrap(), vec![true, false]);
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }
    #[test]
    #[ignore = "requires an interactive terminal; uses only in-memory fixture events"]
    fn terminal_cancel_fixture() {
        assert!(io::stdin().is_terminal());
        let mut out = io::stdout().lock();
        interactive(Arc::new(Fixture), None, None, "", None, &mut out).unwrap();
        assert_eq!(
            *CANCELLATIONS.lock().unwrap(),
            vec![(
                "10000000-0000-4000-8000-000000000001".to_owned(),
                "20000000-0000-4000-8000-000000000002".to_owned()
            )]
        );
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }

    #[test]
    fn unreleased_notes_gate_fails_before_login_or_network_access() {
        let options = ChatOptions {
            prompt: Some("Explain this".into()),
            note_ids: vec!["10000000-0000-4000-8000-000000000001".into()],
            ..Default::default()
        };
        let error = execute_with_note_gate(options, &mut Vec::new(), false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unavailable until its gateway route is ready")
        );
    }
}
