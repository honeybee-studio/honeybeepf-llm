use std::io::Read;

use flate2::read::GzDecoder;
use once_cell::sync::Lazy;
use serde_json::Value;

use honeybeepf_llm_core::byte_utils;
use honeybeepf_llm_core::providers::{ConfigurableProvider, ProviderRegistry};
use honeybeepf_llm_core::types::{SseChunkDelta, UsageInfo};

/// Cached providers - built once at initialization
static CACHED_PROVIDERS: Lazy<Vec<ConfigurableProvider>> = Lazy::new(|| {
    let registry = load_provider_registry();
    registry
        .providers
        .into_iter()
        .map(ConfigurableProvider::new)
        .collect()
});

/// Global provider registry - for host/path matching only
static PROVIDER_REGISTRY: Lazy<ProviderRegistry> = Lazy::new(load_provider_registry);

fn load_provider_registry() -> ProviderRegistry {
    // 1. Try loading from config file
    // 1. Try loading from config file (supports YAML, JSON, TOML via config crate)
    if let Ok(path) = std::env::var("LLM_PROVIDERS_CONFIG_FILE") {
        if let Ok(settings) = config::Config::builder()
            .add_source(config::File::with_name(&path))
            .build()
            && let Ok(reg) = settings.try_deserialize::<ProviderRegistry>()
        {
            log::info!("Loaded LLM providers from: {}", path);
            return reg;
        }
        log::warn!("Failed to load or parse config file: {}", path);
    }

    // 2. Try inline JSON (for Kubernetes ConfigMap)
    if let Ok(config) = std::env::var("LLM_PROVIDERS_CONFIG")
        && !config.is_empty()
        && let Ok(reg) = ProviderRegistry::from_json(&config)
    {
        return reg;
    }

    // 3. Use defaults
    ProviderRegistry::with_defaults()
}

/// Get cached providers (no allocation per call)
fn get_providers() -> &'static [ConfigurableProvider] {
    &CACHED_PROVIDERS
}

/// Protocol-specific parser interface
pub trait ProtocolParser: Send + Sync {
    /// Detect if this buffer is an LLM request. Returns detected path/info if yes.
    /// Uses the global PROVIDER_REGISTRY for host/path matching.
    fn detect_request(&self, buffer: &[u8]) -> Option<String>;

    /// Extract request text for token estimation
    fn extract_request_text(&self, buffer: &[u8]) -> String;

    /// Parse response buffer. Returns UsageInfo if complete.
    fn parse_response(&self, buffer: &[u8]) -> Option<UsageInfo>;
}

// --- HTTP/1.1 ---
#[derive(Clone)]
pub struct Http11Parser;

impl ProtocolParser for Http11Parser {
    fn detect_request(&self, buffer: &[u8]) -> Option<String> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);

        if req.parse(buffer).is_ok()
            && let Some(path) = req.path
        {
            let host_header = req
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("Host"))
                .map(|h| String::from_utf8_lossy(h.value).to_string())
                .unwrap_or_default();

            // Check if host+path matches any configured provider
            if PROVIDER_REGISTRY
                .find_provider(&host_header, path)
                .is_some()
            {
                return Some(path.to_string());
            }
        }
        None
    }

    fn extract_request_text(&self, buffer: &[u8]) -> String {
        let body_start = byte_utils::find_pattern(buffer, b"\r\n\r\n")
            .map(|i| i + 4)
            .unwrap_or(0);

        let json_body = String::from_utf8_lossy(&buffer[body_start..]);
        extract_text_from_json(&json_body)
    }

    fn parse_response(&self, buffer: &[u8]) -> Option<UsageInfo> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut resp = httparse::Response::new(&mut headers);

        let body_offset = match resp.parse(buffer) {
            Ok(httparse::Status::Complete(n)) => n,
            _ => return None,
        };

        let is_sse = resp.headers.iter().any(|h| {
            h.name.eq_ignore_ascii_case("Content-Type")
                && String::from_utf8_lossy(h.value).contains("text/event-stream")
        });
        let is_chunked = resp.headers.iter().any(|h| {
            h.name.eq_ignore_ascii_case("Transfer-Encoding")
                && String::from_utf8_lossy(h.value).contains("chunked")
        });
        let is_gzip = resp.headers.iter().any(|h| {
            h.name.eq_ignore_ascii_case("Content-Encoding")
                && String::from_utf8_lossy(h.value).contains("gzip")
        });

        let body = &buffer[body_offset..];

        // Decode chunked transfer encoding first
        let dechunked = if is_chunked {
            byte_utils::decode_chunked_body(body)
        } else {
            std::borrow::Cow::Borrowed(body)
        };

        // SSE events can straddle chunk boundaries, so they are parsed only after dechunking.
        if is_sse {
            // A truncated read desyncs the chunk framing; once the stream has ended, fall back to raw lines.
            return parse_sse_body(&dechunked).or_else(|| {
                if is_chunked && body.ends_with(b"0\r\n\r\n") {
                    parse_sse_body(body)
                } else {
                    None
                }
            });
        }

        // Then decompress gzip if needed
        let decompressed: std::borrow::Cow<'_, [u8]> = if is_gzip {
            match decompress_gzip(&dechunked) {
                Ok(data) => std::borrow::Cow::Owned(data),
                Err(_) => return None,
            }
        } else {
            dechunked
        };

        let trimmed = byte_utils::trim_trailing_whitespace(&decompressed);

        // Streamed JSON arrays (Gemini streamGenerateContent without alt=sse): the first element is not the final usage.
        let is_json_array = trimmed.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'[');
        if is_json_array || !trimmed.ends_with(b"}") {
            return None;
        }

        let s = String::from_utf8_lossy(trimmed);
        let start = s.find('{')?;
        let json_body = &s[start..];

        parse_response_json(json_body)
    }
}

