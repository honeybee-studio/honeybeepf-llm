use std::io::Read;

use flate2::read::GzDecoder;
use once_cell::sync::Lazy;
use serde_json::Value;

use honeybeepf_llm_core::byte_utils;
use honeybeepf_llm_core::providers::{ConfigurableProvider, ProviderRegistry, path_matches};
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

/// Distinct provider hosts: the HTTP/2 pre-check scans the whole buffer once per entry.
static KNOWN_HOSTS: Lazy<Vec<&'static str>> = Lazy::new(|| {
    let mut hosts: Vec<&'static str> = Vec::new();
    for host in PROVIDER_REGISTRY.providers.iter().flat_map(|p| &p.hosts) {
        if !hosts.contains(&host.as_str()) {
            hosts.push(host);
        }
    }
    hosts
});

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

    /// Whether the client writes while a response is still arriving (HTTP/2 flow control, pings).
    fn writes_during_response(&self) -> bool {
        false
    }
}

// --- HTTP/1.1 ---
#[derive(Clone)]
pub struct Http11Parser;

impl Http11Parser {
    /// Whether the buffer starts with a complete HTTP/1.1 request head.
    pub fn is_request_head(buffer: &[u8]) -> bool {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        matches!(req.parse(buffer), Ok(httparse::Status::Complete(_)))
    }

    /// Whether the buffer holds a whole HTTP/1.1 response by its framing, after any 1xx interim responses.
    pub fn is_complete_response(buffer: &[u8]) -> bool {
        let mut rest = buffer;
        loop {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut resp = httparse::Response::new(&mut headers);
            let Ok(httparse::Status::Complete(n)) = resp.parse(rest) else {
                return false;
            };
            let body = &rest[n..];
            if resp.code.is_some_and(|c| (100..200).contains(&c)) {
                rest = body;
                continue;
            }
            let header = |name: &str| {
                resp.headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case(name))
                    .map(|h| h.value)
            };
            if let Some(len) = header("Content-Length")
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|v| v.trim().parse::<usize>().ok())
            {
                return body.len() >= len;
            }
            return header("Transfer-Encoding")
                .is_some_and(|v| byte_utils::contains_pattern(v, b"chunked"))
                && body.ends_with(b"0\r\n\r\n");
        }
    }
}

impl ProtocolParser for Http11Parser {
    fn detect_request(&self, buffer: &[u8]) -> Option<String> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);

        // Generation calls are POST; GET/DELETE on the same paths read or delete stored objects.
        if req.parse(buffer).is_ok()
            && req.method == Some("POST")
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

// --- SSE Parsing ---

/// End-of-stream markers: OpenAI `[DONE]`/`finish_reason`, Anthropic `message_stop`, Gemini `finishReason`
/// or prompt `blockReason`, OpenAI Responses terminal event types.
const SSE_END_MARKERS: [&str; 8] = [
    "data: [DONE]",
    "\"finish_reason\"",
    "\"message_stop\"",
    "\"finishReason\"",
    "\"blockReason\"",
    "\"response.completed\"",
    "\"response.failed\"",
    "\"response.incomplete\"",
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
        match acc.as_mut() {
            Some(prev) => merge_sse_event(prev, val),
            None => acc = Some(val),
        }
    }

    acc.as_ref().and_then(parse_response_json_value)
}

