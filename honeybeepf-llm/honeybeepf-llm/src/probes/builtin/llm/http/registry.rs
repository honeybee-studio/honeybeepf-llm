//! Provider registry loaded from config and shared by the HTTP parsers.

use once_cell::sync::Lazy;

use honeybeepf_llm_core::providers::{
    ConfigurableProvider, ProviderRegistry, host_matches, path_matches,
};

/// Cached providers - built once at initialization
static CACHED_PROVIDERS: Lazy<Vec<ConfigurableProvider>> = Lazy::new(|| {
    PROVIDER_REGISTRY
        .providers
        .iter()
        .cloned()
        .map(ConfigurableProvider::new)
        .collect()
});

/// Providers from LLM_PROVIDERS_CONFIG_FILE or LLM_PROVIDERS_CONFIG, loaded once.
static CONFIGURED: Lazy<Option<ProviderRegistry>> = Lazy::new(load_configured);

/// Global provider registry - for host/path matching only
pub(super) static PROVIDER_REGISTRY: Lazy<ProviderRegistry> =
    Lazy::new(|| merged(CONFIGURED.clone()));

/// Distinct provider hosts: the HTTP/2 pre-check scans the whole buffer once per entry.
pub(super) static KNOWN_HOSTS: Lazy<Vec<&'static str>> = Lazy::new(|| {
    let mut hosts: Vec<&'static str> = Vec::new();
    for pattern in PROVIDER_REGISTRY.providers.iter().flat_map(|p| &p.hosts) {
        // A `*` pattern is pre-checked by its first literal part (`aiplatform.`, not `.rep.googleapis.com`).
        let host = pattern
            .split('*')
            .find(|s| !s.is_empty())
            .unwrap_or(pattern.as_str());
        if !host.is_empty() && !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    hosts
});

fn load_configured() -> Option<ProviderRegistry> {
    let file = std::env::var("LLM_PROVIDERS_CONFIG_FILE").ok();
    let inline = std::env::var("LLM_PROVIDERS_CONFIG").ok();
    configured_from(file.as_deref(), inline.as_deref())
}

/// The configured providers, from the file if it loads, else from the inline JSON.
fn configured_from(file: Option<&str>, inline: Option<&str>) -> Option<ProviderRegistry> {
    // 1. Try loading from config file (supports YAML, JSON, TOML via config crate)
    if let Some(path) = file {
        if let Ok(settings) = config::Config::builder()
            .add_source(config::File::with_name(path))
            .build()
            && let Ok(reg) = settings.try_deserialize::<ProviderRegistry>()
        {
            log::info!("Loaded LLM providers from: {}", path);
            return Some(reg);
        }
        log::warn!("Failed to load or parse config file: {}", path);
    }

    // 2. Try inline JSON (for Kubernetes ConfigMap)
    let config = inline.filter(|config| !config.is_empty())?;
    match ProviderRegistry::from_json(config) {
        Ok(reg) => Some(reg),
        Err(e) => {
            log::warn!("Ignoring LLM_PROVIDERS_CONFIG, built-in providers only: {e}");
            None
        }
    }
}

/// Built-in providers plus the configured ones.
fn merged(configured: Option<ProviderRegistry>) -> ProviderRegistry {
    let defaults = ProviderRegistry::with_defaults();
    match configured {
        Some(reg) => defaults.merge(reg),
        None => defaults,
    }
}

#[cfg(test)]
fn registry_from(file: Option<&str>, inline: Option<&str>) -> ProviderRegistry {
    merged(configured_from(file, inline))
}

/// Configured entries that change what is recorded without saying so in the provider list.
#[derive(Debug, Default, PartialEq)]
struct ConfigReview {
    /// Built-in names a configured entry replaces, dropping the built-in hosts and paths of that name.
    replaced_builtins: Vec<String>,
    /// Configured names with an entry that matches unrelated hosts, such as no hosts, `""`, `*` or `.`.
    every_host: Vec<String>,
}

/// A provider host and a bare IP: a pattern matching both is not about any one service.
const UNRELATED_HOSTS: [&str; 2] = ["api.anthropic.com", "198.51.100.7:443"];

fn review(defaults: &ProviderRegistry, configured: &ProviderRegistry) -> ConfigReview {
    let mut out = ConfigReview::default();
    for p in &configured.providers {
        let name = &p.name;
        if defaults.providers.iter().any(|d| &d.name == name)
            && !out.replaced_builtins.contains(name)
        {
            out.replaced_builtins.push(name.clone());
        }
        // Asks the real matcher, so the check cannot drift from what the entry actually matches.
        let every_host = p.hosts.is_empty()
            || p.hosts
                .iter()
                .any(|h| UNRELATED_HOSTS.iter().all(|host| host_matches(host, h)));
        if every_host && !out.every_host.contains(name) {
            out.every_host.push(name.clone());
        }
    }
    out
}

