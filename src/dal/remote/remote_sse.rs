//! Bounded decoder for the remote command stream's JSON event frames.

use crate::domain::models::remote_control::{RemoteEvent, RemoteEventError};
use serde_json::from_slice;

const MAX_FRAME: usize = 256 * 1024;

#[derive(Default)]
pub struct RemoteSseDecoder {
    pending: Vec<u8>,
    id: Option<u64>,
    data: Vec<u8>,
    frame_bytes: usize,
}

impl RemoteSseDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<RemoteEvent>, RemoteEventError> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.frame_bytes = self.frame_bytes.saturating_add(line.len() + 1);
            if self.frame_bytes > MAX_FRAME {
                return Err(RemoteEventError);
            }
            if line.is_empty() {
                if !self.data.is_empty() {
                    if self.data.last() == Some(&b'\n') {
                        self.data.pop();
                    }
                    let event: RemoteEvent =
                        from_slice(&self.data).map_err(|_| RemoteEventError)?;
                    if self.id != Some(event.id) || event.id == 0 {
                        return Err(RemoteEventError);
                    }
                    events.push(event);
                }
                self.id = None;
                self.data.clear();
                self.frame_bytes = 0;
                continue;
            }
            if line[0] == b':' {
                continue;
            }
            let boundary = line
                .iter()
                .position(|byte| *byte == b':')
                .unwrap_or(line.len());
            let field = &line[..boundary];
            let mut value = if boundary < line.len() {
                &line[boundary + 1..]
            } else {
                &[]
            };
            if value.first() == Some(&b' ') {
                value = &value[1..];
            }
            match field {
                b"id" => {
                    let text = std::str::from_utf8(value).map_err(|_| RemoteEventError)?;
                    if text.contains('\0') {
                        return Err(RemoteEventError);
                    }
                    self.id = Some(text.parse().map_err(|_| RemoteEventError)?);
                }
                b"data" => {
                    self.data.extend_from_slice(value);
                    self.data.push(b'\n');
                }
                _ => {}
            }
        }
        if self.pending.len() + self.frame_bytes > MAX_FRAME {
            return Err(RemoteEventError);
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::RemoteSseDecoder;
    use crate::domain::models::remote_control::RemoteEventKind;

    #[test]
    fn remote_event_survives_every_byte_boundary_and_crlf() {
        let frame = "id: 42\r\ndata: {\"id\":42,\"kind\":\"prompt\",\"payload\":{\"prompt\":\"héllo\",\"file_ids\":[]},\"created_at_ms\":10}\r\n\r\n".as_bytes();
        for split in 0..=frame.len() {
            let mut decoder = RemoteSseDecoder::default();
            let mut events = decoder.feed(&frame[..split]).unwrap();
            events.extend(decoder.feed(&frame[split..]).unwrap());
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].id, 42);
            assert_eq!(events[0].kind, RemoteEventKind::Prompt);
            assert_eq!(events[0].payload["prompt"], "héllo");
        }
    }

    #[test]
    fn remote_event_requires_matching_cursor_and_payload_id() {
        let mut decoder = RemoteSseDecoder::default();
        assert!(decoder.feed(b"id: 4\ndata: {\"id\":5,\"kind\":\"cancel\",\"payload\":{},\"created_at_ms\":0}\n\n").is_err());
    }
}
