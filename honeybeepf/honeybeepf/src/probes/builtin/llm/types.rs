pub use honeybeepf_common::LlmDirection;
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
    pub usage: Option<Value>,
}
