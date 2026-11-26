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
            login_context, sse::SseFrameDecoder,
        },
    },
    domain::{
        models::{
            annotations::SelectedContext,
            chat::{ChatFailure, ChatPage, StreamEvent, valid_id},
            native_auth::{AuthFailure, Session},
            remote_control::{RemoteEvent, RemoteEventKind},
            tools::{ToolName, ToolResult},
        },
        services::{
            annotations::{selected_context, upload_context},
            auth::AuthService,
            chat::{ChatApi, ChatState, follow, latest_chat, owned_history, running_output},
            remote_control::reconnect_delay,
            telemetry::{Telemetry, TrackingApi, establish_consent},
            tools::execute_readonly,
        },
    },
};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    io::{self, IsTerminal, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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

fn complete_voice_job(
    job: VoiceJob,
    result: Result<String, crate::dal::local::voice::VoiceFailure>,
    editor: &mut Editor,
) -> Option<String> {
    let cancelled = job.cancel.load(Ordering::Relaxed);
    job.join_completed();
    if cancelled {
        return Some("Transcription cancelled; no text inserted.".into());
    }
    match result {
        Ok(text) => {
            insert_voice_transcript(editor, &text);
            None
        }
        Err(error) => Some(error.to_string()),
    }
}

fn cancel_voice_capture<T>(capture: &mut Option<T>) -> bool {
    capture.take().is_some()
}

fn start_recording<C>(
    recording: &mut Option<(C, Instant, bool)>,
    last_repeat: &mut Option<Instant>,
    capture: C,
    now: Instant,
    reports_key_releases: bool,
) {
    *recording = Some((capture, now, reports_key_releases));
    *last_repeat = (!reports_key_releases).then_some(now);
}

fn handle_space_repeat<C, E>(
    recording: &mut Option<(C, Instant, bool)>,
    pending_space: &mut Option<Instant>,
    last_repeat: &mut Option<Instant>,
    now: Instant,
    reports_key_releases: bool,
    start: impl FnOnce() -> Result<C, E>,
) -> Result<bool, E> {
    if recording.is_some() {
        repeat_recording(recording, last_repeat, now);
        return Ok(false);
    }
    if let Some(pending) = *pending_space {
        if now.duration_since(pending) >= KITTY_HOLD_THRESHOLD {
            let capture = start()?;
            start_recording(recording, last_repeat, capture, now, reports_key_releases);
            *pending_space = None;
            return Ok(true);
        }
    } else {
        *pending_space = Some(now);
    }
    Ok(false)
}

fn repeat_recording<C>(
    recording: &Option<(C, Instant, bool)>,
    last_repeat: &mut Option<Instant>,
    now: Instant,
) {
    if recording
        .as_ref()
        .is_some_and(|(_, _, hold_mode)| !hold_mode)
    {
        *last_repeat = Some(now);
    }
}

fn release_recording<C>(recording: &mut Option<(C, Instant, bool)>) -> Option<C> {
    if recording
        .as_ref()
        .is_some_and(|(_, _, hold_mode)| *hold_mode)
    {
        recording.take().map(|(capture, _, _)| capture)
    } else {
        None
    }
}

fn flush_pending_space(
    pending_space: &mut Option<Instant>,
    editor: &mut Editor,
    now: Instant,
    reports_key_releases: bool,
) -> bool {
    let Some(pending) = pending_space.take() else {
        return false;
    };
    if !reports_key_releases || now.duration_since(pending) < KITTY_HOLD_THRESHOLD {
        editor.insert(" ");
        true
    } else {
        false
    }
}

fn pause_recording<C, R, E>(
    recording: &mut Option<(C, Instant, bool)>,
    last_repeat: &mut Option<Instant>,
    now: Instant,
    transcribe: impl FnOnce(C) -> Result<R, E>,
) -> Option<Result<R, E>> {
    let due = recording.as_ref().is_some_and(|(_, _, hold_mode)| {
        !hold_mode
            && last_repeat.is_some_and(|last| now.duration_since(last) >= FALLBACK_RELEASE_TIMEOUT)
    });
    if !due {
        return None;
    }
    let (capture, _, _) = recording.take().expect("due capture exists");
    *last_repeat = None;
    Some(transcribe(capture))
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

enum PublishMessage {
    Event(RemoteEventKind, Value),
    Flush(mpsc::SyncSender<()>),
    Stop,
}

#[derive(Clone)]
struct RemoteTurnContext {
    state: Arc<Mutex<RemoteTurnState>>,
}

#[derive(Clone)]
struct RemoteTurnPublisher {
    publisher: RemotePublisher,
    context: Option<RemoteTurnContext>,
}

#[derive(Default)]
struct RemoteTurnState {
    turn_id: Option<String>,
    outcome: Option<RemoteTurnOutcome>,
}

enum RemoteTurnOutcome {
    Complete {
        session_id: Option<String>,
        output_id: Option<String>,
        total_ms: Option<u64>,
    },
    Error(String),
    Incomplete(String),
}

impl RemoteTurnContext {
    fn new(output_id: Option<&str>) -> Self {
        let turn_id = output_id.filter(|id| valid_id(id)).map(str::to_owned);
        Self {
            state: Arc::new(Mutex::new(RemoteTurnState {
                turn_id,
                outcome: None,
            })),
        }
    }

    fn observe_event(&self, event: &StreamEvent) -> Option<String> {
        let receipt = event.name == "stage"
            && event.data["stage"] == "accept"
            && event.data["status"] == "started";
        let output = if receipt || event.name == "done" {
            event.data["output_id"]
                .as_str()
                .filter(|id| valid_id(id))
                .map(str::to_owned)
        } else {
            None
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.turn_id.is_none() {
            state.turn_id = output;
        }
        state.turn_id.clone()
    }

    fn set_turn_id(&self, output_id: Option<&str>) {
        let Some(output_id) = output_id.filter(|id| valid_id(id)) else {
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.turn_id.is_none() {
            state.turn_id = Some(output_id.to_owned());
        }
    }

    fn turn_id(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .turn_id
            .clone()
    }

    fn record(&self, outcome: RemoteTurnOutcome) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.outcome.is_none() {
            state.outcome = Some(outcome);
        }
    }

    fn record_failure(&self, error: ChatFailure) {
        let message = error.to_string();
        match error {
            ChatFailure::Connection | ChatFailure::IncompleteStream => {
                self.record(RemoteTurnOutcome::Incomplete(message));
            }
            _ => self.record(RemoteTurnOutcome::Error(message)),
        }
    }

    fn take_terminal(&self) -> Option<Value> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let outcome = state.outcome.take()?;
        let mut payload = match outcome {
            RemoteTurnOutcome::Complete {
                session_id,
                output_id,
                total_ms,
            } => json!({
                "status": "complete",
                "session_id": session_id,
                "output_id": output_id,
                "total_ms": total_ms
            }),
            RemoteTurnOutcome::Error(message) => json!({
                "status": "error",
                "message": message
            }),
            RemoteTurnOutcome::Incomplete(message) => json!({
                "status": "incomplete",
                "message": message
            }),
        };
        if let Some(turn_id) = state.turn_id.as_ref() {
            payload["turn_id"] = json!(turn_id);
        }
        Some(payload)
    }
}

fn publish_turn_event(
    publisher: Option<&RemotePublisher>,
    kind: RemoteEventKind,
    mut payload: Value,
    turn_id: Option<&str>,
) {
    let Some(publisher) = publisher else { return };
    if let Some(turn_id) = turn_id.filter(|id| valid_id(id)) {
        payload["turn_id"] = json!(turn_id);
    }
    publisher.publish(kind, payload);
}

fn publish_turn_terminal(publisher: Option<&RemotePublisher>, context: Option<&RemoteTurnContext>) {
    let (Some(publisher), Some(context)) = (publisher, context) else {
        return;
    };
    if let Some(payload) = context.take_terminal() {
        publisher.publish(RemoteEventKind::TurnEnd, payload);
    }
}

fn cleanup_remote_turn(
    finish_pending: &mut bool,
    finished: bool,
    receiver: &mut Option<mpsc::Receiver<Result<StreamEvent, ChatFailure>>>,
    child_tracker: &mut Option<BoundChildTracker>,
    child_state: &mut Option<ChatState>,
    publisher: Option<&RemotePublisher>,
    context: Option<&RemoteTurnContext>,
) -> bool {
    if cleanup_finished_turn(
        finish_pending,
        finished,
        receiver,
        child_tracker,
        child_state,
    ) {
        publish_turn_terminal(publisher, context);
        true
    } else {
        false
    }
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
    commands: Arc<dyn RemoteCommandApi>,
    registered: bool,
    session_id: String,
    receiver: mpsc::Receiver<RemoteEvent>,
    cursor: Arc<AtomicU64>,
    listener_online: Arc<AtomicBool>,
    publisher: RemotePublisher,
    stop: Arc<AtomicBool>,
}

trait RemoteCommandApi: Send + Sync {
    fn chat_api(&self) -> &dyn ChatApi;
    fn cancel_output(&self, chat: &str, output: &str) -> Result<(), ChatFailure>;
    fn acknowledge_command(&self, session: &str, id: u64, status: &str) -> Result<(), ChatFailure>;
}

impl RemoteCommandApi for CortexClient {
    fn chat_api(&self) -> &dyn ChatApi {
        self
    }
    fn cancel_output(&self, chat: &str, output: &str) -> Result<(), ChatFailure> {
        self.cancel(chat, output)
    }
    fn acknowledge_command(&self, session: &str, id: u64, status: &str) -> Result<(), ChatFailure> {
        self.acknowledge_remote_command(session, id, status)
    }
}

#[derive(Default)]
struct TurnControl {
    active: bool,
    stop_requested: bool,
    cancel_pending: bool,
}

struct ActionWorker {
    cancel: Option<Arc<AtomicBool>>,
    result: Option<mpsc::Receiver<Result<String, crate::domain::services::tools::ToolError>>>,
    thread: Option<JoinHandle<()>>,
    retry_at: Instant,
    cancel_command: Option<u64>,
    owned_action_id: Option<String>,
    owned_call_id: Option<String>,
    owned_approval: Option<bool>,
}

impl Default for ActionWorker {
    fn default() -> Self {
        Self {
            cancel: None,
            result: None,
            thread: None,
            retry_at: Instant::now(),
            cancel_command: None,
            owned_action_id: None,
            owned_call_id: None,
            owned_approval: None,
        }
    }
}

impl ActionWorker {
    fn start(
        &mut self,
        request: crate::domain::models::tools::ToolRequest,
        preimage: crate::domain::services::tools::ActionPreimage,
        action_id: String,
        approved: bool,
    ) {
        self.cancel();
        self.join();
        let owned_call_id = request.call_id.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let result = crate::domain::services::tools::execute_approved_action(
                &request,
                &preimage,
                &worker_cancel,
            );
            let _ = sender.send(result);
        });
        self.cancel = Some(cancel);
        self.result = Some(receiver);
        self.thread = Some(handle);
        self.retry_at = Instant::now();
        self.owned_action_id = Some(action_id);
        self.owned_call_id = Some(owned_call_id);
        self.owned_approval = Some(approved);
    }

    fn owns_approval(&self, action_id: &str, call_id: &str, approved: bool) -> bool {
        self.owned_action_id.as_deref() == Some(action_id)
            && self.owned_call_id.as_deref() == Some(call_id)
            && self.owned_approval == Some(approved)
    }

    fn cancel(&self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Release);
        }
    }

    fn take_result(&mut self) -> Option<Result<String, crate::domain::services::tools::ToolError>> {
        let receiver = self.result.as_ref()?;
        match receiver.try_recv() {
            Ok(result) => {
                self.result = None;
                self.join();
                self.cancel = None;
                Some(result)
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.result = None;
                self.join();
                self.cancel = None;
                Some(Err(crate::domain::services::tools::ToolError::ReadFailed))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }

    fn join(&mut self) {
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ActionWorker {
    fn drop(&mut self) {
        self.cancel();
        self.join();
    }
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
            api: api.clone(),
            commands: api,
            registered: true,
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
        if self.registered {
            let _ = self.api.unregister_remote_session(&self.session_id);
        }
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

fn publish_chat_event(
    publisher: Option<&RemotePublisher>,
    context: Option<&RemoteTurnContext>,
    turn_id: Option<&str>,
    event: &StreamEvent,
) {
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
            publish_turn_event(publisher, RemoteEventKind::Progress, progress, turn_id);
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
            publish_turn_event(publisher, RemoteEventKind::Progress, progress, turn_id);
        }
        "notice" => publish_turn_event(
            publisher,
            RemoteEventKind::Progress,
            json!({"event":"notice", "notice_number":event.data["notice_number"]}),
            turn_id,
        ),
        "error" => {
            if let Some(context) = context {
                context.record(RemoteTurnOutcome::Error(event.message.clone()));
            }
            publish_turn_event(
                publisher,
                RemoteEventKind::Progress,
                json!({"stage":"parent_error","message":event.message}),
                turn_id,
            );
        }
        "done" => {
            if let Some(context) = context {
                context.record(RemoteTurnOutcome::Complete {
                    session_id: event.data["session_id"].as_str().map(str::to_owned),
                    output_id: event.data["output_id"].as_str().map(str::to_owned),
                    total_ms: event.data["total_ms"].as_u64(),
                });
            }
            publish_turn_event(
                publisher,
                RemoteEventKind::Progress,
                json!({"stage":"parent_complete","message":"Parent output completed; confirming the bound child."}),
                turn_id,
            );
        }
        _ => {}
    }
}

fn follow_remote_turn<G: ChatApi>(
    api: &G,
    stream: Box<dyn Read + Send>,
    state: &mut ChatState,
    publisher: Option<&RemotePublisher>,
    context: Option<&RemoteTurnContext>,
    mut on_event: impl FnMut(StreamEvent) -> Result<(), ChatFailure>,
) -> Result<(), ChatFailure> {
    let mut answer = String::new();
    let result = follow(api, stream, state, |event| {
        let turn_id = context.and_then(|context| context.observe_event(&event));
        if event.name == "token" && publisher.is_some() {
            answer.push_str(event.data["text"].as_str().unwrap_or(""));
            if answer.len() >= 1024 {
                publish_turn_event(
                    publisher,
                    RemoteEventKind::AnswerPart,
                    json!({"text":std::mem::take(&mut answer)}),
                    turn_id.as_deref(),
                );
            }
        } else {
            if !answer.is_empty() {
                publish_turn_event(
                    publisher,
                    RemoteEventKind::AnswerPart,
                    json!({"text":std::mem::take(&mut answer)}),
                    turn_id.as_deref(),
                );
            }
            publish_chat_event(publisher, context, turn_id.as_deref(), &event);
        }
        on_event(event)
    });
    if let Some(context) = context {
        context.set_turn_id(state.output_id.as_deref());
        if let Err(error) = result {
            context.record_failure(error);
            publish_turn_event(
                publisher,
                RemoteEventKind::Progress,
                json!({"stage":"parent_error","message":error.to_string()}),
                state.output_id.as_deref(),
            );
        }
    }
    if !answer.is_empty() {
        publish_turn_event(
            publisher,
            RemoteEventKind::AnswerPart,
            json!({"text":answer}),
            context.and_then(RemoteTurnContext::turn_id).as_deref(),
        );
    }
    result
}

fn spawn_turn<G: ChatApi + 'static>(
    api: Arc<G>,
    chat_id: Option<String>,
    prompt: String,
    file_ids: Vec<String>,
    remote_turn: Option<RemoteTurnPublisher>,
    telemetry: Option<Arc<Telemetry>>,
    sender: mpsc::Sender<Result<StreamEvent, ChatFailure>>,
) {
    thread::spawn(move || {
        let publisher = remote_turn.as_ref().map(|turn| &turn.publisher);
        let turn_context = remote_turn.as_ref().and_then(|turn| turn.context.as_ref());
        let mut state = ChatState {
            chat_id: chat_id.clone(),
            ..Default::default()
        };
        let result = api
            .send_tools_with_files(chat_id.as_deref(), &prompt, &file_ids)
            .and_then(|stream| {
                follow_remote_turn(
                    api.as_ref(),
                    stream,
                    &mut state,
                    publisher,
                    turn_context,
                    |event| sender.send(Ok(event)).map_err(|_| ChatFailure::Connection),
                )
            });
        if let Some(telemetry) = telemetry {
            telemetry.chat_result(result);
        }
        if let Err(error) = result {
            let _ = sender.send(Err(error));
        }
    });
}

fn begin_turn_with_context<G: ChatApi + 'static>(
    api: Arc<G>,
    chat_id: Option<String>,
    prompt: String,
    file_ids: Vec<String>,
    publisher: Option<RemotePublisher>,
    turn_context: Option<RemoteTurnContext>,
    telemetry: Option<Arc<Telemetry>>,
) -> mpsc::Receiver<Result<StreamEvent, ChatFailure>> {
    let (sender, receiver) = mpsc::channel();
    spawn_turn(
        api,
        chat_id,
        prompt,
        file_ids,
        publisher.map(|publisher| RemoteTurnPublisher {
            publisher,
            context: turn_context,
        }),
        telemetry,
        sender,
    );
    receiver
}

