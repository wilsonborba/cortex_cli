//! Authenticated Cortex routes through the gateway. No backend identity headers.

use reqwest::{
    blocking::{Client, RequestBuilder, Response},
    redirect::Policy,
};
use serde_json::{Value, json};
use std::{io::Read, sync::Arc, time::Duration};

use crate::domain::{
    models::{
        chat::{ChatFailure, ChatPage, ChatSummary, valid_id},
        native_auth::Session,
        remote_control::{RemoteEvent, RemoteEventKind, RemoteSession},
        tools::{MAX_TOOL_RESULT_BYTES, OutputActionReceipt, ToolRequest, ToolResult},
    },
    services::chat::ChatApi,
};

#[derive(Clone)]
pub struct CortexClient {
    client: Client,
    base: String,
    session: Arc<Session>,
}

impl CortexClient {
    pub fn new(session: Arc<Session>) -> Result<Self, ChatFailure> {
        Self::with_base(
            &format!(
                "{}{}",
                crate::core::settings::GATEWAY_URL,
                crate::core::settings::CORTEX_API_PATH
            ),
            session,
        )
    }

    fn with_base(base: &str, session: Arc<Session>) -> Result<Self, ChatFailure> {
        if !session.is_valid() {
            return Err(ChatFailure::Unauthorized);
        }
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(900))
            .build()
            .map_err(|_| ChatFailure::Connection)?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
            session,
        })
    }

    fn authenticated(&self, request: RequestBuilder, mutation: bool) -> RequestBuilder {
        let request = request.header(
            "Cookie",
            format!(
                "sid={}; csrf={}",
                self.session.session_id, self.session.csrf_token
            ),
        );
        if mutation {
            request.header("X-CSRF-Token", &self.session.csrf_token)
        } else {
            request
        }
    }

    fn checked(response: Response) -> Result<Response, ChatFailure> {
        if response.status().is_success() {
            return Ok(response);
        }
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(ChatFailure::Unauthorized);
        }
        let value = Self::json(response).ok();
        let number = value
            .as_ref()
            .and_then(|v| v["data"]["error_number"].as_u64())
            .and_then(|v| u16::try_from(v).ok());
        Err(ChatFailure::Rejected(number))
    }

    fn json(response: Response) -> Result<Value, ChatFailure> {
        let mut body = Vec::new();
        response
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut body)
            .map_err(|_| ChatFailure::Connection)?;
        if body.len() > 4 * 1024 * 1024 {
            return Err(ChatFailure::InvalidResponse);
        }
        serde_json::from_slice(&body).map_err(|_| ChatFailure::InvalidResponse)
    }

    fn get(&self, path: &str) -> Result<Value, ChatFailure> {
        let response = self
            .authenticated(
                self.client
                    .get(format!("{}{path}", self.base))
                    .timeout(Duration::from_secs(20)),
                false,
            )
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        Self::json(Self::checked(response)?)
    }

    fn output_action(response: Response) -> Result<OutputActionReceipt, ChatFailure> {
        let value = Self::json(Self::checked(response)?)?;
        serde_json::from_value(value["data"].clone()).map_err(|_| ChatFailure::InvalidResponse)
    }

    pub fn register_remote_session(&self) -> Result<RemoteSession, ChatFailure> {
        let request = self
            .client
            .post(format!("{}/remote/sessions", self.base))
            .json(&json!({
                "device_name": "Cortex CLI",
                "platform": std::env::consts::OS
            }));
        let response = self
            .authenticated(request, true)
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        let value = Self::json(Self::checked(response)?)?;
        let remote: RemoteSession = serde_json::from_value(value["data"].clone())
            .map_err(|_| ChatFailure::InvalidResponse)?;
        if !valid_id(&remote.session_id) || !remote.online {
            return Err(ChatFailure::InvalidResponse);
        }
        Ok(remote)
    }

    pub fn unregister_remote_session(&self, session: &str) -> Result<(), ChatFailure> {
        if !valid_id(session) {
            return Err(ChatFailure::InvalidInput);
        }
        let request = self
            .client
            .delete(format!("{}/remote/sessions/{session}", self.base))
            .timeout(Duration::from_secs(20));
        let response = self
            .authenticated(request, true)
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        Self::checked(response).map(|_| ())
    }

    pub fn remote_commands(
        &self,
        session: &str,
        cursor: u64,
    ) -> Result<Box<dyn Read + Send>, ChatFailure> {
        if !valid_id(session) {
            return Err(ChatFailure::InvalidInput);
        }
        let request = self
            .client
            .get(format!(
                "{}/remote/sessions/{session}/commands?cursor={cursor}",
                self.base
            ))
            .header("Accept", "text/event-stream");
        let response = self
            .authenticated(request, false)
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        let response = Self::checked(response)?;
        let kind = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        if kind != "text/event-stream" {
            return Err(ChatFailure::InvalidResponse);
        }
        Ok(Box::new(response))
    }

    pub fn post_remote_event(
        &self,
        session: &str,
        kind: RemoteEventKind,
        payload: Value,
        ack_command_id: Option<u64>,
    ) -> Result<RemoteEvent, ChatFailure> {
        if !valid_id(session) {
            return Err(ChatFailure::InvalidInput);
        }
        let request = self
            .client
            .post(format!("{}/remote/sessions/{session}/events", self.base))
            .timeout(Duration::from_secs(20))
            .json(&json!({
                "kind": kind,
                "payload": payload,
                "ack_command_id": ack_command_id
            }));
        let response = self
            .authenticated(request, true)
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        let value = Self::json(Self::checked(response)?)?;
        serde_json::from_value(value["data"].clone()).map_err(|_| ChatFailure::InvalidResponse)
    }

    pub fn acknowledge_remote_command(
        &self,
        session: &str,
        command_id: u64,
        status: &str,
    ) -> Result<(), ChatFailure> {
        self.post_remote_event(
            session,
            RemoteEventKind::Progress,
            json!({ "status": status, "command_id": command_id }),
            Some(command_id),
        )
        .map(|_| ())
    }
}

