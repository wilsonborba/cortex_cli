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
            remote_control::{RemoteEvent, RemoteEventKind},
        },
        services::{
            auth::AuthService,
            chat::{ChatApi, ChatState, follow, latest_chat, owned_history, running_output},
            remote_control::reconnect_delay,
        },
    },
};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    io::{self, IsTerminal, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
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

enum PublishMessage {
    Event(RemoteEventKind, Value),
    Flush(mpsc::SyncSender<()>),
    Stop,
}

#[derive(Clone)]
struct RemotePublisher {
    sender: mpsc::SyncSender<PublishMessage>,
    failed: Arc<AtomicBool>,
}

impl RemotePublisher {
    fn publish(&self, kind: RemoteEventKind, payload: Value) {
        if self.failed.load(Ordering::Relaxed) {
            return;
        }
        if self
            .sender
            .try_send(PublishMessage::Event(kind, payload))
            .is_err()
        {
            self.failed.store(true, Ordering::Relaxed);
        }
    }

    fn flush(&self) {
        let (sender, receiver) = mpsc::sync_channel(1);
        if self.sender.send(PublishMessage::Flush(sender)).is_ok() {
            let _ = receiver.recv_timeout(Duration::from_secs(5));
        }
    }

    fn stop(&self) {
        let _ = self.sender.try_send(PublishMessage::Stop);
    }
}

struct RemoteSessionGuard {
    api: Arc<CortexClient>,
    session_id: String,
    receiver: mpsc::Receiver<RemoteEvent>,
    cursor: Arc<AtomicU64>,
    listener_online: Arc<AtomicBool>,
    publisher: RemotePublisher,
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
struct TurnControl {
    active: bool,
    stop_requested: bool,
    cancel_pending: bool,
}

struct RemoteCommandQueue {
    pending: VecDeque<RemoteEvent>,
    seen: HashSet<u64>,
    prompts: VecDeque<(String, Vec<String>)>,
    retry_at: Instant,
    retry_attempt: u32,
}

impl Default for RemoteCommandQueue {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            seen: HashSet::new(),
            prompts: VecDeque::new(),
            retry_at: Instant::now(),
            retry_attempt: 0,
        }
    }
}

impl RemoteSessionGuard {
    fn start(api: Arc<CortexClient>, out: &mut impl Write) -> io::Result<Self> {
        let session = api.register_remote_session().map_err(failure)?;
        let session_id = session.session_id;
        let (sender, receiver) = mpsc::channel();
        let cursor = Arc::new(AtomicU64::new(0));
        let listener_online = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let (publish_sender, publish_receiver) = mpsc::sync_channel(128);
        let publisher = RemotePublisher {
            sender: publish_sender,
            failed: failed.clone(),
        };
        let guard = Self {
            api,
            session_id,
            receiver,
            cursor,
            listener_online,
            publisher,
            stop,
        };

        let listener_api = guard.api.clone();
        let listener_session = guard.session_id.clone();
        let listener_cursor = guard.cursor.clone();
        let listener_online_flag = guard.listener_online.clone();
        let listener_stop = guard.stop.clone();
        thread::Builder::new()
            .name("cortex-remote-listener".into())
            .spawn(move || {
                remote_listener(
                    listener_api,
                    listener_session,
                    listener_cursor,
                    sender,
                    listener_online_flag,
                    listener_stop,
                );
            })
            .map_err(failure)?;

        let publish_api = guard.api.clone();
        let publish_session = guard.session_id.clone();
        let publish_failed = failed.clone();
        thread::Builder::new()
            .name("cortex-remote-publisher".into())
            .spawn(move || {
                while let Ok(message) = publish_receiver.recv() {
                    match message {
                        PublishMessage::Event(kind, payload) => {
                            if !publish_failed.load(Ordering::Relaxed)
                                && publish_api
                                    .post_remote_event(&publish_session, kind, payload, None)
                                    .is_err()
                            {
                                publish_failed.store(true, Ordering::Relaxed);
                            }
                        }
                        PublishMessage::Flush(reply) => {
                            let _ = reply.send(());
                        }
                        PublishMessage::Stop => break,
                    }
                }
            })
            .map_err(failure)?;

        writeln!(
            out,
            "Remote control is active for session {}.",
            guard.session_id
        )?;
        Ok(guard)
    }
}

