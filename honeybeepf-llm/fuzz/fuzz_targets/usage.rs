//! Response and request JSON read by every built-in provider's usage and request rules.
#![no_main]

use honeybeepf_llm_core::providers::{ConfigurableProvider, ProviderRegistry};
use libfuzzer_sys::fuzz_target;
use std::sync::LazyLock;

static PROVIDERS: LazyLock<Vec<ConfigurableProvider>> = LazyLock::new(|| {
    ProviderRegistry::with_defaults()
        .providers
        .into_iter()
        .map(ConfigurableProvider::new)
        .collect()
});

fuzz_target!(|data: &[u8]| {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    for provider in PROVIDERS.iter() {
        let _ = provider.parse_usage(&json);
        if provider.detect_request(&json) {
            let _ = provider.extract_request_text(&json);
        }
    }
});
