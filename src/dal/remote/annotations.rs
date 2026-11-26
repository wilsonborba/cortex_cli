//! Owner-scoped Annotations reads through the authenticated gateway.

use crate::domain::models::{
    annotations::{NoteSection, NotesApi, SelectedNote},
    chat::{ChatFailure, valid_id},
    native_auth::Session,
};
use reqwest::{
    blocking::{Client, Response},
    redirect::Policy,
};
use serde_json::Value;
use std::{io::Read, sync::Arc, time::Duration};

const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct AnnotationsClient {
    client: Client,
    base: String,
    session: Arc<Session>,
}

impl AnnotationsClient {
    pub fn new(session: Arc<Session>) -> Result<Self, ChatFailure> {
        Self::with_base(
            &format!(
                "{}{}",
                crate::core::settings::GATEWAY_URL,
                "/apps/annotations/v1"
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
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| ChatFailure::Connection)?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
            session,
        })
    }

    fn json(response: Response) -> Result<Value, ChatFailure> {
        if response.status().as_u16() == 401 || response.status().as_u16() == 403 {
            return Err(ChatFailure::Unauthorized);
        }
        if !response.status().is_success() {
            return Err(ChatFailure::Rejected(None));
        }
        let mut body = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|_| ChatFailure::Connection)?;
        if body.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(ChatFailure::InvalidResponse);
        }
        serde_json::from_slice(&body).map_err(|_| ChatFailure::InvalidResponse)
    }

    fn read_note(&self, note_id: &str) -> Result<SelectedNote, ChatFailure> {
        if !valid_id(note_id) {
            return Err(ChatFailure::InvalidInput);
        }
        let response = self
            .client
            .get(format!("{}/me/notes/{note_id}?view=typed", self.base))
            .header(
                "Cookie",
                format!(
                    "sid={}; csrf={}",
                    self.session.session_id, self.session.csrf_token
                ),
            )
            .header("Accept", "application/json")
            .send()
            .map_err(|_| ChatFailure::Connection)?;
        let value = Self::json(response)?;
        let returned_id = value["note_id"]
            .as_str()
            .filter(|id| valid_id(id) && id.eq_ignore_ascii_case(note_id))
            .ok_or(ChatFailure::InvalidResponse)?;
        let revision = value["revision"]
            .as_u64()
            .filter(|revision| *revision > 0)
            .ok_or(ChatFailure::InvalidResponse)?;
        let typed = value["representations"]
            .as_array()
            .ok_or(ChatFailure::InvalidResponse)?
            .iter()
            .filter(|representation| representation["representation"] == "typed")
            .collect::<Vec<_>>();
        if typed.len() != 1 {
            return Err(ChatFailure::InvalidResponse);
        }
        let revision_id = typed[0]["revision_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or(ChatFailure::InvalidResponse)?;
        let document = &typed[0]["document"];
        let blocks = document["blocks"]
            .as_array()
            .ok_or(ChatFailure::InvalidResponse)?;
        let mut sections = Vec::new();
        for block in blocks {
            let id = block["id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 128)
                .ok_or(ChatFailure::InvalidResponse)?;
            let text = block_text(block)?;
            if !text.is_empty() {
                sections.push(NoteSection {
                    id: id.to_owned(),
                    text,
                });
            }
        }
        let title = value["title"].as_str().map(str::to_owned);
        Ok(SelectedNote {
            note_id: returned_id.to_owned(),
            revision,
            revision_id: revision_id.to_owned(),
            title,
            sections,
        })
    }
}

fn spans(value: &Value) -> Result<String, ChatFailure> {
    let values = value.as_array().ok_or(ChatFailure::InvalidResponse)?;
    let mut result = String::new();
    for span in values {
        result.push_str(span["text"].as_str().ok_or(ChatFailure::InvalidResponse)?);
    }
    Ok(result)
}

fn block_text(block: &Value) -> Result<String, ChatFailure> {
    match block["kind"].as_str().ok_or(ChatFailure::InvalidResponse)? {
        "paragraph" => spans(&block["spans"]),
        "heading" => {
            let level = block["level"]
                .as_u64()
                .filter(|n| (1..=6).contains(n))
                .ok_or(ChatFailure::InvalidResponse)?;
            Ok(format!(
                "{} {}",
                "#".repeat(level as usize),
                spans(&block["spans"])?
            ))
        }
        "list" => {
            let items = block["items"]
                .as_array()
                .ok_or(ChatFailure::InvalidResponse)?;
            let ordered = block["ordered"]
                .as_bool()
                .ok_or(ChatFailure::InvalidResponse)?;
            let mut lines = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let prefix = if ordered {
                    format!("{}. ", index + 1)
                } else {
                    "• ".to_owned()
                };
                lines.push(format!("{prefix}{}", spans(&item["spans"])?));
            }
            Ok(lines.join("\n"))
        }
        "table" => {
            let rows = block["rows"]
                .as_array()
                .ok_or(ChatFailure::InvalidResponse)?;
            let header = block
                .get("header")
                .map(|value| value.as_bool().ok_or(ChatFailure::InvalidResponse))
                .transpose()?
                .unwrap_or(false);
            let mut lines = Vec::with_capacity(rows.len() + usize::from(header));
            for (index, row) in rows.iter().enumerate() {
                let cells = row["cells"]
                    .as_array()
                    .ok_or(ChatFailure::InvalidResponse)?;
                let mut values = Vec::with_capacity(cells.len());
                for cell in cells {
                    values.push(spans(&cell["spans"])?);
                }
                if header {
                    lines.push(format!("| {} |", values.join(" | ")));
                    if index == 0 {
                        lines.push(format!("| {} |", vec!["---"; values.len()].join(" | ")));
                    }
                } else {
                    lines.push(values.join(" | "));
                }
            }
            Ok(lines.join("\n"))
        }
        "figure" => block
            .get("caption")
            .map(spans)
            .transpose()
            .map(|v| v.unwrap_or_default()),
        // Handwriting is an ink payload, not typed text.
        "handwriting" => Ok(String::new()),
        _ => Err(ChatFailure::InvalidResponse),
    }
}