impl Drop for RemoteSessionGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.publisher.stop();
        let _ = self.api.unregister_remote_session(&self.session_id);
    }
}

fn remote_listener(
    api: Arc<CortexClient>,
    session_id: String,
    cursor: Arc<AtomicU64>,
    sender: mpsc::Sender<RemoteEvent>,
    online: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    remote_listener_with(
        |after| api.remote_commands(&session_id, after),
        cursor,
        sender,
        online,
        stop,
    );
}

fn remote_listener_with<F>(
    mut connect: F,
    cursor: Arc<AtomicU64>,
    sender: mpsc::Sender<RemoteEvent>,
    online: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) where
    F: FnMut(u64) -> Result<Box<dyn Read + Send>, ChatFailure>,
{
    let mut attempt = 0;
    while !stop.load(Ordering::Relaxed) {
        let after = cursor.load(Ordering::Relaxed);
        let connected = connect(after);
        match connected {
            Ok(mut stream) => {
                online.store(true, Ordering::Relaxed);
                let mut decoder = crate::dal::remote::remote_sse::RemoteSseDecoder::default();
                let mut buffer = [0; 8192];
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => match decoder.feed(&buffer[..count]) {
                            Ok(events) => {
                                for event in events {
                                    if sender.send(event).is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(_) => break,
                        },
                    }
                }
                online.store(false, Ordering::Relaxed);
            }
            Err(_) => online.store(false, Ordering::Relaxed),
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(reconnect_delay(attempt));
        attempt = attempt.saturating_add(1);
    }
}

fn publish_chat_event(publisher: Option<&RemotePublisher>, event: &StreamEvent) {
    let Some(publisher) = publisher else { return };
    match event.name.as_str() {
        "stage" => {
            let mut progress = json!({
                "event": "stage",
                "message": event.message,
                "stage": event.data["stage"],
                "status": event.data["status"]
            });
            if let Some(expected) = event.data.get("expected_ms") {
                progress["expected_ms"] = expected.clone();
            }
            publisher.publish(RemoteEventKind::Progress, progress);
        }
        "step" => {
            let mut progress = json!({
                "event": "step",
                "message": event.message,
                "step": event.data["step"]
            });
            for key in ["progress", "elapsed_ms"] {
                if let Some(value) = event.data.get(key) {
                    progress[key] = value.clone();
                }
            }
            publisher.publish(RemoteEventKind::Progress, progress);
        }
        "notice" => publisher.publish(
            RemoteEventKind::Progress,
            json!({"event":"notice", "notice_number":event.data["notice_number"]}),
        ),
        "error" => publisher.publish(
            RemoteEventKind::TurnEnd,
            json!({"status":"error", "error_number":event.data["error_number"]}),
        ),
        "done" => publisher.publish(
            RemoteEventKind::TurnEnd,
            json!({
                "status":"complete",
                "session_id":event.data["session_id"],
                "output_id":event.data["output_id"],
                "total_ms":event.data["total_ms"]
            }),
        ),
        _ => {}
    }
}

fn spawn_turn<G: ChatApi + 'static>(
    api: Arc<G>,
    chat_id: Option<String>,
    prompt: String,
    file_ids: Vec<String>,
    publisher: Option<RemotePublisher>,
    sender: mpsc::Sender<Result<StreamEvent, ChatFailure>>,
) {
    thread::spawn(move || {
        let mut state = ChatState {
            chat_id: chat_id.clone(),
            ..Default::default()
        };
        let mut answer = String::new();
        let result = api
            .send_with_files(chat_id.as_deref(), &prompt, &file_ids)
            .and_then(|stream| {
                follow(api.as_ref(), stream, &mut state, |event| {
                    if event.name == "token" && publisher.is_some() {
                        answer.push_str(event.data["text"].as_str().unwrap_or(""));
                        if answer.len() >= 1024
                            && let Some(publisher) = publisher.as_ref()
                        {
                            publisher.publish(
                                RemoteEventKind::AnswerPart,
                                json!({"text":std::mem::take(&mut answer)}),
                            );
                        }
                    } else {
                        if !answer.is_empty()
                            && let Some(publisher) = publisher.as_ref()
                        {
                            publisher.publish(
                                RemoteEventKind::AnswerPart,
                                json!({"text":std::mem::take(&mut answer)}),
                            );
                        }
                        publish_chat_event(publisher.as_ref(), &event);
                    }
                    sender.send(Ok(event)).map_err(|_| ChatFailure::Connection)
                })
            });
        if !answer.is_empty()
            && let Some(publisher) = publisher.as_ref()
        {
            publisher.publish(RemoteEventKind::AnswerPart, json!({"text":answer}));
        }
        if result.is_err()
            && let Some(publisher) = publisher.as_ref()
        {
            publisher.publish(RemoteEventKind::TurnEnd, json!({"status":"incomplete"}));
        }
        if let Err(error) = result {
            let _ = sender.send(Err(error));
        }
    });
}

