//! AWS event stream bodies (Bedrock `converse-stream`, `invoke-with-response-stream`): CRC-checked binary messages.

use std::borrow::Cow;

use serde::Deserialize;
use serde_json::Value;

use honeybeepf_llm_core::byte_utils;
use honeybeepf_llm_core::types::UsageInfo;

use super::body::parse_response_json_value;
use super::sse::merge_usage_event;

/// Total length, headers length and prelude CRC, each a big-endian u32.
const PRELUDE_LEN: usize = 12;
const CRC_LEN: usize = 4;
/// Largest message the format allows (AWS SDKs reject longer ones).
const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;
const HEADER_STRING: u8 = 7;
/// On the last `chunk` of an invoke stream, for every model.
const INVOCATION_METRICS: &str = "amazon-bedrock-invocationMetrics";

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
    /// A `chunk` payload's base64 decoded, reused across messages.
    decoded: Vec<u8>,
    progress: Progress,
    /// Usage-bearing events merged in order, as for SSE.
    acc: Option<Value>,
}

/// An invoke stream `chunk` payload: the model's own event, base64-encoded.
#[derive(Deserialize)]
struct Chunk<'a> {
    #[serde(borrow)]
    bytes: Cow<'a, str>,
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
            self.decoded = Vec::new();
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
        match header(headers, ":event-type") {
            Some(b"chunk") => self.read_chunk(payload),
            // converse-stream: the last event, and the only one with usage.
            Some(b"metadata") => {
                if let Ok(event) = serde_json::from_slice::<Value>(payload) {
                    merge_usage_event(&mut self.acc, event);
                }
                self.progress = Progress::Ended;
            }
            _ => {}
        }
    }

    fn read_chunk(&mut self, payload: &[u8]) {
        let Ok(chunk) = serde_json::from_slice::<Chunk>(payload) else {
            return;
        };
        if !decode_base64(chunk.bytes.as_bytes(), &mut self.decoded) {
            return;
        }
        // Most chunks are text deltas: only one naming usage or the metrics is parsed as JSON.
        if !byte_utils::contains_pattern(&self.decoded, b"usage")
            && !byte_utils::contains_pattern(&self.decoded, INVOCATION_METRICS.as_bytes())
        {
            return;
        }
        let Ok(mut event) = serde_json::from_slice::<Value>(&self.decoded) else {
            return;
        };
        let metrics = event
            .as_object_mut()
            .and_then(|e| e.remove(INVOCATION_METRICS));
        let has_usage = event.get("usage").is_some()
            || event
                .get("message")
                .is_some_and(|m| m.get("usage").is_some());
        if has_usage {
            merge_usage_event(&mut self.acc, event);
        }
        // Put back at the root: unwrapping a `message` or `response` member must not drop it.
        if let Some(metrics) = metrics {
            let acc = self
                .acc
                .get_or_insert_with(|| Value::Object(Default::default()));
            if let Some(acc) = acc.as_object_mut() {
                acc.insert(INVOCATION_METRICS.to_owned(), metrics);
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

/// Standard padded base64 into `out`; false on a byte outside the alphabet.
fn decode_base64(text: &[u8], out: &mut Vec<u8>) -> bool {
    out.clear();
    out.reserve(text.len() / 4 * 3);
    let (mut bits, mut held) = (0u32, 0u32);
    for &c in text.iter().take_while(|&&c| c != b'=') {
        let sextet = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return false,
        };
        bits = bits << 6 | u32::from(sextet);
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((bits >> held) as u8);
        }
    }
    true
}

#[cfg(test)]
mod tests;
