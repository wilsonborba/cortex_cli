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

#[test]
fn view_file_rejects_zero_and_ranges_over_500_inclusive_lines() {
    for arguments in [
        json!({"path":"Cargo.toml","start_line":0}),
        json!({"path":"Cargo.toml","end_line":0}),
        json!({"path":"Cargo.toml","start_line":0,"end_line":0}),
        json!({"path":"Cargo.toml","start_line":1,"end_line":501}),
    ] {
        assert!(
            ToolRequest::from_event(
                &json!({"call_id":CALL,"name":"view_file","arguments":arguments})
            )
            .is_err()
        );
    }
    assert!(ToolRequest::from_event(
        &json!({"call_id":CALL,"name":"view_file","arguments":{"path":"Cargo.toml","start_line":1,"end_line":500}})
    ).is_ok());
}

#[test]
fn action_digest_matches_shared_api_golden_and_canonicalizes_nested_objects() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/output_action_digest.json")).unwrap();
    let call_id = fixture["call_id"].as_str().unwrap();
    let parse_args = |raw: &str| serde_json::from_str::<serde_json::Value>(raw).unwrap();
    let make_request = |call_id: &str, name: ToolName, arguments: serde_json::Value| ToolRequest {
        call_id: call_id.to_owned(),
        name,
        arguments,
    };
    let first = parse_args(fixture["arguments_raw"][0].as_str().unwrap());
    let expected = fixture["digest"].as_str().unwrap();

    assert_eq!(
        make_request(call_id, ToolName::RunCommand, first.clone()).action_digest(),
        expected
    );
    assert_eq!(
        make_request(
            call_id,
            ToolName::RunCommand,
            parse_args(fixture["arguments_raw"][1].as_str().unwrap()),
        )
        .action_digest(),
        expected,
        "reordered nested object keys must preserve the digest"
    );
    assert_eq!(
        make_request(
            fixture["changed_call_id"]["value"].as_str().unwrap(),
            ToolName::RunCommand,
            first.clone(),
        )
        .action_digest(),
        fixture["changed_call_id"]["digest"].as_str().unwrap()
    );
    assert_eq!(
        make_request(call_id, ToolName::ViewFile, first.clone()).action_digest(),
        fixture["changed_tool_name"]["digest"].as_str().unwrap()
    );
    assert_eq!(
        make_request(
            call_id,
            ToolName::RunCommand,
            parse_args(fixture["changed_arguments"]["raw"].as_str().unwrap()),
        )
        .action_digest(),
        fixture["changed_arguments"]["digest"].as_str().unwrap()
    );
    assert_eq!(
        make_request(
            call_id,
            ToolName::RunCommand,
            parse_args(fixture["changed_array_order"]["raw"].as_str().unwrap()),
        )
        .action_digest(),
        fixture["changed_array_order"]["digest"].as_str().unwrap(),
        "array order must remain significant"
    );
    let mixed_case = "30000000-0000-4000-A000-000000000003";
    let lowercase = mixed_case.to_ascii_lowercase();
    assert_eq!(
        make_request(mixed_case, ToolName::RunCommand, first.clone()).action_digest(),
        make_request(&lowercase, ToolName::RunCommand, first).action_digest(),
        "UUID text is normalized to lowercase canonical form"
    );

    let valid_args = parse_args(fixture["valid_command"]["arguments_raw"].as_str().unwrap());
    let valid_request = ToolRequest::from_event(&json!({
        "call_id":call_id,
        "name":"run_command",
        "arguments":valid_args
    }))
    .unwrap();
    assert_eq!(
        valid_request.action_digest(),
        fixture["valid_command"]["digest"].as_str().unwrap()
    );
    let uppercase_uuid = ToolRequest::from_event(&json!({
        "call_id":"30000000-0000-4000-A000-000000000003",
        "name":"run_command",
        "arguments":parse_args(fixture["valid_command"]["arguments_raw"].as_str().unwrap())
    }))
    .unwrap();
    assert_eq!(
        uppercase_uuid.call_id,
        "30000000-0000-4000-a000-000000000003"
    );
}
