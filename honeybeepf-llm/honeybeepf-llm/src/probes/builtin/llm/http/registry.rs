//! Provider registry loaded from config and shared by the HTTP parsers.

use once_cell::sync::Lazy;

use honeybeepf_llm_core::providers::{ConfigurableProvider, ProviderRegistry, path_matches};

/// Cached providers - built once at initialization
static CACHED_PROVIDERS: Lazy<Vec<ConfigurableProvider>> = Lazy::new(|| {
    let registry = load_provider_registry();
    registry
        .providers
        .into_iter()
        .map(ConfigurableProvider::new)
        .collect()
});

/// Global provider registry - for host/path matching only
pub(super) static PROVIDER_REGISTRY: Lazy<ProviderRegistry> = Lazy::new(load_provider_registry);

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

fn load_provider_registry() -> ProviderRegistry {
    let file = std::env::var("LLM_PROVIDERS_CONFIG_FILE").ok();
    let inline = std::env::var("LLM_PROVIDERS_CONFIG").ok();
    registry_from(file.as_deref(), inline.as_deref())
}

/// Built-in providers plus the configured ones, from the file if it loads, else from the inline JSON.
fn registry_from(file: Option<&str>, inline: Option<&str>) -> ProviderRegistry {
    let defaults = ProviderRegistry::with_defaults();

    // 1. Try loading from config file (supports YAML, JSON, TOML via config crate)
    if let Some(path) = file {
        if let Ok(settings) = config::Config::builder()
            .add_source(config::File::with_name(path))
            .build()
            && let Ok(reg) = settings.try_deserialize::<ProviderRegistry>()
        {
            log::info!("Loaded LLM providers from: {}", path);
            return defaults.merge(reg);
        }
        log::warn!("Failed to load or parse config file: {}", path);
    }

    // 2. Try inline JSON (for Kubernetes ConfigMap)
    if let Some(config) = inline
        && !config.is_empty()
        && let Ok(reg) = ProviderRegistry::from_json(config)
    {
        return defaults.merge(reg);
    }

    // 3. Use defaults
    defaults
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
