//! HTTP/2 request detection and usage parsing from raw bytes, without frame or HPACK decoding.

use serde_json::Value;

use honeybeepf_llm_core::byte_utils;
use honeybeepf_llm_core::types::UsageInfo;

use super::ProtocolParser;
use super::body::{extract_text_from_json, is_responses_event, parse_response_json_value};
use super::registry::{KNOWN_HOSTS, is_llm_path};

#[derive(Clone)]
pub struct Http2Parser;

impl ProtocolParser for Http2Parser {
    fn detect_request(&self, buffer: &[u8]) -> Option<String> {
        // Skip if this looks like HTTP/1.1 (starts with HTTP method)
        let preview = &buffer[..std::cmp::min(10, buffer.len())];
        if preview.starts_with(b"GET ")
            || preview.starts_with(b"POST ")
            || preview.starts_with(b"PUT ")
            || preview.starts_with(b"DELETE ")
            || preview.starts_with(b"PATCH ")
            || preview.starts_with(b"HEAD ")
        {
            return None;
        }

        // Check if buffer contains a known provider host before JSON detection.
        // HTTP/2 HEADERS frames typically contain the :authority host string.
        // This prevents false positives from non-LLM HTTP/2 traffic.
        let has_known_host = KNOWN_HOSTS
            .iter()
            .any(|h| byte_utils::contains_pattern(buffer, h.as_bytes()));

        if !has_known_host {
            return None;
        }

        // H2 body-based detection via JSON keys (only after host check)
        if byte_utils::contains_pattern(buffer, b"\"messages\"")
            || byte_utils::contains_pattern(buffer, b"\"contents\"")
            || byte_utils::contains_pattern(buffer, b"\"prompt\"")
            || byte_utils::contains_pattern(buffer, b"\"model\"")
        {
            return Some("h2_body_detected".to_string());
        }

        // Fallback: cleartext path (rare in H2)
        if is_llm_path(&String::from_utf8_lossy(buffer)) {
            return Some("h2_path_detected".to_string());
        }

        None
    }

    fn writes_during_response(&self) -> bool {
        true
    }

    fn extract_request_text(&self, buffer: &[u8]) -> String {
        let json_objects = byte_utils::extract_h2_json_all(buffer);
        for payload in &json_objects {
            let json_str = String::from_utf8_lossy(payload);
            let text = extract_text_from_json(&json_str);
            if !text.is_empty() {
                return text;
            }
        }

        let text = String::from_utf8_lossy(buffer);
        extract_text_from_json(&text)
    }

    fn parse_response(&self, buffer: &[u8]) -> Option<UsageInfo> {
        let json_objects = byte_utils::extract_h2_json_all(buffer);
        if json_objects.is_empty() {
            return None;
        }

        for payload in &json_objects {
            let s = String::from_utf8_lossy(payload);

            // Skip request-shaped objects (no usage data)
            if (s.contains("\"messages\"")
                || s.contains("\"contents\"")
                || s.contains("\"prompt\""))
                && !s.contains("\"usage\"")
                && !s.contains("\"usageMetadata\"")
            {
                continue;
            }

            let Ok(mut val) = serde_json::from_str::<Value>(&s) else {
                continue;
            };
            if is_responses_event(&val) {
                val = val["response"].take();
            }
            if let Some(info) = parse_response_json_value(&val) {
                // A Gemini stream chunk before the last has no candidatesTokenCount yet; it is not a 0-token answer.
                let gemini_partial = info.completion_tokens == 0
                    && s.contains("\"usageMetadata\"")
                    && !s.contains("\"candidatesTokenCount\"")
                    && !s.contains("\"finishReason\"")
                    && !s.contains("\"blockReason\"");
                if !gemini_partial {
                    return Some(info);
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::Http11Parser;
    use super::super::test_support::{GEMINI_ZERO_USAGE, RESPONSES_CREATED, tokens};
    use super::*;

    #[test]
    fn h2_gemini_stream_waits_for_the_last_chunk() {
        let first = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"h\"}]}}],\"usageMetadata\":{\"promptTokenCount\":7}}\r\n\r\n";
        assert_eq!(tokens(Http2Parser.parse_response(first.as_bytes())), None);

        let last = format!(
            "{first}data: {{\"candidates\":[{{\"content\":{{\"role\":\"model\"}},\"finishReason\":\"MAX_TOKENS\"}}],{GEMINI_ZERO_USAGE}}}\r\n\r\n"
        );
        assert_eq!(
            tokens(Http2Parser.parse_response(last.as_bytes())),
            Some((1, 0))
        );
    }

    #[test]
    fn h2_responses_sse_reads_nested_usage() {
        let body = format!(
            "{RESPONSES_CREATED}event: response.completed\n\
             data: {{\"type\":\"response.completed\",\"response\":{{\"model\":\"gpt-6\",\"status\":\"completed\",\"error\":null,\
             \"usage\":{{\"input_tokens\":30,\"output_tokens\":9}}}}}}\n\n"
        );
        let info = Http2Parser.parse_response(body.as_bytes());
        assert_eq!(tokens(info), Some((30, 9)));
    }

    #[test]
    fn only_http2_writes_during_response() {
        assert!(Http2Parser.writes_during_response());
        assert!(!Http11Parser.writes_during_response());
    }
}
