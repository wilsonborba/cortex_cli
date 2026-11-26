//! The existing gateway contract. Never expose upstream errors, bodies or URLs.

use std::{io::Read, time::Duration};

use reqwest::{
    blocking::{Client, Response},
    redirect::Policy,
};
use serde::Serialize;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::domain::{
    models::{
        native_auth::{AuthFailure, FinishedLogin, Handoff, Session},
        telemetry::{LegalStatus, TrackingBatch, TrackingError},
    },
    services::auth::Gateway,
    services::telemetry::{LegalTelemetryApi, TelemetryError, TrackingApi},
};

pub struct GatewayClient {
    client: Client,
    base: String,
}

impl GatewayClient {
    pub fn new(base: &str) -> Result<Self, AuthFailure> {
        let url = reqwest::Url::parse(base).map_err(|_| AuthFailure::Invalid)?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(AuthFailure::Invalid);
        }
        Self::with_base(base)
    }

    fn with_base(base: &str) -> Result<Self, AuthFailure> {
        let client = Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(Policy::none())
            .build()
            .map_err(|_| AuthFailure::Transport)?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    fn exchange(&self, handoff: &Handoff, cancel: bool) -> Result<Value, AuthFailure> {
        #[derive(Serialize)]
        struct Exchange<'a> {
            app: &'static str,
            handoff_id: &'a str,
            secret: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            cancel: Option<bool>,
        }
        let body = Exchange {
            app: "cortex",
            handoff_id: &handoff.id,
            secret: &handoff.secret,
            cancel: cancel.then_some(true),
        };
        let payload = Zeroizing::new(serde_json::to_vec(&body).map_err(|_| AuthFailure::Invalid)?);
        let response = self
            .client
            .post(format!("{}/apps/api/v1/exchange", self.base))
            .header("content-type", "application/json")
            .body(payload.to_vec())
            .send()
            .map_err(|_| AuthFailure::Transport)?;
        decode(response)
    }

    fn authenticated_json<T: Serialize>(
        &self,
        session: &Session,
        path: &str,
        body: Option<&T>,
    ) -> Result<Value, AuthFailure> {
        if !session.is_valid() {
            return Err(AuthFailure::Invalid);
        }
        let url = format!("{}{path}", self.base);
        let mut request = if body.is_some() {
            self.client.post(url)
        } else {
            self.client.get(url)
        }
        .header(
            "Cookie",
            format!("sid={}; csrf={}", session.session_id, session.csrf_token),
        );
        if let Some(body) = body {
            request = request
                .header("X-CSRF-Token", &session.csrf_token)
                .json(body);
        }
        let response = request.send().map_err(|_| AuthFailure::Transport)?;
        decode(response)
    }

    pub fn post_tracking_batch(
        &self,
        session: &Session,
        batch: &TrackingBatch,
    ) -> Result<(), AuthFailure> {
        self.authenticated_json(session, "/apps/api/v1/telemetry/events", Some(batch))
            .map(|_| ())
    }

    pub fn post_tracking_error(
        &self,
        session: &Session,
        error: &TrackingError,
    ) -> Result<(), AuthFailure> {
        self.authenticated_json(session, "/apps/api/v1/telemetry/errors", Some(error))
            .map(|_| ())
    }
}

impl LegalTelemetryApi for GatewayClient {
    fn legal_status(&self, session: &Session) -> Result<LegalStatus, TelemetryError> {
        let value = self
            .authenticated_json::<Value>(session, "/apps/domain/v1/legal/status", None)
            .map_err(|_| TelemetryError)?;
        let status: LegalStatus =
            serde_json::from_value(value["data"].clone()).map_err(|_| TelemetryError)?;
        if status.privacy_policy.current_version.is_empty()
            || status.privacy_policy.current_version.len() > 40
            || (status.privacy_policy.accepted
                && status.privacy_policy.version.as_deref()
                    != Some(status.privacy_policy.current_version.as_str()))
        {
            return Err(TelemetryError);
        }
        Ok(status)
    }
}

impl TrackingApi for GatewayClient {
    fn post_batch(&self, session: &Session, batch: &TrackingBatch) {
        let _ = self.post_tracking_batch(session, batch);
    }

    fn post_error(&self, session: &Session, error: &TrackingError) {
        let _ = self.post_tracking_error(session, error);
    }
}

impl Gateway for GatewayClient {
    fn finish(&self, handoff: &Handoff) -> Result<FinishedLogin, AuthFailure> {
        let value = self.exchange(handoff, false)?;
        let data = &value["data"];
        let session = Session {
            session_id: data["session_id"]
                .as_str()
                .ok_or(AuthFailure::Invalid)?
                .to_owned(),
            csrf_token: data["csrf_token"]
                .as_str()
                .ok_or(AuthFailure::Invalid)?
                .to_owned(),
        };
        if !session.is_valid() || data["expires_in"].as_u64().is_none_or(|n| n == 0) {
            return Err(AuthFailure::Invalid);
        }
        Ok(FinishedLogin {
            session,
            city: data["location"]["city"].as_str().map(str::to_owned),
            country: data["location"]["country"].as_str().map(str::to_owned),
        })
    }