// --- HTTP/2 ---
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
        let has_known_host = PROVIDER_REGISTRY.providers.iter().any(|p| {
            p.hosts
                .iter()
                .any(|h| byte_utils::contains_pattern(buffer, h.as_bytes()))
        });

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

            if let Some(info) = parse_response_json(&s) {
                return Some(info);
            }
        }

        None
    }
}

// --- SSE Parsing ---

/// End-of-stream markers: OpenAI `[DONE]`/`finish_reason`, Anthropic `message_stop`, Gemini `finishReason`.
const SSE_END_MARKERS: [&str; 4] = [
    "data: [DONE]",
    "\"finish_reason\"",
    "\"message_stop\"",
    "\"finishReason\"",
];

fn parse_sse_body(body: &[u8]) -> Option<UsageInfo> {
    let text = String::from_utf8_lossy(body);
    // Only complete lines count: a partial last event can show its end marker before its usage.
    let text = text.rfind('\n').map_or("", |end| &text[..end]);

    if !SSE_END_MARKERS.iter().any(|m| text.contains(m)) {
        return None;
    }

    // Usage-bearing events merged in order: Anthropic sends input tokens in
    // message_start and only output tokens in message_delta.
    let mut acc: Option<Value> = None;

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("data: ") {
            continue;
        }
        let data = &line["data: ".len()..];
        if data == "[DONE]" {
            break;
        }

        let is_usage_event =
            serde_json::from_str::<SseChunkDelta>(data).is_ok_and(|chunk| chunk.has_usage());
        if !is_usage_event {
            continue;
        }
        let Ok(mut val) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if val.get("message").is_some_and(|m| m.get("usage").is_some()) {
            val = val["message"].take();
        }
        match acc.as_mut() {
            Some(prev) => merge_sse_event(prev, val),
            None => acc = Some(val),
        }
    }

    acc.as_ref().and_then(parse_response_json_value)
}

/// Overlay a later SSE event onto the accumulated one, merging nested objects one level deep.
fn merge_sse_event(acc: &mut Value, update: Value) {
    let (Some(acc), Value::Object(update)) = (acc.as_object_mut(), update) else {
        return;
    };
    for (key, value) in update {
        match (acc.get_mut(&key), value) {
            (Some(Value::Object(prev)), Value::Object(next)) => prev.extend(next),
            (Some(slot), value) => *slot = value,
            (None, value) => {
                acc.insert(key, value);
            }
        }
    }
}

// --- Core dispatch to configurable providers ---

/// Try all providers on a response JSON string. Returns first match.
fn parse_response_json(json_str: &str) -> Option<UsageInfo> {
    let val: Value = serde_json::from_str(json_str).ok()?;
    parse_response_json_value(&val)
}

fn parse_response_json_value(val: &Value) -> Option<UsageInfo> {
    // Check for error response
    if val.get("error").is_some() {
        return Some(UsageInfo {
            prompt_tokens: 0,
            completion_tokens: 0,
            thoughts_tokens: None,
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

// --- Helpers ---

/// Decompress gzip data
fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut decoder = GzDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

fn is_llm_path(path: &str) -> bool {
    PROVIDER_REGISTRY
        .providers
        .iter()
        .any(|config| config.paths.iter().any(|p| path.contains(p)))
}

/// Extract user text from request JSON by trying all providers.
fn extract_text_from_json(json: &str) -> String {
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
    use super::*;

    fn http11_sse(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{body}").into_bytes()
    }

    fn tokens(info: Option<UsageInfo>) -> Option<(u64, u64)> {
        info.map(|u| (u.prompt_tokens, u.completion_tokens))
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

    #[test]
    fn anthropic_sse_without_message_stop_is_incomplete() {
        let info = Http11Parser.parse_response(&http11_sse(ANTHROPIC_SSE));
        assert_eq!(tokens(info), None);
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
    fn chunked_sse_usage_event_split_across_chunks() {
        let ev1 = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let ev2a = "data: {\"model\":\"gpt-4o\",\"choices\":[],\"usa";
        let ev2b = "ge\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n\
             {:x}\r\n{ev1}\r\n{:x}\r\n{ev2a}\r\n{:x}\r\n{ev2b}\r\n0\r\n\r\n",
            ev1.len(),
            ev2a.len(),
            ev2b.len(),
        );
        let info = Http11Parser.parse_response(resp.as_bytes());
        assert_eq!(tokens(info), Some((11, 4)));
    }

    #[test]
    fn chunked_sse_with_lost_bytes_falls_back_to_raw_lines() {
        let ev1 = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let ev2 = "data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n";
        // The first chunk claims 40 bytes more than arrived, as after a truncated SSL_read.
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n\
             {:x}\r\n{ev1}\r\n{:x}\r\n{ev2}\r\n0\r\n\r\n",
            ev1.len() + 40,
            ev2.len(),
        );
        let info = Http11Parser.parse_response(resp.as_bytes());
        assert_eq!(tokens(info), Some((11, 4)));
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
    fn json_array_stream_is_not_parsed_from_first_element() {
        let resp = concat!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n",
            "[{\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":9,\"candidatesTokenCount\":1}}",
        );
        let info = Http11Parser.parse_response(resp.as_bytes());
        assert_eq!(tokens(info), None);
    }
}
