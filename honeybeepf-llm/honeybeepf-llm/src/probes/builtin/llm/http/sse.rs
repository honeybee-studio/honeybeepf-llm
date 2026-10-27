//! Server-sent event bodies: end of stream and usage merged across events.

use serde_json::Value;

use honeybeepf_llm_core::types::{SseChunkDelta, UsageInfo};

use super::body::{is_responses_event, parse_response_json_value};

/// End-of-stream markers: OpenAI `finish_reason`, Anthropic `message_stop`, Gemini `finishReason`
/// or prompt `blockReason`, OpenAI Responses terminal event types.
const SSE_END_MARKERS: [&str; 7] = [
    "\"finish_reason\"",
    "\"message_stop\"",
    "\"finishReason\"",
    "\"blockReason\"",
    "\"response.completed\"",
    "\"response.failed\"",
    "\"response.incomplete\"",
];

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
        } else if !cursor.ended && SSE_END_MARKERS.iter().any(|m| line.contains(m)) {
            cursor.ended = true;
        }
        if cursor.done || !line.starts_with("data: ") {
            continue;
        }
        let data = &line["data: ".len()..];

        // A failed Responses stream may end without usage; its status still has to finish the call.
        let is_usage_event = serde_json::from_str::<SseChunkDelta>(data).is_ok_and(|chunk| {
            chunk.has_usage()
                || chunk
                    .response
                    .as_ref()
                    .is_some_and(|r| r.get("status").and_then(Value::as_str) == Some("failed"))
        });
        if !is_usage_event {
            continue;
        }
        let Ok(mut val) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if val.get("message").is_some_and(|m| m.get("usage").is_some()) {
            val = val["message"].take();
        } else if is_responses_event(&val) {
            val = val["response"].take();
        }
        match cursor.acc.as_mut() {
            Some(prev) => merge_sse_event(prev, val),
            None => cursor.acc = Some(val),
        }
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
    /// Usage-bearing events merged in order: Anthropic sends input tokens in
    /// message_start and only output tokens in message_delta.
    acc: Option<Value>,
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

    /// The merged usage once an end marker or `[DONE]` has been read.
    pub(super) fn usage(&self) -> Option<UsageInfo> {
        if !self.ended {
            return None;
        }
        self.acc.as_ref().and_then(parse_response_json_value)
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
                prev.extend(next.into_iter().filter(|(_, v)| !v.is_null()))
            }
            (Some(slot), value) => *slot = value,
            (None, value) => {
                acc.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{GEMINI_ZERO_USAGE, RESPONSES_CREATED, tokens};
    use super::super::{Http2Parser, Http11Parser, ProtocolParser};
    use super::*;

    fn http11_sse(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{body}").into_bytes()
    }

    const ANTHROPIC_SSE: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\n\n",
    );

    #[test]
    fn anthropic_sse_merges_message_start_and_delta_usage() {
        let body =
            format!("{ANTHROPIC_SSE}event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n");
        let info = Http11Parser
            .parse_response(&http11_sse(&body))
            .expect("usage");
        assert_eq!((info.prompt_tokens, info.completion_tokens), (25, 15));
        assert_eq!(info.model.as_deref(), Some("claude-test"));
    }

    #[test]
    fn anthropic_sse_delta_with_both_counts() {
        let body = concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":25,\"output_tokens\":15}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let info = Http11Parser.parse_response(&http11_sse(body));
        assert_eq!(tokens(info), Some((25, 15)));
    }

    const DONE_IN_TEXT: &str = "The stream ends with data: [DONE]";

    #[test]
    fn anthropic_sse_text_quoting_done_waits_for_message_stop() {
        let start = format!(
            "event: message_start\n\
             data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m1\",\"model\":\"claude-test\",\"usage\":{{\"input_tokens\":25,\"output_tokens\":1}}}}}}\n\n\
             event: content_block_delta\n\
             data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{DONE_IN_TEXT}\"}}}}\n\n"
        );
        let info = Http11Parser.parse_response(&http11_sse(&start));
        assert_eq!(tokens(info), None);

        let full = format!(
            "{start}event: message_delta\n\
             data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":15}}}}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        let info = Http11Parser.parse_response(&http11_sse(&full));
        assert_eq!(tokens(info), Some((25, 15)));
    }

    #[test]
    fn gemini_sse_text_quoting_done_is_incomplete() {
        let body = format!(
            "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{DONE_IN_TEXT}\"}}]}}}}],\
             \"usageMetadata\":{{\"promptTokenCount\":7,\"candidatesTokenCount\":2}},\"modelVersion\":\"gemini-test\"}}\r\n\r\n"
        );
        let info = Http11Parser.parse_response(&http11_sse(&body));
        assert_eq!(tokens(info), None);
    }

    #[test]
    fn anthropic_sse_without_message_stop_is_incomplete() {
        let info = Http11Parser.parse_response(&http11_sse(ANTHROPIC_SSE));
        assert_eq!(tokens(info), None);
    }

    fn cache(info: &UsageInfo) -> (Option<u64>, Option<u64>) {
        (info.cache_read_tokens, info.cache_write_tokens)
    }

    const ANTHROPIC_SSE_CACHE_START: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"model\":\"claude-test\",",
        "\"usage\":{\"input_tokens\":50,\"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":100000,\"output_tokens\":1}}}\n\n",
    );

    #[test]
    fn anthropic_sse_input_total_includes_cache() {
        let body = format!(
            "{ANTHROPIC_SSE_CACHE_START}event: message_delta\n\
             data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":300}}}}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        let info = Http11Parser.parse_response(&http11_sse(&body));
        assert_eq!(tokens(info), Some((102_050, 300)));
    }

    #[test]
    fn anthropic_sse_cumulative_delta_overrides_message_start_cache() {
        let body = format!(
            "{ANTHROPIC_SSE_CACHE_START}event: message_delta\n\
             data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":60,\
             \"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":100500,\"output_tokens\":300}}}}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        let info = Http11Parser
            .parse_response(&http11_sse(&body))
            .expect("usage");
        assert_eq!((info.prompt_tokens, info.completion_tokens), (102_560, 300));
        assert_eq!(cache(&info), (Some(100_500), Some(2000)));
    }

    #[test]
    fn anthropic_sse_null_delta_counts_keep_message_start_cache() {
        let body = format!(
            "{ANTHROPIC_SSE_CACHE_START}event: message_delta\n\
             data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":null,\
             \"cache_creation_input_tokens\":null,\"cache_read_input_tokens\":null,\"output_tokens\":300}}}}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        let info = Http11Parser
            .parse_response(&http11_sse(&body))
            .expect("usage");
        assert_eq!((info.prompt_tokens, info.completion_tokens), (102_050, 300));
        assert_eq!(cache(&info), (Some(100_000), Some(2000)));
    }

    #[test]
    fn anthropic_sse_keeps_message_start_speed_geo_and_1h_writes() {
        // message_delta usage has no speed, inference_geo or cache_creation; null must not erase them.
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":50,",
            "\"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":0,\"cache_creation\":{\"ephemeral_5m_input_tokens\":500,",
            "\"ephemeral_1h_input_tokens\":1500},\"output_tokens\":1,\"speed\":\"fast\",\"inference_geo\":\"us\"}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":50,",
            "\"cache_creation_input_tokens\":2000,\"cache_read_input_tokens\":0,\"output_tokens\":300,\"speed\":null}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let info = Http11Parser
            .parse_response(&http11_sse(body))
            .expect("usage");
        assert_eq!((info.prompt_tokens, info.completion_tokens), (2050, 300));
        assert_eq!(info.cache_write_long_ttl_tokens, Some(1500));
        assert_eq!(
            (info.speed.as_deref(), info.inference_geo.as_deref()),
            (Some("fast"), Some("us"))
        );
    }

    #[test]
    fn responses_sse_reads_served_tier_and_cache_writes_from_response_completed() {
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-6.1-sol\",\"service_tier\":\"auto\",",
            "\"status\":\"in_progress\",\"error\":null,\"usage\":null}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-6.1-sol\",\"service_tier\":\"default\",",
            "\"status\":\"completed\",\"error\":null,\"usage\":{\"input_tokens\":12000,\"output_tokens\":500,",
            "\"input_tokens_details\":{\"cached_tokens\":4000,\"cache_write_tokens\":7000,",
            "\"cache_write_tokens_details\":{\"ttl_30m_tokens\":3000,\"ttl_12h_tokens\":4000}}}}}\n\n",
        );
        let info = Http11Parser
            .parse_response(&http11_sse(body))
            .expect("usage");
        assert_eq!(cache(&info), (Some(4000), Some(7000)));
        assert_eq!(info.cache_write_long_ttl_tokens, Some(4000));
        assert_eq!(info.service_tier.as_deref(), Some("default"));
    }

    #[test]
    fn openai_chat_sse_reads_service_tier_from_the_usage_chunk() {
        let body = concat!(
            "data: {\"model\":\"gpt-6.1-sol\",\"service_tier\":\"flex\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
            "data: {\"model\":\"gpt-6.1-sol\",\"service_tier\":\"flex\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let info = Http11Parser
            .parse_response(&http11_sse(body))
            .expect("usage");
        assert_eq!(info.service_tier.as_deref(), Some("flex"));
    }

    #[test]
    fn openai_chat_sse_cached_tokens_stay_inside_prompt() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":2000,\"completion_tokens\":40,",
            "\"prompt_tokens_details\":{\"cached_tokens\":1536}}}\n\n",
            "data: [DONE]\n\n",
        );
        let info = Http11Parser
            .parse_response(&http11_sse(body))
            .expect("usage");
        assert_eq!((info.prompt_tokens, info.completion_tokens), (2000, 40));
        assert_eq!(cache(&info), (Some(1536), None));
    }

    #[test]
    fn gemini_sse_reads_cached_content_tokens() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"i\"}]},\"finishReason\":\"STOP\"}],",
            "\"usageMetadata\":{\"promptTokenCount\":5000,\"candidatesTokenCount\":20,\"cachedContentTokenCount\":4096},",
            "\"modelVersion\":\"gemini-test\"}\r\n\r\n",
        );
        let info = Http11Parser
            .parse_response(&http11_sse(body))
            .expect("usage");
        assert_eq!((info.prompt_tokens, info.completion_tokens), (5000, 20));
        assert_eq!(cache(&info), (Some(4096), None));
    }

    #[test]
    fn gemini_sse_reads_usage_metadata() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"h\"}]}}],\"usageMetadata\":{\"promptTokenCount\":7},\"modelVersion\":\"gemini-test\"}\r\n\r\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"i\"}]},\"finishReason\":\"STOP\"}],",
            "\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":3},\"modelVersion\":\"gemini-test\"}\r\n\r\n",
        );
        let info = Http11Parser
            .parse_response(&http11_sse(body))
            .expect("usage");
        assert_eq!((info.prompt_tokens, info.completion_tokens), (7, 3));
        assert_eq!(info.model.as_deref(), Some("gemini-test"));
    }

    #[test]
    fn gemini_sse_zero_output_is_recorded() {
        for finish in ["MAX_TOKENS", "STOP"] {
            let body = format!(
                "data: {{\"candidates\":[{{\"content\":{{\"role\":\"model\"}},\"finishReason\":\"{finish}\",\"index\":0}}],\
                 {GEMINI_ZERO_USAGE},\"modelVersion\":\"gemini-test\"}}\r\n\r\n"
            );
            let info = Http11Parser.parse_response(&http11_sse(&body));
            assert_eq!(tokens(info), Some((1, 0)), "{finish}");
        }
    }

    #[test]
    fn gemini_prompt_block_is_recorded_on_every_path() {
        let blocked = "{\"promptFeedback\":{\"blockReason\":\"SAFETY\"},\
            \"usageMetadata\":{\"promptTokenCount\":8,\"totalTokenCount\":8},\"modelVersion\":\"gemini-test\"}";
        let json = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{blocked}");
        let sse = http11_sse(&format!("data: {blocked}\r\n\r\n"));
        let h2 = format!("data: {blocked}\r\n\r\n");

        assert_eq!(
            tokens(Http11Parser.parse_response(json.as_bytes())),
            Some((8, 0))
        );
        assert_eq!(tokens(Http11Parser.parse_response(&sse)), Some((8, 0)));
        assert_eq!(
            tokens(Http2Parser.parse_response(h2.as_bytes())),
            Some((8, 0))
        );
    }

    #[test]
    fn gemini_sse_partial_last_event_is_incomplete() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"h\"}]}}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":2}}\r\n\r\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"i\"}]},\"finishReason\":\"STOP\"}],\"usageMet",
        );
        let info = Http11Parser.parse_response(&http11_sse(body));
        assert_eq!(tokens(info), None);
    }

    #[test]
    fn responses_sse_reads_usage_from_response_completed() {
        let body = format!(
            "{RESPONSES_CREATED}event: response.completed\n\
             data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"r1\",\"model\":\"gpt-6\",\"status\":\"completed\",\"error\":null,\
             \"usage\":{{\"input_tokens\":30,\"input_tokens_details\":{{\"cached_tokens\":5}},\"output_tokens\":9,\
             \"output_tokens_details\":{{\"reasoning_tokens\":2}},\"total_tokens\":39}}}}}}\n\n"
        );
        let info = Http11Parser
            .parse_response(&http11_sse(&body))
            .expect("usage");
        assert_eq!((info.prompt_tokens, info.completion_tokens), (30, 9));
        assert_eq!(
            (info.thoughts_tokens, info.cache_read_tokens),
            (Some(2), Some(5))
        );
        assert_eq!(info.model.as_deref(), Some("gpt-6"));
    }

    #[test]
    fn responses_sse_incomplete_keeps_usage() {
        let body = format!(
            "{RESPONSES_CREATED}event: response.incomplete\n\
             data: {{\"type\":\"response.incomplete\",\"response\":{{\"model\":\"gpt-6\",\"status\":\"incomplete\",\"error\":null,\
             \"incomplete_details\":{{\"reason\":\"max_output_tokens\"}},\"usage\":{{\"input_tokens\":30,\"output_tokens\":16}}}}}}\n\n"
        );
        let info = Http11Parser.parse_response(&http11_sse(&body));
        assert_eq!(tokens(info), Some((30, 16)));
    }

    #[test]
    fn responses_sse_failed_without_usage_is_error() {
        let body = format!(
            "{RESPONSES_CREATED}event: response.failed\n\
             data: {{\"type\":\"response.failed\",\"response\":{{\"model\":\"gpt-6\",\"status\":\"failed\",\
             \"error\":{{\"code\":\"server_error\",\"message\":\"boom\"}},\"usage\":null}}}}\n\n"
        );
        let info = Http11Parser.parse_response(&http11_sse(&body));
        assert_eq!(tokens(info), Some((0, 0)));
    }

    #[test]
    fn responses_sse_without_terminal_event_is_incomplete() {
        let info = Http11Parser.parse_response(&http11_sse(RESPONSES_CREATED));
        assert_eq!(tokens(info), None);
    }

    #[test]
    fn sse_string_response_field_keeps_top_level_usage() {
        let body = concat!(
            "data: {\"response\":\"\",\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        );
        let info = Http11Parser.parse_response(&http11_sse(body));
        assert_eq!(tokens(info), Some((5, 3)));
    }
}
