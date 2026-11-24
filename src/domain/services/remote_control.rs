//! Remote command cursor and reconnect policy.

use crate::domain::models::remote_control::{RemoteEvent, RemoteEventError};
use std::time::Duration;

pub struct CommandCursor {
    acknowledged: u64,
    pending: Option<u64>,
}

impl CommandCursor {
    pub fn new(acknowledged: u64) -> Self {
        Self {
            acknowledged,
            pending: None,
        }
    }

    pub fn value(&self) -> u64 {
        self.acknowledged
    }

    pub fn receive(&mut self, event: &RemoteEvent) -> Result<bool, RemoteEventError> {
        if event.id <= self.acknowledged {
            return Ok(false);
        }
        match self.pending {
            Some(id) if id == event.id => Ok(false),
            Some(_) => Err(RemoteEventError),
            None => {
                self.pending = Some(event.id);
                Ok(true)
            }
        }
    }

    pub fn acknowledge(&mut self, id: u64) -> Result<(), RemoteEventError> {
        if self.pending != Some(id) {
            return Err(RemoteEventError);
        }
        self.acknowledged = id;
        self.pending = None;
        Ok(())
    }
}

pub fn reconnect_delay(attempt: u32) -> Duration {
    let shift = attempt.min(5);
    Duration::from_millis((250u64 << shift).min(8_000))
}

#[cfg(test)]
mod tests {
    use super::{CommandCursor, reconnect_delay};
    use crate::domain::models::remote_control::{RemoteEvent, RemoteEventKind};
    use serde_json::json;
    use std::time::Duration;

    fn event(id: u64) -> RemoteEvent {
        RemoteEvent {
            id,
            kind: RemoteEventKind::Prompt,
            payload: json!({"prompt":"hello", "file_ids":[]}),
            created_at_ms: id,
        }
    }

    #[test]
    fn cursor_advances_only_after_command_ack_and_deduplicates_replay() {
        let mut cursor = CommandCursor::new(3);
        assert!(!cursor.receive(&event(3)).unwrap());
        assert!(cursor.receive(&event(4)).unwrap());
        assert!(!cursor.receive(&event(4)).unwrap());
        assert_eq!(cursor.value(), 3);
        cursor.acknowledge(4).unwrap();
        assert_eq!(cursor.value(), 4);
        assert!(!cursor.receive(&event(4)).unwrap());
        assert!(cursor.receive(&event(5)).unwrap());
    }

    #[test]
    fn reconnect_backoff_has_a_fixed_upper_bound() {
        assert_eq!(reconnect_delay(0), Duration::from_millis(250));
        assert_eq!(reconnect_delay(20), Duration::from_secs(8));
    }
}