impl CortexClient {
    fn send_request(
        &self,
        chat: Option<&str>,
        text: &str,
        file_ids: &[String],
        tools: bool,
    ) -> Result<Box<dyn Read + Send>, ChatFailure> {
        if text.trim().is_empty() {
            return Err(ChatFailure::InvalidInput);
        }
        let mut input = json!({"text":text,"stream":true});
        if !file_ids.is_empty() {
            input["file_ids"] = json!(file_ids);
        }
        let (path, body) = match chat {
            Some(id) if valid_id(id) => (format!("/chats/{id}/inputs"), input),
            Some(_) => return Err(ChatFailure::InvalidInput),
            None => {
                input["app_id"] = json!("cortex");
                ("/chats".into(), input)
            }
        };
        let request = self
            .client
            .post(format!("{}{path}", self.base))
            .json(&body)
            .header("Accept", "text/event-stream");
        let request = if tools {
            request.header("x-cortex-tools", "mapped-v1")
        } else {
            request
        };
        let response = self
            .authenticated(request, true)
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        let response = Self::checked(response)?;
        let kind = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        if kind != "text/event-stream" {
            return Err(ChatFailure::InvalidResponse);
        }
        Ok(Box::new(response))
    }
}

impl ChatApi for CortexClient {
    fn send(&self, chat: Option<&str>, text: &str) -> Result<Box<dyn Read + Send>, ChatFailure> {
        self.send_with_files(chat, text, &[])
    }

    fn send_with_files(
        &self,
        chat: Option<&str>,
        text: &str,
        file_ids: &[String],
    ) -> Result<Box<dyn Read + Send>, ChatFailure> {
        self.send_request(chat, text, file_ids, false)
    }

    fn send_tools_with_files(
        &self,
        chat: Option<&str>,
        text: &str,
        file_ids: &[String],
    ) -> Result<Box<dyn Read + Send>, ChatFailure> {
        self.send_request(chat, text, file_ids, true)
    }