fn spawn_reconnect_turn<G: ChatApi + 'static>(
    api: Arc<G>,
    chat_id: String,
    output_id: String,
    publisher: Option<RemotePublisher>,
    turn_context: Option<RemoteTurnContext>,
    telemetry: Option<Arc<Telemetry>>,
    sender: mpsc::Sender<Result<StreamEvent, ChatFailure>>,
) {
    thread::spawn(move || {
        let mut state = ChatState {
            chat_id: Some(chat_id.clone()),
            output_id: Some(output_id.clone()),
            ..Default::default()
        };
        if let Some(context) = turn_context.as_ref() {
            context.set_turn_id(Some(&output_id));
        }
        let result = api.reconnect(&chat_id, &output_id, 0).and_then(|stream| {
            follow_remote_turn(
                api.as_ref(),
                stream,
                &mut state,
                publisher.as_ref(),
                turn_context.as_ref(),
                |event| sender.send(Ok(event)).map_err(|_| ChatFailure::Connection),
            )
        });
        if let Some(telemetry) = telemetry {
            telemetry.chat_result(result);
        }
        if let Err(error) = result {
            let _ = sender.send(Err(error));
        }
    });
}

enum BoundChildMessage {
    Bound(String, String),
    Output(StreamEvent),
    Lifecycle(StreamEvent),
    ConfirmedTerminal(String),
    Error(ChatFailure),
}

struct BoundChildTracker {
    receiver: mpsc::Receiver<BoundChildMessage>,
    stop: Arc<AtomicBool>,
    terminal: bool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildRunEvent {
    seq: u64,
    event_id: String,
    kind: String,
    #[serde(deserialize_with = "deserialize_present_option")]
    message: Option<String>,
    #[serde(deserialize_with = "deserialize_present_option")]
    result: Option<String>,
    #[serde(deserialize_with = "deserialize_present_option")]
    error_code: Option<String>,
    #[serde(deserialize_with = "deserialize_present_option")]
    action_id: Option<String>,
    #[serde(deserialize_with = "deserialize_present_option")]
    action_kind: Option<String>,
    #[serde(deserialize_with = "deserialize_present_option")]
    payload_digest: Option<String>,
    #[serde(deserialize_with = "deserialize_present_option")]
    display: Option<String>,
    #[serde(deserialize_with = "deserialize_present_option")]
    approved: Option<bool>,
    created_at: String,
}

fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer)
}

fn decode_child_lifecycle_frame(
    frame: crate::dal::remote::sse::SseFrame,
    after: u64,
) -> Result<StreamEvent, ChatFailure> {
    if frame.name == "error" {
        return Err(ChatFailure::Connection);
    }
    let event: ChildRunEvent =
        serde_json::from_slice(&frame.data).map_err(|_| ChatFailure::InvalidResponse)?;
    let valid_kind = matches!(
        event.kind.as_str(),
        "queued"
            | "progress"
            | "completed"
            | "failed"
            | "cancel_requested"
            | "cancelled"
            | "action_requested"
            | "action_answered"
    );
    if frame.id != Some(event.seq)
        || event.seq <= after
        || frame.name != event.kind
        || !valid_kind
        || !valid_id(&event.event_id)
        || event.created_at.is_empty()
        || event.created_at.len() > 64
        || event
            .message
            .as_ref()
            .is_some_and(|value| value.len() > 64_000)
        || event
            .result
            .as_ref()
            .is_some_and(|value| value.len() > 64_000)
        || event
            .error_code
            .as_ref()
            .is_some_and(|value| value.len() > 128)
        || event
            .action_id
            .as_ref()
            .is_some_and(|value| !valid_id(value))
        || event
            .action_kind
            .as_ref()
            .is_some_and(|value| value.len() > 128)
        || event
            .payload_digest
            .as_ref()
            .is_some_and(|value| value.len() > 128)
        || event
            .display
            .as_ref()
            .is_some_and(|value| value.len() > 64_000)
    {
        return Err(ChatFailure::InvalidResponse);
    }
    let message = event.message.clone().unwrap_or_default();
    let data = json!({
        "seq": event.seq,
        "event_id": event.event_id,
        "kind": event.kind,
        "message": event.message,
        "result": event.result,
        "error_code": event.error_code,
        "action_id": event.action_id,
        "action_kind": event.action_kind,
        "payload_digest": event.payload_digest,
        "display": event.display,
        "approved": event.approved,
        "created_at": event.created_at,
    });
    Ok(StreamEvent {
        id: frame.id,
        name: frame.name,
        message,
        data,
    })
}

#[derive(Default)]
struct ParentEventPump {
    disconnected: bool,
    failed: bool,
}

enum ParentEventPoll {
    Event(Result<StreamEvent, ChatFailure>),
    Empty,
    WaitingForChild,
    Disconnected,
    Failed,
}

impl ParentEventPump {
    fn poll(
        &mut self,
        receiver: &mpsc::Receiver<Result<StreamEvent, ChatFailure>>,
        child_live: bool,
    ) -> ParentEventPoll {
        if self.failed {
            return if child_live {
                ParentEventPoll::WaitingForChild
            } else {
                ParentEventPoll::Failed
            };
        }
        if self.disconnected {
            return if child_live {
                ParentEventPoll::WaitingForChild
            } else {
                ParentEventPoll::Disconnected
            };
        }
        match receiver.try_recv() {
            Ok(event) => ParentEventPoll::Event(event),
            Err(mpsc::TryRecvError::Empty) => ParentEventPoll::Empty,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.disconnected = true;
                if child_live {
                    ParentEventPoll::WaitingForChild
                } else {
                    ParentEventPoll::Disconnected
                }
            }
        }
    }

    fn fail(&mut self, child_live: bool) -> ParentEventPoll {
        self.failed = true;
        if child_live {
            ParentEventPoll::WaitingForChild
        } else {
            ParentEventPoll::Failed
        }
    }
}

fn verify_child_binding(
    detail: &crate::domain::services::chat::BoundChildDetail,
    child_id: &str,
    parent_chat: &str,
    parent_output: &str,
    parent_call: &str,
) -> Result<(), ChatFailure> {
    if detail.child_id != child_id
        || detail.parent_chat_session_id.as_deref() != Some(parent_chat)
        || detail.parent_output_id.as_deref() != Some(parent_output)
        || detail.parent_call_id.as_deref() != Some(parent_call)
    {
        return Err(ChatFailure::InvalidResponse);
    }
    Ok(())
}

fn confirm_child_terminal(
    api: &dyn ChatApi,
    remote_session: &str,
    child_id: &str,
    parent_chat: &str,
    parent_output: &str,
    parent_call: &str,
    sender: &mpsc::Sender<BoundChildMessage>,
) -> bool {
    let Ok(detail) = api.bound_child_detail(remote_session, child_id) else {
        return false;
    };
    if verify_child_binding(&detail, child_id, parent_chat, parent_output, parent_call).is_err()
        || !matches!(detail.status.as_str(), "completed" | "failed" | "cancelled")
    {
        return false;
    }
    sender
        .send(BoundChildMessage::ConfirmedTerminal(detail.status))
        .is_ok()
}

fn observe_child_lifecycle(tracker: &mut BoundChildTracker, event: &StreamEvent) -> bool {
    if matches!(event.name.as_str(), "completed" | "failed" | "cancelled") {
        tracker.terminal = true;
        true
    } else {
        false
    }
}

fn parent_can_finish(parent_done_pending: bool, tracker: Option<&BoundChildTracker>) -> bool {
    parent_done_pending && tracker.is_none_or(|tracker| tracker.terminal)
}

fn cleanup_finished_turn(
    finish_pending: &mut bool,
    finished: bool,
    receiver: &mut Option<mpsc::Receiver<Result<StreamEvent, ChatFailure>>>,
    child_tracker: &mut Option<BoundChildTracker>,
    child_state: &mut Option<ChatState>,
) -> bool {
    *finish_pending |= finished;
    if !*finish_pending
        || child_tracker
            .as_ref()
            .is_some_and(|tracker| !tracker.terminal)
    {
        return false;
    }
    *finish_pending = false;
    *receiver = None;
    *child_tracker = None;
    *child_state = None;
    true
}

impl Drop for BoundChildTracker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn begin_bound_child_tracker<G: ChatApi + 'static>(
    api: Arc<G>,
    remote_session: String,
    child_id: String,
    parent_chat: String,
    parent_output: String,
    parent_call: String,
) -> BoundChildTracker {
    let (sender, receiver) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let lifecycle_api = api.clone();
    let lifecycle_sender = sender.clone();
    let lifecycle_remote = remote_session.clone();
    let lifecycle_child = child_id.clone();
    let lifecycle_parent_chat = parent_chat.clone();
    let lifecycle_parent_output = parent_output.clone();
    let lifecycle_parent_call = parent_call.clone();
    let lifecycle_stop = stop.clone();
    thread::spawn(move || {
        let mut after = 0;
        let mut reported_error = false;
        while !lifecycle_stop.load(Ordering::Acquire) {
            let mut stream = match lifecycle_api.bound_child_events(
                &lifecycle_remote,
                &lifecycle_child,
                after,
            ) {
                Ok(stream) => stream,
                Err(error) => {
                    if !reported_error {
                        let _ = lifecycle_sender.send(BoundChildMessage::Error(error));
                        reported_error = true;
                    }
                    if confirm_child_terminal(
                        lifecycle_api.as_ref(),
                        &lifecycle_remote,
                        &lifecycle_child,
                        &lifecycle_parent_chat,
                        &lifecycle_parent_output,
                        &lifecycle_parent_call,
                        &lifecycle_sender,
                    ) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
            };
            let mut decoder = SseFrameDecoder::default();
            let mut buffer = [0u8; 8192];
            'stream: while !lifecycle_stop.load(Ordering::Acquire) {
                let count = match stream.read(&mut buffer) {
                    Ok(count) => count,
                    Err(_) => {
                        if !reported_error {
                            let _ = lifecycle_sender
                                .send(BoundChildMessage::Error(ChatFailure::Connection));
                            reported_error = true;
                        }
                        break;
                    }
                };
                if count == 0 {
                    break;
                }
                let frames = match decoder.feed(&buffer[..count]) {
                    Ok(frames) => frames,
                    Err(error) => {
                        if !reported_error {
                            let _ = lifecycle_sender.send(BoundChildMessage::Error(error));
                            reported_error = true;
                        }
                        break;
                    }
                };
                for frame in frames {
                    let event = match decode_child_lifecycle_frame(frame, after) {
                        Ok(event) => event,
                        Err(error) => {
                            if !reported_error {
                                let _ = lifecycle_sender.send(BoundChildMessage::Error(error));
                                reported_error = true;
                            }
                            break 'stream;
                        }
                    };
                    after = event.id.unwrap_or(after);
                    let terminal =
                        matches!(event.name.as_str(), "completed" | "failed" | "cancelled");
                    if lifecycle_sender
                        .send(BoundChildMessage::Lifecycle(event))
                        .is_err()
                    {
                        return;
                    }
                    if terminal {
                        return;
                    }
                }
            }
            if lifecycle_stop.load(Ordering::Acquire) {
                return;
            }
            if confirm_child_terminal(
                lifecycle_api.as_ref(),
                &lifecycle_remote,
                &lifecycle_child,
                &lifecycle_parent_chat,
                &lifecycle_parent_output,
                &lifecycle_parent_call,
                &lifecycle_sender,
            ) {
                return;
            }
            thread::sleep(Duration::from_millis(250));
        }
    });

    let detail_stop = stop.clone();
    thread::spawn(move || {
        let result = (|| {
            let deadline = Instant::now() + Duration::from_secs(45);
            let (chat_id, output_id) = loop {
                if detail_stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                let detail = match api.bound_child_detail(&remote_session, &child_id) {
                    Ok(detail) => detail,
                    Err(ChatFailure::Connection) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(250));
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                verify_child_binding(
                    &detail,
                    &child_id,
                    &parent_chat,
                    &parent_output,
                    &parent_call,
                )?;
                match (
                    detail.child_chat_session_id,
                    detail.child_output_id,
                    detail.status.as_str(),
                ) {
                    (Some(chat), Some(output), _) => break (chat, output),
                    (None, None, "completed" | "failed" | "cancelled") => return Ok(()),
                    (None, None, _) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(250));
                    }
                    _ => return Err(ChatFailure::InvalidResponse),
                }
            };
            if !valid_id(&chat_id) || !valid_id(&output_id) {
                return Err(ChatFailure::InvalidResponse);
            }
            sender
                .send(BoundChildMessage::Bound(chat_id.clone(), output_id.clone()))
                .map_err(|_| ChatFailure::Connection)?;
            let mut state = ChatState {
                chat_id: Some(chat_id.clone()),
                output_id: Some(output_id.clone()),
                ..Default::default()
            };
            let stream = api.reconnect(&chat_id, &output_id, 0)?;
            follow(api.as_ref(), stream, &mut state, |event| {
                sender
                    .send(BoundChildMessage::Output(event))
                    .map_err(|_| ChatFailure::Connection)
            })
        })();
        if let Err(error) = result {
            let _ = sender.send(BoundChildMessage::Error(error));
        }
    });
    BoundChildTracker {
        receiver,
        stop,
        terminal: false,
    }
}

