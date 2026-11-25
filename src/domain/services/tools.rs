use std::{
    collections::VecDeque,
    fs,
    path::{Component, Path, PathBuf},
};

use crate::domain::models::tools::{
    MAX_TOOL_RESULT_BYTES, MAX_VIEW_FILE_LINES, ToolName, ToolRequest,
};
use serde::Deserialize;

const MAX_GREP_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_GREP_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_GREP_VISITED_PATHS: usize = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolError {
    InvalidWorkspace,
    InvalidPath,
    ReadFailed,
    ResultTooLarge,
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidWorkspace => "The current workspace is unavailable.",
            Self::InvalidPath => "The requested path is outside the current workspace.",
            Self::ReadFailed => "The requested workspace content could not be read.",
            Self::ResultTooLarge => "The tool result exceeded the supported size.",
        })
    }
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

pub fn execute_readonly(request: &ToolRequest) -> Result<Option<String>, ToolError> {
    let root = std::env::current_dir()
        .and_then(|path| path.canonicalize())
        .map_err(|_| ToolError::InvalidWorkspace)?;
    execute_readonly_at(request, &root)
}

fn execute_readonly_at(request: &ToolRequest, root: &Path) -> Result<Option<String>, ToolError> {
    let result = match request.name {
        ToolName::ViewFile => {
            let args: ViewFileArgs = serde_json::from_value(request.arguments.clone())
                .map_err(|_| ToolError::ReadFailed)?;
            view_file(root, &args)?
        }
        ToolName::GrepSearch => {
            let args: GrepArgs = serde_json::from_value(request.arguments.clone())
                .map_err(|_| ToolError::ReadFailed)?;
            grep(root, &args)?
        }
        ToolName::InvokeSubagent | ToolName::ReplaceFileContent | ToolName::RunCommand => {
            return Ok(None);
        }
    };
    if result.len() > MAX_TOOL_RESULT_BYTES {
        return Err(ToolError::ResultTooLarge);
    }
    Ok(Some(result))
}

fn resolve(root: &Path, path: &str) -> Result<PathBuf, ToolError> {
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(ToolError::InvalidPath);
    }
    let path = root
        .join(relative)
        .canonicalize()
        .map_err(|_| ToolError::ReadFailed)?;
    if !path.starts_with(root) {
        return Err(ToolError::InvalidPath);
    }
    Ok(path)
}

fn view_file(root: &Path, args: &ViewFileArgs) -> Result<String, ToolError> {
    let start_line = args.start_line.unwrap_or(1);
    if start_line == 0 || args.end_line == Some(0) {
        return Err(ToolError::ReadFailed);
    }
    let end_line = args
        .end_line
        .unwrap_or_else(|| start_line.saturating_add(MAX_VIEW_FILE_LINES - 1));
    if end_line < start_line || end_line.saturating_sub(start_line) >= MAX_VIEW_FILE_LINES {
        return Err(ToolError::ReadFailed);
    }
    let path = resolve(root, &args.path)?;
    if fs::metadata(&path)
        .map_err(|_| ToolError::ReadFailed)?
        .len()
        > 1024 * 1024
    {
        return Err(ToolError::ResultTooLarge);
    }
    let content = fs::read_to_string(path).map_err(|_| ToolError::ReadFailed)?;
    let start = start_line as usize;
    let take = end_line
        .saturating_sub(start_line)
        .saturating_add(1)
        .min(MAX_VIEW_FILE_LINES) as usize;
    let mut output = String::new();
    for (index, line) in content.lines().enumerate().skip(start - 1).take(take) {
        output.push_str(&format!("{}: {}\n", index + 1, line));
        if output.len() > MAX_TOOL_RESULT_BYTES {
            return Err(ToolError::ResultTooLarge);
        }
    }
    Ok(if output.is_empty() {
        "No lines in the requested range.".into()
    } else {
        output
    })
}