/// Distinct provider names in match order.
fn provider_names(registry: &ProviderRegistry) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::new();
    for p in &registry.providers {
        if !names.contains(&p.name.as_str()) {
            names.push(&p.name);
        }
    }
    names
}

/// FNV-1a over every merged provider field, so it also changes when an upgrade changes the built-ins.
fn config_hash(registry: &ProviderRegistry) -> String {
    let text = format!("{:?}", registry.providers);
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// The startup INFO line and one warning per risky configured name.
fn config_messages(
    names: &[&str],
    entries: usize,
    hash: &str,
    notes: &ConfigReview,
) -> (String, Vec<String>) {
    // Names come from the ConfigMap: Debug-escape them so a newline cannot forge a log line.
    let info = format!(
        "LLM providers: {} {names:?}, {entries} entries, config hash {hash}",
        names.len()
    );
    let replaced = notes.replaced_builtins.iter().map(|name| {
        format!(
            "Configured LLM provider {name:?} replaces the built-in one: only the configured hosts and paths are recorded as {name:?}"
        )
    });
    let every_host = notes.every_host.iter().map(|name| {
        format!(
            "Configured LLM provider {name:?} has an entry matching any host: it claims HTTP/1.1 calls to unrelated hosts on its paths and keeps the connection filter from skipping non-LLM connections"
        )
    });
    (info, replaced.chain(every_host).collect())
}

/// Logs and exports the provider set the probe runs with, and warns about risky configured entries.
pub fn report_provider_config() {
    let registry = &*PROVIDER_REGISTRY;
    let names = provider_names(registry);
    let hash = config_hash(registry);
    let notes = CONFIGURED
        .as_ref()
        .map(|c| review(&ProviderRegistry::with_defaults(), c))
        .unwrap_or_default();

    let (info, warnings) = config_messages(&names, registry.providers.len(), &hash, &notes);
    log::info!("{info}");
    for warning in warnings {
        log::warn!("{warning}");
    }
    crate::telemetry::record_provider_config(
        &hash,
        &names.join(","),
        names.len(),
        &notes.replaced_builtins.join(","),
    );
}

/// Get cached providers (no allocation per call)
pub(super) fn get_providers() -> &'static [ConfigurableProvider] {
    &CACHED_PROVIDERS
}

