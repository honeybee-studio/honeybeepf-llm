use serde_json::Value;

use super::config::ProviderConfig;
use super::request::{RequestExtractor, get_extractor};
use crate::byte_utils::get_nested_value;
use crate::types::UsageInfo;

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
