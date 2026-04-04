use serde_json::Value;

use super::{
    config::ProviderConfig,
    request::{RequestExtractor, get_extractor},
};
use crate::probes::builtin::llm::types::UsageInfo;

/// A provider instance created from configuration
pub struct ConfigurableProvider {
    config: ProviderConfig,
    extractor: Box<dyn RequestExtractor>,
}

impl ConfigurableProvider {
    pub fn new(config: ProviderConfig) -> Self {
        let extractor = get_extractor(&config.request_extractor);
        Self { config, extractor }
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Check if this provider matches the given host and path
    pub fn matches(&self, host: &str, path: &str) -> bool {
        let host_match =
            self.config.hosts.is_empty() || self.config.hosts.iter().any(|h| host.contains(h));
        let path_match =
            self.config.paths.is_empty() || self.config.paths.iter().any(|p| path.contains(p));
        host_match && path_match
    }

    /// Check if request JSON looks like this provider's format
    pub fn detect_request(&self, json: &Value) -> bool {
        // Try to extract text - if we get something, it's likely a match
        let text = self.extractor.extract(json);
        !text.is_empty()
    }

    /// Extract text from request for token estimation
    pub fn extract_request_text(&self, json: &Value) -> String {
        self.extractor.extract(json)
    }

    /// Parse usage from response JSON using configured paths
    pub fn parse_usage(&self, json: &Value) -> Option<UsageInfo> {
        let response_config = &self.config.response;

        // Get usage object using configured path
        let usage = get_nested_value(json, &response_config.usage_path)?;

        // Extract token counts
        let prompt =
            get_nested_value(usage, &response_config.prompt_tokens).and_then(|v| v.as_u64())?;
        let completion =
            get_nested_value(usage, &response_config.completion_tokens).and_then(|v| v.as_u64())?;

        // Optional: thoughts/reasoning tokens
        let thoughts = response_config
            .thoughts_tokens
            .as_ref()
            .and_then(|path| get_nested_value(usage, path))
            .and_then(|v| v.as_u64());

        // Model name from root
        let model = get_nested_value(json, &response_config.model_path)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Some(UsageInfo {
            prompt_tokens: prompt,
            completion_tokens: completion,
            thoughts_tokens: thoughts,
            model,
        })
    }
}

/// Get a nested value using dot-notation path (e.g., "usage.prompt_tokens")
fn get_nested_value<'a>(json: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = json;
    for key in path.split('.') {
        current = current.get(key)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        super::config::{ProviderConfig, RequestExtractorType, ResponseConfig},
        *,
    };

    fn openai_config() -> ProviderConfig {
        ProviderConfig {
            name: "openai".to_string(),
            hosts: vec!["api.openai.com".to_string()],
            paths: vec!["/chat/completions".to_string()],
            response: ResponseConfig {
                usage_path: "usage".to_string(),
                prompt_tokens: "prompt_tokens".to_string(),
                completion_tokens: "completion_tokens".to_string(),
                thoughts_tokens: None,
                model_path: "model".to_string(),
            },
            request_extractor: RequestExtractorType::Messages,
        }
    }

    fn gemini_config() -> ProviderConfig {
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
        }
    }

    #[test]
    fn test_openai_matching() {
        let provider = ConfigurableProvider::new(openai_config());
        assert!(provider.matches("api.openai.com", "/v1/chat/completions"));
        assert!(!provider.matches("api.anthropic.com", "/v1/messages"));
    }

    #[test]
    fn test_openai_parse_usage() {
        let provider = ConfigurableProvider::new(openai_config());
        let response = json!({
            "id": "chatcmpl-123",
            "model": "gpt-4",
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30
            }
        });

        let usage = provider.parse_usage(&response).unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.model, Some("gpt-4".to_string()));
    }

    #[test]
    fn test_gemini_parse_usage() {
        let provider = ConfigurableProvider::new(gemini_config());
        let response = json!({
            "candidates": [{"content": {"parts": [{"text": "Hello!"}]}}],
            "usageMetadata": {
                "promptTokenCount": 15,
                "candidatesTokenCount": 25,
                "thoughtsTokenCount": 100
            },
            "modelVersion": "gemini-1.5-pro"
        });

        let usage = provider.parse_usage(&response).unwrap();
        assert_eq!(usage.prompt_tokens, 15);
        assert_eq!(usage.completion_tokens, 25);
        assert_eq!(usage.thoughts_tokens, Some(100));
        assert_eq!(usage.model, Some("gemini-1.5-pro".to_string()));
    }

    #[test]
    fn test_extract_request_text() {
        let provider = ConfigurableProvider::new(openai_config());
        let request = json!({
            "model": "gpt-4",
            "messages": [
                {"role": "user", "content": "Hello, world!"}
            ]
        });

        let text = provider.extract_request_text(&request);
        assert_eq!(text, "Hello, world!");
    }

    #[test]
    fn test_nested_path() {
        let json = json!({
            "outer": {
                "inner": {
                    "value": 42
                }
            }
        });

        let value = get_nested_value(&json, "outer.inner.value").unwrap();
        assert_eq!(value.as_u64(), Some(42));
    }
}
