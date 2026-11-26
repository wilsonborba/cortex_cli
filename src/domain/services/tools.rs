use std::{
    collections::VecDeque,
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
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
    ActionTimedOut,
    ActionOutcomeUnknown,
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidWorkspace => "The current workspace is unavailable.",
            Self::InvalidPath => "The requested path is outside the current workspace.",
            Self::ReadFailed => "The requested workspace content could not be read.",
            Self::ResultTooLarge => "The tool result exceeded the supported size.",
            Self::ActionCancelled => "The approved action was cancelled before completion.",
            Self::ActionTimedOut => "The command was stopped at its time limit.",
            Self::ActionOutcomeUnknown => {
                "The approved action may have a partial effect; it will not run again automatically."
            }
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
            let mut file = fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)
                .map_err(|_| ToolError::ReadFailed)?;
            file.write_all(changed.as_bytes())
                .map_err(|_| ToolError::ActionOutcomeUnknown)?;
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
    let mut command = Command::new(&args.argv[0]);
    command
        .args(&args.argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|_| ToolError::ReadFailed)?;
    let mut stdout = child.stdout.take().ok_or(ToolError::ReadFailed)?;
    let mut stderr = child.stderr.take().ok_or(ToolError::ReadFailed)?;
    #[cfg(unix)]
    if set_nonblocking(&stdout).is_err() || set_nonblocking(&stderr).is_err() {
        terminate_process_tree(&mut child);
        return Err(ToolError::ActionOutcomeUnknown);
    }
    let mut stdout_bytes = Vec::with_capacity(MAX_COMMAND_OUTPUT);
    let mut stderr_bytes = Vec::with_capacity(MAX_COMMAND_OUTPUT);
    let mut stdout_truncated = false;
    let mut stderr_truncated = false;
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let started = Instant::now();
    let mut exited_at = None;
    let status = loop {
        if cancelled.load(Ordering::Acquire) {
            terminate_process_tree(&mut child);
            return Err(ToolError::ActionCancelled);
        }
        if started.elapsed() >= COMMAND_TIMEOUT {
            terminate_process_tree(&mut child);
            return Err(ToolError::ActionTimedOut);
        }
        if read_capped(
            &mut stdout,
            &mut stdout_bytes,
            &mut stdout_truncated,
            MAX_COMMAND_OUTPUT,
            &mut stdout_eof,
        )
        .is_err()
            || read_capped(
                &mut stderr,
                &mut stderr_bytes,
                &mut stderr_truncated,
                MAX_COMMAND_OUTPUT,
                &mut stderr_eof,
            )
            .is_err()
        {
            terminate_process_tree(&mut child);
            return Err(ToolError::ActionOutcomeUnknown);
        }
        if exited_at.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // A command may leave descendants holding our pipes. They
                    // belong to its process group and must not outlive the action.
                    terminate_process_group(child.id());
                    exited_at = Some((Instant::now(), status));
                }
                Ok(None) => {}
                Err(_) => {
                    terminate_process_tree(&mut child);
                    return Err(ToolError::ActionOutcomeUnknown);
                }
            }
        }
        if let Some((exit_time, status)) = exited_at
            && ((stdout_eof && stderr_eof) || exit_time.elapsed() >= Duration::from_millis(250))
        {
            break status;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let mut output = format!(
        "exit code: {}\nstdout:\n{}\nstderr:\n{}{}",
        status.code().unwrap_or(-1),
        String::from_utf8_lossy(&stdout_bytes),
        String::from_utf8_lossy(&stderr_bytes),
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

fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    terminate_process_group(child.id());
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(unix)]
fn terminate_process_group(process_id: u32) {
    use nix::{
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };
    let group = Pid::from_raw(process_id as i32);
    if killpg(group, None).is_err() {
        return;
    }
    let _ = killpg(group, Signal::SIGTERM);
    let deadline = Instant::now() + Duration::from_millis(150);
    while Instant::now() < deadline && killpg(group, None).is_ok() {
        thread::sleep(Duration::from_millis(10));
    }
    let _ = killpg(group, Signal::SIGKILL);
}

#[cfg(not(unix))]
fn terminate_process_group(_process_id: u32) {}

fn read_capped(
    reader: &mut impl Read,
    output: &mut Vec<u8>,
    truncated: &mut bool,
    limit: usize,
    eof: &mut bool,
) -> io::Result<()> {
    let mut buffer = [0u8; 4096];
    let mut drained = 0usize;
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if count == 0 {
            *eof = true;
            return Ok(());
        }
        drained += count;
        let remaining = limit.saturating_sub(output.len());
        let kept = count.min(remaining);
        output.extend_from_slice(&buffer[..kept]);
        *truncated |= kept < count;
        if drained >= 64 * 1024 {
            return Ok(());
        }
    }
}

