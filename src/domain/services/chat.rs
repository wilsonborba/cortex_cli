//! Conversation selection and stream state from actual Cortex event data.

use crate::{
    dal::remote::sse::SseDecoder,
    domain::models::chat::{ChatFailure, ChatPage, ChatSummary, StreamEvent, valid_id},
};
use serde_json::Value;
use std::io::Read;

pub trait ChatApi: Send + Sync {
    fn send(&self, chat: Option<&str>, text: &str) -> Result<Box<dyn Read + Send>, ChatFailure>;
    fn chats(&self) -> Result<Vec<ChatSummary>, ChatFailure>;
    fn history(&self, id: &str) -> Result<ChatPage, ChatFailure>;
    fn reconnect(
        &self,
        chat: &str,
        output: &str,
        after: u64,
    ) -> Result<Box<dyn Read + Send>, ChatFailure>;
    fn cancel(&self, chat: &str, output: &str) -> Result<(), ChatFailure>;
}

pub fn latest_chat(api: &impl ChatApi) -> Result<Option<String>, ChatFailure> {
    // API order is pinned-first, so select by actual update time instead.
    Ok(api
        .chats()?
        .into_iter()
        .filter(|chat| chat.app_id == "cortex")
        .max_by(|a, b| {
            let utc = |value: &str| {
                value
                    .trim_end_matches('Z')
                    .trim_end_matches("+00:00")
                    .to_owned()
            };
            utc(&a.updated_at).cmp(&utc(&b.updated_at))
        })
        .map(|chat| chat.session_id))
}

pub fn owned_history(api: &impl ChatApi, id: &str) -> Result<ChatPage, ChatFailure> {
    let page = api.history(id)?;
    if page.chat.app_id != "cortex" {
        return Err(ChatFailure::InvalidInput);
    }
    if page.outputs.iter().any(|output| output.status == "running") {
        return Err(ChatFailure::RunningOutput);
    }
    Ok(page)
}

#[derive(Default)]
pub struct ChatState {
    pub chat_id: Option<String>,
    pub output_id: Option<String>,
    pub last_event: u64,
    pub answer: String,
    pub activity: Vec<String>,
    pub stage: String,
    pub expected_ms: Option<u64>,
    pub complete: bool,
    pub total_ms: Option<u64>,
}

impl ChatState {
    pub fn apply(&mut self, event: &StreamEvent) -> Result<bool, ChatFailure> {
        if event.id.is_some_and(|id| id <= self.last_event) {
            return Ok(false);
        }
        if let Some(id) = event.id {
            self.last_event = id;
        }
        match event.name.as_str() {
            "token" => self.answer.push_str(
                event.data["text"]
                    .as_str()
                    .ok_or(ChatFailure::InvalidResponse)?,
            ),
            "stage" => {
                self.stage = event.data["stage"]
                    .as_str()
                    .ok_or(ChatFailure::InvalidResponse)?
                    .to_owned();
                if let Some(expected) = event.data.get("expected_ms") {
                    self.expected_ms = expected.as_u64();
                }
                self.push_activity(format!(
                    "{}: {}",
                    self.stage,
                    event.data["status"].as_str().unwrap_or("")
                ));
            }
            "step" => {
                let step = event.data["step"]
                    .as_str()
                    .ok_or(ChatFailure::InvalidResponse)?;
                let detail = event
                    .data
                    .get("params")
                    .filter(|v| !v.as_object().is_some_and(|o| o.is_empty()))
                    .map(Value::to_string)
                    .unwrap_or_default();
                let progress = event.data["progress"]
                    .as_array()
                    .filter(|items| items.len() == 2)
                    .and_then(|items| Some((items[0].as_u64()?, items[1].as_u64()?)))
                    .filter(|(current, total)| *total > 0 && current <= total)
                    .map(|(current, total)| format!(" {current}/{total}"))
                    .unwrap_or_default();
                self.push_activity(format!("{} ({step}){progress} {detail}", event.message));
            }
            "thinking" => self.push_activity(
                event.data["text"]
                    .as_str()
                    .ok_or(ChatFailure::InvalidResponse)?
                    .to_owned(),
            ),
            "notice" => self.push_activity(format!(
                "Notice {}",
                event.data["notice_number"]
                    .as_u64()
                    .ok_or(ChatFailure::InvalidResponse)?
            )),
            "error" => {
                return Err(ChatFailure::Rejected(
                    event.data["error_number"]
                        .as_u64()
                        .and_then(|n| u16::try_from(n).ok()),
                ));
            }
            "sources" | "memories" => self.push_activity(event.message.clone()),
            "done" => {
                let chat = event.data["session_id"]
                    .as_str()
                    .filter(|id| valid_id(id))
                    .ok_or(ChatFailure::InvalidResponse)?;
                let output = event.data["output_id"]
                    .as_str()
                    .filter(|id| valid_id(id))
                    .ok_or(ChatFailure::InvalidResponse)?;
                if self
                    .chat_id
                    .as_ref()
                    .is_some_and(|expected| !expected.eq_ignore_ascii_case(chat))
                {
                    return Err(ChatFailure::InvalidResponse);
                }
                self.chat_id = Some(chat.into());
                self.output_id = Some(output.into());
                self.total_ms = Some(
                    event.data["total_ms"]
                        .as_u64()
                        .ok_or(ChatFailure::InvalidResponse)?,
                );
                self.complete = true;
            }
            _ => {}
        }
        Ok(true)
    }

    fn push_activity(&mut self, text: String) {
        if self.activity.last() == Some(&text) {
            return;
        }
        if self.activity.len() == 256 {
            self.activity.remove(0);
        }
        self.activity.push(text);
    }
}

pub fn consume(
    mut stream: Box<dyn Read + Send>,
    mut sink: impl FnMut(StreamEvent) -> Result<(), ChatFailure>,
) -> Result<(), ChatFailure> {
    let mut decoder = SseDecoder::default();
    let mut buffer = [0; 8192];
    loop {
        let count = stream
            .read(&mut buffer)
            .map_err(|_| ChatFailure::Connection)?;
        if count == 0 {
            return Err(ChatFailure::IncompleteStream);
        }
        for event in decoder.feed(&buffer[..count])? {
            let terminal = matches!(event.name.as_str(), "done" | "error");
            sink(event)?;
            if terminal {
                return Ok(());
            }
        }
    }
}
