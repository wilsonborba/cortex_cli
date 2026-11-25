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
        remote::{chat::CortexClient, gateway::GatewayClient, login_context},
    },
    domain::{
        models::{
            chat::{ChatFailure, ChatPage, StreamEvent},
            native_auth::{AuthFailure, Session},
            remote_control::{RemoteEvent, RemoteEventKind},
            tools::{ToolName, ToolResult},
        },
        services::{
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
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
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
    telemetry: Option<Arc<Telemetry>>,
    sender: mpsc::Sender<Result<StreamEvent, ChatFailure>>,
) {
    thread::spawn(move || {
        let mut state = ChatState {
            chat_id: chat_id.clone(),
            ..Default::default()
        };
        let mut answer = String::new();
        let result = api
            .send_tools_with_files(chat_id.as_deref(), &prompt, &file_ids)
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
        if let Some(telemetry) = telemetry {
            telemetry.chat_result(result);
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
    telemetry: Option<Arc<Telemetry>>,
) -> mpsc::Receiver<Result<StreamEvent, ChatFailure>> {
    let (sender, receiver) = mpsc::channel();
    spawn_turn(api, chat_id, prompt, file_ids, publisher, telemetry, sender);
    receiver
}

fn submit_tool_call(
    api: &dyn ChatApi,
    state: &mut ChatState,
    remote_session: Option<&str>,
) -> Result<(), ChatFailure> {
    let request = state
        .tool_request
        .take()
        .ok_or(ChatFailure::InvalidResponse)?;
    let (chat, output) = match (state.chat_id.as_deref(), state.output_id.as_deref()) {
        (Some(chat), Some(output)) => (chat, output),
        _ => return Err(ChatFailure::InvalidResponse),
    };
    let output_text = match execute_readonly(&request) {
        Ok(Some(output)) => output,
        Ok(None) => match request.name {
            ToolName::InvokeSubagent => {
                "Not executed: a real child runner is not available for this request.".into()
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
                    publisher.publish(
                        RemoteEventKind::Progress,
                        json!({"status":"cancelled","command_id":command_id}),
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
                        "Command outcome is unknown. It was stopped and will not be rerun automatically.".into(),
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

    fn poll(
        &mut self,
        remote: &RemoteSessionGuard,
        state: &mut ChatState,
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
                            if state
                                .pending_action
                                .as_ref()
                                .is_some_and(|pending| action_id == pending.receipt.action_id)
                            {
                                match answer_pending_action(
                                    remote.commands.chat_api(),
                                    state,
                                    &remote.session_id,
                                    approved,
                                    worker,
                                ) {
                                    Ok(()) => "approval_received",
                                    Err(_) => "approval_failed",
                                }
                            } else if state.pending_action.is_none()
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
    let signed_in = session()?;
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
        let mut state = ChatState {
            chat_id: chat.clone(),
            ..Default::default()
        };
        let mut text = TerminalText::default();
        let stream = api.send(chat.as_deref(), &prompt).map_err(failure)?;
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
    interactive(api, chat, history, telemetry, out, remote_api)
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
    let mut action_worker = ActionWorker::default();
    let mut remote_online = true;
    let mut remote_delivery_reported = false;
    let mut dirty = true;
    if let (Some(chat_id), Some(output_id)) = (chat.clone(), running) {
        let (tx, rx) = mpsc::channel();
        receiver = Some(rx);
        started = Some(Instant::now());
        let api = api.clone();
        let publisher = remote_publisher.clone();
        let telemetry = telemetry.clone();
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
            if let Some(telemetry) = telemetry {
                telemetry.chat_result(result);
            }
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
            remote_commands.poll(
                remote,
                &mut state,
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
        poll_action_worker(
            api.as_ref(),
            &mut state,
            &mut action_worker,
            remote.as_ref().map(|remote| &remote.publisher),
        );
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
                            if event.name == "tool_call" {
                                if let Err(error) = submit_tool_call(
                                    api.as_ref(),
                                    &mut state,
                                    remote.as_ref().map(|session| session.session_id.as_str()),
                                ) {
                                    surface.text(out, &format!("\n{error}\n"))?;
                                    finished = true;
                                    break;
                                }
                                if let Some(action) = state.pending_action.as_ref() {
                                    surface.text(
                                        out,
                                        &format!(
                                            "\nApproval requested for this action:\n{}\nPress Y to approve or N to deny.\n",
                                            clean(&action.receipt.display)
                                        ),
                                    )?;
                                } else {
                                    surface.text(out, "\nTool result returned to Cortex.\n")?;
                                }
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
                telemetry.clone(),
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
                if state.pending_action.is_some()
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
                        state.pending_action = None;
                        continue;
                    };
                    match answer_pending_action(
                        api.as_ref(),
                        &mut state,
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
                        receiver = Some(begin_turn(
                            api.clone(),
                            chat.clone(),
                            text,
                            Vec::new(),
                            remote_publisher.clone(),
                            telemetry.clone(),
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
            poll_action_worker(&api, &mut state, &mut worker, None);
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
        interactive(Arc::new(Fixture), None, None, None, &mut out, None).unwrap();
        assert_eq!(*DETAIL_STATES.lock().unwrap(), vec![true, false]);
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }
    #[test]
    #[ignore = "requires an interactive terminal; uses only in-memory fixture events"]
    fn terminal_cancel_fixture() {
        assert!(io::stdin().is_terminal());
        let mut out = io::stdout().lock();
        interactive(Arc::new(Fixture), None, None, None, &mut out, None).unwrap();
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
