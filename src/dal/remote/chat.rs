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
        Self::with_base("https://api.asodya.com/apps/cortex/v1", session)
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
}

impl ChatApi for CortexClient {
    fn send(&self, chat: Option<&str>, text: &str) -> Result<Box<dyn Read + Send>, ChatFailure> {
        if text.trim().is_empty() {
            return Err(ChatFailure::InvalidInput);
        }
        let (path, body) = match chat {
            Some(id) if valid_id(id) => (
                format!("/chats/{id}/inputs"),
                json!({"text":text,"stream":true}),
            ),
            Some(_) => return Err(ChatFailure::InvalidInput),
            None => (
                "/chats".into(),
                json!({"app_id":"cortex","text":text,"stream":true}),
            ),
        };
        let request = self
            .client
            .post(format!("{}{path}", self.base))
            .json(&body)
            .header("Accept", "text/event-stream");
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
}
