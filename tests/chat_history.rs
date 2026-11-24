use cortex_cli::domain::{
    models::chat::{ChatFailure, ChatOutput, ChatPage, ChatSummary},
    services::chat::{ChatApi, latest_chat, owned_history, running_output},
};
use std::io::Read;

const FIRST: &str = "10000000-0000-4000-8000-000000000001";
const SECOND: &str = "20000000-0000-4000-8000-000000000002";

struct Fixture {
    rows: Vec<ChatSummary>,
    running: bool,
    app: &'static str,
}
impl ChatApi for Fixture {
    fn chats(&self) -> Result<Vec<ChatSummary>, ChatFailure> {
        Ok(self.rows.clone())
    }
    fn history(&self, id: &str) -> Result<ChatPage, ChatFailure> {
        Ok(ChatPage {
            chat: ChatSummary {
                session_id: id.into(),
                app_id: self.app.into(),
                title: "fixture".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            },
            inputs: Vec::new(),
            outputs: vec![ChatOutput {
                output_id: SECOND.into(),
                input_id: FIRST.into(),
                status: if self.running { "running" } else { "completed" }.into(),
                text: "fixture output".into(),
            }],
        })
    }
    fn send(&self, _: Option<&str>, _: &str) -> Result<Box<dyn Read + Send>, ChatFailure> {
        Err(ChatFailure::InvalidInput)
    }
    fn reconnect(&self, _: &str, _: &str, _: u64) -> Result<Box<dyn Read + Send>, ChatFailure> {
        Err(ChatFailure::InvalidInput)
    }
    fn cancel(&self, _: &str, _: &str) -> Result<(), ChatFailure> {
        Err(ChatFailure::InvalidInput)
    }
}

#[test]
fn latest_uses_actual_time_not_pin_order_and_excludes_other_apps() {
    let row = |id: &str, app: &str, time: &str| ChatSummary {
        session_id: id.into(),
        app_id: app.into(),
        title: "fixture".into(),
        updated_at: time.into(),
    };
    let api = Fixture {
        rows: vec![
            row(FIRST, "cortex", "2026-01-01T00:00:00Z"),
            row(SECOND, "cortex", "2026-01-01T00:00:00.250Z"),
            row(FIRST, "annotations", "2027-01-01T00:00:00Z"),
        ],
        running: false,
        app: "cortex",
    };
    assert_eq!(latest_chat(&api).unwrap().as_deref(), Some(SECOND));
}

#[test]
fn finished_history_is_available_without_guessing_running_identifiers() {
    let mut api = Fixture {
        rows: Vec::new(),
        running: false,
        app: "cortex",
    };
    assert!(owned_history(&api, FIRST).is_ok());
    api.running = true;
    let page = owned_history(&api, FIRST).unwrap();
    assert_eq!(running_output(&page).unwrap().as_deref(), Some(SECOND));
    let mut ambiguous = page;
    ambiguous.outputs.push(ambiguous.outputs[0].clone());
    assert_eq!(running_output(&ambiguous), Err(ChatFailure::RunningOutput));
    api.running = false;
    api.app = "annotations";
    assert!(matches!(
        owned_history(&api, FIRST),
        Err(ChatFailure::InvalidInput)
    ));
}