fn begin_turn<G: ChatApi + 'static>(
    api: Arc<G>,
    chat_id: Option<String>,
    prompt: String,
    file_ids: Vec<String>,
    publisher: Option<RemotePublisher>,
) -> mpsc::Receiver<Result<StreamEvent, ChatFailure>> {
    let (sender, receiver) = mpsc::channel();
    spawn_turn(api, chat_id, prompt, file_ids, publisher, sender);
    receiver
}

impl RemoteCommandQueue {
    fn enqueue(&mut self, event: RemoteEvent, acknowledged_cursor: u64) {
        if event.id > acknowledged_cursor && self.seen.insert(event.id) {
            self.pending.push_back(event);
        }
    }

    fn poll(
        &mut self,
        remote: &RemoteSessionGuard,
        state: &ChatState,
        controls: &mut TurnControl,
        out: &mut impl Write,
        surface: &mut Surface,
    ) -> io::Result<()> {
        while let Ok(event) = remote.receiver.try_recv() {
            self.enqueue(event, remote.cursor.load(Ordering::Relaxed));
        }
        if Instant::now() < self.retry_at {
            return Ok(());
        }
        while let Some(event) = self.pending.front() {
            if event.id <= remote.cursor.load(Ordering::Relaxed) {
                let event = self.pending.pop_front().expect("front exists");
                self.seen.remove(&event.id);
                continue;
            }
            let event = event.clone();
            let mut accepted_prompt = None;
            let status = match event.kind {
                RemoteEventKind::Prompt => match event.prompt() {
                    Ok((text, file_ids)) => {
                        accepted_prompt = Some((text.to_owned(), file_ids));
                        if controls.active {
                            "prompt_queued"
                        } else {
                            "prompt_received"
                        }
                    }
                    Err(_) => "invalid_prompt",
                },
                RemoteEventKind::Cancel => {
                    if controls.active {
                        if let (Some(chat), Some(output)) =
                            (state.chat_id.as_deref(), state.output_id.as_deref())
                        {
                            match remote.api.cancel(chat, output) {
                                Ok(()) => {
                                    controls.stop_requested = true;
                                    surface.text(out, "\nCancellation requested from phone.\n")?;
                                    "cancelled"
                                }
                                Err(_) => "cancel_failed",
                            }
                        } else {
                            controls.cancel_pending = true;
                            "cancel_pending"
                        }
                    } else {
                        "no_active_turn"
                    }
                }
                RemoteEventKind::ApprovalAnswer => {
                    if event.payload["approved"].as_bool().is_some()
                        && event.payload["approval_id"]
                            .as_str()
                            .is_some_and(|id| !id.trim().is_empty())
                    {
                        "approval_received"
                    } else {
                        "invalid_approval_answer"
                    }
                }
                RemoteEventKind::CommandExpired => "command_expired",
                _ => "unsupported_command",
            };
            if remote
                .api
                .acknowledge_remote_command(&remote.session_id, event.id, status)
                .is_err()
            {
                self.retry_at = Instant::now() + reconnect_delay(self.retry_attempt);
                self.retry_attempt = self.retry_attempt.saturating_add(1);
                break;
            }
            self.retry_attempt = 0;
            self.retry_at = Instant::now();
            remote.cursor.fetch_max(event.id, Ordering::Relaxed);
            self.pending.pop_front();
            self.seen.remove(&event.id);
            if status == "approval_received" {
                surface.text(
                    out,
                    if event.payload["approved"].as_bool() == Some(true) {
                        "\nPhone sent an approval response: approved.\n"
                    } else {
                        "\nPhone sent an approval response: denied.\n"
                    },
                )?;
            }
            if let Some(prompt) = accepted_prompt {
                self.prompts.push_back(prompt);
                if controls.active {
                    surface.text(out, "\nA remote prompt is queued.\n")?;
                }
            } else if status == "invalid_prompt" {
                surface.text(out, "\nIgnored an invalid remote prompt.\n")?;
            }
            if status == "cancel_failed" {
                surface.text(out, "\nThe phone cancellation request failed.\n")?;
            }
            if status == "cancel_pending" {
                surface.text(
                    out,
                    "\nPhone cancellation is waiting for output acceptance.\n",
                )?;
            }
        }
        Ok(())
    }
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
    let remote_api = options.remote_control.then(|| api.clone());
    interactive(api, chat, history, out, remote_api)
}