/// OpenAI Responses events nest the response, with its usage and status, under a `response` object.
fn is_responses_event(val: &Value) -> bool {
    val.get("usage").is_none() && val.get("response").is_some_and(Value::is_object)
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

// --- Core dispatch to configurable providers ---

/// Try all providers on a response JSON string. Returns first match.
fn parse_response_json(json_str: &str) -> Option<UsageInfo> {
    let val: Value = serde_json::from_str(json_str).ok()?;
    parse_response_json_value(&val)
}

fn parse_response_json_value(val: &Value) -> Option<UsageInfo> {
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
        .any(|config| config.paths.iter().any(|p| path_matches(path, p)))
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

    fn cache(info: &UsageInfo) -> (Option<u64>, Option<u64>) {
        (info.cache_read_tokens, info.cache_write_tokens)
    }

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

    /// Usage Gemini sends when nothing was generated: no `candidatesTokenCount`.
    const GEMINI_ZERO_USAGE: &str = "\"usageMetadata\":{\"promptTokenCount\":1,\"totalTokenCount\":1,\
        \"promptTokensDetails\":[{\"modality\":\"TEXT\",\"tokenCount\":1}],\"serviceTier\":\"standard\"}";

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
    fn gemini_json_without_usage_metadata_is_not_recorded() {
        let resp = concat!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n",
            "{\"candidates\":[{\"content\":{\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"modelVersion\":\"gemini-test\"}",
        );
        assert_eq!(tokens(Http11Parser.parse_response(resp.as_bytes())), None);
    }

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

    const RESPONSES_CREATED: &str = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"model\":\"gpt-6\",\"status\":\"in_progress\",\"error\":null,\"usage\":null}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
    );

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
    fn sse_string_response_field_keeps_top_level_usage() {
        let body = concat!(
            "data: {\"response\":\"\",\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        );
        let info = Http11Parser.parse_response(&http11_sse(body));
        assert_eq!(tokens(info), Some((5, 3)));
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
    fn responses_create_is_detected_but_retrieval_is_not() {
        let post = b"POST /v1/responses HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{\"input\":\"hi\"}";
        assert_eq!(
            Http11Parser.detect_request(post).as_deref(),
            Some("/v1/responses")
        );
        let get = b"GET /v1/responses/r1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n";
        assert_eq!(Http11Parser.detect_request(get), None);
    }

    #[test]
    fn only_post_to_a_generation_path_is_detected() {
        let post = b"POST /v1/chat/completions HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{}";
        assert!(Http11Parser.detect_request(post).is_some());
        for req in [
            &b"GET /v1/chat/completions?limit=20 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n"[..],
            b"GET /v1/chat/completions/chatcmpl-1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n",
            b"DELETE /v1/chat/completions/chatcmpl-1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n",
            b"POST /v1/chat/completions/chatcmpl-1 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{\"metadata\":{}}",
            b"POST /v1/messages/count_tokens HTTP/1.1\r\nHost: api.anthropic.com\r\n\r\n{}",
            b"GET /v1/messages/batches/msgbatch_01 HTTP/1.1\r\nHost: api.anthropic.com\r\n\r\n",
        ] {
            let line = String::from_utf8_lossy(req);
            assert_eq!(Http11Parser.detect_request(req), None, "{line}");
        }
    }

    #[test]
    fn request_head_needs_a_complete_http11_head() {
        assert!(Http11Parser::is_request_head(
            b"POST /v1/embeddings HTTP/1.1\r\nHost: api.openai.com\r\n\r\n{}"
        ));
        assert!(!Http11Parser::is_request_head(
            b"POST /v1/embeddings HTTP/1.1\r\nHost: api.op"
        ));
        assert!(!Http11Parser::is_request_head(
            b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"
        ));
    }

    #[test]
    fn complete_response_follows_framing_after_interim_responses() {
        let complete: [&[u8]; 3] = [
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 2\r\n\r\nok",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n",
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ];
        for buf in complete {
            assert!(Http11Parser::is_complete_response(buf), "{buf:?}");
        }
        let incomplete: [&[u8]; 4] = [
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nok",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {}\n\n",
            b"HTTP/1.1 100 Continue\r\n\r\n",
        ];
        for buf in incomplete {
            assert!(!Http11Parser::is_complete_response(buf), "{buf:?}");
        }
    }

    #[test]
    fn only_http2_writes_during_response() {
        assert!(Http2Parser.writes_during_response());
        assert!(!Http11Parser.writes_during_response());
    }

    #[test]
    fn known_hosts_are_distinct() {
        let hosts = &*KNOWN_HOSTS;
        assert!(hosts.contains(&"api.openai.com"));
        assert!(
            hosts
                .iter()
                .enumerate()
                .all(|(i, h)| !hosts[..i].contains(h))
        );
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
