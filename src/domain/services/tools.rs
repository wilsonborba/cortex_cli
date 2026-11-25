use std::{
    collections::VecDeque,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
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
    ActionCancelled,
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidWorkspace => "The current workspace is unavailable.",
            Self::InvalidPath => "The requested path is outside the current workspace.",
            Self::ReadFailed => "The requested workspace content could not be read.",
            Self::ResultTooLarge => "The tool result exceeded the supported size.",
            Self::ActionCancelled => "The approved action was cancelled before completion.",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionPreimage {
    File { path: PathBuf, digest: String },
    Command { cwd: PathBuf },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceActionArgs {
    path: String,
    find: String,
    replace: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandActionArgs {
    cwd: String,
    argv: Vec<String>,
}

fn digest_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn prepare_action(request: &ToolRequest) -> Result<ActionPreimage, ToolError> {
    let root = std::env::current_dir()
        .and_then(|path| path.canonicalize())
        .map_err(|_| ToolError::InvalidWorkspace)?;
    prepare_action_at(request, &root)
}

fn prepare_action_at(request: &ToolRequest, root: &Path) -> Result<ActionPreimage, ToolError> {
    match request.name {
        ToolName::ReplaceFileContent => {
            let args: ReplaceActionArgs = serde_json::from_value(request.arguments.clone())
                .map_err(|_| ToolError::ReadFailed)?;
            let path = resolve(root, &args.path)?;
            if fs::metadata(&path)
                .map_err(|_| ToolError::ReadFailed)?
                .len()
                > 1024 * 1024
            {
                return Err(ToolError::ResultTooLarge);
            }
            let content = fs::read_to_string(&path).map_err(|_| ToolError::ReadFailed)?;
            if args.find.is_empty() || content.matches(&args.find).count() != 1 {
                return Err(ToolError::ReadFailed);
            }
            Ok(ActionPreimage::File {
                path,
                digest: digest_bytes(content.as_bytes()),
            })
        }
        ToolName::RunCommand => {
            let args: CommandActionArgs = serde_json::from_value(request.arguments.clone())
                .map_err(|_| ToolError::ReadFailed)?;
            if args.argv.is_empty() || args.argv[0].trim().is_empty() {
                return Err(ToolError::ReadFailed);
            }
            let cwd = resolve(root, &args.cwd)?;
            if !fs::metadata(&cwd)
                .map_err(|_| ToolError::ReadFailed)?
                .is_dir()
            {
                return Err(ToolError::InvalidPath);
            }
            Ok(ActionPreimage::Command { cwd })
        }
        _ => Err(ToolError::ReadFailed),
    }
}

pub fn execute_approved_action(
    request: &ToolRequest,
    preimage: &ActionPreimage,
    cancelled: &AtomicBool,
) -> Result<String, ToolError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(ToolError::ActionCancelled);
    }
    let root = std::env::current_dir()
        .and_then(|path| path.canonicalize())
        .map_err(|_| ToolError::InvalidWorkspace)?;
    execute_approved_action_at(request, preimage, cancelled, &root)
}

fn execute_approved_action_at(
    request: &ToolRequest,
    preimage: &ActionPreimage,
    cancelled: &AtomicBool,
    root: &Path,
) -> Result<String, ToolError> {
    match (request.name, preimage) {
        (ToolName::ReplaceFileContent, ActionPreimage::File { path, digest }) => {
            let args: ReplaceActionArgs = serde_json::from_value(request.arguments.clone())
                .map_err(|_| ToolError::ReadFailed)?;
            if resolve(root, &args.path)? != *path {
                return Err(ToolError::InvalidPath);
            }
            let content = fs::read_to_string(path).map_err(|_| ToolError::ReadFailed)?;
            if digest_bytes(content.as_bytes()) != *digest
                || content.matches(&args.find).count() != 1
            {
                return Err(ToolError::ReadFailed);
            }
            let changed = content.replacen(&args.find, &args.replace, 1);
            if changed.len() > 1024 * 1024 {
                return Err(ToolError::ResultTooLarge);
            }
            if cancelled.load(Ordering::Acquire) || resolve(root, &args.path)? != *path {
                return Err(ToolError::ActionCancelled);
            }
            let current = fs::read_to_string(path).map_err(|_| ToolError::ReadFailed)?;
            if digest_bytes(current.as_bytes()) != *digest {
                return Err(ToolError::ReadFailed);
            }
            fs::write(path, changed).map_err(|_| ToolError::ReadFailed)?;
            Ok("File change applied.".into())
        }
        (ToolName::RunCommand, ActionPreimage::Command { cwd }) => {
            let args: CommandActionArgs = serde_json::from_value(request.arguments.clone())
                .map_err(|_| ToolError::ReadFailed)?;
            let current_cwd = resolve(root, &args.cwd)?;
            if current_cwd != *cwd || !current_cwd.is_dir() {
                return Err(ToolError::InvalidPath);
            }
            if cancelled.load(Ordering::Acquire) {
                return Err(ToolError::ActionCancelled);
            }
            run_command(&args, cwd, cancelled)
        }
        _ => Err(ToolError::ReadFailed),
    }
}

fn run_command(
    args: &CommandActionArgs,
    cwd: &Path,
    cancelled: &AtomicBool,
) -> Result<String, ToolError> {
    const MAX_COMMAND_OUTPUT: usize = 16 * 1024;
    const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
    let mut child = Command::new(&args.argv[0])
        .args(&args.argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| ToolError::ReadFailed)?;
    let stdout = child.stdout.take().ok_or(ToolError::ReadFailed)?;
    let stderr = child.stderr.take().ok_or(ToolError::ReadFailed)?;
    let stdout_reader = thread::spawn(move || read_capped(stdout, MAX_COMMAND_OUTPUT));
    let stderr_reader = thread::spawn(move || read_capped(stderr, MAX_COMMAND_OUTPUT));
    let started = Instant::now();
    let status = loop {
        if cancelled.load(Ordering::Acquire) || started.elapsed() >= COMMAND_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(ToolError::ActionCancelled);
        }
        if let Some(status) = child.try_wait().map_err(|_| ToolError::ReadFailed)? {
            break status;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let (stdout, stdout_truncated) = stdout_reader.join().map_err(|_| ToolError::ReadFailed)??;
    let (stderr, stderr_truncated) = stderr_reader.join().map_err(|_| ToolError::ReadFailed)??;
    let mut output = format!(
        "exit code: {}\nstdout:\n{}\nstderr:\n{}{}",
        status.code().unwrap_or(-1),
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr),
        if stdout_truncated || stderr_truncated {
            "\noutput truncated"
        } else {
            ""
        }
    );
    if output.len() > MAX_TOOL_RESULT_BYTES {
        let mut end = MAX_TOOL_RESULT_BYTES;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
    }
    Ok(output)
}

fn read_capped(mut reader: impl Read, limit: usize) -> Result<(Vec<u8>, bool), ToolError> {
    let mut output = Vec::with_capacity(limit.min(4096));
    let mut buffer = [0u8; 4096];
    let mut truncated = false;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|_| ToolError::ReadFailed)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        let kept = count.min(remaining);
        output.extend_from_slice(&buffer[..kept]);
        truncated |= kept < count;
    }
    Ok((output, truncated))
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
    use super::{
        ActionPreimage, ToolError, execute_approved_action_at, execute_readonly_at,
        prepare_action_at,
    };
    use crate::domain::models::tools::ToolRequest;
    use serde_json::json;
    use std::{
        fs,
        os::unix::fs::symlink,
        sync::atomic::AtomicBool,
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

    #[test]
    fn approved_file_action_rechecks_the_preimage_before_writing() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("cortex-cli-action-preimage-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("note.txt");
        fs::write(&path, "before").unwrap();
        let request = ToolRequest::from_event(&json!({
            "call_id":"30000000-0000-4000-8000-000000000003",
            "name":"replace_file_content",
            "arguments":{"path":"note.txt","find":"before","replace":"after"}
        }))
        .unwrap();
        let preimage = prepare_action_at(&request, &root).unwrap();
        assert!(matches!(preimage, ActionPreimage::File { .. }));
        fs::write(&path, "changed after approval").unwrap();
        assert_eq!(
            execute_approved_action_at(&request, &preimage, &AtomicBool::new(false), &root),
            Err(ToolError::ReadFailed)
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "changed after approval");
        fs::remove_dir_all(root).unwrap();
    }
}