    fn submit_tool_result(
        &self,
        chat: &str,
        output: &str,
        result: &ToolResult,
    ) -> Result<(), ChatFailure> {
        if !valid_id(chat)
            || !valid_id(output)
            || !valid_id(&result.call_id)
            || result.output.len() > MAX_TOOL_RESULT_BYTES
        {
            return Err(ChatFailure::InvalidInput);
        }
        let response = self
            .authenticated(
                self.client
                    .post(format!(
                        "{}/chats/{chat}/outputs/{output}/tool-results",
                        self.base
                    ))
                    .timeout(Duration::from_secs(20))
                    .json(&json!({
                        "call_id": result.call_id,
                        "name": result.name.as_str(),
                        "result": {"output": result.output}
                    })),
                true,
            )
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        let value = Self::json(Self::checked(response)?)?;
        if value["data"]["accepted"] != true || value["data"]["call_id"] != result.call_id {
            return Err(ChatFailure::InvalidResponse);
        }
        Ok(())
    }

    fn create_output_action(
        &self,
        remote_session: &str,
        chat: &str,
        output: &str,
        request: &ToolRequest,
    ) -> Result<OutputActionReceipt, ChatFailure> {
        if !valid_id(remote_session)
            || !valid_id(chat)
            || !valid_id(output)
            || !valid_id(&request.call_id)
        {
            return Err(ChatFailure::InvalidInput);
        }
        let response = self
            .authenticated(
                self.client
                    .post(format!(
                        "{}/remote/sessions/{remote_session}/outputs/{chat}/{output}/actions",
                        self.base
                    ))
                    .timeout(Duration::from_secs(20))
                    .json(&json!({"call_id":request.call_id})),
                true,
            )
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        let action = Self::output_action(response)?;
        if !action.matches(request) || action.status != "pending" {
            return Err(ChatFailure::InvalidResponse);
        }
        Ok(action)
    }

    fn get_output_action(
        &self,
        remote_session: &str,
        chat: &str,
        output: &str,
        action_id: &str,
    ) -> Result<OutputActionReceipt, ChatFailure> {
        if !valid_id(remote_session) || !valid_id(chat) || !valid_id(output) || !valid_id(action_id)
        {
            return Err(ChatFailure::InvalidInput);
        }
        let value = self.get(&format!(
            "/remote/sessions/{remote_session}/outputs/{chat}/{output}/actions/{action_id}"
        ))?;
        serde_json::from_value(value["data"].clone()).map_err(|_| ChatFailure::InvalidResponse)
    }

    fn answer_output_action(
        &self,
        remote_session: &str,
        chat: &str,
        output: &str,
        action_id: &str,
        approved: bool,
    ) -> Result<OutputActionReceipt, ChatFailure> {
        if !valid_id(remote_session) || !valid_id(chat) || !valid_id(output) || !valid_id(action_id)
        {
            return Err(ChatFailure::InvalidInput);
        }
        let response = self
            .authenticated(
                self.client
                    .post(format!(
                        "{}/remote/sessions/{remote_session}/outputs/{chat}/{output}/actions/{action_id}/answer",
                        self.base
                    ))
                    .timeout(Duration::from_secs(20))
                    .json(&json!({"approved":approved})),
                true,
            )
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        Self::output_action(response)
    }

    fn claim_output_action(
        &self,
        remote_session: &str,
        chat: &str,
        output: &str,
        action_id: &str,
        request: &ToolRequest,
    ) -> Result<OutputActionReceipt, ChatFailure> {
        if !valid_id(remote_session)
            || !valid_id(chat)
            || !valid_id(output)
            || !valid_id(action_id)
            || !valid_id(&request.call_id)
        {
            return Err(ChatFailure::InvalidInput);
        }
        let response = self
            .authenticated(
                self.client
                    .post(format!(
                        "{}/remote/sessions/{remote_session}/outputs/{chat}/{output}/actions/{action_id}/claim",
                        self.base
                    ))
                    .timeout(Duration::from_secs(20))
                    .json(&json!({
                        "call_id":request.call_id,
                        "action_digest":request.action_digest()
                    })),
                true,
            )
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        let action = Self::output_action(response)?;
        if !action.matches(request) || action.status != "claimed" {
            return Err(ChatFailure::InvalidResponse);
        }
        Ok(action)
    }

    fn chats(&self) -> Result<Vec<ChatSummary>, ChatFailure> {
        let value = self.get("/chats")?;
        let rows: Vec<ChatSummary> = serde_json::from_value(value["data"].clone())
            .map_err(|_| ChatFailure::InvalidResponse)?;
        if rows.iter().any(|row| !valid_id(&row.session_id)) {
            return Err(ChatFailure::InvalidResponse);
        }
        Ok(rows)
    }