impl NotesApi for AnnotationsClient {
    fn selected_note(&self, note_id: &str) -> Result<SelectedNote, ChatFailure> {
        self.read_note(note_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::TcpListener, thread};

    const NOTE: &str = "10000000-0000-4000-8000-000000000001";

    fn fixture(body: String, status: u16) -> (AnnotationsClient, thread::JoinHandle<String>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 65536);
            }
            write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            String::from_utf8(request).unwrap()
        });
        let client = AnnotationsClient::with_base(
            &format!("http://{address}/apps/annotations/v1"),
            Arc::new(Session {
                session_id: "fixture-session".into(),
                csrf_token: "fixture-csrf".into(),
            }),
        )
        .unwrap();
        (client, worker)
    }

    #[test]
    fn selected_note_uses_owner_route_typed_view_and_gateway_session() {
        let body = serde_json::json!({
            "note_id": NOTE,
            "revision": 4,
            "title": "Cells",
            "representations": [{
                "representation": "typed",
                "revision_id": "20000000-0000-4000-8000-000000000002",
                "document": {"blocks": [
                    {"id": "b1", "kind": "paragraph", "spans": [{"text": "Cell "}, {"text": "wall ไทย"}]},
                    {"id": "b2", "kind": "list", "ordered": false, "items": [{"spans": [{"text": "Alpha"}]}, {"spans": [{"text": "Beta"}]}]},
                    {"id": "b3", "kind": "table", "header": true, "rows": [
                        {"cells": [{"spans": [{"text": "Los Angeles"}]}, {"spans": [{"text": "12"}]}]},
                        {"cells": [{"spans": [{"text": "New York"}]}, {"spans": [{"text": "13"}]}]}
                    ]},
                    {"id": "b4", "kind": "figure", "file_id": "30000000-0000-4000-8000-000000000003", "caption": [{"text": "Diagram"}]}
                ]}
            }],
            "publication": null
        }).to_string();
        let (client, worker) = fixture(body, 200);
        let note = client.selected_note(NOTE).unwrap();
        let request = worker.join().unwrap();
        assert!(request.starts_with(&format!(
            "GET /apps/annotations/v1/me/notes/{NOTE}?view=typed HTTP/1.1"
        )));
        let lower = request.to_lowercase();
        assert!(lower.contains("cookie: sid=fixture-session; csrf=fixture-csrf"));
        assert!(lower.contains("accept: application/json"));
        assert!(!lower.contains("x-uuid:"));
        assert_eq!(note.revision, 4);
        assert_eq!(note.sections[0].id, "b1");
        assert_eq!(note.sections[0].text, "Cell wall ไทย");
        assert_eq!(note.sections[1].text, "• Alpha\n• Beta");
        assert_eq!(
            note.sections[2].text,
            "| Los Angeles | 12 |\n| --- | --- |\n| New York | 13 |"
        );
        assert_eq!(note.sections[3].text, "Diagram");
    }

    #[test]
    fn selected_note_rejects_wrong_owner_identity_and_wrong_shape() {
        let body =
            serde_json::json!({"note_id": "20000000-0000-4000-8000-000000000002"}).to_string();
        let (client, worker) = fixture(body, 200);
        assert!(matches!(
            client.selected_note(NOTE),
            Err(ChatFailure::InvalidResponse)
        ));
        worker.join().unwrap();

        let (client, worker) = fixture("{}".into(), 404);
        assert!(matches!(
            client.selected_note(NOTE),
            Err(ChatFailure::Rejected(None))
        ));
        worker.join().unwrap();

        let (client, worker) = fixture("{}".into(), 403);
        assert!(matches!(
            client.selected_note(NOTE),
            Err(ChatFailure::Unauthorized)
        ));
        worker.join().unwrap();
    }
}