pub(super) fn is_llm_path(path: &str) -> bool {
    PROVIDER_REGISTRY
        .providers
        .iter()
        .any(|config| config.paths.iter().any(|p| path_matches(path, p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hosts_are_distinct() {
        let hosts = &*KNOWN_HOSTS;
        assert!(hosts.contains(&"api.openai.com"));
        assert!(
            hosts
                .iter()
                .enumerate()
                .all(|(i, h)| !hosts[..i].contains(h))
        );
        // Wildcard hosts are pre-checked by their literal part.
        assert!(hosts.contains(&"bedrock-mantle.") && !hosts.contains(&".rep.googleapis.com"));
        assert!(hosts.iter().all(|h| !h.contains('*') && !h.is_empty()));
    }

    #[test]
    fn configured_providers_keep_the_builtins() -> std::io::Result<()> {
        let ollama = r#"{"providers":[{"name":"ollama","hosts":["ollama.internal"],"paths":["/api/chat$"]}]}"#;
        let path = std::env::temp_dir().join("hb-test-providers-keep-builtins.json");
        std::fs::write(&path, ollama)?;
        for registry in [
            registry_from(None, Some(ollama)),
            registry_from(path.to_str(), None),
        ] {
            let found =
                |host: &str, url: &str| registry.find_provider(host, url).map(|p| p.name.clone());
            assert_eq!(
                found("api.openai.com", "/v1/chat/completions").as_deref(),
                Some("openai")
            );
            assert_eq!(
                found("ollama.internal", "/api/chat").as_deref(),
                Some("ollama")
            );
        }
        std::fs::remove_file(path)
    }

    fn configured(json: &str) -> ProviderRegistry {
        configured_from(None, Some(json)).expect("test config parses")
    }

    #[test]
    fn review_flags_replaced_builtins_and_entries_for_every_host() {
        let defaults = ProviderRegistry::with_defaults();
        let cases: [(&str, &[&str], &[&str]); 7] = [
            (
                r#"{"providers":[{"name":"ollama","hosts":["ollama.internal"],"paths":["/api/chat$"]}]}"#,
                &[],
                &[],
            ),
            // Same name as a built-in, pointed at a host nobody calls: OpenAI calls stop being recorded.
            (
                r#"{"providers":[{"name":"openai","hosts":["nowhere.invalid"]}]}"#,
                &["openai"],
                &[],
            ),
            (
                r#"{"providers":[{"name":"openai","hosts":["a"]},{"name":"openai","hosts":["b"]}]}"#,
                &["openai"],
                &[],
            ),
            (
                r#"{"providers":[{"name":"vllm","paths":["/v1/chat$"]}]}"#,
                &[],
                &["vllm"],
            ),
            (
                r#"{"providers":[{"name":"vllm","hosts":["vllm.internal",""]},{"name":"x","hosts":["*"]}]}"#,
                &[],
                &["vllm", "x"],
            ),
            // Case differs, so it is a new provider, not a replacement.
            (
                r#"{"providers":[{"name":"OpenAI","hosts":["*.internal"]}]}"#,
                &[],
                &[],
            ),
            // `.` is a substring of nearly every host; `**` matches none (one `*` splits, the rest is literal).
            (
                r#"{"providers":[{"name":"openai","hosts":["."]},{"name":"y","hosts":["**"]}]}"#,
                &["openai"],
                &["openai"],
            ),
        ];
        for (json, replaced, every_host) in cases {
            let got = review(&defaults, &configured(json));
            assert_eq!(got.replaced_builtins, replaced, "{json}");
            assert_eq!(got.every_host, every_host, "{json}");
        }
    }

    #[test]
    fn config_messages_cannot_be_split_by_a_provider_name() {
        let json = r#"{"providers":[{"name":"openai\nWARN forged line","hosts":[]},{"name":"openai","hosts":["x"]}]}"#;
        let configured = configured(json);
        let registry = merged(Some(configured.clone()));
        let notes = review(&ProviderRegistry::with_defaults(), &configured);
        let (info, warnings) = config_messages(
            &provider_names(&registry),
            registry.providers.len(),
            &config_hash(&registry),
            &notes,
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        for line in std::iter::once(&info).chain(&warnings) {
            assert!(!line.contains('\n'), "{line}");
        }
        assert!(info.contains(r#""openai\nWARN forged line""#), "{info}");
        assert!(warnings[0].contains(r#"provider "openai" replaces"#));
    }

    #[test]
    fn config_hash_follows_every_provider_field() {
        let builtin = config_hash(&merged(None));
        assert_eq!(builtin, config_hash(&merged(None)));
        assert_eq!(builtin.len(), 16);

        let ollama = r#"{"providers":[{"name":"ollama","hosts":["ollama.internal"],"paths":["/api/chat$"]}]}"#;
        let with_ollama = config_hash(&merged(Some(configured(ollama))));
        assert_ne!(with_ollama, builtin);
        let other_path = config_hash(&merged(Some(configured(
            &ollama.replace("/api/chat$", "/api/generate$"),
        ))));
        assert_ne!(other_path, with_ollama);
        let other_usage = r#"{"providers":[{"name":"ollama","hosts":["ollama.internal"],"paths":["/api/chat$"],"response":{"prompt_tokens":"prompt_eval_count"}}]}"#;
        assert_ne!(
            config_hash(&merged(Some(configured(other_usage)))),
            with_ollama
        );
    }

    #[test]
    fn provider_names_are_distinct_in_match_order() {
        let replaced = configured(
            r#"{"providers":[{"name":"openai","hosts":["x"]},{"name":"ollama","hosts":["o"]}]}"#,
        );
        let registry = merged(Some(replaced));
        let names = provider_names(&registry);
        // Built-in openai has two entries (chat, responses); the replacement takes the first one's place.
        assert_eq!(names.first(), Some(&"openai"));
        assert_eq!(names.last(), Some(&"ollama"));
        assert!(
            names
                .iter()
                .enumerate()
                .all(|(i, n)| !names[..i].contains(n))
        );
        assert_eq!(
            registry
                .providers
                .iter()
                .filter(|p| p.name == "openai")
                .count(),
            1
        );
    }

    // LLM_PROVIDERS_CONFIG_FILE goes through the config crate, which also turns `yes` and `1` into booleans.
    #[test]
    fn config_file_reads_flat_response_fields() -> Result<(), config::ConfigError> {
        let yaml = "providers:\n  - name: proxy\n    response:\n      usage_path: meta.usage\n      \
                    prompt_tokens: in\n      cached_tokens: hit\n      prompt_excludes_cache: yes\n      \
                    completion_optional: 1\n      model_path: served.model\n      speed: mode\n";
        let registry = config::Config::builder()
            .add_source(config::File::from_str(yaml, config::FileFormat::Yaml))
            .build()
            .and_then(|c| c.try_deserialize::<ProviderRegistry>())?;
        let provider = ConfigurableProvider::new(registry.providers[0].clone());
        let body = serde_json::json!({"served": {"model": "m-1"},
            "meta": {"usage": {"in": 10, "hit": 3, "mode": "fast"}}});
        let Some(usage) = provider.parse_usage(&body) else {
            panic!("no usage read from {body}");
        };
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (13, 0));
        assert_eq!(usage.cache_read_tokens, Some(3));
        assert_eq!(usage.model.as_deref(), Some("m-1"));
        assert_eq!(usage.speed.as_deref(), Some("fast"));
        Ok(())
    }
}
