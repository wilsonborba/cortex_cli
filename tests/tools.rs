use cortex_cli::domain::{
    models::{
        chat::ChatFailure,
        tools::{ToolName, ToolRequest},
    },
    services::tools::{ToolError, execute_readonly},
};
use serde_json::json;

const CALL: &str = "30000000-0000-4000-8000-000000000003";

fn request(name: &str, arguments: serde_json::Value) -> ToolRequest {
    ToolRequest::from_event(&json!({"call_id":CALL,"name":name,"arguments":arguments})).unwrap()
}

#[test]
fn view_file_reads_only_a_bounded_range_from_the_current_workspace() {
    let tool = request(
        "view_file",
        json!({"path":"Cargo.toml","start_line":1,"end_line":2}),
    );
    let result = execute_readonly(&tool).unwrap().unwrap();
    assert!(result.starts_with("1: [package]"));
    assert!(result.lines().count() <= 2);

    let outside = request(
        "view_file",
        json!({"path":"../outside","start_line":1,"end_line":2}),
    );
    assert_eq!(execute_readonly(&outside), Err(ToolError::InvalidPath));
}

#[test]
fn grep_is_literal_bounded_and_does_not_cross_workspace_paths() {
    let tool = request(
        "grep_search",
        json!({"query":"name =","paths":["Cargo.toml"],"max_results":1}),
    );
    let result = execute_readonly(&tool).unwrap().unwrap();
    assert!(result.contains("Cargo.toml:2:"));
    assert_eq!(result.lines().count(), 1);

    let outside = request(
        "grep_search",
        json!({"query":"secret","paths":["../outside"]}),
    );
    assert_eq!(execute_readonly(&outside), Err(ToolError::InvalidPath));
}

#[test]
fn write_command_and_subagent_requests_have_no_unapproved_effect() {
    for (name, args) in [
        (
            "replace_file_content",
            json!({"path":"Cargo.toml","find":"old","replace":"new"}),
        ),
        (
            "run_command",
            json!({"cwd":".","argv":["touch","unexpected"]}),
        ),
        (
            "invoke_subagent",
            json!({"role":"reviewer","task":"Inspect the change"}),
        ),
    ] {
        assert_eq!(execute_readonly(&request(name, args)), Ok(None));
    }
}

#[test]
fn event_parser_accepts_only_the_five_mapped_tool_names() {
    let call = request("run_command", json!({"cwd":".","argv":["echo","hi"]}));
    assert_eq!(call.name, ToolName::RunCommand);
    assert!(matches!(
        ToolRequest::from_event(&json!({"call_id":CALL,"name":"mcp_call","arguments":{}})),
        Err(ChatFailure::InvalidResponse)
    ));
}