fn submit_tool_call(
    api: &dyn ChatApi,
    state: &mut ChatState,
    remote_session: Option<&str>,
    allow_child_creation: bool,
) -> Result<(), ChatFailure> {
    let request = state
        .tool_request
        .take()
        .ok_or(ChatFailure::InvalidResponse)?;
    let (chat, output) = match (state.chat_id.as_deref(), state.output_id.as_deref()) {
        (Some(chat), Some(output)) => (chat, output),
        _ => return Err(ChatFailure::InvalidResponse),
    };
    if allow_child_creation
        && request.name == ToolName::InvokeSubagent
        && let Some(remote_session) = remote_session
    {
        let child_id = api.register_bound_child(remote_session, chat, output, &request.call_id)?;
        state.child_id = Some(child_id);
        state.child_call_id = Some(request.call_id);
        return Ok(());
    }
    let output_text = match execute_readonly(&request) {
        Ok(Some(output)) => output,
        Ok(None) => match request.name {
            ToolName::InvokeSubagent => {
                if allow_child_creation {
                    "Not executed: an active owner-bound session is required for child work.".into()
                } else {
                    "Not executed: nested child delegation is outside this bounded runner.".into()
                }
            }
            ToolName::ReplaceFileContent | ToolName::RunCommand => {
                if let Some(remote_session) = remote_session {
                    let preimage = match crate::domain::services::tools::prepare_action(&request) {
                        Ok(preimage) => preimage,
                        Err(error) => {
                            api.submit_tool_result(
                                chat,
                                output,
                                &ToolResult {
                                    call_id: request.call_id,
                                    name: request.name,
                                    output: format!("Not executed: {error}"),
                                },
                            )?;
                            return Ok(());
                        }
                    };
                    match api.create_output_action(remote_session, chat, output, &request) {
                        Ok(receipt) if receipt.matches(&request) && receipt.status == "pending" => {
                            state.pending_action =
                                Some(crate::domain::services::chat::PendingOutputAction {
                                    request,
                                    receipt,
                                    preimage,
                                    completed_result: None,
                                });
                            return Ok(());
                        }
                        _ => {
                            "Not executed: a verified output action could not be registered.".into()
                        }
                    }
                } else {
                    "Not executed: an active owner-bound session is required for approval.".into()
                }
            }
            ToolName::ViewFile | ToolName::GrepSearch => {
                return Err(ChatFailure::InvalidResponse);
            }
        },
        Err(error) => error.to_string(),
    };
    if output_text.len() > crate::domain::models::tools::MAX_TOOL_RESULT_BYTES {
        return Err(ChatFailure::InvalidResponse);
    }
    api.submit_tool_result(
        chat,
        output,
        &ToolResult {
            call_id: request.call_id,
            name: request.name,
            output: output_text,
        },
    )
}

fn answer_pending_action(
    api: &dyn ChatApi,
    state: &mut ChatState,
    remote_session: &str,
    approved: bool,
    worker: &mut ActionWorker,
) -> Result<(), ChatFailure> {
    let pending = state
        .pending_action
        .as_ref()
        .cloned()
        .ok_or(ChatFailure::InvalidResponse)?;
    let (chat, output) = match (state.chat_id.clone(), state.output_id.clone()) {
        (Some(chat), Some(output)) => (chat, output),
        _ => return Err(ChatFailure::InvalidResponse),
    };
    if let Some(result) = pending.completed_result {
        api.submit_tool_result(
            &chat,
            &output,
            &ToolResult {
                call_id: pending.request.call_id,
                name: pending.request.name,
                output: result,
            },
        )?;
        state.pending_action = None;
        return Ok(());
    }
    let current =
        api.get_output_action(remote_session, &chat, &output, &pending.receipt.action_id)?;
    if !current.matches(&pending.request) {
        return Err(ChatFailure::InvalidResponse);
    }
    if current.status == "claimed"
        && worker.owns_approval(
            &pending.receipt.action_id,
            &pending.request.call_id,
            approved,
        )
    {
        return Ok(());
    }
    let effective_approval =
        effective_approval(&current.status, approved).ok_or(ChatFailure::InvalidResponse)?;
    let answered = if current.status == "pending" {
        api.answer_output_action(
            remote_session,
            &chat,
            &output,
            &pending.receipt.action_id,
            effective_approval,
        )?
    } else {
        current
    };
    if !answered.matches(&pending.request)
        || answered.status
            != if effective_approval {
                "approved"
            } else {
                "denied"
            }
    {
        return Err(ChatFailure::InvalidResponse);
    }
    let output_text = if effective_approval {
        let claimed = api.claim_output_action(
            remote_session,
            &chat,
            &output,
            &pending.receipt.action_id,
            &pending.request,
        )?;
        if !claimed.matches(&pending.request) || claimed.status != "claimed" {
            return Err(ChatFailure::InvalidResponse);
        }
        worker.start(
            pending.request.clone(),
            pending.preimage.clone(),
            pending.receipt.action_id.clone(),
            true,
        );
        return Ok(());
    } else {
        "Not executed: approval was denied.".into()
    };
    let action = state
        .pending_action
        .as_mut()
        .ok_or(ChatFailure::InvalidResponse)?;
    action.completed_result = Some(output_text);
    submit_completed_action_result(api, state, &chat, &output, worker)
}

fn submit_completed_action_result(
    api: &dyn ChatApi,
    state: &mut ChatState,
    chat: &str,
    output: &str,
    worker: &mut ActionWorker,
) -> Result<(), ChatFailure> {
    let pending = state
        .pending_action
        .as_ref()
        .ok_or(ChatFailure::InvalidResponse)?;
    let result = pending
        .completed_result
        .as_ref()
        .ok_or(ChatFailure::InvalidResponse)?;
    api.submit_tool_result(
        chat,
        output,
        &ToolResult {
            call_id: pending.request.call_id.clone(),
            name: pending.request.name,
            output: result.clone(),
        },
    )?;
    state.pending_action = None;
    worker.retry_at = Instant::now();
    Ok(())
}

fn poll_action_worker(
    api: &impl ChatApi,
    state: &mut ChatState,
    worker: &mut ActionWorker,
    turn_id: Option<&str>,
    publisher: Option<&RemotePublisher>,
) {
    if state.pending_action.is_none() {
        return;
    }
    if let Some(result) = worker.take_result() {
        match result {
            Ok(result) => {
                if let Some(action) = state.pending_action.as_mut() {
                    action.completed_result = Some(result);
                }
            }
            Err(crate::domain::services::tools::ToolError::ActionCancelled) => {
                if let (Some(command_id), Some(publisher)) =
                    (worker.cancel_command.take(), publisher)
                {
                    publish_turn_event(
                        Some(publisher),
                        RemoteEventKind::Progress,
                        json!({"status":"cancelled","command_id":command_id}),
                        turn_id,
                    );
                }
                state.pending_action = None;
                return;
            }
            Err(crate::domain::services::tools::ToolError::ActionTimedOut) => {
                if let Some(action) = state.pending_action.as_mut() {
                    action.completed_result = Some(
                        "Command was stopped at its time limit; it may have partial side effects. It will not be rerun automatically.".into(),
                    );
                }
            }
            Err(crate::domain::services::tools::ToolError::ActionOutcomeUnknown) => {
                if let Some(action) = state.pending_action.as_mut() {
                    action.completed_result = Some(
                        "The approved action may have a partial effect. Its result will be reported without running it again.".into(),
                    );
                }
            }
            Err(error) => {
                if let Some(action) = state.pending_action.as_mut() {
                    action.completed_result = Some(format!("Not executed: {error}"));
                }
            }
        }
    }
    if worker.result.is_some() || Instant::now() < worker.retry_at {
        return;
    }
    let Some((chat, output)) = state.chat_id.clone().zip(state.output_id.clone()) else {
        return;
    };
    if state
        .pending_action
        .as_ref()
        .is_some_and(|action| action.completed_result.is_some())
    {
        match submit_completed_action_result(api, state, &chat, &output, worker) {
            Ok(()) => {}
            Err(_) => worker.retry_at = Instant::now() + Duration::from_secs(1),
        }
    }
}

fn effective_approval(status: &str, requested: bool) -> Option<bool> {
    match status {
        "approved" => Some(true),
        "denied" => Some(false),
        "pending" => Some(requested),
        _ => None,
    }
}

