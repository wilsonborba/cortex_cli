//! Cortex-native chat data. Conversation text has no Debug formatter.

use serde::Deserialize;
use serde_json::Value;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatFailure {
    Connection,
    Unauthorized,
    InvalidResponse,
    IncompleteStream,
    Rejected(Option<u16>),
    InvalidInput,
    RunningOutput,
}

impl fmt::Display for ChatFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connection => f.write_str("Cannot contact Cortex. Check your connection."),
            Self::Unauthorized => {
                f.write_str("Asodya refused this request. Sign in or check your account access.")
            }
            Self::InvalidResponse => f.write_str("Cortex returned an invalid response."),
            Self::IncompleteStream => {
                f.write_str("The connection ended before the output completed.")
            }
            Self::Rejected(Some(number)) => {
                write!(f, "Cortex could not complete the request (error {number}).")
            }
            Self::Rejected(None) => f.write_str("Cortex could not complete the request."),
            Self::InvalidInput => f.write_str("Enter a prompt or a valid conversation identifier."),
            Self::RunningOutput => f.write_str(
                "This conversation has a running output. Resume without a new prompt, or wait for it to finish.",
            ),
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct ChatSummary {
    pub session_id: String,
    pub app_id: String,
    pub title: String,
    pub updated_at: String,
}

#[derive(Deserialize)]
pub struct ChatInput {
    pub input_id: String,
    pub text: String,
}

#[derive(Clone, Deserialize)]
pub struct ChatOutput {
    pub output_id: String,
    pub input_id: String,
    pub status: String,
    pub text: String,
}

#[derive(Deserialize)]
pub struct ChatPage {
    pub chat: ChatSummary,
    pub inputs: Vec<ChatInput>,
    pub outputs: Vec<ChatOutput>,
}

pub struct StreamEvent {
    pub id: Option<u64>,
    pub name: String,
    pub message: String,
    pub data: Value,
}

impl StreamEvent {
    pub fn from_payload(
        id: Option<u64>,
        name: String,
        payload: &[u8],
    ) -> Result<Self, ChatFailure> {
        let value: Value =
            serde_json::from_slice(payload).map_err(|_| ChatFailure::InvalidResponse)?;
        let object = value.as_object().ok_or(ChatFailure::InvalidResponse)?;
        if object.len() != 2 || !value["data"].is_object() {
            return Err(ChatFailure::InvalidResponse);
        }
        let message = value["message"]
            .as_str()
            .ok_or(ChatFailure::InvalidResponse)?
            .to_owned();
        Ok(Self {
            id,
            name,
            message,
            data: value["data"].clone(),
        })
    }
}

pub fn valid_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}
