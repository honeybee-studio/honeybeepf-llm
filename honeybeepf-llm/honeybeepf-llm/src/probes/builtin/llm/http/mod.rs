//! HTTP parsing for LLM API traffic
//!
//! This module handles HTTP/1.1 and HTTP/2 parsing to extract
//! LLM request/response data.

mod body;
#[cfg(test)]
mod chunk_cases;
mod encoding;
mod eventstream;
mod h1;
mod h2;
mod json_ends;
mod provider_report;
mod registry;
mod sse;

// Re-export main types
pub use h1::{Http11Parser, ResponseProgress};
pub use h2::Http2Parser;
pub use json_ends::JsonEnds;
pub use provider_report::report_provider_config;

use honeybeepf_llm_core::types::UsageInfo;

/// Protocol-specific parser interface
pub trait ProtocolParser: Send + Sync {
    /// Detect if this buffer is an LLM request. Returns detected path/info if yes.
    /// Uses the global PROVIDER_REGISTRY for host/path matching.
    fn detect_request(&self, buffer: &[u8]) -> Option<String>;

    /// Extract request text for token estimation
    fn extract_request_text(&self, buffer: &[u8]) -> String;

    /// Parse response buffer. Returns UsageInfo if complete.
    fn parse_response(&self, buffer: &[u8]) -> Option<UsageInfo>;

    /// Parse a response buffer that only grew since the last call with the same progress.
    fn parse_response_from(
        &self,
        buffer: &[u8],
        _progress: &mut ResponseProgress,
    ) -> Option<UsageInfo> {
        self.parse_response(buffer)
    }

    /// Whether the client writes while a response is still arriving (HTTP/2 flow control, pings).
    fn writes_during_response(&self) -> bool {
        false
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(super) fn tokens(info: Option<UsageInfo>) -> Option<(u64, u64)> {
        info.map(|u| (u.prompt_tokens, u.completion_tokens))
    }

    /// One AWS event stream message with string headers, framed with both CRCs.
    pub(crate) fn es_message(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let crc = |bytes: &[u8]| {
            let mut crc = flate2::Crc::new();
            crc.update(bytes);
            crc.sum().to_be_bytes()
        };
        let mut packed = Vec::new();
        for (name, value) in headers {
            packed.push(name.len() as u8);
            packed.extend_from_slice(name.as_bytes());
            packed.push(7);
            packed.extend_from_slice(&(value.len() as u16).to_be_bytes());
            packed.extend_from_slice(value.as_bytes());
        }
        let total = 12 + packed.len() + payload.len() + 4;
        let mut msg = (total as u32).to_be_bytes().to_vec();
        msg.extend_from_slice(&(packed.len() as u32).to_be_bytes());
        msg.extend_from_slice(&crc(&msg));
        msg.extend_from_slice(&packed);
        msg.extend_from_slice(payload);
        msg.extend_from_slice(&crc(&msg));
        msg
    }

    /// A Bedrock event message with a JSON payload.
    pub(crate) fn es_event(event_type: &str, payload: &str) -> Vec<u8> {
        let headers = [
            (":event-type", event_type),
            (":content-type", "application/json"),
            (":message-type", "event"),
        ];
        es_message(&headers, payload.as_bytes())
    }

    /// Usage Gemini sends when nothing was generated: no `candidatesTokenCount`.
    pub(super) const GEMINI_ZERO_USAGE: &str = "\"usageMetadata\":{\"promptTokenCount\":1,\"totalTokenCount\":1,\
        \"promptTokensDetails\":[{\"modality\":\"TEXT\",\"tokenCount\":1}],\"serviceTier\":\"standard\"}";

    pub(super) const RESPONSES_CREATED: &str = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"model\":\"gpt-6\",\"status\":\"in_progress\",\"error\":null,\"usage\":null}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
    );
}
