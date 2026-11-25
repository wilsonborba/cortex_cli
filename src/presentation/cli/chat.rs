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
        local::session::OsSessionStore,
        remote::{chat::CortexClient, gateway::GatewayClient, login_context},
    },
    domain::{
        models::{
            chat::{ChatFailure, ChatPage, StreamEvent},
            native_auth::{AuthFailure, Session},
        },
        services::{
            auth::AuthService,
            chat::{ChatApi, ChatState, follow, latest_chat, owned_history, running_output},
        },
    },
};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const KITTY_HOLD_THRESHOLD: Duration = Duration::from_millis(260);
const FALLBACK_TAP_TIMEOUT: Duration = Duration::from_millis(700);
const FALLBACK_RELEASE_TIMEOUT: Duration = Duration::from_millis(350);

struct VoiceJob {
    receiver: mpsc::Receiver<Result<String, crate::dal::local::voice::VoiceFailure>>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl VoiceJob {
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn join_completed(mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for VoiceJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn spawn_voice_job<F>(samples: Vec<f32>, transcribe: F) -> VoiceJob
where
    F: FnOnce(&[f32], &Arc<AtomicBool>) -> Result<String, crate::dal::local::voice::VoiceFailure>
        + Send
        + 'static,
{
    let (sender, receiver) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let worker = thread::spawn(move || {
        let samples = zeroize::Zeroizing::new(samples);
        let result = transcribe(&samples, &worker_cancel);
        drop(samples);
        let _ = sender.send(result);
    });
    VoiceJob {
        receiver,
        cancel,
        worker: Some(worker),
    }
}

fn transcribe_in_background(
    recorder: crate::dal::local::voice::Recorder,
) -> Result<VoiceJob, crate::dal::local::voice::VoiceFailure> {
    let samples = recorder.stop()?;
    Ok(spawn_voice_job(
        samples,
        crate::dal::local::voice::transcribe,
    ))
}

fn insert_voice_transcript(editor: &mut Editor, transcript: &str) {
    editor.insert(transcript);
}

fn cancel_voice_capture<T>(capture: &mut Option<T>) -> bool {
    capture.take().is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VoiceKeyAction {
    Cancel,
    Exit,
    Ignore,
}

fn voice_key_action(code: KeyCode, modifiers: KeyModifiers) -> VoiceKeyAction {
    if code == KeyCode::Esc
        || (modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c' | 'C')))
    {
        VoiceKeyAction::Cancel
    } else if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('d' | 'D'))
    {
        VoiceKeyAction::Exit
    } else {
        VoiceKeyAction::Ignore
    }
}

fn handle_voice_job_key(
    job: &VoiceJob,
    code: KeyCode,
    modifiers: KeyModifiers,
    exit_after_voice: &mut bool,
    status: &mut Option<String>,
) {
    match voice_key_action(code, modifiers) {
        VoiceKeyAction::Cancel => {
            job.cancel();
            *status = Some("Cancellation requested; stopping local transcription...".into());
        }
        VoiceKeyAction::Exit => {
            *exit_after_voice = true;
            job.cancel();
            *status = Some("Stopping local transcription before exit...".into());
        }
        VoiceKeyAction::Ignore => {}
    }
}

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
    if options.prompt.is_none() && (!io::stdin().is_terminal() || !io::stdout().is_terminal()) {
        return out.write_all(super::formatters::help().as_bytes());
    }
    let api = Arc::new(CortexClient::new(session()?).map_err(failure)?);
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
        let stream = api.send(chat.as_deref(), &prompt).map_err(failure)?;
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
        .map_err(failure)?;
        return writeln!(out);
    }
    interactive(api, chat, history, out)
}

fn interactive<G: ChatApi + 'static>(
    api: Arc<G>,
    mut chat: Option<String>,
    history: Option<ChatPage>,
    out: &mut impl Write,
) -> io::Result<()> {
    let _raw = RawTerminal::enter(out)?;
    let reports_key_releases = _raw.reports_key_releases();
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
    let mut pending_space: Option<Instant> = None;
    let mut recording: Option<(crate::dal::local::voice::Recorder, Instant, bool)> = None;
    let mut last_repeat: Option<Instant> = None;
    let mut voice_job: Option<VoiceJob> = None;
    let mut voice_status: Option<String> = None;
    let mut exit_after_voice = false;
    if let (Some(chat_id), Some(output_id)) = (chat.clone(), running) {
        let (tx, rx) = mpsc::channel();
        receiver = Some(rx);
        started = Some(Instant::now());
        let api = api.clone();
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
            if let Err(error) = result {
                let _ = tx.send(Err(error));
            }
        });
    }
    loop {
        let now = Instant::now();
        if let Some(pending) = pending_space {
            if reports_key_releases && now.duration_since(pending) >= KITTY_HOLD_THRESHOLD {
                pending_space = None;
                match crate::dal::local::voice::Recorder::start() {
                    Ok(recorder) => recording = Some((recorder, now, true)),
                    Err(error) => voice_status = Some(error.to_string()),
                }
            } else if !reports_key_releases && now.duration_since(pending) >= FALLBACK_TAP_TIMEOUT {
                pending_space = None;
                editor.insert(" ");
                dirty = true;
            }
        }
        if let Some((_, started_at, fallback)) = recording.as_ref() {
            voice_status = Some(format!(
                "● Recording locally  {:.1}s  Esc cancels",
                started_at.elapsed().as_secs_f64()
            ));
            if *fallback
                && last_repeat
                    .is_some_and(|last| now.duration_since(last) >= FALLBACK_RELEASE_TIMEOUT)
            {
                let (recorder, _, _) = recording.take().expect("recording is present");
                last_repeat = None;
                match transcribe_in_background(recorder) {
                    Ok(job) => {
                        voice_job = Some(job);
                        voice_status = Some("Transcribing locally...".into());
                    }
                    Err(error) => voice_status = Some(error.to_string()),
                }
            }
        }
        if let Some(job) = voice_job.as_ref() {
            match job.receiver.try_recv() {
                Ok(result) => {
                    if let Some(job) = voice_job.take() {
                        job.join_completed();
                    }
                    match result {
                        Ok(text) => {
                            insert_voice_transcript(&mut editor, &text);
                            voice_status = None;
                        }
                        Err(error) => voice_status = Some(error.to_string()),
                    }
                    dirty = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(job) = voice_job.take() {
                        job.join_completed();
                    }
                    voice_status = Some("Local speech transcription failed.".into());
                    dirty = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
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
        if dirty || receiver.is_some() || recording.is_some() || voice_job.is_some() {
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
                voice_status.as_deref(),
            )?;
            dirty = false;
        }
        if exit_after_voice && voice_job.is_none() {
            surface.clear(out)?;
            return surface.text(out, "\n");
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        dirty = true;
        match event::read()? {
            Event::Resize(w, h) => surface.resize(out, w, h)?,
            Event::Paste(text) if receiver.is_none() => editor.insert(&text),
            Event::Key(key)
                if key.kind == KeyEventKind::Release
                    && reports_key_releases
                    && key.code == KeyCode::Char(' ') =>
            {
                if let Some((recorder, _, true)) = recording.take() {
                    match transcribe_in_background(recorder) {
                        Ok(job) => {
                            voice_job = Some(job);
                            voice_status = Some("Transcribing locally...".into());
                        }
                        Err(error) => voice_status = Some(error.to_string()),
                    }
                } else if pending_space.take().is_some() {
                    editor.insert(" ");
                }
            }
            Event::Key(key)
                if key.kind != KeyEventKind::Release
                    && receiver.is_none()
                    && voice_job.is_none()
                    && key.code == KeyCode::Char(' ') =>
            {
                let now = Instant::now();
                if let Some((_, _, true)) = recording.as_ref() {
                    continue;
                }
                if let Some((_, _, false)) = recording.as_ref() {
                    last_repeat = Some(now);
                } else if let Some(pending) = pending_space {
                    if now.duration_since(pending) >= KITTY_HOLD_THRESHOLD {
                        match crate::dal::local::voice::Recorder::start() {
                            Ok(recorder) => {
                                recording = Some((recorder, now, false));
                                last_repeat = Some(now);
                            }
                            Err(error) => voice_status = Some(error.to_string()),
                        }
                        pending_space = None;
                    }
                } else {
                    pending_space = Some(now);
                }
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if let Some(pending) = pending_space.take()
                    && (!reports_key_releases
                        || Instant::now().duration_since(pending) < KITTY_HOLD_THRESHOLD)
                {
                    editor.insert(" ");
                }
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                if control && matches!(key.code, KeyCode::Char('o' | 'O')) {
                    expanded = !expanded;
                    #[cfg(test)]
                    tests::DETAIL_STATES.lock().unwrap().push(expanded);
                    continue;
                }
                if recording.is_some() {
                    if key.code == KeyCode::Esc {
                        cancel_voice_capture(&mut recording);
                        last_repeat = None;
                        voice_status = Some("Recording cancelled.".into());
                    }
                    continue;
                }
                if let Some(job) = voice_job.as_ref() {
                    handle_voice_job_key(
                        job,
                        key.code,
                        key.modifiers,
                        &mut exit_after_voice,
                        &mut voice_status,
                    );
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
                        let id = chat.clone();
                        thread::spawn(move || {
                            let mut state = ChatState {
                                chat_id: id.clone(),
                                ..Default::default()
                            };
                            let result = api.send(id.as_deref(), &text).and_then(|stream| {
                                follow(api.as_ref(), stream, &mut state, |event| {
                                    tx.send(Ok(event)).map_err(|_| ChatFailure::Connection)
                                })
                            });
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
    static SENDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    struct CaptureProbe {
        samples: Vec<f32>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for CaptureProbe {
        fn drop(&mut self) {
            crate::dal::local::voice::clear_audio_samples(&mut self.samples);
            self.dropped.store(
                self.samples.iter().all(|sample| *sample == 0.0),
                std::sync::atomic::Ordering::SeqCst,
            );
        }
    }

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
            SENDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    fn synthetic_voice_cancel_drops_samples_without_sending_a_prompt() {
        SENDS.store(0, std::sync::atomic::Ordering::SeqCst);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut capture = Some(CaptureProbe {
            samples: vec![0.125, -0.25, 0.5],
            dropped: dropped.clone(),
        });

        assert!(cancel_voice_capture(&mut capture));
        assert!(capture.is_none());
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(SENDS.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn synthetic_voice_transcript_stays_editable_and_preserves_unicode_without_sending() {
        SENDS.store(0, std::sync::atomic::Ordering::SeqCst);
        let transcript = "สวัสดี, olá, hello";
        let mut editor = Editor::default();

        insert_voice_transcript(&mut editor, transcript);

        assert_eq!(editor.text, transcript);
        assert_eq!(editor.cursor, transcript.len());
        assert_eq!(SENDS.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn voice_job_escape_and_ctrl_c_cancel_while_ctrl_d_waits_for_worker_exit() {
        use crate::dal::local::voice::VoiceFailure;

        assert_eq!(
            voice_key_action(KeyCode::Esc, KeyModifiers::NONE),
            VoiceKeyAction::Cancel
        );
        assert_eq!(
            voice_key_action(KeyCode::Char('c'), KeyModifiers::CONTROL),
            VoiceKeyAction::Cancel
        );
        assert_eq!(
            voice_key_action(KeyCode::Char('d'), KeyModifiers::CONTROL),
            VoiceKeyAction::Exit
        );

        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_started = Arc::clone(&started);
        let worker_stopped = Arc::clone(&stopped);
        let job = spawn_voice_job(vec![0.25, -0.5], move |samples, cancel| {
            assert_eq!(samples, [0.25, -0.5]);
            worker_started.store(true, Ordering::SeqCst);
            while !cancel.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
            worker_stopped.store(true, Ordering::SeqCst);
            Err(VoiceFailure::Cancelled)
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !started.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(started.load(Ordering::SeqCst));
        assert!(matches!(
            job.receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let mut exit_after_voice = false;
        let mut status = None;
        handle_voice_job_key(
            &job,
            KeyCode::Esc,
            KeyModifiers::NONE,
            &mut exit_after_voice,
            &mut status,
        );
        assert!(job.cancel.load(Ordering::SeqCst));
        assert!(!exit_after_voice);
        assert!(
            status
                .as_deref()
                .unwrap()
                .contains("Cancellation requested")
        );
        handle_voice_job_key(
            &job,
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
            &mut exit_after_voice,
            &mut status,
        );
        assert!(exit_after_voice);
        assert!(status.as_deref().unwrap().contains("before exit"));
        let result = job.receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(result, Err(VoiceFailure::Cancelled));
        assert!(stopped.load(Ordering::SeqCst));
        job.join_completed();
    }

    #[test]
    #[ignore = "requires an interactive terminal; uses only in-memory fixture events"]
    fn terminal_review_fixture() {
        assert!(io::stdin().is_terminal());
        let mut out = io::stdout().lock();
        write!(out, "\x1b[2J\x1b[H").unwrap();
        interactive(Arc::new(Fixture), None, None, &mut out).unwrap();
        assert_eq!(*DETAIL_STATES.lock().unwrap(), vec![true, false]);
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }
    #[test]
    #[ignore = "requires an interactive terminal; uses only in-memory fixture events"]
    fn terminal_cancel_fixture() {
        assert!(io::stdin().is_terminal());
        let mut out = io::stdout().lock();
        interactive(Arc::new(Fixture), None, None, &mut out).unwrap();
        assert_eq!(
            *CANCELLATIONS.lock().unwrap(),
            vec![(
                "10000000-0000-4000-8000-000000000001".to_owned(),
                "20000000-0000-4000-8000-000000000002".to_owned()
            )]
        );
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }
}
