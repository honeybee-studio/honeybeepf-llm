//! Server-sent event bodies: end of stream and usage merged across events.

use serde_json::Value;

use honeybeepf_llm_core::types::{SseChunkDelta, UsageInfo};

use super::body::{is_responses_event, parse_response_json_value};

/// End-of-stream markers: OpenAI `finish_reason`, Anthropic `message_stop`, Gemini `finishReason`
/// or prompt `blockReason`, OpenAI Responses terminal event types, Gemini Interactions completion.
const SSE_END_MARKERS: [&str; 8] = [
    "\"finish_reason\"",
    "\"message_stop\"",
    "\"finishReason\"",
    "\"blockReason\"",
    "\"response.completed\"",
    "\"response.failed\"",
    "\"response.incomplete\"",
    "\"interaction.completed\"",
];

/// Cap on captured fallback blocks per stream; a real fallback chain has a handful, so this only
/// bounds a hostile stream that sends one per event.
const MAX_FALLBACK_BLOCKS: usize = 64;

pub(super) fn parse_sse_body(body: &[u8]) -> Option<UsageInfo> {
    let mut cursor = SseCursor::default();
    cursor.feed(body);
    cursor.usage()
}

fn read_lines(cursor: &mut SseCursor, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines() {
        let line = line.trim();
        // `[DONE]` counts only as its own line: generated text can quote it unescaped inside a JSON string.
        if line == "data: [DONE]" {
            cursor.ended = true;
            cursor.done = true;
        } else if line == "event: error" {
            // A Gemini Interactions error event aborts the call with no usage; later events on the
            // stream are no longer trusted, so nothing is recorded (the call is counted unparsed).
            cursor.aborted = true;
        } else if !cursor.ended && SSE_END_MARKERS.iter().any(|m| line.contains(m)) {
            cursor.ended = true;
        }
        if cursor.done || cursor.aborted || !line.starts_with("data: ") {
            continue;
        }
        let data = &line["data: ".len()..];

        // Rare: a server-side fallback names each declined attempt's refusal category in a
        // content_block_start fallback block; normal deltas skip the parse on the cheap check.
        if cursor.fallbacks.len() < MAX_FALLBACK_BLOCKS
            && data.contains("\"fallback\"")
            && let Ok(v) = serde_json::from_str::<Value>(data)
            && v.get("type").and_then(Value::as_str) == Some("content_block_start")
            && let Some(block) = v
                .get("content_block")
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("fallback"))
        {
            cursor.fallbacks.push(block.clone());
        }

        // A failed Responses or Interactions stream may end without usage; its status still has to
        // finish the call.
        let is_usage_event = serde_json::from_str::<SseChunkDelta>(data).is_ok_and(|chunk| {
            chunk.has_usage()
                || chunk
                    .response
                    .as_ref()
                    .is_some_and(|r| r.get("status").and_then(Value::as_str) == Some("failed"))
                || chunk.interaction.as_ref().is_some_and(interaction_failed)
        });
        if !is_usage_event {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        merge_usage_event(&mut cursor.acc, val);
    }
}

/// A failed interaction ends the call with no usage, as a failed Responses stream does; its status
/// still has to finish the call. Cancelled or incomplete ones record from the usage they carry.
fn interaction_failed(interaction: &Value) -> bool {
    interaction.get("status").and_then(Value::as_str) == Some("failed")
}

/// Merge a usage-bearing event into `acc`, unwrapping Anthropic `message_start`, a Responses event
/// or a Gemini Interactions event so the merged body carries usage and model at its top level.
pub(super) fn merge_usage_event(acc: &mut Option<Value>, mut val: Value) {
    if val.get("message").is_some_and(|m| m.get("usage").is_some()) {
        val = val["message"].take();
    } else if is_responses_event(&val) {
        val = val["response"].take();
    } else if val
        .get("interaction")
        .is_some_and(|i| i.get("usage").is_some() || interaction_failed(i))
    {
        val = val["interaction"].take();
    }
    match acc.as_mut() {
        Some(prev) => merge_sse_event(prev, val),
        None => *acc = Some(val),
    }
}

/// The complete lines of one SSE body read so far, so each read parses only the lines it completes.
#[derive(Default)]
pub(super) struct SseCursor {
    /// Bytes after the last newline.
    partial: Vec<u8>,
    /// An end marker or `[DONE]` was read, so the merged usage is final.
    ended: bool,
    /// `[DONE]` was read; nothing after it is merged.
    done: bool,
    /// An error event aborted the stream; the call is not recorded whatever follows.
    aborted: bool,
    /// Usage-bearing events merged in order: Anthropic sends input tokens in
    /// message_start and only output tokens in message_delta.
    acc: Option<Value>,
    /// `fallback` content blocks from content_block_start, attached to the merged body so a
    /// server-side fallback's declined attempts can be matched to their refusal category.
    fallbacks: Vec<Value>,
}

impl SseCursor {
    /// Read the lines these bytes complete.
    pub(super) fn feed(&mut self, bytes: &[u8]) {
        // Only complete lines count: a partial last event can show its end marker before its usage.
        let Some(last) = bytes.iter().rposition(|&b| b == b'\n') else {
            self.partial.extend_from_slice(bytes);
            return;
        };
        let (complete, rest) = bytes.split_at(last + 1);
        if self.partial.is_empty() {
            read_lines(self, complete);
        } else {
            let mut joined = std::mem::take(&mut self.partial);
            joined.extend_from_slice(complete);
            read_lines(self, &joined);
            joined.clear();
            self.partial = joined;
        }
        self.partial.extend_from_slice(rest);
    }

    /// Bytes held of a line that has not ended yet.
    pub(super) fn partial_len(&self) -> usize {
        self.partial.len()
    }

    /// The merged usage once an end marker or `[DONE]` has been read.
    pub(super) fn usage(&self) -> Option<UsageInfo> {
        if self.aborted || !self.ended {
            return None;
        }
        if self.fallbacks.is_empty() {
            return self.acc.as_ref().and_then(parse_response_json_value);
        }
        // The stream carried fallback blocks; add them to `content` so the parser reads their
        // categories. Cloned only here, once the stream has ended, never per event.
        let mut acc = self.acc.clone()?;
        match acc.get_mut("content").and_then(Value::as_array_mut) {
            Some(content) => content.extend(self.fallbacks.iter().cloned()),
            None => {
                acc.as_object_mut()?
                    .insert("content".to_string(), Value::Array(self.fallbacks.clone()));
            }
        }
        parse_response_json_value(&acc)
    }
}

/// Overlay a later SSE event onto the accumulated one, merging nested objects one level deep.
fn merge_sse_event(acc: &mut Value, update: Value) {
    let (Some(acc), Value::Object(update)) = (acc.as_object_mut(), update) else {
        return;
    };
    for (key, value) in update {
        match (acc.get_mut(&key), value) {
            // A null count in a later event (e.g. Anthropic message_delta) must not erase an earlier one.
            (Some(Value::Object(prev)), Value::Object(next)) => {
                prev.extend(next.into_iter().filter(|(_, v)| !v.is_null()));
            }
            (Some(slot), value) => *slot = value,
            (None, value) => {
                acc.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests;
