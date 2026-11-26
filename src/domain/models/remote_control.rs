//! Data returned by the accepted Cortex remote session routes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEventKind {
    Prompt,
    Progress,
    AnswerPart,
    ApprovalRequest,
    ApprovalAnswer,
    Artifact,
    TurnEnd,
    Cancel,
    CommandExpired,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RemoteEvent {
    pub id: u64,
    pub kind: RemoteEventKind,
    pub payload: Value,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RemoteSession {
    pub session_id: String,
    pub device_name: String,
    pub platform: String,
    pub last_seen_ms: u64,
    pub online: bool,
}

impl RemoteEvent {
    pub fn prompt(&self) -> Result<(&str, Vec<String>), RemoteEventError> {
        if self.kind != RemoteEventKind::Prompt {
            return Err(RemoteEventError);
        }
        let text = self.payload["prompt"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .ok_or(RemoteEventError)?;
        let file_ids = self.payload["file_ids"]
            .as_array()
            .ok_or(RemoteEventError)?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .filter(|id| !id.trim().is_empty())
                    .map(str::to_owned)
                    .ok_or(RemoteEventError)
            })
            .collect::<Result<_, _>>()?;
        Ok((text, file_ids))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteEventError;

impl std::fmt::Display for RemoteEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cortex returned an invalid remote event")
    }
}

impl std::error::Error for RemoteEventError {}

#[cfg(test)]
mod tests {
    use super::{RemoteEvent, RemoteEventKind};
    use serde_json::json;

    #[test]
    fn remote_prompt_requires_text_and_file_reference_list() {
        let valid = RemoteEvent {
            id: 7,
            kind: RemoteEventKind::Prompt,
            payload: json!({"prompt":"Explain this", "file_ids":["fsm-file-1"]}),
            created_at_ms: 123,
        };
        assert_eq!(
            valid.prompt().unwrap(),
            ("Explain this", vec!["fsm-file-1".to_owned()])
        );

        for payload in [
            json!({"prompt":" " ,"file_ids":[]}),
            json!({"prompt":"go"}),
            json!({"prompt":"go", "file_ids":[1]}),
        ] {
            let event = RemoteEvent {
                payload,
                ..valid.clone()
            };
            assert!(event.prompt().is_err());
        }
    }
}