#[cfg(unix)]
fn set_nonblocking(pipe: &impl std::os::fd::AsRawFd) -> nix::Result<()> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    let fd = pipe.as_raw_fd();
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map(|_| ())
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
        path::PathBuf,
        process::Command,
        sync::atomic::AtomicBool,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn partial_write_fixture_child() {
        let Ok(path) = std::env::var("CORTEX_CLI_PARTIAL_WRITE_PATH") else {
            return;
        };
        let path = PathBuf::from(path);
        fs::write(&path, "before").unwrap();
        let request = ToolRequest::from_event(&json!({
            "call_id":"30000000-0000-4000-8000-000000000003",
            "name":"replace_file_content",
            "arguments":{"path":"partial.txt","find":"before","replace":"x".repeat(4096)}
        }))
        .unwrap();
        let root = path.parent().unwrap().canonicalize().unwrap();
        let preimage = prepare_action_at(&request, &root).unwrap();
        assert_eq!(
            execute_approved_action_at(&request, &preimage, &AtomicBool::new(false), &root),
            Err(ToolError::ActionOutcomeUnknown)
        );
        let written = fs::metadata(&path).unwrap().len();
        assert!(
            written > 0 && written < 4096,
            "expected a partial write, got {written} bytes"
        );
    }

    #[test]
    fn partial_file_write_is_not_reported_as_zero_effect() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = std::env::temp_dir().join(format!("cortex-cli-partial-write-{nonce}"));
        fs::create_dir_all(&temp).unwrap();
        let path = temp.join("partial.txt");
        let test_binary = std::env::current_exe().unwrap();
        let output = Command::new("sh")
            .args(["-c", "ulimit -f 1; trap '' XFSZ; exec \"$@\"", "sh"])
            .arg(test_binary)
            .args([
                "--exact",
                "domain::services::tools::tests::partial_write_fixture_child",
                "--nocapture",
            ])
            .env("CORTEX_CLI_PARTIAL_WRITE_PATH", &path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "limited child failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let written = fs::metadata(&path).unwrap().len();
        assert!(
            written > 0 && written < 4096,
            "expected a partial write, got {written} bytes"
        );
        fs::remove_dir_all(temp).unwrap();
    }

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

    #[test]
    fn cancelled_command_stops_its_process_group_without_affecting_unrelated_processes() {
        use std::{
            process::Command,
            sync::{Arc, atomic::AtomicBool},
            thread,
            time::{Duration, Instant},
        };
        let root = std::env::current_dir().unwrap().canonicalize().unwrap();
        let request = ToolRequest::from_event(&serde_json::json!({
            "call_id":"30000000-0000-4000-8000-000000000003",
            "name":"run_command",
            "arguments":{"cwd":".","argv":["sh","-c","sleep 30 & wait"]}
        }))
        .unwrap();
        let preimage = prepare_action_at(&request, &root).unwrap();
        let mut unrelated = Command::new("sleep").arg("2").spawn().unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = cancellation.clone();
        let action = std::thread::spawn(move || {
            execute_approved_action_at(&request, &preimage, &worker_cancellation, &root)
        });
        thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        cancellation.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(action.join().unwrap(), Err(ToolError::ActionCancelled));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "unrelated child was terminated"
        );
        let _ = unrelated.kill();
        let _ = unrelated.wait();
    }
}
