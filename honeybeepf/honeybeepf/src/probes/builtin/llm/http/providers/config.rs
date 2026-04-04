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

    /// Create with default providers (OpenAI, Anthropic, Gemini)
    pub fn with_defaults() -> Self {
        Self {
            providers: vec![
                ProviderConfig {
                    name: "openai".to_string(),
                    hosts: vec!["api.openai.com".to_string()],
                    paths: vec![
                        "/chat/completions".to_string(),
                        "/v1/completions".to_string(),
                    ],
                    response: ResponseConfig {
                        usage_path: "usage".to_string(),
                        prompt_tokens: "prompt_tokens".to_string(),
                        completion_tokens: "completion_tokens".to_string(),
                        thoughts_tokens: Some(
                            "completion_tokens_details.reasoning_tokens".to_string(),
                        ),
                        model_path: "model".to_string(),
                    },
                    request_extractor: RequestExtractorType::Messages,
                },
                ProviderConfig {
                    name: "anthropic".to_string(),
                    hosts: vec!["api.anthropic.com".to_string()],
                    paths: vec!["/v1/messages".to_string()],
                    response: ResponseConfig {
                        usage_path: "usage".to_string(),
                        prompt_tokens: "input_tokens".to_string(),
                        completion_tokens: "output_tokens".to_string(),
                        thoughts_tokens: None,
                        model_path: "model".to_string(),
                    },
                    request_extractor: RequestExtractorType::Messages,
                },
                ProviderConfig {
                    name: "gemini".to_string(),
                    hosts: vec!["generativelanguage.googleapis.com".to_string()],
                    paths: vec!["generateContent".to_string()],
                    response: ResponseConfig {
                        usage_path: "usageMetadata".to_string(),
                        prompt_tokens: "promptTokenCount".to_string(),
                        completion_tokens: "candidatesTokenCount".to_string(),
                        thoughts_tokens: Some("thoughtsTokenCount".to_string()),
                        model_path: "modelVersion".to_string(),
                    },
                    request_extractor: RequestExtractorType::Contents,
                },
            ],
        }
    }

    /// Find matching provider for given host and path
    pub fn find_provider(&self, host: &str, path: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| {
            let host_match = p.hosts.is_empty() || p.hosts.iter().any(|h| host.contains(h));
            let path_match = p.paths.is_empty() || p.paths.iter().any(|pat| path.contains(pat));
            host_match && path_match
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_providers() {
        let registry = ProviderRegistry::with_defaults();
        assert_eq!(registry.providers.len(), 3);
    }

    #[test]
    fn test_find_provider() {
        let registry = ProviderRegistry::with_defaults();

        let openai = registry.find_provider("api.openai.com", "/v1/chat/completions");
        assert!(openai.is_some());
        assert_eq!(openai.unwrap().name, "openai");

        let gemini = registry.find_provider(
            "generativelanguage.googleapis.com",
            "/v1/models/gemini:generateContent",
        );
        assert!(gemini.is_some());
        assert_eq!(gemini.unwrap().name, "gemini");
    }

    #[test]
    fn test_custom_provider_json() {
        let json = r#"{
            "providers": [
                {
                    "name": "my-llm",
                    "hosts": ["llm.internal.com"],
                    "paths": ["/api/generate"],
                    "response": {
                        "usage_path": "meta.usage",
                        "prompt_tokens": "input",
                        "completion_tokens": "output"
                    },
                    "request_extractor": "prompt"
                }
            ]
        }"#;

        let registry = ProviderRegistry::from_json(json).unwrap();
        assert_eq!(registry.providers.len(), 1);
        assert_eq!(registry.providers[0].name, "my-llm");
    }
}
