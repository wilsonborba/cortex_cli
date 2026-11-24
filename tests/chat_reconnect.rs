use cortex_cli::domain::{
    models::chat::{ChatFailure, ChatPage, ChatSummary, StreamEvent},
    services::chat::{ChatApi, ChatState, follow},
};
use serde_json::{Value, json};
use std::{
    io::{Cursor, Read},
    sync::Mutex,
};
const CHAT: &str = "10000000-0000-4000-8000-000000000001";
const OUTPUT: &str = "20000000-0000-4000-8000-000000000002";
fn event(id: u64, name: &str, data: Value) -> StreamEvent {
    StreamEvent::from_payload(
        Some(id),
        name.into(),
        json!({"message":"fixture","data":data})
            .to_string()
            .as_bytes(),
    )
    .unwrap()
}
fn frame(event: StreamEvent) -> String {
    format!(
        "id: {}\nevent: {}\ndata: {}\n\n",
        event.id.unwrap(),
        event.name,
        json!({"message":event.message,"data":event.data})
    )
}
fn accepted() -> StreamEvent {
    event(
        1,
        "stage",
        json!({"stage":"accept","status":"started","session_id":CHAT,"output_id":OUTPUT}),
    )
}
struct Fixture {
    calls: Mutex<Vec<(String, String, u64)>>,
}
impl ChatApi for Fixture {
    fn send(&self, _: Option<&str>, _: &str) -> Result<Box<dyn Read + Send>, ChatFailure> {
        panic!("a lost response must not resend the prompt")
    }
    fn chats(&self) -> Result<Vec<ChatSummary>, ChatFailure> {
        panic!("must not select a different chat")
    }
    fn history(&self, _: &str) -> Result<ChatPage, ChatFailure> {
        panic!("must not select the newest output")
    }
    fn reconnect(
        &self,
        chat: &str,
        output: &str,
        after: u64,
    ) -> Result<Box<dyn Read + Send>, ChatFailure> {
        self.calls
            .lock()
            .unwrap()
            .push((chat.into(), output.into(), after));
        let body = frame(event(2, "token", json!({"text":"first"})))
            + &frame(event(3, "token", json!({"text":" second"})))
            + &frame(event(
                4,
                "done",
                json!({"session_id":CHAT,"output_id":OUTPUT,"total_ms":10}),
            ));
        Ok(Box::new(Cursor::new(body.into_bytes())))
    }
    fn cancel(&self, _: &str, _: &str) -> Result<(), ChatFailure> {
        panic!("not a cancellation")
    }
}
#[test]
fn accepted_ids_enable_control_before_done_and_cannot_change() {
    let mut state = ChatState::default();
    state.apply(&accepted()).unwrap();
    assert_eq!(state.chat_id.as_deref(), Some(CHAT));
    assert_eq!(state.output_id.as_deref(), Some(OUTPUT));
    assert!(!state.complete);
    for data in [
        json!({"session_id":CHAT,"output_id":CHAT,"total_ms":1}),
        json!({"session_id":OUTPUT,"output_id":OUTPUT,"total_ms":1}),
    ] {
        assert_eq!(
            state.apply(&event(2, "done", data)),
            Err(ChatFailure::InvalidResponse)
        );
        state.last_event = 1;
    }
    let mut invalid = ChatState::default();
    assert_eq!(
        invalid.apply(&event(
            1,
            "stage",
            json!({"stage":"accept","status":"started","session_id":CHAT})
        )),
        Err(ChatFailure::InvalidResponse)
    );
}
#[test]
fn transport_loss_reconnects_exact_output_after_cursor_without_duplicate_tokens() {
    let api = Fixture {
        calls: Mutex::new(Vec::new()),
    };
    let body = frame(accepted()) + &frame(event(2, "token", json!({"text":"first"})));
    let mut state = ChatState::default();
    let mut text = String::new();
    follow(
        &api,
        Box::new(Cursor::new(body.into_bytes())),
        &mut state,
        |event| {
            if event.name == "token" {
                text.push_str(event.data["text"].as_str().unwrap());
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(text, "first second");
    assert!(state.complete);
    assert_eq!(
        *api.calls.lock().unwrap(),
        vec![(CHAT.into(), OUTPUT.into(), 2)]
    );
}
#[test]
fn unaccepted_or_rejected_stream_never_guesses_identity() {
    let api = Fixture {
        calls: Mutex::new(Vec::new()),
    };
    for (body, expected) in [
        (
            frame(event(1, "token", json!({"text":"partial"}))),
            ChatFailure::IncompleteStream,
        ),
        (
            frame(event(1, "error", json!({"error_number":1201}))),
            ChatFailure::Rejected(Some(1201)),
        ),
    ] {
        assert_eq!(
            follow(
                &api,
                Box::new(Cursor::new(body.into_bytes())),
                &mut ChatState::default(),
                |_| Ok(())
            ),
            Err(expected)
        );
    }
    assert!(api.calls.lock().unwrap().is_empty());
}
