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
    // 1. Try loading from config file
    // 1. Try loading from config file (supports YAML, JSON, TOML via config crate)
    if let Ok(path) = std::env::var("LLM_PROVIDERS_CONFIG_FILE") {
        if let Ok(settings) = config::Config::builder()
            .add_source(config::File::with_name(&path))
            .build()
            && let Ok(reg) = settings.try_deserialize::<ProviderRegistry>()
        {
            log::info!("Loaded LLM providers from: {}", path);
            return reg;
        }
        log::warn!("Failed to load or parse config file: {}", path);
    }

    // 2. Try inline JSON (for Kubernetes ConfigMap)
    if let Ok(config) = std::env::var("LLM_PROVIDERS_CONFIG")
        && !config.is_empty()
        && let Ok(reg) = ProviderRegistry::from_json(&config)
    {
        return reg;
    }

    // 3. Use defaults
    ProviderRegistry::with_defaults()
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
}
