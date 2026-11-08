pub use honeybeepf_llm_common::LlmDirection;
use serde::Deserialize;
use serde_json::Value;

/// The kind of call a metric row describes, as OTel `gen_ai.operation.name`; a closed set keeps the
/// `operation` label low-cardinality.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Operation {
    /// Text or chat generation, the Responses API included.
    #[default]
    Chat,
    /// Embedding calls, billed on input tokens only.
    Embeddings,
}

impl Operation {
    /// The `operation` label value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Embeddings => "embeddings",
        }
    }
}

/// Non-text modality token counts for one direction; text is the remainder of the direction's total.
#[derive(Debug, Default, PartialEq, Clone, Copy)]
pub struct ModalityTokens {
    pub audio: u64,
    pub image: u64,
    pub video: u64,
}

impl ModalityTokens {
    /// The non-text tokens, so text is the direction's total minus this.
    pub fn non_text(self) -> u64 {
        self.audio
            .saturating_add(self.image)
            .saturating_add(self.video)
    }

    /// Drop the split when the modalities exceed the direction's total (bad data), so text (the
    /// remainder) is never negative and the modality counts always sum to the total.
    pub fn clamped_to(self, total: u64) -> Self {
        if self.non_text() > total {
            Self::default()
        } else {
            self
        }
    }
}

/// Parsed usage info from an LLM response
#[derive(Debug, Default, PartialEq)]
pub struct UsageInfo {
    /// All input tokens, cache reads and writes included (OTel `gen_ai.usage.input_tokens`).
    pub prompt_tokens: u64,
    /// All output tokens, reasoning included (OTel `gen_ai.usage.output_tokens`).
    pub completion_tokens: u64,
    /// Non-text input modalities (audio/image/video); text is `prompt_tokens` minus these.
    pub input_modalities: ModalityTokens,
    /// Non-text output modalities (audio/image/video); text is `completion_tokens` minus these.
    pub output_modalities: ModalityTokens,
    /// Reasoning tokens, a subset of `completion_tokens`.
    pub thoughts_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    /// Cache writes with the longer, pricier lifetime (Anthropic 1h, OpenAI 12h), a subset of `cache_write_tokens`.
    pub cache_write_long_ttl_tokens: Option<u64>,
    pub model: Option<String>,
    /// Whether this is a generation or an embedding call (the `operation` label).
    pub operation: Operation,
    /// Tier that served the call as the response reports it (OpenAI `default`, `flex`, `priority`).
    pub service_tier: Option<String>,
    /// Speed mode that served the call (Anthropic `standard`, `fast`).
    pub speed: Option<String>,
    /// Where inference ran (Anthropic `global`, `us`).
    pub inference_geo: Option<String>,
    /// Earlier billed attempts of a server-side fallback response, each under its own model;
    /// empty unless the response carried `usage.iterations`.
    pub iterations: Vec<UsageInfo>,
    /// A declined fallback attempt whose refusal category is not billed before output: its tokens
    /// are still recorded, but no cost.
    pub cost_suppressed: bool,
}

/// Lightweight struct for SSE chunk detection (only checks if usage field exists)
#[derive(Deserialize, Default)]
pub struct SseChunkDelta {
    #[serde(alias = "usageMetadata")]
    pub usage: Option<Value>,
    /// Anthropic `message_start` nests the response, with its usage, under `message`.
    pub message: Option<Value>,
    /// OpenAI Responses events nest the response under `response`; usage is null until it ends.
    pub response: Option<Value>,
    /// Gemini Interactions streaming nests the interaction, with its usage, under `interaction`.
    pub interaction: Option<Value>,
}

impl SseChunkDelta {
    /// Whether this event carries usage at the top level, under `message`, `response` or `interaction`.
    pub fn has_usage(&self) -> bool {
        self.usage.is_some()
            || self
                .message
                .as_ref()
                .is_some_and(|m| m.get("usage").is_some())
            || self
                .response
                .as_ref()
                .is_some_and(|r| r.get("usage").is_some_and(|u| !u.is_null()))
            || self
                .interaction
                .as_ref()
                .is_some_and(|i| i.get("usage").is_some_and(|u| !u.is_null()))
    }
}
