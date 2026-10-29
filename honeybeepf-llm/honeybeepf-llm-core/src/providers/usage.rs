use serde_json::Value;

use super::config::{
    CacheConfig, IdentityConfig, ProviderConfig, TokenConfig, host_matches, path_matches,
};
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
            self.config.hosts.is_empty() || self.config.hosts.iter().any(|h| host_matches(host, h));
        let path_match =
            self.config.paths.is_empty() || self.config.paths.iter().any(|p| path_matches(path, p));
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
        let response = &self.config.response;
        let usage = get_nested_value(json, &response.tokens.usage_path)?;
        if !is_own_usage(&response.identity, usage) {
            return None;
        }

        let tokens = read_tokens(&response.tokens, usage)?;
        let cache = read_cache(&response.cache, usage);
        let prompt = if response.cache.prompt_excludes_cache {
            tokens
                .prompt
                .saturating_add(cache.read.unwrap_or(0))
                .saturating_add(cache.write.unwrap_or(0))
        } else {
            tokens.prompt
        };
        let pricing = &response.pricing;

        Some(UsageInfo {
            prompt_tokens: prompt,
            completion_tokens: tokens.completion,
            thoughts_tokens: tokens.thoughts,
            cache_read_tokens: cache.read,
            cache_write_tokens: cache.write,
            cache_write_long_ttl_tokens: cache.write_long_ttl,
            model: text(json, &response.identity.model_path),
            service_tier: optional_text(json, &pricing.service_tier_path),
            speed: optional_text(usage, &pricing.speed),
            inference_geo: optional_text(usage, &pricing.inference_geo),
        })
    }
}

struct Tokens {
    prompt: u64,
    completion: u64,
    thoughts: Option<u64>,
}

struct Cache {
    read: Option<u64>,
    write: Option<u64>,
    write_long_ttl: Option<u64>,
}

fn is_own_usage(config: &IdentityConfig, usage: &Value) -> bool {
    config
        .required_field
        .as_ref()
        .is_none_or(|field| get_nested_value(usage, field).is_some())
}

/// Prompt, completion and reasoning counts; `None` when a required count is missing.
fn read_tokens(config: &TokenConfig, usage: &Value) -> Option<Tokens> {
    let prompt = get_nested_value(usage, &config.prompt_tokens).and_then(|v| v.as_u64())?;
    let completion = match get_nested_value(usage, &config.completion_tokens) {
        Some(v) => v.as_u64()?,
        None if config.completion_optional => 0,
        None => return None,
    };
    let prompt =
        prompt.saturating_add(optional_count(usage, &config.extra_prompt_tokens).unwrap_or(0));
    let thoughts = optional_count(usage, &config.thoughts_tokens);
    let completion = if config.completion_excludes_thoughts {
        completion.saturating_add(thoughts.unwrap_or(0))
    } else {
        completion
    };
    Some(Tokens {
        prompt,
        completion,
        thoughts,
    })
}

fn read_cache(config: &CacheConfig, usage: &Value) -> Cache {
    let write = optional_count(usage, &config.cache_write_tokens);
    // The breakdown and the total can come from different SSE events; keep it inside the total.
    let write_long_ttl = optional_count(usage, &config.cache_write_long_ttl_tokens)
        .zip(write)
        .map(|(long, all)| long.min(all));
    Cache {
        read: optional_count(usage, &config.cache_read_tokens),
        write,
        write_long_ttl,
    }
}

fn optional_count(root: &Value, path: &Option<String>) -> Option<u64> {
    path.as_ref()
        .and_then(|path| get_nested_value(root, path))
        .and_then(|v| v.as_u64())
}

fn text(root: &Value, path: &str) -> Option<String> {
    get_nested_value(root, path)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn optional_text(root: &Value, path: &Option<String>) -> Option<String> {
    path.as_ref().and_then(|path| text(root, path))
}