    fn cancel(&self, handoff: &Handoff) -> Result<(), AuthFailure> {
        let value = self.exchange(handoff, true)?;
        if value["data"]["status"] != "cancelled" {
            return Err(AuthFailure::Invalid);
        }
        Ok(())
    }

    fn logout(&self, session: &Session) -> Result<(), AuthFailure> {
        if !session.is_valid() {
            return Err(AuthFailure::Invalid);
        }
        let response = self
            .client
            .post(format!("{}/user/sync/v1/log-out", self.base))
            .header(
                "Cookie",
                format!("sid={}; csrf={}", session.session_id, session.csrf_token),
            )
            .header("X-CSRF-Token", &session.csrf_token)
            .send()
            .map_err(|_| AuthFailure::Transport)?;
        decode(response).map(|_| ())
    }
}

fn decode(response: Response) -> Result<Value, AuthFailure> {
    let status = response.status();
    let mut body = Zeroizing::new(Vec::new());
    response
        .take(65537)
        .read_to_end(&mut body)
        .map_err(|_| AuthFailure::Transport)?;
    if body.len() > 65536 {
        return Err(AuthFailure::Invalid);
    }
    let value: Value = serde_json::from_slice(&body).map_err(|_| AuthFailure::Invalid)?;
    if status.is_success() {
        return Ok(value);
    }
    Err(match value["data"]["code"].as_str().unwrap_or("") {
        "HANDOFF_NOT_FINISHED" => AuthFailure::NotFinished,
        "HANDOFF_EXPIRED" => AuthFailure::Expired,
        "HANDOFF_USED" => AuthFailure::Used,
        "HANDOFF_CANCELLED" => AuthFailure::Cancelled,
        "LOGIN_REFUSED_NETWORK" => AuthFailure::Refused,
        "NATIVE_HANDOFF_DISABLED" => AuthFailure::Disabled,
        "INVALID_HANDOFF" => AuthFailure::Invalid,
        "RATE_LIMITED" => AuthFailure::RateLimited,
        _ if status.as_u16() == 429 => AuthFailure::RateLimited,
        _ if status.is_server_error() => AuthFailure::Transport,
        _ => AuthFailure::Invalid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;
    use std::{io::Write, net::TcpListener, thread};

    fn server(status: u16, body: Value) -> (GatewayClient, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
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
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            let mut payload = vec![0; length];
            stream.read_exact(&mut payload).unwrap();
            raw.extend(payload);
            let body = body.to_string();
            write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            String::from_utf8(raw).unwrap()
        });
        (
            GatewayClient::with_base(&format!("http://{address}")).unwrap(),
            worker,
        )
    }

    fn assert_exchange(request: &str, cancelled: bool) {
        assert!(request.starts_with("POST /apps/api/v1/exchange HTTP/1.1\r\n"));
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(!headers.to_lowercase().contains("origin:"));
        assert!(!headers.to_lowercase().contains("authorization:"));
        let value: Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["app"], "cortex");
        assert_eq!(
            value.as_object().unwrap().len(),
            if cancelled { 4 } else { 3 }
        );
        for field in ["handoff_id", "secret"] {
            let text = value[field].as_str().unwrap();
            assert_eq!(text.len(), 43);
            assert_eq!(URL_SAFE_NO_PAD.decode(text).unwrap().len(), 32);
        }
        assert_eq!(value["handoff_id"], URL_SAFE_NO_PAD.encode([42; 32]));
        assert_eq!(value["secret"], URL_SAFE_NO_PAD.encode([24; 32]));
        assert_eq!(value.get("cancel"), cancelled.then_some(&Value::Bool(true)));
    }

    #[test]
    fn finish_and_cancel_use_the_existing_native_contract() {
        let handoff = Handoff::from_bytes([42; 32], [24; 32]);
        let (client, worker) = server(
            200,
            json!({"message":"Success","data":{"session_id":"fixture-session","csrf_token":"fixture-csrf","expires_in":2592000,"location":{"country":"TH"}}}),
        );
        let result = client.finish(&handoff).unwrap();
        assert_eq!(result.session.session_id, "fixture-session");
        assert_eq!(result.country.as_deref(), Some("TH"));
        assert_exchange(&worker.join().unwrap(), false);
        let (client, worker) = server(
            200,
            json!({"message":"Success","data":{"status":"cancelled"}}),
        );
        client.cancel(&handoff).unwrap();
        assert_exchange(&worker.join().unwrap(), true);
    }

    #[test]
    fn stable_gateway_errors_map_to_safe_messages() {
        for (status, code, expected) in [
            (409, "HANDOFF_NOT_FINISHED", AuthFailure::NotFinished),
            (410, "HANDOFF_EXPIRED", AuthFailure::Expired),
            (409, "HANDOFF_USED", AuthFailure::Used),
            (409, "HANDOFF_CANCELLED", AuthFailure::Cancelled),
            (403, "LOGIN_REFUSED_NETWORK", AuthFailure::Refused),
            (403, "NATIVE_HANDOFF_DISABLED", AuthFailure::Disabled),
            (400, "INVALID_HANDOFF", AuthFailure::Invalid),
            (429, "RATE_LIMITED", AuthFailure::RateLimited),
            (500, "UPSTREAM_BODY_MUST_NOT_ESCAPE", AuthFailure::Transport),
        ] {
            let (client, worker) = server(
                status,
                json!({"message":"UPSTREAM_BODY_MUST_NOT_ESCAPE","data":{"code":code}}),
            );
            let result = client.finish(&Handoff::from_bytes([42; 32], [24; 32]));
            assert!(matches!(result, Err(e) if e == expected));
            assert!(
                !expected
                    .to_string()
                    .contains("UPSTREAM_BODY_MUST_NOT_ESCAPE")
            );
            assert_exchange(&worker.join().unwrap(), false);
        }
    }

    #[test]
    fn invalid_sessions_and_unexpected_response_shapes_are_rejected() {
        for body in [
            json!({"session_id":"fixture-session","csrf_token":"fixture-csrf","expires_in":30}),
            json!({"data":{"session_id":"fixture-session","csrf_token":"fixture-csrf"}}),
            json!({"data":{"session_id":"fixture-session","csrf_token":"fixture-csrf","expires_in":0}}),
            json!({"data":{"session_id":"fixture\r\nInjected: value","csrf_token":"fixture-csrf","expires_in":30}}),
        ] {
            let (client, worker) = server(200, body);
            assert!(matches!(
                client.finish(&Handoff::from_bytes([42; 32], [24; 32])),
                Err(AuthFailure::Invalid)
            ));
            worker.join().unwrap();
        }
    }

    #[test]
    fn logout_sends_only_the_current_device_cookie_and_csrf() {
        let (client, worker) = server(
            200,
            json!({"message":"Logged out successfully.","data":null}),
        );
        client
            .logout(&Session {
                session_id: "fixture-session".into(),
                csrf_token: "fixture-csrf".into(),
            })
            .unwrap();
        let request = worker.join().unwrap().to_lowercase();
        assert!(request.starts_with("post /user/sync/v1/log-out http/1.1\r\n"));
        assert!(request.contains("cookie: sid=fixture-session; csrf=fixture-csrf\r\n"));
        assert!(request.contains("x-csrf-token: fixture-csrf\r\n"));
        assert!(!request.contains("all_devices"));
    }

    #[test]
    fn telemetry_uses_the_existing_legal_status_and_tracking_contracts() {
        let session = Session {
            session_id: "fixture-session".into(),
            csrf_token: "fixture-csrf".into(),
        };
        let (client, worker) = server(
            200,
            json!({"data":{"privacy_policy":{"accepted":true,"version":"2026-09-14","current_version":"2026-09-14"}}}),
        );
        let status = client.legal_status(&session).unwrap();
        assert!(status.privacy_policy.accepted_current_version());
        let request = worker.join().unwrap().to_lowercase();
        assert!(request.starts_with("get /apps/domain/v1/legal/status http/1.1\r\n"));
        assert!(request.contains("cookie: sid=fixture-session; csrf=fixture-csrf\r\n"));

        let (client, worker) = server(200, json!({"data":null}));
        client
            .post_tracking_batch(
                &session,
                &TrackingBatch {
                    app_id: "cortex",
                    session_id: "00000000-0000-4000-8000-000000000001".into(),
                    events: vec![crate::domain::models::telemetry::TrackingEvent {
                        seq: 0,
                        event_type: "submit",
                        screen: "terminal_chat",
                        route: "/chat",
                        path: "/chat",
                        element_id: "chat.request.completed",
                        element_label: "Chat request completed",
                        element_path: "terminal > chat > request",
                        value: None,
                        sensitive: false,
                    }],
                },
            )
            .unwrap();
        let request = worker.join().unwrap().to_lowercase();
        assert!(request.starts_with("post /apps/api/v1/telemetry/events http/1.1\r\n"));
        assert!(request.contains("x-csrf-token: fixture-csrf\r\n"));
        let body = request.split_once("\r\n\r\n").unwrap().1;
        let payload: Value = serde_json::from_str(body).unwrap();
        assert_eq!(payload["app_id"], "cortex");
        assert_eq!(payload["events"][0]["element_id"], "chat.request.completed");
        assert!(payload["events"][0]["value"].is_null());
        assert!(!body.contains("prompt"));
        assert!(!body.contains("response text"));
    }

    #[test]
    fn production_constructor_rejects_insecure_or_credentialed_origins() {
        for url in [
            "http://example.test",
            "https://user:password@example.test",
            "https://example.test/path",
            "https://example.test/?token=value",
        ] {
            assert!(matches!(GatewayClient::new(url), Err(AuthFailure::Invalid)));
        }
    }
}
