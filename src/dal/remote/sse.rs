//! Bounded SSE decoding, independent of network chunk and UTF-8 boundaries.

use crate::domain::models::chat::{ChatFailure, StreamEvent};

const MAX_FRAME: usize = 256 * 1024;

#[derive(Default)]
pub struct SseFrameDecoder {
    pending: Vec<u8>,
    name: String,
    id: Option<u64>,
    data: Vec<u8>,
    frame_bytes: usize,
}

pub struct SseFrame {
    pub id: Option<u64>,
    pub name: String,
    pub data: Vec<u8>,
}

impl SseFrameDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>, ChatFailure> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.frame_bytes += line.len() + 1;
            if self.frame_bytes > MAX_FRAME {
                return Err(ChatFailure::InvalidResponse);
            }
            if line.is_empty() {
                if !self.data.is_empty() {
                    if self.data.last() == Some(&b'\n') {
                        self.data.pop();
                    }
                    events.push(SseFrame {
                        id: self.id,
                        name: self.name.clone(),
                        data: std::mem::take(&mut self.data),
                    });
                }
                self.name.clear();
                self.data.clear();
                self.id = None;
                self.frame_bytes = 0;
                continue;
            }
            if line[0] == b':' {
                continue;
            }
            let boundary = line.iter().position(|b| *b == b':').unwrap_or(line.len());
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
                b"event" => {
                    self.name = String::from_utf8(value.to_vec())
                        .map_err(|_| ChatFailure::InvalidResponse)?
                }
                b"id" => {
                    self.id = Some(
                        std::str::from_utf8(value)
                            .ok()
                            .and_then(|s| s.parse::<u64>().ok())
                            .filter(|n| *n > 0)
                            .ok_or(ChatFailure::InvalidResponse)?,
                    );
                }
                b"data" => {
                    self.data.extend_from_slice(value);
                    self.data.push(b'\n');
                }
                _ => {}
            }
        }
        if self.pending.len() + self.frame_bytes > MAX_FRAME {
            return Err(ChatFailure::InvalidResponse);
        }
        Ok(events)
    }
}

#[derive(Default)]
pub struct SseDecoder {
    frames: SseFrameDecoder,
}

impl SseDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<StreamEvent>, ChatFailure> {
        self.frames
            .feed(bytes)?
            .into_iter()
            .map(|frame| StreamEvent::from_payload(frame.id, frame.name, &frame.data))
            .collect()
    }
}