fn grep(root: &Path, args: &GrepArgs) -> Result<String, ToolError> {
    let max = args.max_results.unwrap_or(50).min(100) as usize;
    let mut pending = VecDeque::new();
    for path in &args.paths {
        pending.push_back(resolve(root, path)?);
    }
    let mut output = String::new();
    let mut visited = 0usize;
    let mut scanned_bytes = 0u64;
    let mut result_count = 0usize;
    'search: while let Some(path) = pending.pop_back() {
        visited += 1;
        if visited > MAX_GREP_VISITED_PATHS || output.len() >= MAX_TOOL_RESULT_BYTES {
            break;
        }
        let path = path.canonicalize().map_err(|_| ToolError::ReadFailed)?;
        if !path.starts_with(root) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|_| ToolError::ReadFailed)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path).map_err(|_| ToolError::ReadFailed)? {
                let entry = entry.map_err(|_| ToolError::ReadFailed)?;
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                if entry
                    .file_type()
                    .map_err(|_| ToolError::ReadFailed)?
                    .is_symlink()
                {
                    continue;
                }
                if pending.len() + visited < MAX_GREP_VISITED_PATHS {
                    pending.push_back(entry.path());
                }
            }
            continue;
        }
        if metadata.len() > MAX_GREP_FILE_BYTES {
            continue;
        }
        scanned_bytes = scanned_bytes.saturating_add(metadata.len());
        if scanned_bytes > MAX_GREP_SCAN_BYTES {
            break;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for (line_number, line) in content.lines().enumerate() {
            if line.contains(&args.query) {
                let shown = path
                    .strip_prefix(root)
                    .map_err(|_| ToolError::InvalidPath)?;
                output.push_str(&format!(
                    "{}:{}: {}\n",
                    shown.display(),
                    line_number + 1,
                    line
                ));
                result_count += 1;
                if result_count >= max {
                    break 'search;
                }
                if output.len() > MAX_TOOL_RESULT_BYTES {
                    return Err(ToolError::ResultTooLarge);
                }
            }
        }
    }
    if output.len() > MAX_TOOL_RESULT_BYTES {
        return Err(ToolError::ResultTooLarge);
    }
    Ok(if output.is_empty() {
        "No matches found.".into()
    } else {
        output
    })
}

#[cfg(test)]
mod tests {
    use super::{ToolError, execute_readonly_at};
    use crate::domain::models::tools::ToolRequest;
    use serde_json::json;
    use std::{
        fs,
        os::unix::fs::symlink,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn symlinks_resolving_outside_the_workspace_are_rejected() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = std::env::temp_dir().join(format!("cortex-cli-tool-symlink-{nonce}"));
        let root = temp.join("workspace");
        let outside = temp.join("outside.txt");
        fs::create_dir_all(&root).unwrap();
        fs::write(&outside, "private fixture").unwrap();
        symlink(&outside, root.join("escape.txt")).unwrap();
        let call_id = "30000000-0000-4000-8000-000000000003";

        for arguments in [
            json!({"path":"escape.txt"}),
            json!({"query":"private","paths":["escape.txt"]}),
        ] {
            let name = if arguments.get("path").is_some() {
                "view_file"
            } else {
                "grep_search"
            };
            let request = ToolRequest::from_event(
                &json!({"call_id":call_id,"name":name,"arguments":arguments}),
            )
            .unwrap();
            assert_eq!(
                execute_readonly_at(&request, &root),
                Err(ToolError::InvalidPath)
            );
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn grep_result_limit_applies_across_files_in_a_directory() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("cortex-cli-grep-bound-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.join(name), "needle\n").unwrap();
        }
        for paths in [
            serde_json::json!(["a.txt", "b.txt", "c.txt"]),
            serde_json::json!(["."]),
        ] {
            let request = ToolRequest::from_event(&serde_json::json!({
                "call_id":"30000000-0000-4000-8000-000000000003",
                "name":"grep_search",
                "arguments":{"query":"needle","paths":paths,"max_results":1}
            }))
            .unwrap();

            let result = execute_readonly_at(&request, &root).unwrap().unwrap();
            assert_eq!(result.lines().count(), 1);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
