use serde::Deserialize;
use serde_json::Value;

use super::chat::{ChatFailure, valid_id};

pub const MAX_TOOL_RESULT_BYTES: usize = 32 * 1024;

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
    pub fn from_event(data: &Value) -> Result<Self, ChatFailure> {
        let object = data.as_object().ok_or(ChatFailure::InvalidResponse)?;
        if object.len() != 3 {
            return Err(ChatFailure::InvalidResponse);
        }
        let call_id = data["call_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or(ChatFailure::InvalidResponse)?
            .to_owned();
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
                    || args
                        .end_line
                        .is_some_and(|end| end < first || end.saturating_sub(first) > 500)
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