    fn history(&self, id: &str) -> Result<ChatPage, ChatFailure> {
        if !valid_id(id) {
            return Err(ChatFailure::InvalidInput);
        }
        let value = self.get(&format!("/chats/{id}?limit=20"))?;
        let page: ChatPage = serde_json::from_value(value["data"].clone())
            .map_err(|_| ChatFailure::InvalidResponse)?;
        if !page.chat.session_id.eq_ignore_ascii_case(id) {
            return Err(ChatFailure::InvalidResponse);
        }
        Ok(page)
    }

    fn reconnect(
        &self,
        chat: &str,
        output: &str,
        after: u64,
    ) -> Result<Box<dyn Read + Send>, ChatFailure> {
        if !valid_id(chat) || !valid_id(output) {
            return Err(ChatFailure::InvalidInput);
        }
        let response = self
            .authenticated(
                self.client.get(format!(
                    "{}/chats/{chat}/outputs/{output}/events?after={after}",
                    self.base
                )),
                false,
            )
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        Ok(Box::new(Self::checked(response)?))
    }

    fn cancel(&self, chat: &str, output: &str) -> Result<(), ChatFailure> {
        if !valid_id(chat) || !valid_id(output) {
            return Err(ChatFailure::InvalidInput);
        }
        let request = self
            .client
            .patch(format!("{}/chats/{chat}/outputs/{output}", self.base))
            .timeout(Duration::from_secs(20))
            .json(&json!({"status":"cancelled"}));
        let response = self
            .authenticated(request, true)
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        Self::checked(response).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::TcpListener, thread};
    const CHAT: &str = "10000000-0000-4000-8000-000000000001";
    const OUTPUT: &str = "20000000-0000-4000-8000-000000000002";

