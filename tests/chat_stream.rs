use cortex_cli::{
    dal::remote::sse::SseDecoder,
    domain::{
        models::chat::{ChatFailure, StreamEvent},
        services::chat::{ChatState, consume},
    },
};
use serde_json::json;
use std::io::Cursor;

const CHAT: &str = "10000000-0000-4000-8000-000000000001";
const OUTPUT: &str = "20000000-0000-4000-8000-000000000002";

fn frame(id: u64, event: &str, data: serde_json::Value) -> String {
    format!(
        "id: {id}\r\nevent: {event}\r\ndata: {}\r\n\r\n",
        json!({"message":"fixture","data":data})
    )
}

#[test]
fn utf8_and_frames_survive_every_network_boundary() {
    let source = frame(1, "token", json!({"text":"สวัสดี\nPortuguês"}));
    for boundary in 1..source.len() {
        let mut parser = SseDecoder::default();
        let mut events = parser.feed(&source.as_bytes()[..boundary]).unwrap();
        events.extend(parser.feed(&source.as_bytes()[boundary..]).unwrap());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["text"], "สวัสดี\nPortuguês");
        assert_eq!(events[0].id, Some(1));
    }
}

#[test]
fn comments_multiline_data_and_multiple_frames_follow_sse() {
    let source=": keepalive\n\nevent: token\nid: 2\ndata: {\"message\":\"fixture\",\ndata: \"data\":{\"text\":\"hello\"}}\n\n".to_owned()+&frame(3,"notice",json!({"notice_number":1201}));
    let events = SseDecoder::default().feed(source.as_bytes()).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].data["text"], "hello");
    assert_eq!(events[1].id, Some(3));
}

#[test]
fn malformed_and_oversized_payloads_are_rejected() {
    for source in [
        "event: token\ndata: {not-json}\n\n".to_owned(),
        "event: token\nid: invalid\ndata: {}\n\n".to_owned(),
        "event: token\ndata: {\"text\":\"legacy top-level\"}\n\n".to_owned(),
        "data: ".to_owned() + &"x".repeat(256 * 1024),
    ] {
        assert!(matches!(
            SseDecoder::default().feed(source.as_bytes()),
            Err(ChatFailure::InvalidResponse)
        ));
    }
}

#[test]
fn actual_tokens_and_actual_estimates_drive_state() {
    let mut state = ChatState::default();
    let event = |id, name: &str, data: serde_json::Value| {
        StreamEvent::from_payload(
            Some(id),
            name.into(),
            json!({"message":"fixture","data":data})
                .to_string()
                .as_bytes(),
        )
        .unwrap()
    };
    state
        .apply(&event(
            1,
            "stage",
            json!({"stage":"accept","status":"started","expected_ms":null}),
        ))
        .unwrap();
    assert_eq!(state.expected_ms, None);
    let token = event(2, "token", json!({"text":"real answer"}));
    state.apply(&token).unwrap();
    assert!(!state.apply(&token).unwrap());
    assert_eq!(state.answer, "real answer");
    state
        .apply(&event(
            3,
            "stage",
            json!({"stage":"answer","status":"started","expected_ms":4200}),
        ))
        .unwrap();
    assert_eq!(state.expected_ms, Some(4200));
    state
        .apply(&event(
            4,
            "done",
            json!({"session_id":CHAT,"output_id":OUTPUT,"total_ms":4321}),
        ))
        .unwrap();
    assert!(state.complete);
    assert_eq!(state.chat_id.as_deref(), Some(CHAT));
    assert_eq!(state.output_id.as_deref(), Some(OUTPUT));
    assert_eq!(state.total_ms, Some(4321));
}

#[test]
fn typed_tool_call_is_validated_and_bound_to_its_server_id() {
    let mut state = ChatState::default();
    let request = StreamEvent::from_payload(
        Some(1),
        "tool_call".into(),
        json!({
            "message":"Tool requested",
            "data":{
                "call_id":"30000000-0000-4000-8000-000000000003",
                "name":"view_file",
                "arguments":{"path":"Cargo.toml","start_line":1,"end_line":10}
            }
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    assert!(state.apply(&request).unwrap());
    let request = state.tool_request.unwrap();
    assert_eq!(request.name.as_str(), "view_file");
    assert_eq!(request.call_id, "30000000-0000-4000-8000-000000000003");
    assert_eq!(request.arguments["path"], "Cargo.toml");
}

#[test]
fn typed_tool_call_rejects_unknown_fields_names_and_invalid_ids() {
    for data in [
        json!({"call_id":"30000000-0000-4000-8000-000000000003","name":"view_file","arguments":{"path":"Cargo.toml","unexpected":true}}),
        json!({"call_id":"30000000-0000-4000-8000-000000000003","name":"unknown_tool","arguments":{}}),
        json!({"call_id":"not-a-uuid","name":"view_file","arguments":{"path":"Cargo.toml"}}),
        json!({"call_id":"30000000-0000-4000-8000-000000000003","name":"view_file","arguments":{"path":"../secret"},"extra":true}),
    ] {
        let event = StreamEvent::from_payload(
            Some(1),
            "tool_call".into(),
            json!({"message":"Tool requested","data":data})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            ChatState::default().apply(&event),
            Err(ChatFailure::InvalidResponse)
        );
    }
}

#[test]
fn eof_does_not_fabricate_a_completed_output() {
    let source = frame(1, "token", json!({"text":"partial"}));
    let mut state = ChatState::default();
    let result = consume(Box::new(Cursor::new(source.into_bytes())), |event| {
        state.apply(&event).map(|_| ())
    });
    assert_eq!(result, Err(ChatFailure::IncompleteStream));
    assert_eq!(state.answer, "partial");
    assert!(!state.complete);
}

#[test]
fn done_and_error_are_the_only_terminal_events() {
    let source = frame(
        1,
        "done",
        json!({"session_id":CHAT,"output_id":OUTPUT,"total_ms":1}),
    );
    let mut state = ChatState::default();
    consume(Box::new(Cursor::new(source.into_bytes())), |event| {
        state.apply(&event).map(|_| ())
    })
    .unwrap();
    assert!(state.complete);
    let error = frame(1, "error", json!({"error_number":1201}));
    assert_eq!(
        consume(Box::new(Cursor::new(error.into_bytes())), |event| {
            ChatState::default().apply(&event).map(|_| ())
        }),
        Err(ChatFailure::Rejected(Some(1201)))
    );
}

#[test]
fn continuation_cannot_silently_switch_to_another_chat() {
    let mut state = ChatState {
        chat_id: Some(CHAT.into()),
        ..Default::default()
    };
    let event=StreamEvent::from_payload(Some(1),"done".into(),json!({"message":"Output complete","data":{"session_id":OUTPUT,"output_id":OUTPUT,"total_ms":1}}).to_string().as_bytes()).unwrap();
    assert!(matches!(
        state.apply(&event),
        Err(ChatFailure::InvalidResponse)
    ));
    assert_eq!(state.chat_id.as_deref(), Some(CHAT));
    assert!(!state.complete);
}
