//! JSON bodies: usage through the provider list, and request text for token estimation.

use serde_json::Value;

use honeybeepf_llm_core::types::UsageInfo;

use super::registry::get_providers;

/// OpenAI Responses events nest the response, with its usage and status, under a `response` object.
pub(super) fn is_responses_event(val: &Value) -> bool {
    val.get("usage").is_none() && val.get("response").is_some_and(Value::is_object)
}

/// Try all providers on a response JSON string. Returns first match.
pub(super) fn parse_response_json(json_str: &str) -> Option<UsageInfo> {
    let val: Value = serde_json::from_str(json_str).ok()?;
    parse_response_json_value(&val)
}

pub(super) fn parse_response_json_value(val: &Value) -> Option<UsageInfo> {
    // Responses API bodies always carry `error` (null on success) and mark failure in `status`.
    let is_error = val.get("error").is_some_and(|e| !e.is_null())
        || val.get("status").and_then(Value::as_str) == Some("failed");
    if is_error {
        return Some(UsageInfo {
            prompt_tokens: 0,
            completion_tokens: 0,
            thoughts_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            model: None,
        });
    }

    for provider in get_providers() {
        if let Some(info) = provider.parse_usage(val) {
            return Some(info);
        }
    }
    None
}

/// Extract user text from request JSON by trying all providers.
pub(super) fn extract_text_from_json(json: &str) -> String {
    // Try parsing as complete JSON first
    if let Ok(val) = serde_json::from_str::<Value>(json) {
        for provider in get_providers() {
            if provider.detect_request(&val) {
                let text = provider.extract_request_text(&val);
                if !text.is_empty() {
                    return text;
                }
            }
        }

        // Fallback: try all providers' extract even without detection
        for provider in get_providers() {
            let text = provider.extract_request_text(&val);
            if !text.is_empty() {
                return text;
            }
        }
    }

    // Fallback for incomplete/truncated JSON: extract raw text content
    extract_text_from_incomplete_json(json)
}

/// Extract text from incomplete JSON by finding "text" or "content" field values.
/// Used when the request buffer was truncated and full JSON parsing fails.
fn extract_text_from_incomplete_json(json: &str) -> String {
    let mut texts = Vec::new();

    for pattern in [r#""text":"#, r#""content":"#] {
        let mut pos = 0;
        while let Some(idx) = json[pos..].find(pattern) {
            let start = pos + idx + pattern.len();
            let rest = json[start..].trim_start();

            if rest.starts_with('"')
                && let Some(text) = extract_json_string(&rest[1..])
                && text.len() > 10
            {
                texts.push(text);
            }
            pos = start;
        }
    }

    texts.join(" ")
}

/// Extract a JSON string value, handling basic escape sequences.
fn extract_json_string(s: &str) -> Option<String> {
    let mut result = String::new();
    let mut escape = false;

    for c in s.chars() {
        if escape {
            match c {
                'n' => result.push('\n'),
                't' => result.push('\t'),
                '"' => result.push('"'),
                '\\' => result.push('\\'),
                _ => result.push(c),
            }
            escape = false;
        } else if c == '\\' {
            escape = true;
        } else if c == '"' {
            return Some(result);
        } else {
            result.push(c);
        }
    }

    // Truncated string - return what we have
    if !result.is_empty() {
        Some(result)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{GEMINI_ZERO_USAGE, tokens};
    use super::super::{Http11Parser, ProtocolParser};

    #[test]
    fn anthropic_json_input_total_includes_cache() {
        let resp = concat!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n",
            "{\"id\":\"m1\",\"type\":\"message\",\"model\":\"claude-test\",\"content\":[],",
            "\"usage\":{\"input_tokens\":50,\"cache_creation_input_tokens\":2000,",
            "\"cache_read_input_tokens\":100000,\"output_tokens\":300}}",
        );
        let info = Http11Parser.parse_response(resp.as_bytes());
        assert_eq!(tokens(info), Some((102_050, 300)));
    }

    #[test]
    fn gemini_json_zero_output_is_recorded() {
        for finish in ["MAX_TOKENS", "STOP"] {
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n\
                 {{\"candidates\":[{{\"content\":{{\"role\":\"model\"}},\"finishReason\":\"{finish}\",\"index\":0}}],\
                 {GEMINI_ZERO_USAGE},\"modelVersion\":\"gemini-test\"}}"
            );
            let info = Http11Parser.parse_response(resp.as_bytes());
            assert_eq!(tokens(info), Some((1, 0)), "{finish}");
        }
    }

    #[test]
    fn vertex_json_thoughts_without_candidates_is_recorded() {
        // Recorded Vertex AI body (enum-encoding=int): thinking used the whole output budget.
        let resp = concat!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n",
            "{\"candidates\":[{\"content\":{\"role\":\"model\"},\"finishReason\":2}],",
            "\"usageMetadata\":{\"promptTokenCount\":5,\"totalTokenCount\":9,\"trafficType\":1,",
            "\"promptTokensDetails\":[{\"modality\":1,\"tokenCount\":5}],\"thoughtsTokenCount\":4},",
            "\"modelVersion\":\"gemini-2.5-pro\"}",
        );
        let info = Http11Parser.parse_response(resp.as_bytes()).expect("usage");
        assert_eq!((info.prompt_tokens, info.completion_tokens), (5, 0));
        assert_eq!(info.thoughts_tokens, Some(4));
    }

    #[test]
    fn gemini_json_without_usage_metadata_is_not_recorded() {
        let resp = concat!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n",
            "{\"candidates\":[{\"content\":{\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"modelVersion\":\"gemini-test\"}",
        );
        assert_eq!(tokens(Http11Parser.parse_response(resp.as_bytes())), None);
    }

    #[test]
    fn responses_json_with_null_error_reads_usage() {
        let resp = concat!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n",
            "{\"id\":\"r1\",\"object\":\"response\",\"model\":\"gpt-6\",\"status\":\"completed\",\"error\":null,",
            "\"usage\":{\"input_tokens\":30,\"input_tokens_details\":{\"cached_tokens\":0},",
            "\"output_tokens\":9,\"output_tokens_details\":{\"reasoning_tokens\":2},\"total_tokens\":39}}",
        );
        let info = Http11Parser.parse_response(resp.as_bytes()).expect("usage");
        assert_eq!((info.prompt_tokens, info.completion_tokens), (30, 9));
        assert_eq!(info.thoughts_tokens, Some(2));
    }

    #[test]
    fn bedrock_converse_json_reads_camel_case_usage() {
        let resp = concat!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n",
            "{\"output\":{\"message\":{\"role\":\"assistant\",\"content\":[{\"text\":\"hi\"}]}},\"stopReason\":\"end_turn\",",
            "\"usage\":{\"inputTokens\":30,\"outputTokens\":628,\"totalTokens\":658},\"metrics\":{\"latencyMs\":1275}}",
        );
        let info = Http11Parser.parse_response(resp.as_bytes());
        assert_eq!(tokens(info), Some((30, 628)));
    }
}