fn interactive<G: ChatApi + 'static>(
    api: Arc<G>,
    mut chat: Option<String>,
    history: Option<ChatPage>,
    out: &mut impl Write,
    remote_api: Option<Arc<CortexClient>>,
) -> io::Result<()> {
    let remote = remote_api
        .map(|remote_api| RemoteSessionGuard::start(remote_api, out))
        .transpose()?;
    let remote_publisher = remote.as_ref().map(|session| session.publisher.clone());
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
    let mut controls = TurnControl::default();
    let mut remote_commands = RemoteCommandQueue::default();
    let mut remote_online = true;
    let mut remote_delivery_reported = false;
    let mut dirty = true;
    if let (Some(chat_id), Some(output_id)) = (chat.clone(), running) {
        let (tx, rx) = mpsc::channel();
        receiver = Some(rx);
        started = Some(Instant::now());
        let api = api.clone();
        let publisher = remote_publisher.clone();
        thread::spawn(move || {
            let mut state = ChatState {
                chat_id: Some(chat_id.clone()),
                output_id: Some(output_id.clone()),
                ..Default::default()
            };
            let result = api.reconnect(&chat_id, &output_id, 0).and_then(|stream| {
                let mut answer = String::new();
                follow(api.as_ref(), stream, &mut state, |event| {
                    if event.name == "token" && publisher.is_some() {
                        answer.push_str(event.data["text"].as_str().unwrap_or(""));
                        if answer.len() >= 1024
                            && let Some(publisher) = publisher.as_ref()
                        {
                            publisher.publish(
                                RemoteEventKind::AnswerPart,
                                json!({"text":std::mem::take(&mut answer)}),
                            );
                        }
                    } else {
                        if !answer.is_empty()
                            && let Some(publisher) = publisher.as_ref()
                        {
                            publisher.publish(
                                RemoteEventKind::AnswerPart,
                                json!({"text":std::mem::take(&mut answer)}),
                            );
                        }
                        publish_chat_event(publisher.as_ref(), &event);
                    }
                    tx.send(Ok(event)).map_err(|_| ChatFailure::Connection)
                })
            });
            if let Err(error) = result {
                let _ = tx.send(Err(error));
            }
        });
    }
    loop {
        if let Some(remote) = remote.as_ref() {
            let online = remote.listener_online.load(Ordering::Relaxed);
            if online != remote_online {
                remote_online = online;
                surface.text(
                    out,
                    if online {
                        "\nPhone connection restored.\n"
                    } else {
                        "\nPhone connection interrupted. Reconnecting.\n"
                    },
                )?;
                dirty = true;
            }
            controls.active = receiver.is_some();
            remote_commands.poll(remote, &state, &mut controls, out, &mut surface)?;
            if remote.publisher.failed.load(Ordering::Relaxed) && !remote_delivery_reported {
                remote_delivery_reported = true;
                surface.text(out, "\nPhone updates stopped after a connection error.\n")?;
                dirty = true;
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
                            if controls.cancel_pending
                                && let (Some(chat_id), Some(output_id)) =
                                    (state.chat_id.as_deref(), state.output_id.as_deref())
                            {
                                match api.cancel(chat_id, output_id) {
                                    Ok(()) => {
                                        controls.cancel_pending = false;
                                        controls.stop_requested = true;
                                        surface
                                            .text(out, "\nCancellation requested from phone.\n")?;
                                    }
                                    Err(error) => {
                                        controls.cancel_pending = false;
                                        surface.text(out, &format!("\n{error}\n"))?;
                                    }
                                }
                            }
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
                controls.stop_requested = false;
                dirty = true;
            }
        }
        if receiver.is_none()
            && let Some((prompt, file_ids)) = remote_commands.prompts.pop_front()
        {
            surface.text(out, &format!("\n> {}\n", clean(&prompt)))?;
            state = ChatState {
                chat_id: chat.clone(),
                ..Default::default()
            };
            cleaner = TerminalText::default();
            started = Some(Instant::now());
            receiver = Some(begin_turn(
                api.clone(),
                chat.clone(),
                prompt,
                file_ids,
                remote_publisher.clone(),
            ));
            dirty = true;
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
                            if !controls.stop_requested {
                                api.cancel(chat, output).map_err(failure)?;
                                controls.stop_requested = true;
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
                        if let Some(remote) = remote.as_ref() {
                            remote.publisher.flush();
                        }
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
                        receiver = Some(begin_turn(
                            api.clone(),
                            chat.clone(),
                            text,
                            Vec::new(),
                            remote_publisher.clone(),
                        ));
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
    use std::{
        collections::VecDeque,
        io::{Cursor, Read},
    };
    pub(super) static DETAIL_STATES: std::sync::Mutex<Vec<bool>> =
        std::sync::Mutex::new(Vec::new());
    static CANCELLATIONS: std::sync::Mutex<Vec<(String, String)>> =
        std::sync::Mutex::new(Vec::new());

    struct AckWaitReader {
        frame: Cursor<Vec<u8>>,
        cursor: Arc<AtomicU64>,
    }

    impl Read for AckWaitReader {
        fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
            let count = self.frame.read(target)?;
            if count > 0 {
                return Ok(count);
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.cursor.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            Ok(0)
        }
    }

    #[test]
    fn remote_listener_reconnects_after_the_acknowledged_command_cursor() {
        let cursor = Arc::new(AtomicU64::new(0));
        let online = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let requested_cursors = Arc::new(std::sync::Mutex::new(Vec::new()));
        let requests = requested_cursors.clone();
        let reader_cursor = cursor.clone();
        let listener_cursor = cursor.clone();
        let reconnect_stop = stop.clone();
        let listener = thread::spawn(move || {
            remote_listener_with(
                move |after| {
                    let mut seen = requests.lock().unwrap();
                    seen.push(after);
                    if seen.len() == 1 {
                        let frame = "id: 7\ndata: {\"id\":7,\"kind\":\"prompt\",\"payload\":{\"prompt\":\"hello\",\"file_ids\":[]},\"created_at_ms\":7}\n\n";
                        Ok::<Box<dyn Read + Send>, ChatFailure>(Box::new(AckWaitReader {
                            frame: Cursor::new(frame.as_bytes().to_vec()),
                            cursor: reader_cursor.clone(),
                        }))
                    } else {
                        reconnect_stop.store(true, Ordering::Relaxed);
                        Ok(Box::new(Cursor::new(Vec::new())))
                    }
                },
                listener_cursor,
                sender,
                online,
                stop,
            );
        });

        let event = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(event.id, 7);
        cursor.store(event.id, Ordering::Relaxed);
        listener.join().unwrap();
        assert_eq!(*requested_cursors.lock().unwrap(), vec![0, 7]);
    }

    #[test]
    fn command_queue_deduplicates_replayed_prompts_across_acknowledged_cursor() {
        let make_prompt = |id| RemoteEvent {
            id,
            kind: RemoteEventKind::Prompt,
            payload: json!({"prompt":"hello", "file_ids":[]}),
            created_at_ms: id,
        };
        let mut queue = RemoteCommandQueue::default();
        queue.enqueue(make_prompt(1), 0);
        queue.enqueue(make_prompt(1), 0);
        assert_eq!(queue.pending.len(), 1);

        let accepted = queue.pending.pop_front().unwrap();
        queue.seen.remove(&accepted.id);
        queue.enqueue(make_prompt(1), 1);
        queue.enqueue(make_prompt(2), 1);
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(queue.pending.front().unwrap().id, 2);
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
        interactive(Arc::new(Fixture), None, None, &mut out, None).unwrap();
        assert_eq!(*DETAIL_STATES.lock().unwrap(), vec![true, false]);
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }
    #[test]
    #[ignore = "requires an interactive terminal; uses only in-memory fixture events"]
    fn terminal_cancel_fixture() {
        assert!(io::stdin().is_terminal());
        let mut out = io::stdout().lock();
        interactive(Arc::new(Fixture), None, None, &mut out, None).unwrap();
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
