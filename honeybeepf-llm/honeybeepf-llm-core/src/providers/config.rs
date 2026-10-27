use serde::Deserialize;

/// Built-in providers in the `LLM_PROVIDERS_CONFIG` format; serde skips their `note` fields.
const DEFAULT_PROVIDERS: &str = include_str!("default_providers.json");

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

/// Configuration for parsing usage from response JSON, read from flat keys
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(from = "FlatResponseConfig")]
pub struct ResponseConfig {
    pub tokens: TokenConfig,
    pub cache: CacheConfig,
    pub pricing: PricingConfig,
    pub identity: IdentityConfig,
}

/// Where the prompt, completion and reasoning counts are, and how they add up
#[derive(Debug, Clone)]
pub struct TokenConfig {
    /// JSON path to the usage object (e.g., "usage" or "usageMetadata")
    pub usage_path: String,

    /// Field name for prompt/input tokens within usage object
    pub prompt_tokens: String,

    /// Field name for completion/output tokens within usage object
    pub completion_tokens: String,

    /// Whether a missing `completion_tokens` field means 0 (Gemini leaves it out when nothing was generated)
    pub completion_optional: bool,

    /// Optional: field name for thinking/reasoning tokens
    pub thoughts_tokens: Option<String>,

    /// Whether `completion_tokens` leaves out thinking tokens (Gemini), so the output total adds them back
    pub completion_excludes_thoughts: bool,
}

/// Where the prompt tokens read from and written to the provider cache are
#[derive(Debug, Clone, Default)]
pub struct CacheConfig {
    /// Optional: field name for prompt tokens read from the provider cache
    pub cache_read_tokens: Option<String>,

    /// Optional: field name for prompt tokens written to the provider cache
    pub cache_write_tokens: Option<String>,

    /// Optional: field name for the cache writes with the longer, pricier lifetime
    pub cache_write_long_ttl_tokens: Option<String>,

    /// Whether `prompt_tokens` leaves out cache tokens (Anthropic), so the input total adds them back
    pub prompt_excludes_cache: bool,
}

/// Fields that only feed the price modifiers
#[derive(Debug, Clone, Default)]
pub struct PricingConfig {
    /// Optional: JSON path to the served service tier (from root, e.g., "service_tier")
    pub service_tier_path: Option<String>,

    /// Optional: field name for the speed mode within the usage object
    pub speed: Option<String>,

    /// Optional: field name for the inference geography within the usage object
    pub inference_geo: Option<String>,
}

/// Which usage belongs to this provider and which model served it
#[derive(Debug, Clone)]
pub struct IdentityConfig {
    /// JSON path to model name (from root, e.g., "model" or "modelVersion")
    pub model_path: String,

    /// Optional: usage field that must be present, to tell apart providers sharing token keys
    pub required_field: Option<String>,
}

// Not `#[serde(flatten)]`: it skips the config crate's conversions (`yes` or `1` as a boolean).
// Field order is part of the format: serde also reads `response` as an array in this order.
#[derive(Deserialize)]
struct FlatResponseConfig {
    #[serde(default = "default_usage_path")]
    usage_path: String,
    #[serde(default = "default_prompt_tokens")]
    prompt_tokens: String,
    #[serde(default = "default_completion_tokens")]
    completion_tokens: String,
    #[serde(default)]
    completion_optional: bool,
    thoughts_tokens: Option<String>,
    #[serde(default)]
    completion_excludes_thoughts: bool,
    #[serde(alias = "cached_tokens")]
    cache_read_tokens: Option<String>,
    cache_write_tokens: Option<String>,
    cache_write_long_ttl_tokens: Option<String>,
    speed: Option<String>,
    inference_geo: Option<String>,
    #[serde(default)]
    prompt_excludes_cache: bool,
    required_field: Option<String>,
    #[serde(default = "default_model_path")]
    model_path: String,
    service_tier_path: Option<String>,
}

impl From<FlatResponseConfig> for ResponseConfig {
    fn from(flat: FlatResponseConfig) -> Self {
        Self {
            tokens: TokenConfig {
                usage_path: flat.usage_path,
                prompt_tokens: flat.prompt_tokens,
                completion_tokens: flat.completion_tokens,
                completion_optional: flat.completion_optional,
                thoughts_tokens: flat.thoughts_tokens,
                completion_excludes_thoughts: flat.completion_excludes_thoughts,
            },
            cache: CacheConfig {
                cache_read_tokens: flat.cache_read_tokens,
                cache_write_tokens: flat.cache_write_tokens,
                cache_write_long_ttl_tokens: flat.cache_write_long_ttl_tokens,
                prompt_excludes_cache: flat.prompt_excludes_cache,
            },
            pricing: PricingConfig {
                service_tier_path: flat.service_tier_path,
                speed: flat.speed,
                inference_geo: flat.inference_geo,
            },
            identity: IdentityConfig {
                model_path: flat.model_path,
                required_field: flat.required_field,
            },
        }
    }
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

impl Default for TokenConfig {
    fn default() -> Self {
        Self {
            usage_path: default_usage_path(),
            prompt_tokens: default_prompt_tokens(),
            completion_tokens: default_completion_tokens(),
            completion_optional: false,
            thoughts_tokens: None,
            completion_excludes_thoughts: false,
        }
    }
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            model_path: default_model_path(),
            required_field: None,
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
        // Compiled in and parsed by a test, so an error here is a bug in the repo.
        Self::from_json(DEFAULT_PROVIDERS).unwrap_or_default()
    }

    /// Adds `user` providers; a user provider replaces every one here with the same name.
    pub fn merge(self, user: Self) -> Self {
        let replaced: Vec<String> = user.providers.iter().map(|u| u.name.clone()).collect();
        let mut rest = user.providers;
        let mut providers = Vec::with_capacity(self.providers.len() + rest.len());
        for provider in self.providers {
            if !replaced.contains(&provider.name) {
                providers.push(provider);
                continue;
            }
            // Usage parsing tries providers in order, so replacements take the first replaced one's place.
            let (same, others): (Vec<_>, Vec<_>) =
                rest.into_iter().partition(|u| u.name == provider.name);
            providers.extend(same);
            rest = others;
        }
        providers.extend(rest);
        Self { providers }
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

    /// Whether some provider can match `host`; a provider without hosts matches every host.
    pub fn may_serve_host(&self, host: &str) -> bool {
        self.providers
            .iter()
            .any(|p| p.hosts.is_empty() || p.hosts.iter().any(|h| host_matches(host, h)))
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
