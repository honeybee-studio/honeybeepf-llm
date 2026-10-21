pub use honeybeepf_llm_common::LlmDirection;
use serde::Deserialize;
use serde_json::Value;

/// Parsed usage info from an LLM response
pub struct UsageInfo {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub thoughts_tokens: Option<u64>,
    pub model: Option<String>,
}

/// Lightweight struct for SSE chunk detection (only checks if usage field exists)
#[derive(Deserialize, Default)]
pub struct SseChunkDelta {
    #[serde(alias = "usageMetadata")]
    pub usage: Option<Value>,
    /// Anthropic `message_start` nests the response, with its usage, under `message`.
    pub message: Option<Value>,
}

impl SseChunkDelta {
    /// Whether this event carries usage at the top level or under `message`.
    pub fn has_usage(&self) -> bool {
        self.usage.is_some()
            || self
                .message
                .as_ref()
                .is_some_and(|m| m.get("usage").is_some())
    }
}