    fn server(status: u16, kind: &str, body: String) -> (CortexClient, thread::JoinHandle<String>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let kind = kind.to_owned();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut raw = Vec::new();
            let mut byte = [0];
            while !raw.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                raw.push(byte[0]);
                assert!(raw.len() < 65536);
            }
            let headers = String::from_utf8(raw.clone()).unwrap();
            let size = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut payload = vec![0; size];
            stream.read_exact(&mut payload).unwrap();
            raw.extend(payload);
            write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            String::from_utf8(raw).unwrap()
        });
        let client = CortexClient::with_base(
            &format!("http://{address}/apps/cortex/v1"),
            Arc::new(Session {
                session_id: "fixture-session".into(),
                csrf_token: "fixture-csrf".into(),
            }),
        )
        .unwrap();
        (client, worker)
    }

    #[test]
    fn new_and_next_inputs_match_the_real_gateway_contract() {
        for id in [None, Some(CHAT)] {
            let (client, worker) = server(200, "text/event-stream", String::new());
            drop(client.send(id, "สวัสดี").unwrap());
            let request = worker.join().unwrap();
            let (head, body) = request.split_once("\r\n\r\n").unwrap();
            let expected = id
                .map(|id| format!("/chats/{id}/inputs"))
                .unwrap_or("/chats".into());
            assert!(head.starts_with(&format!("POST /apps/cortex/v1{expected} HTTP/1.1")));
            let lower = head.to_lowercase();
            assert!(lower.contains("cookie: sid=fixture-session; csrf=fixture-csrf"));
            assert!(lower.contains("x-csrf-token: fixture-csrf"));
            assert!(!lower.contains("x-uuid:"));
            let value: Value = serde_json::from_str(body).unwrap();
            assert_eq!(value["text"], "สวัสดี");
            assert_eq!(value["stream"], true);
            if id.is_none() {
                assert_eq!(value["app_id"], "cortex");
                assert_eq!(value.as_object().unwrap().len(), 3);
            } else {
                assert!(value.get("app_id").is_none());
                assert_eq!(value.as_object().unwrap().len(), 2);
            }
            assert!(value.get("session_id").is_none());
            assert!(value.get("user_id").is_none());
        }
    }

    #[test]
    fn opted_in_chat_keeps_dtos_unchanged_and_selects_mapped_tools() {
        let (client, worker) = server(200, "text/event-stream", String::new());
        drop(client.send_tools_with_files(None, "inspect", &[]).unwrap());
        let request = worker.join().unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("POST /apps/cortex/v1/chats HTTP/1.1"));
        assert!(head.to_lowercase().contains("x-cortex-tools: mapped-v1"));
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body,
            json!({"app_id":"cortex","text":"inspect","stream":true})
        );
    }

    #[test]
    fn tool_result_uses_the_exact_output_route_and_matching_call_envelope() {
        let call_id = "30000000-0000-4000-8000-000000000003";
        let response =
            json!({"message":"Tool result accepted","data":{"accepted":true,"call_id":call_id}})
                .to_string();
        let (client, worker) = server(202, "application/json", response);
        client
            .submit_tool_result(
                CHAT,
                OUTPUT,
                &ToolResult {
                    call_id: call_id.into(),
                    name: crate::domain::models::tools::ToolName::ViewFile,
                    output: "1: example".into(),
                },
            )
            .unwrap();
        let request = worker.join().unwrap();
        assert!(request.starts_with(&format!(
            "POST /apps/cortex/v1/chats/{CHAT}/outputs/{OUTPUT}/tool-results HTTP/1.1"
        )));
        let lower = request.to_lowercase();
        assert!(lower.contains("x-csrf-token: fixture-csrf"));
        assert!(lower.contains("cookie: sid=fixture-session; csrf=fixture-csrf"));
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            json!({
                "call_id":call_id,
                "name":"view_file",
                "result":{"output":"1: example"}
            })
        );
    }

    #[test]
    fn output_action_registration_sends_only_the_call_id_and_checks_server_binding() {
        let request = ToolRequest::from_event(&json!({
            "call_id":"30000000-0000-4000-8000-000000000003",
            "name":"run_command",
            "arguments":{"cwd":".","argv":["echo","ok"]}
        }))
        .unwrap();
        let action_id = "40000000-0000-4000-8000-000000000004";
        let digest = request.action_digest();
        let display = "cwd: .\ncommand: [\"echo\",\"ok\"]";
        let body = json!({"data":{
            "action_id":action_id,
            "call_id":&request.call_id,
            "name":"run_command",
            "action_kind":"command",
            "action_digest":digest,
            "display":display,
            "expires_at":"2026-11-25T04:00:00Z",
            "status":"pending"
        }})
        .to_string();
        let (client, worker) = server(201, "application/json", body);
        let action = client
            .create_output_action(
                "50000000-0000-4000-8000-000000000005",
                CHAT,
                OUTPUT,
                &request,
            )
            .unwrap();
        assert_eq!(action.action_id, action_id);
        let raw = worker.join().unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with(&format!("POST /apps/cortex/v1/remote/sessions/50000000-0000-4000-8000-000000000005/outputs/{CHAT}/{OUTPUT}/actions HTTP/1.1")));
        assert!(head.to_lowercase().contains("x-csrf-token:"));
        let payload: Value = serde_json::from_str(body).unwrap();
        assert_eq!(payload, json!({"call_id":request.call_id}));
    }

    #[test]
    fn reconnect_uses_query_cursor_and_cancel_uses_existing_patch() {
        let (client, worker) = server(200, "text/event-stream", String::new());
        drop(client.reconnect(CHAT, OUTPUT, 42).unwrap());
        let request = worker.join().unwrap();
        assert!(request.starts_with(&format!(
            "GET /apps/cortex/v1/chats/{CHAT}/outputs/{OUTPUT}/events?after=42 HTTP/1.1"
        )));
        assert!(!request.to_lowercase().contains("last-event-id:"));
        let (client, worker) = server(
            200,
            "application/json",
            json!({"message":"Output cancel sent","data":{"session_id":CHAT,"output_id":OUTPUT}})
                .to_string(),
        );
        client.cancel(CHAT, OUTPUT).unwrap();
        let request = worker.join().unwrap();
        assert!(request.starts_with(&format!(
            "PATCH /apps/cortex/v1/chats/{CHAT}/outputs/{OUTPUT} HTTP/1.1"
        )));
        assert!(
            request
                .to_lowercase()
                .contains("x-csrf-token: fixture-csrf")
        );
        assert_eq!(
            serde_json::from_str::<Value>(request.split_once("\r\n\r\n").unwrap().1).unwrap(),
            json!({"status":"cancelled"})
        );
    }

    #[test]
    fn process_numbers_and_private_upstream_messages_stay_separate() {
        let (client, worker) = server(
            400,
            "application/json",
            json!({"message":"private upstream value","data":{"error_number":1201}}).to_string(),
        );
        let result = client.send(None, "fixture");
        assert!(matches!(result, Err(ChatFailure::Rejected(Some(1201)))));
        worker.join().unwrap();
        assert!(
            !ChatFailure::Rejected(Some(1201))
                .to_string()
                .contains("private upstream value")
        );
    }

    #[test]
    fn wrong_media_type_is_not_interpreted_as_a_chat_stream() {
        let (client, worker) = server(200, "application/json", "{}".into());
        assert!(matches!(
            client.send(None, "fixture"),
            Err(ChatFailure::InvalidResponse)
        ));
        worker.join().unwrap();
    }

    #[test]
    fn remote_session_registration_and_unregistration_use_the_authenticated_routes() {
        let registration = json!({
            "message":"Remote session registered",
            "data":{
                "session_id":CHAT,
                "device_name":"Cortex CLI",
                "platform":std::env::consts::OS,
                "last_seen_ms":123,
                "online":true
            }
        });
        let (client, worker) = server(201, "application/json", registration.to_string());
        let session = client.register_remote_session().unwrap();
        assert_eq!(session.session_id, CHAT);
        let request = worker.join().unwrap();
        assert!(request.starts_with("POST /apps/cortex/v1/remote/sessions HTTP/1.1"));
        let lower = request.to_lowercase();
        assert!(lower.contains("cookie: sid=fixture-session; csrf=fixture-csrf"));
        assert!(lower.contains("x-csrf-token: fixture-csrf"));
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            json!({"device_name":"Cortex CLI","platform":std::env::consts::OS})
        );

        let (client, worker) = server(
            200,
            "application/json",
            json!({"message":"Remote session unregistered","data":{"session_id":CHAT}}).to_string(),
        );
        client.unregister_remote_session(CHAT).unwrap();
        assert!(worker.join().unwrap().starts_with(&format!(
            "DELETE /apps/cortex/v1/remote/sessions/{CHAT} HTTP/1.1"
        )));
    }

    #[test]
    fn remote_commands_use_the_cursor_and_remote_events_match_the_api_contract() {
        let (client, worker) = server(200, "text/event-stream", String::new());
        drop(client.remote_commands(CHAT, 42).unwrap());
        let request = worker.join().unwrap();
        assert!(request.starts_with(&format!(
            "GET /apps/cortex/v1/remote/sessions/{CHAT}/commands?cursor=42 HTTP/1.1"
        )));
        assert!(request.to_lowercase().contains("accept: text/event-stream"));

        let response = json!({"message":"Remote event recorded","data":{
            "id":7,"kind":"progress","payload":{"status":"command_received","command_id":6},"created_at_ms":321
        }});
        let (client, worker) = server(201, "application/json", response.to_string());
        client
            .acknowledge_remote_command(CHAT, 6, "command_received")
            .unwrap();
        let request = worker.join().unwrap();
        assert!(request.starts_with(&format!(
            "POST /apps/cortex/v1/remote/sessions/{CHAT}/events HTTP/1.1"
        )));
        assert!(
            request
                .to_lowercase()
                .contains("x-csrf-token: fixture-csrf")
        );
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            json!({
                "kind":"progress",
                "payload":{"status":"command_received","command_id":6},
                "ack_command_id":6
            })
        );
    }

    #[test]
    fn remote_prompt_file_ids_are_sent_only_as_explicit_existing_references() {
        let (client, worker) = server(200, "text/event-stream", String::new());
        drop(
            client
                .send_with_files(None, "look at this", &["fsm-ref".to_owned()])
                .unwrap(),
        );
        let request = worker.join().unwrap();
        assert!(request.starts_with("POST /apps/cortex/v1/chats HTTP/1.1"));
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["file_ids"], json!(["fsm-ref"]));
        assert_eq!(body["app_id"], "cortex");
        assert_eq!(body["text"], "look at this");
        assert_eq!(body["stream"], true);
        assert_eq!(body.as_object().unwrap().len(), 4);
    }
}