impl RemoteCommandQueue {
    fn enqueue(&mut self, event: RemoteEvent, acknowledged_cursor: u64) {
        if event.id > acknowledged_cursor && self.seen.insert(event.id) {
            self.pending.push_back(event);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn poll(
        &mut self,
        remote: &RemoteSessionGuard,
        state: &mut ChatState,
        mut child_state: Option<&mut ChatState>,
        controls: &mut TurnControl,
        worker: &mut ActionWorker,
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
                            worker.cancel();
                            match remote.commands.cancel_output(chat, output) {
                                Ok(()) => {
                                    controls.stop_requested = true;
                                    worker.cancel_command = Some(event.id);
                                    surface.text(out, "\nCancellation requested from phone.\n")?;
                                    "cancel_requested"
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
                    let decision = event.payload["approved"].as_bool();
                    let action_id = event.payload["approval_id"].as_str();
                    match (decision, action_id) {
                        (Some(approved), Some(action_id)) => {
                            let child_matches = child_state
                                .as_ref()
                                .and_then(|child| child.pending_action.as_ref())
                                .is_some_and(|pending| action_id == pending.receipt.action_id);
                            let parent_matches = state
                                .pending_action
                                .as_ref()
                                .is_some_and(|pending| action_id == pending.receipt.action_id);
                            if child_matches || parent_matches {
                                let target = if child_matches {
                                    child_state.as_deref_mut().expect("matched child action")
                                } else {
                                    &mut *state
                                };
                                match answer_pending_action(
                                    remote.commands.chat_api(),
                                    target,
                                    &remote.session_id,
                                    approved,
                                    worker,
                                ) {
                                    Ok(()) => "approval_received",
                                    Err(_) => "approval_failed",
                                }
                            } else if state.pending_action.is_none()
                                && child_state
                                    .as_ref()
                                    .is_none_or(|child| child.pending_action.is_none())
                                && worker.owned_action_id.as_deref() == Some(action_id)
                                && worker.owned_approval == Some(approved)
                            {
                                // The exact locally claimed action already received this decision.
                                "approval_received"
                            } else {
                                "stale_approval_answer"
                            }
                        }
                        _ => "invalid_approval_answer",
                    }
                }
                RemoteEventKind::CommandExpired => "command_expired",
                _ => "unsupported_command",
            };
            if status == "approval_failed" {
                self.retry_at = Instant::now() + reconnect_delay(self.retry_attempt);
                self.retry_attempt = self.retry_attempt.saturating_add(1);
                break;
            }
            if remote
                .commands
                .acknowledge_command(&remote.session_id, event.id, status)
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
    if !options.note_ids.is_empty() && !settings::ANNOTATIONS_SOURCE_ENABLED {
        return Err(failure(ChatFailure::SourceUnavailable));
    }
    let signed_in = session()?;
    let context = if options.note_ids.is_empty() {
        SelectedContext {
            text: String::new(),
            omissions: Vec::new(),
        }
    } else {
        let notes = AnnotationsClient::new(signed_in.clone()).map_err(failure)?;
        selected_context(&notes, &options.note_ids).map_err(failure)?
    };
    if let Some(notice) = context.omission_notice() {
        writeln!(out, "{notice}")?;
        if options.prompt.is_some() {
            return Err(failure(ChatFailure::PartialNoteContextNonInteractive));
        }
        writeln!(
            out,
            "Only selected typed text and note titles will be attached."
        )?;
        out.flush()?;
    }
    let telemetry = telemetry_for(&signed_in);
    let api = Arc::new(CortexClient::new(signed_in).map_err(failure)?);
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
        let files = upload_context(&context, false, |document| {
            api.upload_temporary_text("selected-annotations.txt", document)
        })
        .map_err(failure)?;
        let mut state = ChatState {
            chat_id: chat.clone(),
            ..Default::default()
        };
        let mut text = TerminalText::default();
        let stream = api
            .send_with_files(chat.as_deref(), &prompt, &files)
            .map_err(failure)?;
        let result = follow(api.as_ref(), stream, &mut state, |event| {
            if event.name == "token" {
                out.write_all(
                    text.push(event.data["text"].as_str().unwrap_or(""))
                        .as_bytes(),
                )
                .map_err(|_| ChatFailure::Connection)?;
                out.flush().map_err(|_| ChatFailure::Connection)?;
            }
            Ok(())
        });
        if let Some(telemetry) = telemetry {
            telemetry.chat_result(result);
        }
        result.map_err(failure)?;
        return writeln!(out);
    }
    let remote_api = options.remote_control.then(|| api.clone());
    interactive(api, chat, history, telemetry, out, remote_api, context)
}

fn selected_context_files_for_submission(
    api: &dyn ChatApi,
    context: &SelectedContext,
    prompt: &str,
    uploaded: &mut bool,
) -> Result<Vec<String>, ChatFailure> {
    if prompt.trim().is_empty() || *uploaded {
        return Ok(Vec::new());
    }
    let files = upload_context(context, true, |document| {
        api.upload_temporary_text("selected-annotations.txt", document)
    })?;
    *uploaded = true;
    Ok(files)
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
    telemetry: Option<Arc<Telemetry>>,
    out: &mut impl Write,
    remote_api: Option<Arc<CortexClient>>,
    context: SelectedContext,
) -> io::Result<()> {
    let remote = remote_api
        .map(|remote_api| RemoteSessionGuard::start(remote_api, out))
        .transpose()?;
    let remote_publisher = remote.as_ref().map(|session| session.publisher.clone());
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
    let mut active_remote_turn = remote_publisher
        .as_ref()
        .map(|_| RemoteTurnContext::new(running.as_deref()));
    let mut editor = Editor::default();
    let mut context_uploaded = context.text.is_empty();
    let mut expanded = false;
    let mut receiver: Option<mpsc::Receiver<Result<StreamEvent, ChatFailure>>> = None;
    let mut parent_pump = ParentEventPump::default();
    let mut child_tracker: Option<BoundChildTracker> = None;
    let mut child_state: Option<ChatState> = None;
    let mut watched_child: Option<String> = None;
    let mut parent_done_pending = false;
    let mut finish_pending = false;
    let mut started: Option<Instant> = None;
    let mut cleaner = TerminalText::default();
    let mut controls = TurnControl::default();
    let mut remote_commands = RemoteCommandQueue::default();
    let mut action_worker = ActionWorker::default();
    let mut remote_online = true;
    let mut remote_delivery_reported = false;
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
        parent_pump = ParentEventPump::default();
        started = Some(Instant::now());
        spawn_reconnect_turn(
            api.clone(),
            chat_id,
            output_id,
            remote_publisher.clone(),
            active_remote_turn.clone(),
            telemetry.clone(),
            tx,
        );
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
            remote_commands.poll(
                remote,
                &mut state,
                child_state.as_mut(),
                &mut controls,
                &mut action_worker,
                out,
                &mut surface,
            )?;
            if remote.publisher.failed.load(Ordering::Relaxed) && !remote_delivery_reported {
                remote_delivery_reported = true;
                surface.text(out, "\nPhone updates stopped after a connection error.\n")?;
                dirty = true;
            }
        }
        let active_turn_id = active_remote_turn
            .as_ref()
            .and_then(RemoteTurnContext::turn_id);
        poll_action_worker(
            api.as_ref(),
            &mut state,
            &mut action_worker,
            active_turn_id.as_deref(),
            remote.as_ref().map(|remote| &remote.publisher),
        );
        if let Some(child_state) = child_state.as_mut() {
            poll_action_worker(
                api.as_ref(),
                child_state,
                &mut action_worker,
                active_turn_id.as_deref(),
                remote.as_ref().map(|remote| &remote.publisher),
            );
        }
        let child_messages = child_tracker
            .as_ref()
            .map(|tracker| {
                let mut messages = Vec::new();
                while let Ok(message) = tracker.receiver.try_recv() {
                    messages.push(message);
                }
                messages
            })
            .unwrap_or_default();
        for message in child_messages {
            match message {
                BoundChildMessage::Bound(chat, output) => {
                    child_state = Some(ChatState {
                        chat_id: Some(chat),
                        output_id: Some(output),
                        ..Default::default()
                    });
                }
                BoundChildMessage::Output(event) => {
                    if let Some(child_state) = child_state.as_mut()
                        && child_state.apply(&event).unwrap_or(false)
                    {
                        if event.name == "token" {
                            surface.text(
                                out,
                                &format!(
                                    "\nChild: {}\n",
                                    clean(event.data["text"].as_str().unwrap_or(""))
                                ),
                            )?;
                        }
                        if event.name == "tool_call" {
                            if let Err(error) = submit_tool_call(
                                api.as_ref(),
                                child_state,
                                remote.as_ref().map(|session| session.session_id.as_str()),
                                false,
                            ) {
                                surface.text(out, &format!("\nChild tool failed: {error}\n"))?;
                                child_state.pending_action = None;
                            } else if let Some(action) = child_state.pending_action.as_ref() {
                                surface.text(
                                    out,
                                    &format!(
                                        "\nChild approval requested:\n{}\nPress Y to approve or N to deny.\n",
                                        clean(&action.receipt.display)
                                    ),
                                )?;
                            }
                        }
                        dirty = true;
                    }
                }
                BoundChildMessage::Lifecycle(event) => {
                    let message = event.data["message"].as_str().unwrap_or("");
                    match event.name.as_str() {
                        "progress" if !message.is_empty() => {
                            surface.text(out, &format!("\nChild: {}\n", clean(message)))?;
                        }
                        "completed" => {
                            surface.text(out, "\nChild task completed.\n")?;
                            if let Some(tracker) = child_tracker.as_mut() {
                                observe_child_lifecycle(tracker, &event);
                            }
                        }
                        "failed" => {
                            surface.text(out, "\nChild task failed.\n")?;
                            if let Some(tracker) = child_tracker.as_mut() {
                                observe_child_lifecycle(tracker, &event);
                            }
                        }
                        "cancelled" => {
                            surface.text(out, "\nChild stopped after confirmed cancellation.\n")?;
                            if let Some(tracker) = child_tracker.as_mut() {
                                observe_child_lifecycle(tracker, &event);
                            }
                        }
                        _ => {}
                    }
                    dirty = true;
                }
                BoundChildMessage::ConfirmedTerminal(status) => {
                    if let Some(tracker) = child_tracker.as_mut() {
                        tracker.terminal = true;
                    }
                    surface.text(
                        out,
                        &format!("\nChild status confirmed by Cortex: {status}.\n"),
                    )?;
                    dirty = true;
                }
                BoundChildMessage::Error(error) => {
                    surface.text(out, &format!("\nChild stream error: {error}\n"))?;
                    dirty = true;
                }
            }
        }
        let now = Instant::now();
        if let Some(pending) = pending_space {
            if reports_key_releases && now.duration_since(pending) >= KITTY_HOLD_THRESHOLD {
                pending_space = None;
                match crate::dal::local::voice::Recorder::start() {
                    Ok(recorder) => {
                        start_recording(&mut recording, &mut last_repeat, recorder, now, true)
                    }
                    Err(error) => voice_status = Some(error.to_string()),
                }
            } else if !reports_key_releases && now.duration_since(pending) >= FALLBACK_TAP_TIMEOUT {
                pending_space = None;
                editor.insert(" ");
                dirty = true;
            }
        }
        if let Some((_, started_at, _)) = recording.as_ref() {
            voice_status = Some(format!(
                "● Recording locally  {:.1}s  Esc cancels",
                started_at.elapsed().as_secs_f64()
            ));
        }
        if let Some(result) = pause_recording(
            &mut recording,
            &mut last_repeat,
            now,
            transcribe_in_background,
        ) {
            match result {
                Ok(job) => {
                    voice_job = Some(job);
                    voice_status = Some("Transcribing locally...".into());
                }
                Err(error) => voice_status = Some(error.to_string()),
            }
        }
        if let Some(job) = voice_job.as_ref() {
            match job.receiver.try_recv() {
                Ok(result) => {
                    if let Some(job) = voice_job.take() {
                        voice_status = complete_voice_job(job, result, &mut editor);
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
                let child_live = child_tracker
                    .as_ref()
                    .is_some_and(|tracker| !tracker.terminal);
                let result = match parent_pump.poll(rx, child_live) {
                    ParentEventPoll::Event(value) => value,
                    ParentEventPoll::Empty | ParentEventPoll::WaitingForChild => break,
                    ParentEventPoll::Failed => {
                        if let Some(context) = active_remote_turn.as_ref() {
                            context.record_failure(ChatFailure::IncompleteStream);
                        }
                        finished = true;
                        break;
                    }
                    ParentEventPoll::Disconnected => {
                        if !state.complete {
                            let error = ChatFailure::IncompleteStream;
                            surface.text(out, &format!("\n{error}\n"))?;
                            if let Some(context) = active_remote_turn.as_ref() {
                                context.record_failure(error);
                                publish_turn_event(
                                    remote.as_ref().map(|session| &session.publisher),
                                    RemoteEventKind::Progress,
                                    json!({"stage":"parent_error","message":error.to_string()}),
                                    context.turn_id().as_deref(),
                                );
                            }
                            if matches!(parent_pump.fail(child_live), ParentEventPoll::Failed) {
                                finished = true;
                            }
                        }
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
                            if event.name == "tool_call" {
                                if let Err(error) = submit_tool_call(
                                    api.as_ref(),
                                    &mut state,
                                    remote.as_ref().map(|session| session.session_id.as_str()),
                                    true,
                                ) {
                                    surface.text(out, &format!("\n{error}\n"))?;
                                    finished = true;
                                    break;
                                }
                                if let (
                                    Some(remote),
                                    Some(child_id),
                                    Some(parent_chat),
                                    Some(parent_output),
                                    Some(parent_call),
                                ) = (
                                    remote.as_ref(),
                                    state.child_id.as_ref(),
                                    state.chat_id.as_ref(),
                                    state.output_id.as_ref(),
                                    state.child_call_id.as_ref(),
                                ) && watched_child.as_deref() != Some(child_id)
                                {
                                    watched_child = Some(child_id.clone());
                                    child_tracker = Some(begin_bound_child_tracker(
                                        api.clone(),
                                        remote.session_id.clone(),
                                        child_id.clone(),
                                        parent_chat.clone(),
                                        parent_output.clone(),
                                        parent_call.clone(),
                                    ));
                                }
                                if let Some(action) = state.pending_action.as_ref() {
                                    surface.text(
                                        out,
                                        &format!(
                                            "\nApproval requested for this action:\n{}\nPress Y to approve or N to deny.\n",
                                            clean(&action.receipt.display)
                                        ),
                                    )?;
                                } else if state.child_id.is_some() {
                                    surface.text(
                                        out,
                                        "\nBound child started; waiting for its result.\n",
                                    )?;
                                } else {
                                    surface.text(out, "\nTool result returned to Cortex.\n")?;
                                }
                            }
                            if state.complete {
                                chat = state.chat_id.clone();
                                parent_done_pending = true;
                                if parent_can_finish(parent_done_pending, child_tracker.as_ref()) {
                                    finished = true;
                                    break;
                                }
                            }
                        }
                        Ok(false) => {}
                        Err(error) => {
                            surface.text(out, &format!("\n{error}\n"))?;
                            if let Some(context) = active_remote_turn.as_ref() {
                                context.record_failure(error);
                                publish_turn_event(
                                    remote.as_ref().map(|session| &session.publisher),
                                    RemoteEventKind::Progress,
                                    json!({"stage":"parent_error","message":error.to_string()}),
                                    context.turn_id().as_deref(),
                                );
                            }
                            let child_live = child_tracker
                                .as_ref()
                                .is_some_and(|tracker| !tracker.terminal);
                            if matches!(parent_pump.fail(child_live), ParentEventPoll::Failed) {
                                finished = true;
                            }
                            break;
                        }
                    },
                    Err(error) => {
                        surface.text(out, &format!("\n{error}\n"))?;
                        let child_live = child_tracker
                            .as_ref()
                            .is_some_and(|tracker| !tracker.terminal);
                        if matches!(parent_pump.fail(child_live), ParentEventPoll::Failed) {
                            finished = true;
                        }
                        break;
                    }
                }
            }
            if parent_can_finish(parent_done_pending, child_tracker.as_ref()) {
                finished = true;
            }
            if cleanup_remote_turn(
                &mut finish_pending,
                finished,
                &mut receiver,
                &mut child_tracker,
                &mut child_state,
                remote.as_ref().map(|session| &session.publisher),
                active_remote_turn.as_ref(),
            ) {
                active_remote_turn = None;
                started = None;
                parent_done_pending = false;
                parent_pump = ParentEventPump::default();
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
            active_remote_turn = remote_publisher
                .as_ref()
                .map(|_| RemoteTurnContext::new(None));
            cleaner = TerminalText::default();
            started = Some(Instant::now());
            receiver = Some(begin_turn_with_context(
                api.clone(),
                chat.clone(),
                prompt,
                file_ids,
                remote_publisher.clone(),
                active_remote_turn.clone(),
                telemetry.clone(),
            ));
            parent_pump = ParentEventPump::default();
            dirty = true;
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
                if let Some(recorder) = release_recording(&mut recording) {
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
                match handle_space_repeat(
                    &mut recording,
                    &mut pending_space,
                    &mut last_repeat,
                    now,
                    reports_key_releases,
                    crate::dal::local::voice::Recorder::start,
                ) {
                    Ok(_) => {}
                    Err(error) => voice_status = Some(error.to_string()),
                }
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                flush_pending_space(
                    &mut pending_space,
                    &mut editor,
                    Instant::now(),
                    reports_key_releases,
                );
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
                            if !controls.stop_requested {
                                action_worker.cancel();
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
                if (state.pending_action.is_some()
                    || child_state
                        .as_ref()
                        .is_some_and(|child| child.pending_action.is_some()))
                    && !control
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && matches!(key.code, KeyCode::Char('y' | 'Y' | 'n' | 'N'))
                {
                    let approved = matches!(key.code, KeyCode::Char('y' | 'Y'));
                    let Some(remote) = remote.as_ref() else {
                        surface.text(
                            out,
                            "\nApproval session unavailable; action not executed.\n",
                        )?;
                        if let Some(child) = child_state.as_mut()
                            && child.pending_action.is_some()
                        {
                            child.pending_action = None;
                        } else {
                            state.pending_action = None;
                        }
                        continue;
                    };
                    let target = if state.pending_action.is_some() {
                        &mut state
                    } else {
                        child_state.as_mut().expect("child action is pending")
                    };
                    match answer_pending_action(
                        api.as_ref(),
                        target,
                        &remote.session_id,
                        approved,
                        &mut action_worker,
                    ) {
                        Ok(()) => surface.text(
                            out,
                            if approved {
                                "\nApproval accepted; action is running.\n"
                            } else {
                                "\nAction denied; returning its result to Cortex.\n"
                            },
                        )?,
                        Err(error) => {
                            surface.text(
                                out,
                                &format!(
                                    "\n{}: {error}\n",
                                    if state.pending_action.as_ref().is_some_and(|action| action.completed_result.is_some()) {
                                        "Action result delivery is pending; the action will not run again"
                                    } else {
                                        "Approval or claim was not confirmed; this client will not retry execution automatically"
                                    }
                                ),
                            )?
                        }
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
                        let file_ids = match selected_context_files_for_submission(
                            api.as_ref(),
                            &context,
                            &text,
                            &mut context_uploaded,
                        ) {
                            Ok(files) => files,
                            Err(error) => {
                                surface.text(out, &format!("\n{error}\n"))?;
                                continue;
                            }
                        };
                        active_remote_turn = remote_publisher
                            .as_ref()
                            .map(|_| RemoteTurnContext::new(None));
                        receiver = Some(begin_turn_with_context(
                            api.clone(),
                            chat.clone(),
                            text,
                            file_ids,
                            remote_publisher.clone(),
                            active_remote_turn.clone(),
                            telemetry.clone(),
                        ));
                        parent_pump = ParentEventPump::default();
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
    static CONTEXT_UPLOADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static CANCELLATIONS: std::sync::Mutex<Vec<(String, String)>> =
        std::sync::Mutex::new(Vec::new());
    type SubmittedToolResult = (String, String, String, String, String);

    #[test]
    fn invoke_subagent_registers_server_child_without_fake_parent_result() {
        use std::sync::Mutex;

        struct ChildApi {
            registered: Mutex<Vec<(String, String, String, String)>>,
            submitted: Mutex<Vec<SubmittedToolResult>>,
        }
        impl ChatApi for ChildApi {
            fn send(&self, _: Option<&str>, _: &str) -> Result<Box<dyn Read + Send>, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn register_bound_child(
                &self,
                remote: &str,
                chat: &str,
                output: &str,
                call_id: &str,
            ) -> Result<String, ChatFailure> {
                self.registered.lock().unwrap().push((
                    remote.into(),
                    chat.into(),
                    output.into(),
                    call_id.into(),
                ));
                Ok("50000000-0000-4000-8000-000000000005".into())
            }
            fn submit_tool_result(
                &self,
                chat: &str,
                output: &str,
                result: &crate::domain::models::tools::ToolResult,
            ) -> Result<(), ChatFailure> {
                self.submitted.lock().unwrap().push((
                    chat.into(),
                    output.into(),
                    result.call_id.clone(),
                    result.name.as_str().to_owned(),
                    result.output.clone(),
                ));
                Ok(())
            }
            fn chats(&self) -> Result<Vec<ChatSummary>, ChatFailure> {
                Ok(Vec::new())
            }
            fn history(
                &self,
                _: &str,
            ) -> Result<crate::domain::models::chat::ChatPage, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn reconnect(
                &self,
                _: &str,
                _: &str,
                _: u64,
            ) -> Result<Box<dyn Read + Send>, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn cancel(&self, _: &str, _: &str) -> Result<(), ChatFailure> {
                Ok(())
            }
        }

        let call_id = "30000000-0000-4000-8000-000000000003";
        let api = ChildApi {
            registered: Mutex::new(Vec::new()),
            submitted: Mutex::new(Vec::new()),
        };
        let request = crate::domain::models::tools::ToolRequest::from_event(&json!({
            "call_id":call_id,
            "name":"invoke_subagent",
            "arguments":{"role":"reviewer","task":"Inspect selected workspace"}
        }))
        .unwrap();
        let mut state = ChatState {
            chat_id: Some("10000000-0000-4000-8000-000000000001".into()),
            output_id: Some("20000000-0000-4000-8000-000000000002".into()),
            tool_request: Some(request),
            ..Default::default()
        };

        submit_tool_call(
            &api,
            &mut state,
            Some("40000000-0000-4000-8000-000000000004"),
            true,
        )
        .unwrap();

        assert_eq!(
            *api.registered.lock().unwrap(),
            vec![(
                "40000000-0000-4000-8000-000000000004".into(),
                "10000000-0000-4000-8000-000000000001".into(),
                "20000000-0000-4000-8000-000000000002".into(),
                call_id.into()
            )]
        );
        assert_eq!(
            state.child_id.as_deref(),
            Some("50000000-0000-4000-8000-000000000005")
        );
        assert!(api.submitted.lock().unwrap().is_empty());

        let nested_call = "60000000-0000-4000-8000-000000000006";
        let nested_request = crate::domain::models::tools::ToolRequest::from_event(&json!({
            "call_id":nested_call,
            "name":"invoke_subagent",
            "arguments":{"role":"planner","task":"Try to delegate again"}
        }))
        .unwrap();
        let mut child_state = ChatState {
            chat_id: Some("70000000-0000-4000-8000-000000000007".into()),
            output_id: Some("80000000-0000-4000-8000-000000000008".into()),
            tool_request: Some(nested_request),
            ..Default::default()
        };
        submit_tool_call(
            &api,
            &mut child_state,
            Some("40000000-0000-4000-8000-000000000004"),
            false,
        )
        .unwrap();
        assert_eq!(api.registered.lock().unwrap().len(), 1);
        let submitted = api.submitted.lock().unwrap();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].0, "70000000-0000-4000-8000-000000000007");
        assert_eq!(submitted[0].1, "80000000-0000-4000-8000-000000000008");
        assert_eq!(submitted[0].2, nested_call);
        assert_eq!(submitted[0].3, "invoke_subagent");
        assert!(submitted[0].4.contains("nested child delegation"));
    }

    #[test]
    fn bound_child_uses_actual_bare_lifecycle_dto_and_child_chat_sse() {
        struct ChildStreamApi {
            terminal: &'static str,
        }
        const REMOTE: &str = "40000000-0000-4000-8000-000000000004";
        const PARENT_CHAT: &str = "10000000-0000-4000-8000-000000000001";
        const PARENT_OUTPUT: &str = "20000000-0000-4000-8000-000000000002";
        const CALL: &str = "30000000-0000-4000-8000-000000000003";
        const CHILD: &str = "50000000-0000-4000-8000-000000000005";
        const CHILD_CHAT: &str = "70000000-0000-4000-8000-000000000007";
        const CHILD_OUTPUT: &str = "80000000-0000-4000-8000-000000000008";
        fn lifecycle_frame(
            seq: u64,
            kind: &str,
            message: Option<&str>,
            result: Option<&str>,
            error_code: Option<&str>,
        ) -> String {
            let event_id = format!("90000000-0000-4000-8000-{seq:012}");
            let payload = json!({
                "seq": seq,
                "event_id": event_id,
                "kind": kind,
                "message": message,
                "result": result,
                "error_code": error_code,
                "action_id": null,
                "action_kind": null,
                "payload_digest": null,
                "display": null,
                "approved": null,
                "created_at": "2026-11-26T00:00:00Z"
            });
            format!("id: {seq}\nevent: {kind}\ndata: {payload}\n\n")
        }

        struct FragmentedReader {
            cursor: Cursor<Vec<u8>>,
            max_chunk: usize,
        }
        impl Read for FragmentedReader {
            fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
                let length = target.len().min(self.max_chunk);
                self.cursor.read(&mut target[..length])
            }
        }

        impl ChatApi for ChildStreamApi {
            fn send(&self, _: Option<&str>, _: &str) -> Result<Box<dyn Read + Send>, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn bound_child_detail(
                &self,
                _: &str,
                child: &str,
            ) -> Result<crate::domain::services::chat::BoundChildDetail, ChatFailure> {
                Ok(crate::domain::services::chat::BoundChildDetail {
                    child_id: child.into(),
                    parent_chat_session_id: Some(PARENT_CHAT.into()),
                    parent_output_id: Some(PARENT_OUTPUT.into()),
                    parent_call_id: Some(CALL.into()),
                    child_chat_session_id: Some(CHILD_CHAT.into()),
                    child_output_id: Some(CHILD_OUTPUT.into()),
                    status: "running".into(),
                })
            }
            fn bound_child_events(
                &self,
                _: &str,
                _: &str,
                after: u64,
            ) -> Result<Box<dyn Read + Send>, ChatFailure> {
                assert_eq!(after, 0);
                let sse = [
                    lifecycle_frame(1, "queued", None, None, None),
                    lifecycle_frame(2, "progress", Some("processing ไทย"), None, None),
                    lifecycle_frame(
                        3,
                        self.terminal,
                        None,
                        (self.terminal == "completed").then_some("finished"),
                        (self.terminal == "failed").then_some("fixture"),
                    ),
                ]
                .concat();
                Ok(Box::new(FragmentedReader {
                    cursor: Cursor::new(sse.into_bytes()),
                    max_chunk: 3,
                }))
            }
            fn chats(&self) -> Result<Vec<ChatSummary>, ChatFailure> {
                Ok(Vec::new())
            }
            fn history(
                &self,
                _: &str,
            ) -> Result<crate::domain::models::chat::ChatPage, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn reconnect(
                &self,
                chat: &str,
                output: &str,
                _: u64,
            ) -> Result<Box<dyn Read + Send>, ChatFailure> {
                assert_eq!(chat, CHILD_CHAT);
                assert_eq!(output, CHILD_OUTPUT);
                let sse = format!(
                    "id: 1\nevent: tool_call\ndata: {{\"message\":\"Tool requested\",\"data\":{{\"call_id\":\"{CALL}\",\"name\":\"view_file\",\"arguments\":{{\"path\":\"README.md\",\"start_line\":1}}}}}}\n\nid: 2\nevent: done\ndata: {{\"message\":\"Done\",\"data\":{{\"session_id\":\"{CHILD_CHAT}\",\"output_id\":\"{CHILD_OUTPUT}\",\"total_ms\":10}}}}\n\n"
                );
                Ok(Box::new(Cursor::new(sse.into_bytes())))
            }
            fn cancel(&self, _: &str, _: &str) -> Result<(), ChatFailure> {
                Ok(())
            }
        }

        for terminal in ["completed", "failed", "cancelled"] {
            let mut tracker = begin_bound_child_tracker(
                Arc::new(ChildStreamApi { terminal }),
                REMOTE.into(),
                CHILD.into(),
                PARENT_CHAT.into(),
                PARENT_OUTPUT.into(),
                CALL.into(),
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut bound = false;
            let mut tool_call = false;
            let mut output_done = false;
            let mut queued = false;
            let mut progress = false;
            let mut lifecycle_done = false;
            while Instant::now() < deadline
                && !(bound && tool_call && output_done && queued && progress && lifecycle_done)
            {
                match tracker.receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(BoundChildMessage::Bound(chat, output)) => {
                        assert_eq!(chat, CHILD_CHAT);
                        assert_eq!(output, CHILD_OUTPUT);
                        bound = true;
                    }
                    Ok(BoundChildMessage::Output(event)) if event.name == "tool_call" => {
                        assert_eq!(event.data["call_id"], CALL);
                        tool_call = true;
                    }
                    Ok(BoundChildMessage::Output(event)) if event.name == "done" => {
                        output_done = true;
                    }
                    Ok(BoundChildMessage::Lifecycle(event)) if event.name == "queued" => {
                        assert!(event.data["message"].is_null());
                        queued = true;
                    }
                    Ok(BoundChildMessage::Lifecycle(event)) if event.name == "progress" => {
                        assert_eq!(event.message, "processing ไทย");
                        progress = true;
                    }
                    Ok(BoundChildMessage::Lifecycle(event)) if event.name == terminal => {
                        assert_eq!(event.id, Some(3));
                        assert_eq!(
                            event.data["result"],
                            if terminal == "completed" {
                                json!("finished")
                            } else {
                                json!(null)
                            }
                        );
                        assert_eq!(
                            event.data["error_code"],
                            if terminal == "failed" {
                                json!("fixture")
                            } else {
                                json!(null)
                            }
                        );
                        tracker.terminal = observe_child_lifecycle(&mut tracker, &event);
                        lifecycle_done = true;
                    }
                    Ok(BoundChildMessage::Error(error)) => {
                        panic!("child tracker failed: {error}")
                    }
                    Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            assert!(bound && tool_call && output_done && queued && progress && lifecycle_done);
            assert!(tracker.terminal);
        }
    }

    #[test]
    fn child_lifecycle_reconnects_after_transport_eof_and_confirms_cancel_terminal() {
        struct RecoveryApi {
            cancelled: Arc<AtomicBool>,
            cursors: std::sync::Mutex<Vec<u64>>,
        }
        const REMOTE: &str = "40000000-0000-4000-8000-000000000004";
        const PARENT_CHAT: &str = "10000000-0000-4000-8000-000000000001";
        const PARENT_OUTPUT: &str = "20000000-0000-4000-8000-000000000002";
        const CALL: &str = "30000000-0000-4000-8000-000000000003";
        const CHILD: &str = "50000000-0000-4000-8000-000000000005";

        impl ChatApi for RecoveryApi {
            fn send(&self, _: Option<&str>, _: &str) -> Result<Box<dyn Read + Send>, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn bound_child_detail(
                &self,
                _: &str,
                child: &str,
            ) -> Result<crate::domain::services::chat::BoundChildDetail, ChatFailure> {
                Ok(crate::domain::services::chat::BoundChildDetail {
                    child_id: child.into(),
                    parent_chat_session_id: Some(PARENT_CHAT.into()),
                    parent_output_id: Some(PARENT_OUTPUT.into()),
                    parent_call_id: Some(CALL.into()),
                    child_chat_session_id: None,
                    child_output_id: None,
                    status: if self.cancelled.load(Ordering::Acquire) {
                        "cancelled"
                    } else {
                        "queued"
                    }
                    .into(),
                })
            }
            fn bound_child_events(
                &self,
                _: &str,
                _: &str,
                after: u64,
            ) -> Result<Box<dyn Read + Send>, ChatFailure> {
                self.cursors.lock().unwrap().push(after);
                let frames = if after == 0 {
                    let payload = json!({
                        "seq":1,
                        "event_id":"90000000-0000-4000-8000-000000000001",
                        "kind":"queued",
                        "message":null,
                        "result":null,
                        "error_code":null,
                        "action_id":null,
                        "action_kind":null,
                        "payload_digest":null,
                        "display":null,
                        "approved":null,
                        "created_at":"2026-11-26T00:00:00Z"
                    });
                    format!("id: 1\nevent: queued\ndata: {payload}\n\n")
                } else if self.cancelled.load(Ordering::Acquire) {
                    let payload = json!({
                        "seq":2,
                        "event_id":"90000000-0000-4000-8000-000000000002",
                        "kind":"cancelled",
                        "message":null,
                        "result":null,
                        "error_code":null,
                        "action_id":null,
                        "action_kind":null,
                        "payload_digest":null,
                        "display":null,
                        "approved":null,
                        "created_at":"2026-11-26T00:00:00Z"
                    });
                    format!("id: 2\nevent: cancelled\ndata: {payload}\n\n")
                } else {
                    String::new()
                };
                Ok(Box::new(Cursor::new(frames.into_bytes())))
            }
            fn chats(&self) -> Result<Vec<ChatSummary>, ChatFailure> {
                Ok(Vec::new())
            }
            fn history(
                &self,
                _: &str,
            ) -> Result<crate::domain::models::chat::ChatPage, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn reconnect(
                &self,
                _: &str,
                _: &str,
                _: u64,
            ) -> Result<Box<dyn Read + Send>, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn cancel(&self, _: &str, _: &str) -> Result<(), ChatFailure> {
                self.cancelled.store(true, Ordering::Release);
                Ok(())
            }
        }

        let api = Arc::new(RecoveryApi {
            cancelled: Arc::new(AtomicBool::new(false)),
            cursors: std::sync::Mutex::new(Vec::new()),
        });
        let mut tracker = begin_bound_child_tracker(
            api.clone(),
            REMOTE.into(),
            CHILD.into(),
            PARENT_CHAT.into(),
            PARENT_OUTPUT.into(),
            CALL.into(),
        );
        assert!(matches!(
            tracker.receiver.recv_timeout(Duration::from_secs(2)),
            Ok(BoundChildMessage::Lifecycle(StreamEvent { ref name, .. })) if name == "queued"
        ));

        // The parent cancellation is requested while child lifecycle is still nonterminal.
        api.cancel(PARENT_CHAT, PARENT_OUTPUT).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut terminal = false;
        while Instant::now() < deadline && !terminal {
            match tracker.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(BoundChildMessage::Lifecycle(event)) if event.name == "cancelled" => {
                    tracker.terminal = observe_child_lifecycle(&mut tracker, &event);
                    terminal = true;
                }
                Ok(BoundChildMessage::ConfirmedTerminal(status)) if status == "cancelled" => {
                    tracker.terminal = true;
                    terminal = true;
                }
                Ok(BoundChildMessage::Error(error)) => panic!("child tracker failed: {error}"),
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(terminal && tracker.terminal);
        let cursors = api.cursors.lock().unwrap();
        assert_eq!(cursors.first(), Some(&0));
        assert!(cursors.contains(&1));
    }

    #[test]
    fn bound_child_detail_rejects_a_foreign_parent_session_or_call() {
        let detail = crate::domain::services::chat::BoundChildDetail {
            child_id: "50000000-0000-4000-8000-000000000005".into(),
            parent_chat_session_id: Some("10000000-0000-4000-8000-000000000001".into()),
            parent_output_id: Some("20000000-0000-4000-8000-000000000002".into()),
            parent_call_id: Some("30000000-0000-4000-8000-000000000003".into()),
            child_chat_session_id: None,
            child_output_id: None,
            status: "queued".into(),
        };
        assert!(
            verify_child_binding(
                &detail,
                "50000000-0000-4000-8000-000000000005",
                "10000000-0000-4000-8000-000000000001",
                "20000000-0000-4000-8000-000000000002",
                "30000000-0000-4000-8000-000000000003"
            )
            .is_ok()
        );
        assert_eq!(
            verify_child_binding(
                &detail,
                "50000000-0000-4000-8000-000000000005",
                "90000000-0000-4000-8000-000000000009",
                "20000000-0000-4000-8000-000000000002",
                "30000000-0000-4000-8000-000000000003"
            ),
            Err(ChatFailure::InvalidResponse)
        );
        assert_eq!(
            verify_child_binding(
                &detail,
                "50000000-0000-4000-8000-000000000005",
                "10000000-0000-4000-8000-000000000001",
                "20000000-0000-4000-8000-000000000002",
                "60000000-0000-4000-8000-000000000006"
            ),
            Err(ChatFailure::InvalidResponse)
        );
    }

    #[test]
    fn parent_completion_waits_for_child_failure_or_cancel_terminal_event() {
        let (_sender, receiver) = mpsc::channel();
        let mut tracker = BoundChildTracker {
            receiver,
            stop: Arc::new(AtomicBool::new(false)),
            terminal: false,
        };
        assert!(!parent_can_finish(true, Some(&tracker)));
        for status in ["failed", "cancelled"] {
            let event = StreamEvent {
                id: Some(1),
                name: status.into(),
                message: status.into(),
                data: json!({}),
            };
            assert!(observe_child_lifecycle(&mut tracker, &event));
            assert!(parent_can_finish(true, Some(&tracker)));
            tracker.terminal = false;
        }
        assert!(parent_can_finish(true, None));
    }

    #[test]
    fn parent_event_pump_keeps_done_turn_active_after_sender_drop_until_child_terminal() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok(StreamEvent {
                id: Some(1),
                name: "done".into(),
                message: "complete".into(),
                data: json!({
                    "session_id":"10000000-0000-4000-8000-000000000001",
                    "output_id":"20000000-0000-4000-8000-000000000002",
                    "total_ms":12
                }),
            }))
            .unwrap();
        drop(sender);

        let mut parent_state = ChatState::default();
        let mut child_terminal = false;
        let (_child_sender, child_receiver) = mpsc::channel();
        let mut child_tracker = BoundChildTracker {
            receiver: child_receiver,
            stop: Arc::new(AtomicBool::new(false)),
            terminal: false,
        };
        let mut parent_pump = ParentEventPump::default();
        let parent_done_pending = match parent_pump.poll(&receiver, !child_terminal) {
            ParentEventPoll::Event(Ok(event)) => {
                assert!(parent_state.apply(&event).unwrap());
                parent_state.complete
            }
            other => panic!("expected parent done event, got {}", poll_name(&other)),
        };
        assert!(parent_done_pending);
        assert!(parent_state.complete);

        assert!(matches!(
            parent_pump.poll(&receiver, !child_terminal),
            ParentEventPoll::WaitingForChild
        ));
        assert!(!parent_can_finish(
            parent_done_pending,
            Some(&child_tracker)
        ));

        child_terminal = true;
        child_tracker.terminal = child_terminal;
        assert!(matches!(
            parent_pump.poll(&receiver, !child_terminal),
            ParentEventPoll::Disconnected
        ));
        assert!(parent_can_finish(parent_done_pending, Some(&child_tracker)));
    }

    #[test]
    fn consecutive_remote_turns_keep_answer_and_terminal_ids_separate() {
        let (sender, receiver) = mpsc::sync_channel(8);
        let publisher = RemotePublisher {
            sender,
            failed: Arc::new(AtomicBool::new(false)),
        };
        for (turn_id, answer) in [
            (
                "20000000-0000-4000-8000-000000000002",
                "answer for first turn",
            ),
            (
                "70000000-0000-4000-8000-000000000007",
                "answer for second turn",
            ),
        ] {
            let context = RemoteTurnContext::new(None);
            let receipt = StreamEvent {
                id: Some(1),
                name: "stage".into(),
                message: "accepted".into(),
                data: json!({
                    "stage":"accept",
                    "status":"started",
                    "session_id":"10000000-0000-4000-8000-000000000001",
                    "output_id":turn_id
                }),
            };
            assert_eq!(context.observe_event(&receipt).as_deref(), Some(turn_id));
            publish_turn_event(
                Some(&publisher),
                RemoteEventKind::AnswerPart,
                json!({"text":answer}),
                context.turn_id().as_deref(),
            );
            context.record(RemoteTurnOutcome::Complete {
                session_id: receipt.data["session_id"].as_str().map(str::to_owned),
                output_id: Some(turn_id.to_owned()),
                total_ms: Some(25),
            });
            let mut finish_pending = false;
            let mut no_receiver = None;
            let mut no_child = None;
            let mut no_child_state = None;
            assert!(cleanup_remote_turn(
                &mut finish_pending,
                true,
                &mut no_receiver,
                &mut no_child,
                &mut no_child_state,
                Some(&publisher),
                Some(&context),
            ));
        }

        let events = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(events.len(), 4);
        assert!(matches!(
            &events[0],
            PublishMessage::Event(RemoteEventKind::AnswerPart, payload)
                if payload["turn_id"] == "20000000-0000-4000-8000-000000000002"
                    && payload["text"] == "answer for first turn"
        ));
        assert!(matches!(
            &events[1],
            PublishMessage::Event(RemoteEventKind::TurnEnd, payload)
                if payload["turn_id"] == "20000000-0000-4000-8000-000000000002"
                    && payload["status"] == "complete"
        ));
        assert!(matches!(
            &events[2],
            PublishMessage::Event(RemoteEventKind::AnswerPart, payload)
                if payload["turn_id"] == "70000000-0000-4000-8000-000000000007"
                    && payload["text"] == "answer for second turn"
        ));
        assert!(matches!(
            &events[3],
            PublishMessage::Event(RemoteEventKind::TurnEnd, payload)
                if payload["turn_id"] == "70000000-0000-4000-8000-000000000007"
                    && payload["status"] == "complete"
        ));
    }

    #[test]
    fn initial_reconnect_publishes_bound_answer_and_waits_for_cleanup() {
        const CHAT: &str = "10000000-0000-4000-8000-000000000001";
        const OUTPUT: &str = "20000000-0000-4000-8000-000000000002";

        struct ResumeApi;
        impl ChatApi for ResumeApi {
            fn send(&self, _: Option<&str>, _: &str) -> Result<Box<dyn Read + Send>, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn chats(&self) -> Result<Vec<ChatSummary>, ChatFailure> {
                Ok(Vec::new())
            }
            fn history(&self, _: &str) -> Result<ChatPage, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn reconnect(
                &self,
                chat: &str,
                output: &str,
                after: u64,
            ) -> Result<Box<dyn Read + Send>, ChatFailure> {
                assert_eq!((chat, output, after), (CHAT, OUTPUT, 0));
                let frames = [
                    (
                        1,
                        "stage",
                        json!({"stage":"answer","status":"started"}),
                        "resuming",
                    ),
                    (2, "token", json!({"text":"resumed answer"}), "answer"),
                    (
                        3,
                        "done",
                        json!({"session_id":CHAT,"output_id":OUTPUT,"total_ms":80}),
                        "complete",
                    ),
                ];
                let bytes = frames
                    .into_iter()
                    .map(|(id, name, data, message)| {
                        format!(
                            "id: {id}\nevent: {name}\ndata: {}\n\n",
                            json!({"message":message,"data":data})
                        )
                    })
                    .collect::<String>()
                    .into_bytes();
                Ok(Box::new(Cursor::new(bytes)))
            }
            fn cancel(&self, _: &str, _: &str) -> Result<(), ChatFailure> {
                Ok(())
            }
        }

        let (publish_sender, publish_receiver) = mpsc::sync_channel(8);
        let publisher = RemotePublisher {
            sender: publish_sender,
            failed: Arc::new(AtomicBool::new(false)),
        };
        let context = RemoteTurnContext::new(Some(OUTPUT));
        let (sender, receiver) = mpsc::channel();
        spawn_reconnect_turn(
            Arc::new(ResumeApi),
            CHAT.into(),
            OUTPUT.into(),
            Some(publisher.clone()),
            Some(context.clone()),
            None,
            sender,
        );
        let mut saw_done = false;
        while let Ok(Ok(event)) = receiver.recv_timeout(Duration::from_secs(2)) {
            saw_done |= event.name == "done";
            if saw_done {
                break;
            }
        }
        assert!(saw_done);
        let progress_and_answer =
            std::iter::from_fn(|| publish_receiver.try_recv().ok()).collect::<Vec<_>>();
        assert!(progress_and_answer.iter().any(|message| matches!(
            message,
            PublishMessage::Event(RemoteEventKind::Progress, payload)
                if payload["turn_id"] == OUTPUT
        )));
        assert!(progress_and_answer.iter().any(|message| matches!(
            message,
            PublishMessage::Event(RemoteEventKind::AnswerPart, payload)
                if payload["turn_id"] == OUTPUT && payload["text"] == "resumed answer"
        )));
        assert!(
            progress_and_answer.iter().all(|message| !matches!(
                message,
                PublishMessage::Event(RemoteEventKind::TurnEnd, _)
            ))
        );

        let mut finish_pending = false;
        let mut no_receiver = None;
        let mut no_child = None;
        let mut no_child_state = None;
        assert!(cleanup_remote_turn(
            &mut finish_pending,
            true,
            &mut no_receiver,
            &mut no_child,
            &mut no_child_state,
            Some(&publisher),
            Some(&context),
        ));
        assert!(matches!(
            publish_receiver.try_recv(),
            Ok(PublishMessage::Event(RemoteEventKind::TurnEnd, payload))
                if payload["turn_id"] == OUTPUT && payload["status"] == "complete"
        ));
        assert!(publish_receiver.try_recv().is_err());
    }

    #[test]
    fn parent_receiver_error_keeps_child_tracking_until_terminal_confirmation() {
        use std::sync::atomic::{AtomicBool as TestAtomicBool, AtomicUsize};

        const REMOTE: &str = "40000000-0000-4000-8000-000000000004";
        const CHILD: &str = "50000000-0000-4000-8000-000000000005";
        const PARENT_CHAT: &str = "10000000-0000-4000-8000-000000000001";
        const PARENT_OUTPUT: &str = "20000000-0000-4000-8000-000000000002";
        const CHILD_CHAT: &str = "60000000-0000-4000-8000-000000000006";
        const CHILD_OUTPUT: &str = "70000000-0000-4000-8000-000000000007";
        const CALL: &str = "30000000-0000-4000-8000-000000000003";

        struct FailAfterFrame(Cursor<Vec<u8>>);
        impl Read for FailAfterFrame {
            fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
                if self.0.position() == self.0.get_ref().len() as u64 {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "fixture loss",
                    ));
                }
                self.0.read(target)
            }
        }

        struct ParentErrorApi {
            child_terminal: TestAtomicBool,
            detail_reads: AtomicUsize,
        }
        impl ChatApi for ParentErrorApi {
            fn send(&self, _: Option<&str>, _: &str) -> Result<Box<dyn Read + Send>, ChatFailure> {
                let frame = format!(
                    "id: 1\nevent: stage\ndata: {}\n\n",
                    json!({
                        "message": "accepted",
                        "data": {
                            "stage": "accept",
                            "status": "started",
                            "expected_ms": null,
                            "session_id": PARENT_CHAT,
                            "output_id": PARENT_OUTPUT
                        }
                    })
                );
                Ok(Box::new(FailAfterFrame(Cursor::new(frame.into_bytes()))))
            }
            fn chats(&self) -> Result<Vec<ChatSummary>, ChatFailure> {
                Ok(Vec::new())
            }
            fn history(&self, _: &str) -> Result<ChatPage, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn reconnect(
                &self,
                _: &str,
                _: &str,
                _: u64,
            ) -> Result<Box<dyn Read + Send>, ChatFailure> {
                Err(ChatFailure::Connection)
            }
            fn cancel(&self, _: &str, _: &str) -> Result<(), ChatFailure> {
                Ok(())
            }
            fn bound_child_events(
                &self,
                _: &str,
                _: &str,
                _: u64,
            ) -> Result<Box<dyn Read + Send>, ChatFailure> {
                Ok(Box::new(Cursor::new(Vec::<u8>::new())))
            }
            fn bound_child_detail(
                &self,
                _: &str,
                _: &str,
            ) -> Result<crate::domain::services::chat::BoundChildDetail, ChatFailure> {
                self.detail_reads.fetch_add(1, Ordering::AcqRel);
                Ok(crate::domain::services::chat::BoundChildDetail {
                    child_id: CHILD.into(),
                    parent_chat_session_id: Some(PARENT_CHAT.into()),
                    parent_output_id: Some(PARENT_OUTPUT.into()),
                    parent_call_id: Some(CALL.into()),
                    child_chat_session_id: Some(CHILD_CHAT.into()),
                    child_output_id: Some(CHILD_OUTPUT.into()),
                    status: if self.child_terminal.load(Ordering::Acquire) {
                        "completed"
                    } else {
                        "running"
                    }
                    .into(),
                })
            }
        }

        let api = Arc::new(ParentErrorApi {
            child_terminal: TestAtomicBool::new(false),
            detail_reads: AtomicUsize::new(0),
        });
        let mut child_tracker = Some(begin_bound_child_tracker(
            api.clone(),
            REMOTE.into(),
            CHILD.into(),
            PARENT_CHAT.into(),
            PARENT_OUTPUT.into(),
            CALL.into(),
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        while api.detail_reads.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(api.detail_reads.load(Ordering::Acquire) > 0);

        // begin_turn exercises the actual spawn_turn -> follow path. The
        // accepted identity is read, then the fake transport drops and refuses
        // reconnect, producing the same parent Err(Connection) as production.
        let (publish_sender, publish_receiver) = mpsc::sync_channel(16);
        let publisher = RemotePublisher {
            sender: publish_sender,
            failed: Arc::new(AtomicBool::new(false)),
        };
        let turn_context = RemoteTurnContext::new(None);
        let mut receiver = Some(begin_turn_with_context(
            api.clone(),
            None,
            "parent fixture".into(),
            Vec::new(),
            Some(publisher.clone()),
            Some(turn_context.clone()),
            None,
        ));
        let mut parent_pump = ParentEventPump::default();
        let mut child_state = Some(ChatState::default());
        let mut finish_pending = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut parent_error_received = false;
        while Instant::now() < deadline && !parent_error_received {
            match parent_pump.poll(receiver.as_ref().unwrap(), true) {
                ParentEventPoll::Event(Err(ChatFailure::Connection)) => {
                    parent_error_received = true;
                    break;
                }
                ParentEventPoll::Event(Ok(_)) | ParentEventPoll::Empty => {
                    thread::sleep(Duration::from_millis(5));
                }
                other => panic!(
                    "expected actual parent transport error, got {}",
                    poll_name(&other)
                ),
            }
        }
        assert!(parent_error_received);
        assert!(matches!(
            parent_pump.fail(true),
            ParentEventPoll::WaitingForChild
        ));
        assert!(!child_tracker.as_ref().unwrap().terminal);
        assert_eq!(turn_context.turn_id().as_deref(), Some(PARENT_OUTPUT));
        let early_events =
            std::iter::from_fn(|| publish_receiver.try_recv().ok()).collect::<Vec<_>>();
        assert!(early_events.iter().any(|message| matches!(
            message,
            PublishMessage::Event(RemoteEventKind::Progress, payload)
                if payload["turn_id"] == PARENT_OUTPUT && payload["stage"] == "parent_error"
        )));
        assert!(
            early_events.iter().all(|message| !matches!(
                message,
                PublishMessage::Event(RemoteEventKind::TurnEnd, _)
            ))
        );
        // Exercise the same cleanup seam used by the interactive loop after
        // parent errors. The error is visible, but it cannot release the
        // receiver, child recovery state, or block the next remote prompt.
        assert!(!cleanup_remote_turn(
            &mut finish_pending,
            true,
            &mut receiver,
            &mut child_tracker,
            &mut child_state,
            Some(&publisher),
            Some(&turn_context),
        ));
        assert!(receiver.is_some());
        assert!(child_tracker.is_some());
        assert!(child_state.is_some());
        assert!(finish_pending);
        assert!(matches!(
            parent_pump.poll(receiver.as_ref().unwrap(), true),
            ParentEventPoll::WaitingForChild
        ));
        assert!(publish_receiver.try_recv().is_err());

        api.child_terminal.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !child_tracker.as_ref().unwrap().terminal {
            match child_tracker
                .as_ref()
                .unwrap()
                .receiver
                .recv_timeout(Duration::from_millis(50))
            {
                Ok(BoundChildMessage::ConfirmedTerminal(status)) => {
                    assert_eq!(status, "completed");
                    child_tracker.as_mut().unwrap().terminal = true;
                }
                Ok(BoundChildMessage::Lifecycle(event)) => {
                    let tracker = child_tracker.as_mut().unwrap();
                    tracker.terminal = observe_child_lifecycle(tracker, &event);
                }
                Ok(
                    BoundChildMessage::Error(_)
                    | BoundChildMessage::Bound(_, _)
                    | BoundChildMessage::Output(_),
                )
                | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(child_tracker.as_ref().unwrap().terminal);
        assert!(matches!(
            parent_pump.poll(receiver.as_ref().unwrap(), false),
            ParentEventPoll::Failed
        ));
        assert!(cleanup_remote_turn(
            &mut finish_pending,
            false,
            &mut receiver,
            &mut child_tracker,
            &mut child_state,
            Some(&publisher),
            Some(&turn_context),
        ));
        assert!(receiver.is_none());
        assert!(child_tracker.is_none());
        assert!(child_state.is_none());
        assert!(!finish_pending);
        assert!(matches!(
            publish_receiver.try_recv(),
            Ok(PublishMessage::Event(RemoteEventKind::TurnEnd, payload))
                if payload["turn_id"] == PARENT_OUTPUT
                    && payload["status"] == "incomplete"
        ));
        assert!(
            publish_receiver.try_recv().is_err(),
            "terminal is emitted once"
        );
    }

    #[test]
    fn child_lifecycle_decoder_rejects_cursor_kind_and_dto_mismatches() {
        let payload = json!({
            "seq": 2,
            "event_id": "90000000-0000-4000-8000-000000000002",
            "kind": "progress",
            "message": null,
            "result": null,
            "error_code": null,
            "action_id": null,
            "action_kind": null,
            "payload_digest": null,
            "display": null,
            "approved": null,
            "created_at": "2026-11-26T00:00:00Z"
        });
        let frame = || crate::dal::remote::sse::SseFrame {
            id: Some(2),
            name: "progress".into(),
            data: serde_json::to_vec(&payload).unwrap(),
        };
        assert!(decode_child_lifecycle_frame(frame(), 1).is_ok());
        assert!(matches!(
            decode_child_lifecycle_frame(frame(), 2),
            Err(ChatFailure::InvalidResponse)
        ));
        let mut wrong_cursor = frame();
        wrong_cursor.id = Some(3);
        assert!(matches!(
            decode_child_lifecycle_frame(wrong_cursor, 1),
            Err(ChatFailure::InvalidResponse)
        ));
        let mut wrong_kind = frame();
        wrong_kind.name = "queued".into();
        assert!(matches!(
            decode_child_lifecycle_frame(wrong_kind, 1),
            Err(ChatFailure::InvalidResponse)
        ));
        let mut missing_nullable_field = frame();
        let mut missing: Value = serde_json::from_slice(&missing_nullable_field.data).unwrap();
        missing.as_object_mut().unwrap().remove("message");
        missing_nullable_field.data = serde_json::to_vec(&missing).unwrap();
        assert!(matches!(
            decode_child_lifecycle_frame(missing_nullable_field, 1),
            Err(ChatFailure::InvalidResponse)
        ));
    }

    fn poll_name(poll: &ParentEventPoll) -> &'static str {
        match poll {
            ParentEventPoll::Event(_) => "event",
            ParentEventPoll::Empty => "empty",
            ParentEventPoll::WaitingForChild => "waiting_for_child",
            ParentEventPoll::Disconnected => "disconnected",
            ParentEventPoll::Failed => "failed",
        }
    }

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

    #[test]
    fn first_server_decision_wins_over_a_later_terminal_or_phone_answer() {
        assert_eq!(effective_approval("pending", true), Some(true));
        assert_eq!(effective_approval("pending", false), Some(false));
        assert_eq!(effective_approval("approved", false), Some(true));
        assert_eq!(effective_approval("denied", true), Some(false));
        assert_eq!(effective_approval("cancelled", true), None);
        assert_eq!(effective_approval("expired", true), None);
        assert_eq!(effective_approval("claimed", false), None);
    }

    #[test]
    fn manager_replayed_phone_approval_acknowledges_inflight_worker() {
        use crate::domain::models::{chat::ChatPage, tools::OutputActionReceipt};
        use std::sync::atomic::AtomicUsize;

        struct RetryApi {
            request: crate::domain::models::tools::ToolRequest,
            submissions: AtomicUsize,
            outputs: std::sync::Mutex<Vec<String>>,
            claim_response_lost: bool,
            claim_is_ambiguous: AtomicBool,
            claims: AtomicUsize,
            acknowledgements: AtomicUsize,
            fail_first_ack: bool,
            cancellations: AtomicUsize,
        }
        impl RetryApi {
            fn receipt(&self, status: &str) -> OutputActionReceipt {
                let request = &self.request;
                OutputActionReceipt {
                    action_id: "30000000-0000-4000-8000-000000000003".into(),
                    call_id: request.call_id.clone(),
                    name: request.name.as_str().into(),
                    action_kind: "command".into(),
                    action_digest: request.action_digest(),
                    display: "fixture command".into(),
                    expires_at: "later".into(),
                    status: status.into(),
                }
            }
        }
        impl ChatApi for RetryApi {
            fn send(&self, _: Option<&str>, _: &str) -> Result<Box<dyn Read + Send>, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn submit_tool_result(
                &self,
                _: &str,
                _: &str,
                result: &ToolResult,
            ) -> Result<(), ChatFailure> {
                self.outputs.lock().unwrap().push(result.output.clone());
                if self.submissions.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(ChatFailure::Connection)
                } else {
                    Ok(())
                }
            }
            fn get_output_action(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<OutputActionReceipt, ChatFailure> {
                Ok(
                    self.receipt(if self.claim_is_ambiguous.load(Ordering::SeqCst) {
                        "claimed"
                    } else {
                        "pending"
                    }),
                )
            }
            fn answer_output_action(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &str,
                approved: bool,
            ) -> Result<OutputActionReceipt, ChatFailure> {
                Ok(self.receipt(if approved { "approved" } else { "denied" }))
            }
            fn claim_output_action(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &str,
                _: &crate::domain::models::tools::ToolRequest,
            ) -> Result<OutputActionReceipt, ChatFailure> {
                self.claims.fetch_add(1, Ordering::SeqCst);
                if self.claim_response_lost {
                    self.claim_is_ambiguous.store(true, Ordering::SeqCst);
                    Err(ChatFailure::Connection)
                } else {
                    self.claim_is_ambiguous.store(true, Ordering::SeqCst);
                    Ok(self.receipt("claimed"))
                }
            }
            fn chats(&self) -> Result<Vec<ChatSummary>, ChatFailure> {
                Ok(Vec::new())
            }
            fn history(&self, _: &str) -> Result<ChatPage, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn reconnect(
                &self,
                _: &str,
                _: &str,
                _: u64,
            ) -> Result<Box<dyn Read + Send>, ChatFailure> {
                Err(ChatFailure::InvalidInput)
            }
            fn cancel(&self, _: &str, _: &str) -> Result<(), ChatFailure> {
                Ok(())
            }
        }

        impl RemoteCommandApi for RetryApi {
            fn chat_api(&self) -> &dyn ChatApi {
                self
            }
            fn cancel_output(&self, _: &str, _: &str) -> Result<(), ChatFailure> {
                self.cancellations.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn acknowledge_command(&self, _: &str, _: u64, _: &str) -> Result<(), ChatFailure> {
                let attempt = self.acknowledgements.fetch_add(1, Ordering::SeqCst);
                if self.fail_first_ack && attempt == 0 {
                    Err(ChatFailure::Connection)
                } else {
                    Ok(())
                }
            }
        }

        let request = crate::domain::models::tools::ToolRequest::from_event(&json!({
                "call_id":"10000000-0000-4000-8000-000000000001",
                "name":"run_command",
                "arguments":{"cwd":".","argv":["sh","-c","sleep 0.3; printf x >> .cache_retry_action_fixture"]}
            })).unwrap();
        let marker = std::path::Path::new(".cache_retry_action_fixture");
        let _ = std::fs::remove_file(marker);
        std::fs::write(marker, "").unwrap();
        let preimage = crate::domain::services::tools::prepare_action(&request).unwrap();
        let api = RetryApi {
            request: request.clone(),
            submissions: AtomicUsize::new(0),
            outputs: std::sync::Mutex::new(Vec::new()),
            claim_response_lost: false,
            claim_is_ambiguous: AtomicBool::new(false),
            claims: AtomicUsize::new(0),
            acknowledgements: AtomicUsize::new(0),
            fail_first_ack: false,
            cancellations: AtomicUsize::new(0),
        };
        let mut state = ChatState {
            chat_id: Some("10000000-0000-4000-8000-000000000001".into()),
            output_id: Some("20000000-0000-4000-8000-000000000002".into()),
            pending_action: Some(crate::domain::services::chat::PendingOutputAction {
                request: request.clone(),
                receipt: api.receipt("pending"),
                preimage,
                completed_result: None,
            }),
            ..Default::default()
        };
        let mut worker = ActionWorker::default();
        let started = Instant::now();
        answer_pending_action(
            &api,
            &mut state,
            "40000000-0000-4000-8000-000000000004",
            true,
            &mut worker,
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "approval handling must not wait for the effect"
        );
        let deadline = Instant::now() + Duration::from_secs(4);
        while state.pending_action.is_some() && Instant::now() < deadline {
            poll_action_worker(&api, &mut state, &mut worker, None, None);
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            state.pending_action.is_none(),
            "result delivery should retry after the first failure"
        );
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "x");
        let outputs = api.outputs.lock().unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0], outputs[1]);
        drop(outputs);
        std::fs::remove_file(marker).unwrap();

        let cancel_marker = std::path::Path::new(".cache_replayed_approval_cancel_fixture");
        let _ = std::fs::remove_file(cancel_marker);
        let replay_request = crate::domain::models::tools::ToolRequest::from_event(&json!({
            "call_id":"60000000-0000-4000-8000-000000000006",
            "name":"run_command",
            "arguments":{"cwd":".","argv":["sh","-c","sleep 5; printf x > .cache_replayed_approval_cancel_fixture"]}
        })).unwrap();
        let replay_api = Arc::new(RetryApi {
            request: replay_request.clone(),
            submissions: AtomicUsize::new(0),
            outputs: std::sync::Mutex::new(Vec::new()),
            claim_response_lost: false,
            claim_is_ambiguous: AtomicBool::new(false),
            claims: AtomicUsize::new(0),
            acknowledgements: AtomicUsize::new(0),
            fail_first_ack: true,
            cancellations: AtomicUsize::new(0),
        });
        let mut replay_state = ChatState {
            chat_id: Some("10000000-0000-4000-8000-000000000001".into()),
            output_id: Some("20000000-0000-4000-8000-000000000002".into()),
            pending_action: Some(crate::domain::services::chat::PendingOutputAction {
                request: replay_request.clone(),
                receipt: replay_api.receipt("pending"),
                preimage: crate::domain::services::tools::prepare_action(&replay_request).unwrap(),
                completed_result: None,
            }),
            ..Default::default()
        };
        let session_id = "40000000-0000-4000-8000-000000000004".to_owned();
        let (command_tx, command_rx) = mpsc::channel();
        let approval = RemoteEvent {
            id: 20,
            kind: RemoteEventKind::ApprovalAnswer,
            payload: json!({"approval_id":"30000000-0000-4000-8000-000000000003","approved":true}),
            created_at_ms: 20,
        };
        command_tx.send(approval).unwrap();
        let replay_cursor = Arc::new(AtomicU64::new(0));
        let (publish_tx, publish_rx) = mpsc::sync_channel(4);
        let test_client = Arc::new(
            CortexClient::new(Arc::new(crate::domain::models::native_auth::Session {
                session_id: "test-session".into(),
                csrf_token: "test-csrf".into(),
            }))
            .unwrap(),
        );
        let remote = RemoteSessionGuard {
            api: test_client,
            commands: replay_api.clone(),
            registered: false,
            session_id,
            receiver: command_rx,
            cursor: replay_cursor.clone(),
            listener_online: Arc::new(AtomicBool::new(true)),
            publisher: RemotePublisher {
                sender: publish_tx,
                failed: Arc::new(AtomicBool::new(false)),
            },
            stop: Arc::new(AtomicBool::new(false)),
        };
        let mut controls = TurnControl {
            active: true,
            ..Default::default()
        };
        let mut queue = RemoteCommandQueue::default();
        let mut action_worker = ActionWorker::default();
        let mut surface = Surface::new(80, 24, false);
        let mut screen = Vec::new();
        queue
            .poll(
                &remote,
                &mut replay_state,
                None,
                &mut controls,
                &mut action_worker,
                &mut screen,
                &mut surface,
            )
            .unwrap();
        assert_eq!(
            replay_cursor.load(Ordering::Relaxed),
            0,
            "failed ACK keeps the approval command queued"
        );
        assert_eq!(replay_api.claims.load(Ordering::SeqCst), 1);
        assert!(
            action_worker.result.is_some(),
            "first approval starts the local worker"
        );
        queue.retry_at = Instant::now();
        queue
            .poll(
                &remote,
                &mut replay_state,
                None,
                &mut controls,
                &mut action_worker,
                &mut screen,
                &mut surface,
            )
            .unwrap();
        assert_eq!(
            replay_cursor.load(Ordering::Relaxed),
            20,
            "replayed answer is acknowledged"
        );
        assert_eq!(
            replay_api.claims.load(Ordering::SeqCst),
            1,
            "replay never claims or starts a second effect"
        );
        assert_eq!(replay_api.acknowledgements.load(Ordering::SeqCst), 2);
        command_tx
            .send(RemoteEvent {
                id: 21,
                kind: RemoteEventKind::Cancel,
                payload: json!({}),
                created_at_ms: 21,
            })
            .unwrap();
        queue.retry_at = Instant::now();
        queue
            .poll(
                &remote,
                &mut replay_state,
                None,
                &mut controls,
                &mut action_worker,
                &mut screen,
                &mut surface,
            )
            .unwrap();
        assert_eq!(replay_cursor.load(Ordering::Relaxed), 21);
        assert_eq!(replay_api.cancellations.load(Ordering::SeqCst), 1);
        let deadline = Instant::now() + Duration::from_secs(4);
        while replay_state.pending_action.is_some() && Instant::now() < deadline {
            poll_action_worker(
                replay_api.as_ref(),
                &mut replay_state,
                &mut action_worker,
                None,
                Some(&remote.publisher),
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            replay_state.pending_action.is_none(),
            "the cancelled worker reaches terminal stopped state"
        );
        assert!(
            !cancel_marker.exists(),
            "the post-sleep effect did not run after cancellation"
        );
        assert!(
            matches!(publish_rx.try_recv(), Ok(PublishMessage::Event(RemoteEventKind::Progress, payload)) if payload["status"] == "cancelled" && payload["command_id"] == 21)
        );
        drop(remote);

        let ambiguous_marker = std::path::Path::new(".cache_ambiguous_claim_fixture");
        let _ = std::fs::remove_file(ambiguous_marker);
        let ambiguous_request = crate::domain::models::tools::ToolRequest::from_event(&json!({
            "call_id":"50000000-0000-4000-8000-000000000005",
            "name":"run_command",
            "arguments":{"cwd":".","argv":["sh","-c","printf x > .cache_ambiguous_claim_fixture"]}
        }))
        .unwrap();
        let ambiguous_api = RetryApi {
            request: ambiguous_request.clone(),
            submissions: AtomicUsize::new(0),
            outputs: std::sync::Mutex::new(Vec::new()),
            claim_response_lost: true,
            claim_is_ambiguous: AtomicBool::new(false),
            claims: AtomicUsize::new(0),
            acknowledgements: AtomicUsize::new(0),
            fail_first_ack: false,
            cancellations: AtomicUsize::new(0),
        };
        let ambiguous_preimage =
            crate::domain::services::tools::prepare_action(&ambiguous_request).unwrap();
        let mut ambiguous_state = ChatState {
            chat_id: Some("10000000-0000-4000-8000-000000000001".into()),
            output_id: Some("20000000-0000-4000-8000-000000000002".into()),
            pending_action: Some(crate::domain::services::chat::PendingOutputAction {
                request: ambiguous_request,
                receipt: ambiguous_api.receipt("pending"),
                preimage: ambiguous_preimage,
                completed_result: None,
            }),
            ..Default::default()
        };
        let mut ambiguous_worker = ActionWorker::default();
        assert!(
            answer_pending_action(
                &ambiguous_api,
                &mut ambiguous_state,
                "40000000-0000-4000-8000-000000000004",
                true,
                &mut ambiguous_worker
            )
            .is_err()
        );
        assert!(
            ambiguous_worker.result.is_none(),
            "lost claim response must not start execution"
        );
        assert!(
            answer_pending_action(
                &ambiguous_api,
                &mut ambiguous_state,
                "40000000-0000-4000-8000-000000000004",
                true,
                &mut ambiguous_worker
            )
            .is_err()
        );
        assert!(
            ambiguous_worker.result.is_none(),
            "a claimed action remains fail-closed after reconnect"
        );
        assert!(!ambiguous_marker.exists());
    }
    static SENDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    struct CaptureProbe {
        samples: Vec<f32>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    struct FakeCapture(Arc<std::sync::atomic::AtomicUsize>);

    fn transcribe_fake(capture: FakeCapture) -> Result<&'static str, ()> {
        capture.0.fetch_add(1, Ordering::SeqCst);
        Ok("editable transcript")
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
        fn upload_temporary_text(&self, _: &str, _: &str) -> Result<String, ChatFailure> {
            CONTEXT_UPLOADS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("10000000-0000-4000-8000-000000000001".into())
        }
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
    fn fallback_repeat_pause_stops_once_and_transcribes_into_editable_text() {
        let t0 = Instant::now();
        let started = t0 + KITTY_HOLD_THRESHOLD;
        let repeated = started + Duration::from_millis(40);
        let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut recording = None;
        let mut pending_space = Some(t0);
        let mut last_repeat = None;

        assert!(
            handle_space_repeat(
                &mut recording,
                &mut pending_space,
                &mut last_repeat,
                started,
                false,
                || Ok::<_, ()>(FakeCapture(stops.clone()))
            )
            .unwrap()
        );
        assert!(recording.is_some());
        assert_eq!(last_repeat, Some(started));

        assert!(
            !handle_space_repeat(
                &mut recording,
                &mut pending_space,
                &mut last_repeat,
                repeated,
                false,
                || -> Result<FakeCapture, ()> {
                    panic!("a repeat must not start a second recorder")
                }
            )
            .unwrap()
        );
        assert_eq!(last_repeat, Some(repeated));
        assert!(
            pause_recording(
                &mut recording,
                &mut last_repeat,
                repeated + FALLBACK_RELEASE_TIMEOUT - Duration::from_millis(1),
                transcribe_fake,
            )
            .is_none()
        );

        let transcript = pause_recording(
            &mut recording,
            &mut last_repeat,
            repeated + FALLBACK_RELEASE_TIMEOUT,
            transcribe_fake,
        )
        .unwrap()
        .unwrap();
        assert_eq!(transcript, "editable transcript");
        let mut editor = Editor::default();
        insert_voice_transcript(&mut editor, transcript);
        assert_eq!(editor.text, "editable transcript");
        assert_eq!(editor.cursor, editor.text.len());
        assert!(recording.is_none());
        assert_eq!(last_repeat, None);
        assert!(
            pause_recording(
                &mut recording,
                &mut last_repeat,
                repeated + FALLBACK_RELEASE_TIMEOUT + Duration::from_secs(1),
                transcribe_fake,
            )
            .is_none()
        );
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn kitty_release_stops_capture_and_quick_space_stays_text() {
        let t0 = Instant::now();
        let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut recording = None;
        let mut pending_space = Some(t0);
        let mut last_repeat = None;
        assert!(
            handle_space_repeat(
                &mut recording,
                &mut pending_space,
                &mut last_repeat,
                t0 + KITTY_HOLD_THRESHOLD,
                true,
                || Ok::<_, ()>(FakeCapture(stops.clone()))
            )
            .unwrap()
        );
        assert_eq!(last_repeat, None);
        assert_eq!(
            transcribe_fake(release_recording(&mut recording).unwrap()),
            Ok("editable transcript")
        );
        assert!(recording.is_none());
        assert_eq!(stops.load(Ordering::SeqCst), 1);

        let mut editor = Editor::default();
        let mut quick_tap = Some(t0);
        assert!(flush_pending_space(
            &mut quick_tap,
            &mut editor,
            t0 + Duration::from_millis(100),
            false
        ));
        assert_eq!(editor.text, " ");
    }

    #[test]
    fn accepted_cancel_discards_successful_result_from_worker_that_ignores_cancel() {
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_entered = Arc::clone(&entered);
        let worker_release = Arc::clone(&release);
        let job = spawn_voice_job(vec![0.25], move |_, _cancel| {
            worker_entered.store(true, Ordering::SeqCst);
            while !worker_release.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
            Ok("cancelled text must not appear".into())
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !entered.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(entered.load(Ordering::SeqCst));
        let mut editor = Editor::default();
        let mut exit_after_voice = false;
        let mut status = None;
        handle_voice_job_key(
            &job,
            KeyCode::Esc,
            KeyModifiers::NONE,
            &mut exit_after_voice,
            &mut status,
        );
        release.store(true, Ordering::SeqCst);
        let result = job.receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(result.as_deref(), Ok("cancelled text must not appear"));
        let status = complete_voice_job(job, result, &mut editor);
        assert_eq!(editor.text, "");
        assert_eq!(
            status.as_deref(),
            Some("Transcription cancelled; no text inserted.")
        );
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
        interactive(
            Arc::new(Fixture),
            None,
            None,
            None,
            &mut out,
            None,
            SelectedContext {
                text: String::new(),
                omissions: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(*DETAIL_STATES.lock().unwrap(), vec![true, false]);
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }
    #[test]
    #[ignore = "requires an interactive terminal; uses only in-memory fixture events"]
    fn terminal_cancel_fixture() {
        assert!(io::stdin().is_terminal());
        let mut out = io::stdout().lock();
        interactive(
            Arc::new(Fixture),
            None,
            None,
            None,
            &mut out,
            None,
            SelectedContext {
                text: String::new(),
                omissions: Vec::new(),
            },
        )
        .unwrap();
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
    fn selected_context_upload_waits_for_first_nonempty_prompt_submission() {
        CONTEXT_UPLOADS.store(0, std::sync::atomic::Ordering::SeqCst);
        let api = Fixture;
        let context = SelectedContext {
            text: "selected typed note".into(),
            omissions: Vec::new(),
        };
        let mut uploaded = false;
        assert!(
            selected_context_files_for_submission(&api, &context, "", &mut uploaded)
                .unwrap()
                .is_empty()
        );
        assert_eq!(CONTEXT_UPLOADS.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            selected_context_files_for_submission(&api, &context, "first prompt", &mut uploaded)
                .unwrap(),
            ["10000000-0000-4000-8000-000000000001"]
        );
        assert_eq!(CONTEXT_UPLOADS.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            selected_context_files_for_submission(&api, &context, "second prompt", &mut uploaded)
                .unwrap()
                .is_empty()
        );
        assert_eq!(CONTEXT_UPLOADS.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
