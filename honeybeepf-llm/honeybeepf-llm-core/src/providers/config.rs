use serde::Deserialize;

/// Type of request text extractor to use
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RequestExtractorType {
    /// OpenAI/Anthropic style: messages[].content
    #[default]
    Messages,
    /// Gemini style: contents[].parts[].text
    Contents,
    /// Simple prompt field: prompt
    Prompt,
    /// OpenAI Responses style: input as a string or input[].content
    Input,
    /// No extraction (skip request text estimation)
    None,
}

/// Configuration for parsing usage from response JSON
#[derive(Debug, Clone, Deserialize)]
pub struct ResponseConfig {
    /// JSON path to the usage object (e.g., "usage" or "usageMetadata")
    #[serde(default = "default_usage_path")]
    pub usage_path: String,

    /// Field name for prompt/input tokens within usage object
    #[serde(default = "default_prompt_tokens")]
    pub prompt_tokens: String,

    /// Field name for completion/output tokens within usage object
    #[serde(default = "default_completion_tokens")]
    pub completion_tokens: String,

    /// Optional: field name for thinking/reasoning tokens
    pub thoughts_tokens: Option<String>,

    /// Optional: field name for prompt tokens read from the provider cache
    #[serde(alias = "cached_tokens")]
    pub cache_read_tokens: Option<String>,

    /// Optional: field name for prompt tokens written to the provider cache
    pub cache_write_tokens: Option<String>,

    /// Whether `prompt_tokens` leaves out cache tokens (Anthropic), so the input total adds them back
    #[serde(default)]
    pub prompt_excludes_cache: bool,

    /// Optional: usage field that must be present, to tell apart providers sharing token keys
    pub required_field: Option<String>,

    /// JSON path to model name (from root, e.g., "model" or "modelVersion")
    #[serde(default = "default_model_path")]
    pub model_path: String,
}

fn default_usage_path() -> String {
    "usage".to_string()
}
fn default_prompt_tokens() -> String {
    "prompt_tokens".to_string()
}
fn default_completion_tokens() -> String {
    "completion_tokens".to_string()
}
fn default_model_path() -> String {
    "model".to_string()
}

impl Default for ResponseConfig {
    fn default() -> Self {
        Self {
            usage_path: default_usage_path(),
            prompt_tokens: default_prompt_tokens(),
            completion_tokens: default_completion_tokens(),
            thoughts_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            prompt_excludes_cache: false,
            required_field: None,
            model_path: default_model_path(),
        }
    }
}

/// Configuration for a single LLM provider
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    /// Provider name (for logging/metrics)
    pub name: String,

    /// Host patterns to match (e.g., ["api.openai.com"])
    #[serde(default)]
    pub hosts: Vec<String>,

    /// Path patterns to match (e.g., ["/chat/completions"])
    #[serde(default)]
    pub paths: Vec<String>,

    /// Response parsing configuration
    #[serde(default)]
    pub response: ResponseConfig,

    /// Request text extraction type
    #[serde(default)]
    pub request_extractor: RequestExtractorType,
}

/// Registry of all configured providers
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProviderRegistry {
    pub providers: Vec<ProviderConfig>,
}

