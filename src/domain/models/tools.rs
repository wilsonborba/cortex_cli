use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::chat::{ChatFailure, valid_id};

pub const MAX_TOOL_RESULT_BYTES: usize = 32 * 1024;
pub const MAX_VIEW_FILE_LINES: u32 = 500;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    InvokeSubagent,
    ViewFile,
    GrepSearch,
    ReplaceFileContent,
    RunCommand,
}

impl ToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvokeSubagent => "invoke_subagent",
            Self::ViewFile => "view_file",
            Self::GrepSearch => "grep_search",
            Self::ReplaceFileContent => "replace_file_content",
            Self::RunCommand => "run_command",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolRequest {
    pub call_id: String,
    pub name: ToolName,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: ToolName,
    pub output: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutputActionReceipt {
    pub action_id: String,
    pub call_id: String,
    pub name: String,
    pub action_kind: String,
    pub action_digest: String,
    pub display: String,
    pub expires_at: String,
    pub status: String,
}

impl OutputActionReceipt {
    pub fn matches(&self, request: &ToolRequest) -> bool {
        let expected_kind = match request.name {
            ToolName::ReplaceFileContent => "file_change",
            ToolName::RunCommand => "command",
            _ => return false,
        };
        valid_id(&self.action_id)
            && self.call_id == request.call_id
            && self.name == request.name.as_str()
            && self.action_kind == expected_kind
            && self.action_digest == request.action_digest()
            && self.display.len() <= 64_000
            && !self.expires_at.is_empty()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentArgs {
    role: String,
    task: String,
    context: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewFileArgs {
    path: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    query: String,
    paths: Vec<String>,
    max_results: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceArgs {
    path: String,
    find: String,
    replace: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    cwd: String,
    argv: Vec<String>,
}

fn bounded(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.contains('\0')
}

impl ToolRequest {
    pub fn action_digest(&self) -> String {
        // Match Cortex API exactly: lowercase hyphenated UUID text, NUL,
        // snake_case tool name, NUL, recursively key-sorted JSON. Arrays keep
        // their source order.
        let call_id = self.call_id.to_ascii_lowercase();
        let mut canonical_arguments = self.arguments.clone();
        canonical_arguments.sort_all_objects();
        let mut hasher = Sha256::new();
        hasher.update(call_id.as_bytes());
        hasher.update([0]);
        hasher.update(self.name.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(canonical_arguments.to_string().as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn from_event(data: &Value) -> Result<Self, ChatFailure> {
        let object = data.as_object().ok_or(ChatFailure::InvalidResponse)?;
        if object.len() != 3 {
            return Err(ChatFailure::InvalidResponse);
        }
        let call_id = data["call_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or(ChatFailure::InvalidResponse)?
            .to_ascii_lowercase();
        let raw_name = data["name"].as_str().ok_or(ChatFailure::InvalidResponse)?;
        let arguments = data["arguments"].clone();
        if arguments.to_string().len() > 16 * 1024 {
            return Err(ChatFailure::InvalidResponse);
        }
        let name = match raw_name {
            "invoke_subagent" => {
                let args: SubagentArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| ChatFailure::InvalidResponse)?;
                if !bounded(&args.role, 80)
                    || !bounded(&args.task, 8 * 1024)
                    || args.context.as_ref().is_some_and(|v| v.len() > 8 * 1024)
                {
                    return Err(ChatFailure::InvalidResponse);
                }
                ToolName::InvokeSubagent
            }
            "view_file" => {
                let args: ViewFileArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| ChatFailure::InvalidResponse)?;
                let first = args.start_line.unwrap_or(1);
                if !bounded(&args.path, 1024)
                    || args.start_line == Some(0)
                    || args.end_line == Some(0)
                    || args.end_line.is_some_and(|end| {
                        end < first || end.saturating_sub(first) >= MAX_VIEW_FILE_LINES
                    })
                {
                    return Err(ChatFailure::InvalidResponse);
                }
                ToolName::ViewFile
            }
            "grep_search" => {
                let args: GrepArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| ChatFailure::InvalidResponse)?;
                if !bounded(&args.query, 4096)
                    || args.paths.is_empty()
                    || args.paths.len() > 32
                    || args.paths.iter().any(|path| !bounded(path, 1024))
                    || args.max_results.is_some_and(|n| n == 0 || n > 100)
                {
                    return Err(ChatFailure::InvalidResponse);
                }
                ToolName::GrepSearch
            }
            "replace_file_content" => {
                let args: ReplaceArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| ChatFailure::InvalidResponse)?;
                if !bounded(&args.path, 1024)
                    || args.find.is_empty()
                    || args.find.len() > 16 * 1024
                    || args.replace.len() > 16 * 1024
                {
                    return Err(ChatFailure::InvalidResponse);
                }
                ToolName::ReplaceFileContent
            }
            "run_command" => {
                let args: CommandArgs = serde_json::from_value(arguments.clone())
                    .map_err(|_| ChatFailure::InvalidResponse)?;
                if !bounded(&args.cwd, 1024)
                    || args.argv.is_empty()
                    || args.argv[0].trim().is_empty()
                    || args.argv.len() > 64
                    || args
                        .argv
                        .iter()
                        .any(|arg| arg.len() > 4096 || arg.contains('\0'))
                {
                    return Err(ChatFailure::InvalidResponse);
                }
                ToolName::RunCommand
            }
            _ => return Err(ChatFailure::InvalidResponse),
        };
        Ok(Self {
            call_id,
            name,
            arguments,
        })
    }
}
