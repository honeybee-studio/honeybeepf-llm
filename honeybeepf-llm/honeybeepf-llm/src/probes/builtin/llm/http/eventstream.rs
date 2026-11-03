//! AWS event stream bodies (Bedrock `converse-stream`): CRC-checked binary messages.

use serde_json::Value;

use honeybeepf_llm_core::types::UsageInfo;

use super::body::parse_response_json_value;
use super::sse::merge_usage_event;

/// Total length, headers length and prelude CRC, each a big-endian u32.
const PRELUDE_LEN: usize = 12;
const CRC_LEN: usize = 4;
/// Largest message the format allows (AWS SDKs reject longer ones).
const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;
const HEADER_STRING: u8 = 7;

/// Where the stream stands after the messages read so far.
#[derive(Default, PartialEq, Eq)]
enum Progress {
    #[default]
    Open,
    /// The last event was read, so the merged usage is final.
    Ended,
    /// An `exception` or `error` message ended the call.
    Failed,
    /// A CRC or length check failed; message boundaries cannot be found again.
    Broken,
}

/// The complete messages of one event stream body read so far, so each read decodes only the messages it completes.
#[derive(Default)]
pub(super) struct EventStreamCursor {
    /// Bytes of a message the reads so far have not completed.
    partial: Vec<u8>,
    progress: Progress,
    /// Usage-bearing events merged in order, as for SSE.
    acc: Option<Value>,
}

pub(super) fn parse_event_stream_body(body: &[u8]) -> Option<UsageInfo> {
    let mut cursor = EventStreamCursor::default();
    cursor.feed(body);
    cursor.usage()
}

impl EventStreamCursor {
    /// Read the messages these bytes complete.
    pub(super) fn feed(&mut self, bytes: &[u8]) {
        if self.progress != Progress::Open {
            return;
        }
        if self.partial.is_empty() {
            let used = self.read_messages(bytes);
            self.partial.extend_from_slice(&bytes[used..]);
        } else {
            let mut joined = std::mem::take(&mut self.partial);
            joined.extend_from_slice(bytes);
            let used = self.read_messages(&joined);
            joined.drain(..used);
            self.partial = joined;
        }
        if self.progress != Progress::Open {
            self.partial = Vec::new();
        }
    }

    /// The merged usage once the last event was read; a failed stream is an error call.
    pub(super) fn usage(&self) -> Option<UsageInfo> {
        match self.progress {
            Progress::Ended => self.acc.as_ref().and_then(parse_response_json_value),
            Progress::Failed => Some(UsageInfo::default()),
            Progress::Open | Progress::Broken => None,
        }
    }

    /// Read whole messages from the start of `buf`; returns the bytes they took.
    fn read_messages(&mut self, buf: &[u8]) -> usize {
        let mut used = 0;
        while self.progress == Progress::Open {
            match next_message(&buf[used..]) {
                Frame::Partial => break,
                Frame::Broken => self.progress = Progress::Broken,
                Frame::Message {
                    len,
                    headers,
                    payload,
                } => {
                    self.read_message(headers, payload);
                    used += len;
                }
            }
        }
        used
    }

    fn read_message(&mut self, headers: &[u8], payload: &[u8]) {
        // A modeled exception (e.g. throttlingException mid-stream) ends the stream without usage.
        if matches!(
            header(headers, ":message-type"),
            Some(b"exception" | b"error")
        ) {
            self.progress = Progress::Failed;
            return;
        }
        // converse-stream: `metadata` is the last event, and the only one with usage.
        if header(headers, ":event-type") == Some(b"metadata") {
            if let Ok(event) = serde_json::from_slice::<Value>(payload) {
                merge_usage_event(&mut self.acc, event);
            }
            self.progress = Progress::Ended;
        }
    }
}

enum Frame<'b> {
    Partial,
    Broken,
    Message {
        len: usize,
        headers: &'b [u8],
        payload: &'b [u8],
    },
}

/// The message at the start of `buf`, checked against both CRCs.
fn next_message(buf: &[u8]) -> Frame<'_> {
    let Some(prelude) = buf.get(..PRELUDE_LEN) else {
        return Frame::Partial;
    };
    let word = |at: usize| {
        u32::from_be_bytes([
            prelude[at],
            prelude[at + 1],
            prelude[at + 2],
            prelude[at + 3],
        ])
    };
    let (total, headers_len) = (word(0) as usize, word(4) as usize);
    // Checked before waiting for the rest, so garbage never holds the cursor for a huge message.
    if crc32(&prelude[..8]) != word(8)
        || !(PRELUDE_LEN + CRC_LEN..=MAX_MESSAGE_LEN).contains(&total)
        || headers_len > total - PRELUDE_LEN - CRC_LEN
    {
        return Frame::Broken;
    }
    let Some(message) = buf.get(..total) else {
        return Frame::Partial;
    };
    let (checked, crc) = message.split_at(total - CRC_LEN);
    if crc32(checked).to_be_bytes() != crc {
        return Frame::Broken;
    }
    let (headers, payload) = checked[PRELUDE_LEN..].split_at(headers_len);
    Frame::Message {
        len: total,
        headers,
        payload,
    }
}

/// The value of string header `name`; `None` when it is missing, not a string, or the headers are malformed.
fn header<'h>(mut headers: &'h [u8], name: &str) -> Option<&'h [u8]> {
    while let Some((&name_len, rest)) = headers.split_first() {
        let (key, rest) = rest.split_at_checked(usize::from(name_len))?;
        let (&kind, rest) = rest.split_first()?;
        let value_len = match kind {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 => 4,
            5 | 8 => 8,
            9 => 16,
            6 | HEADER_STRING => {
                2 + usize::from(u16::from_be_bytes([*rest.first()?, *rest.get(1)?]))
            }
            _ => return None,
        };
        let (value, rest) = rest.split_at_checked(value_len)?;
        if key == name.as_bytes() {
            return if kind == HEADER_STRING {
                value.get(2..)
            } else {
                None
            };
        }
        headers = rest;
    }
    None
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(bytes);
    crc.sum()
}

#[cfg(test)]
mod tests;