impl ProviderRegistry {
    /// Load from JSON string
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Create with default providers (OpenAI, Anthropic, Gemini, Bedrock), including their cloud endpoints
    pub fn with_defaults() -> Self {
        // OpenAI-compatible routes on Azure OpenAI / Foundry and Bedrock return the same JSON as OpenAI.
        let openai_hosts = vec![
            "api.openai.com".to_string(),
            "openai.azure.com".to_string(),
            "services.ai.azure.com".to_string(),
            "bedrock-mantle.*.api.aws".to_string(),
            "bedrock-runtime.*.amazonaws.com".to_string(),
        ];
        let anthropic_response = ResponseConfig {
            usage_path: "usage".to_string(),
            prompt_tokens: "input_tokens".to_string(),
            completion_tokens: "output_tokens".to_string(),
            cache_read_tokens: Some("cache_read_input_tokens".to_string()),
            cache_write_tokens: Some("cache_creation_input_tokens".to_string()),
            prompt_excludes_cache: true,
            model_path: "model".to_string(),
            ..ResponseConfig::default()
        };
        Self {
            providers: vec![
                ProviderConfig {
                    name: "openai".to_string(),
                    hosts: openai_hosts.clone(),
                    // `$` keeps the stored-completion endpoints (/chat/completions/{id}) out.
                    paths: vec![
                        "/chat/completions$".to_string(),
                        "/v1/completions$".to_string(),
                    ],
                    response: ResponseConfig {
                        usage_path: "usage".to_string(),
                        prompt_tokens: "prompt_tokens".to_string(),
                        completion_tokens: "completion_tokens".to_string(),
                        thoughts_tokens: Some(
                            "completion_tokens_details.reasoning_tokens".to_string(),
                        ),
                        cache_read_tokens: Some("prompt_tokens_details.cached_tokens".to_string()),
                        model_path: "model".to_string(),
                        ..ResponseConfig::default()
                    },
                    request_extractor: RequestExtractorType::Messages,
                },
                // Responses API. Usage parsing tries providers in order without a host check and anthropic
                // has the same input/output_tokens keys; only Responses usage carries input_tokens_details.
                // `$` keeps GET /v1/responses/{id} out.
                ProviderConfig {
                    name: "openai".to_string(),
                    hosts: openai_hosts,
                    paths: vec!["/v1/responses$".to_string()],
                    response: ResponseConfig {
                        usage_path: "usage".to_string(),
                        prompt_tokens: "input_tokens".to_string(),
                        completion_tokens: "output_tokens".to_string(),
                        thoughts_tokens: Some("output_tokens_details.reasoning_tokens".to_string()),
                        cache_read_tokens: Some("input_tokens_details.cached_tokens".to_string()),
                        required_field: Some("input_tokens_details".to_string()),
                        model_path: "model".to_string(),
                        ..ResponseConfig::default()
                    },
                    request_extractor: RequestExtractorType::Input,
                },
                ProviderConfig {
                    name: "anthropic".to_string(),
                    hosts: vec![
                        "api.anthropic.com".to_string(),
                        "aws-external-anthropic.*.api.aws".to_string(),
                        "bedrock-mantle.*.api.aws".to_string(),
                    ],
                    // `$` keeps /v1/messages/count_tokens and /v1/messages/batches out.
                    paths: vec!["/v1/messages$".to_string()],
                    response: anthropic_response.clone(),
                    request_extractor: RequestExtractorType::Messages,
                },
                ProviderConfig {
                    name: "gemini".to_string(),
                    hosts: vec![
                        "generativelanguage.googleapis.com".to_string(),
                        "aiplatform.googleapis.com".to_string(),
                        "aiplatform.*.rep.googleapis.com".to_string(),
                    ],
                    // Path matching is case-sensitive: the streaming method is `:streamGenerateContent`.
                    paths: vec![
                        "generateContent".to_string(),
                        "streamGenerateContent".to_string(),
                    ],
                    response: ResponseConfig {
                        usage_path: "usageMetadata".to_string(),
                        prompt_tokens: "promptTokenCount".to_string(),
                        completion_tokens: "candidatesTokenCount".to_string(),
                        thoughts_tokens: Some("thoughtsTokenCount".to_string()),
                        cache_read_tokens: Some("cachedContentTokenCount".to_string()),
                        model_path: "modelVersion".to_string(),
                        ..ResponseConfig::default()
                    },
                    request_extractor: RequestExtractorType::Contents,
                },
                // Claude on Vertex AI: Anthropic body, model in the path. `claude-` keeps count-tokens:rawPredict
                // and custom endpoints out.
                ProviderConfig {
                    name: "anthropic".to_string(),
                    hosts: vec![
                        "aiplatform.googleapis.com".to_string(),
                        "aiplatform.*.rep.googleapis.com".to_string(),
                    ],
                    paths: vec![
                        "/publishers/anthropic/models/claude-*:rawPredict$".to_string(),
                        "/publishers/anthropic/models/claude-*:streamRawPredict$".to_string(),
                    ],
                    response: anthropic_response,
                    request_extractor: RequestExtractorType::Messages,
                },
                // Bedrock Runtime native API. Converse `inputTokens` leaves out cache tokens. InvokeModel returns the
                // model's own body, so only Anthropic models (read by the anthropic entry) are matched. The `-stream`
                // variants are binary eventstream and stay out.
                ProviderConfig {
                    name: "bedrock".to_string(),
                    hosts: vec!["bedrock-runtime.*.amazonaws.com".to_string()],
                    paths: vec!["/converse$".to_string(), "anthropic.*/invoke$".to_string()],
                    response: ResponseConfig {
                        usage_path: "usage".to_string(),
                        prompt_tokens: "inputTokens".to_string(),
                        completion_tokens: "outputTokens".to_string(),
                        cache_read_tokens: Some("cacheReadInputTokens".to_string()),
                        cache_write_tokens: Some("cacheWriteInputTokens".to_string()),
                        prompt_excludes_cache: true,
                        model_path: "model".to_string(),
                        ..ResponseConfig::default()
                    },
                    request_extractor: RequestExtractorType::Messages,
                },
            ],
        }
    }

    /// Find matching provider for given host and path
    pub fn find_provider(&self, host: &str, path: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| {
            let host_match = p.hosts.is_empty() || p.hosts.iter().any(|h| host_matches(host, h));
            let path_match =
                p.paths.is_empty() || p.paths.iter().any(|pat| path_matches(path, pat));
            host_match && path_match
        })
    }
}

/// Substring match; a pattern with one `*` must cover the whole host (port ignored), so regional hosts like
/// `bedrock-mantle.*.api.aws` don't widen to every `api.aws` service.
pub fn host_matches(host: &str, pattern: &str) -> bool {
    match pattern.split_once('*') {
        Some((head, tail)) => {
            let host = host.split_once(':').map_or(host, |(h, _)| h);
            host.len() >= head.len() + tail.len() && host.starts_with(head) && host.ends_with(tail)
        }
        None => host.contains(pattern),
    }
}

/// Substring match; a trailing `$` anchors the pattern to the path end, ignoring the query and one trailing `/`.
pub fn path_matches(path: &str, pattern: &str) -> bool {
    match pattern.strip_suffix('$') {
        Some(suffix) => {
            let path = path.split_once('?').map_or(path, |(p, _)| p);
            ends_with_pattern(path, suffix)
                || path
                    .strip_suffix('/')
                    .is_some_and(|p| ends_with_pattern(p, suffix))
        }
        None => path.contains(pattern),
    }
}

/// Whether `text` ends with `pattern`, where one `*` stands for any run of characters.
fn ends_with_pattern(text: &str, pattern: &str) -> bool {
    match pattern.split_once('*') {
        Some((head, tail)) => text
            .strip_suffix(tail)
            .is_some_and(|rest| rest.contains(head)),
        None => text.ends_with(pattern),
    }
}
