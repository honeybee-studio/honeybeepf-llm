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
        if let Some(want) = &response.identity.root_object
            && text(json, "object").as_deref() != Some(want)
        {
            return None;
        }
        if let Some(field) = &response.identity.required_root_field
            && get_nested_value(json, field).is_none()
        {
            return None;
        }
        let usage = get_nested_value(json, &response.tokens.usage_path)?;
        if !is_own_usage(&response.identity, usage) {
            return None;
        }

        let mut info = self.read_usage(usage, json)?;

        // Server-side fallback: the served attempt is the final `fallback_message` entry that the
        // top-level usage already counts. Advisor and compaction also use `usage.iterations` but
        // relate to the top level differently, so engage only when that entry is last.
        if let Some(path) = &response.identity.iterations_path
            && let Some(entries) = get_nested_value(usage, path).and_then(Value::as_array)
            && let Some(served) = entries.last()
            && served.get("type").and_then(Value::as_str) == Some("fallback_message")
        {
            // A mid-output streaming fallback names the requested (declined) model at the top level,
            // so take the served model from the fallback_message entry.
            if let Some(model) = text(served, &response.identity.model_path) {
                info.model = Some(model);
            }
            // The earlier entries are the declined attempts, each billed under its own model. The
            // refusal category is read from the `content` fallback block that names the same
            // declined model in `from.model`, not by position: block and attempt counts can differ.
            let categories = fallback_categories(json);
            for entry in &entries[..entries.len() - 1] {
                if let Some(mut attempt) = self.read_usage(entry, entry)
                    && attempt.model.is_some()
                {
                    // Suppress cost only where the provider declares billed_categories: a before-output
                    // decline (no output tokens) in any other category records tokens but no cost; a
                    // mid-output decline always bills.
                    if let Some(billed) = &response.pricing.billed_categories
                        && attempt.completion_tokens == 0
                    {
                        let category = attempt
                            .model
                            .as_deref()
                            .and_then(|model| category_of(&categories, model));
                        attempt.cost_suppressed = !is_billed_category(category, billed);
                    }
                    info.iterations.push(attempt);
                }
            }
        }
        Some(info)
    }

    /// One attempt's usage: `usage` holds the token counts, `root` the model name and service tier.
    fn read_usage(&self, usage: &Value, root: &Value) -> Option<UsageInfo> {
        let response = &self.config.response;
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
            model: text(root, &response.identity.model_path),
            operation: self.config.operation,
            service_tier: optional_text(root, &pricing.service_tier_path),
            speed: optional_text(usage, &pricing.speed),
            inference_geo: optional_text(usage, &pricing.inference_geo),
            iterations: Vec::new(),
            cost_suppressed: false,
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

/// The response's `content` fallback blocks as (declined model, refusal category) pairs, keyed by
/// the block's `from.model`; a block without that model cannot be matched and is dropped.
fn fallback_categories(root: &Value) -> Vec<(String, Option<String>)> {
    let Some(blocks) = root.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("fallback"))
        .filter_map(|b| {
            let from = get_nested_value(b, "from.model").and_then(Value::as_str)?;
            let category = get_nested_value(b, "trigger.category")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some((from.to_string(), category))
        })
        .collect()
}

/// The refusal category recorded for a declined model; `None` when no block named it or it carried none.
fn category_of<'a>(blocks: &'a [(String, Option<String>)], model: &str) -> Option<&'a str> {
    blocks
        .iter()
        .find(|(from, _)| from == model)
        .and_then(|(_, category)| category.as_deref())
}

/// Whether a before-output decline in this category is billed; an unknown category never is.
fn is_billed_category(category: Option<&str>, billed: &[String]) -> bool {
    category.is_some_and(|c| billed.iter().any(|b| b == c))
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
