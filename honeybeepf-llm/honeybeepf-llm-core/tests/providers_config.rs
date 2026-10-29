use honeybeepf_llm_core::providers::{
    ProviderRegistry, RequestExtractorType, host_matches, path_matches,
};
use serde::Deserialize;

/// `host/path` cases grouped by expected provider, checked against the default registry.
const PROVIDER_URLS: &str = include_str!("fixtures/provider_urls.toml");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UrlCase {
    url: String,
    reason: String,
}

/// One array per expected provider; `none` lists URLs no provider may match.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderUrls {
    openai: Vec<UrlCase>,
    anthropic: Vec<UrlCase>,
    bedrock: Vec<UrlCase>,
    gemini: Vec<UrlCase>,
    none: Vec<UrlCase>,
}

#[test]
fn test_default_providers() {
    let registry = ProviderRegistry::with_defaults();
    assert_eq!(registry.providers.len(), 6);
}

// `with_defaults` panics only on a parse error, and serde skips unknown keys, so a key typo would go unnoticed.
#[test]
fn test_default_providers_json_parses_with_known_keys_only() {
    let json = include_str!("../src/providers/default_providers.json");
    ProviderRegistry::from_json(json).unwrap();

    let provider_keys = "name note hosts paths response request_extractor";
    let response_keys = "usage_path prompt_tokens completion_tokens completion_optional extra_prompt_tokens thoughts_tokens \
        completion_excludes_thoughts cache_read_tokens cache_write_tokens cache_write_long_ttl_tokens \
        speed inference_geo prompt_excludes_cache required_field model_path service_tier_path";
    let file: serde_json::Value = serde_json::from_str(json).unwrap();
    for provider in file["providers"].as_array().unwrap() {
        let name = &provider["name"];
        for key in provider.as_object().unwrap().keys() {
            let known = provider_keys.split_whitespace().any(|k| k == key);
            assert!(known, "{name}: {key}");
        }
        for key in provider["response"].as_object().unwrap().keys() {
            let known = response_keys.split_whitespace().any(|k| k == key);
            assert!(known, "{name}: response.{key}");
        }
    }
}

// Claude on Vertex AI returns the Anthropic body, so its copy of the response block must not drift.
#[test]
fn test_vertex_claude_reads_usage_like_anthropic() {
    let registry = ProviderRegistry::with_defaults();
    let anthropic: Vec<_> = registry
        .providers
        .iter()
        .filter(|p| p.name == "anthropic")
        .collect();
    assert_eq!(anthropic.len(), 2);
    assert_eq!(
        format!("{:?}", anthropic[0].response),
        format!("{:?}", anthropic[1].response)
    );
}

#[test]
fn test_default_provider_hosts() {
    let openai = [
        "api.openai.com",
        "openai.azure.com",
        "services.ai.azure.com",
        "bedrock-mantle.*.api.aws",
        "bedrock-runtime.*.amazonaws.com",
    ];
    let vertex = [
        "aiplatform.googleapis.com",
        "aiplatform.*.rep.googleapis.com",
    ];
    let expected: [&[&str]; 6] = [
        &openai,
        &openai,
        &[
            "api.anthropic.com",
            "aws-external-anthropic.*.api.aws",
            "bedrock-mantle.*.api.aws",
            "claude.googleapis.com",
            "services.ai.azure.com",
        ],
        &["generativelanguage.googleapis.com", vertex[0], vertex[1]],
        &vertex,
        &["bedrock-runtime.*.amazonaws.com"],
    ];
    let registry = ProviderRegistry::with_defaults();
    for (provider, hosts) in registry.providers.iter().zip(expected) {
        assert_eq!(provider.hosts, hosts, "{}", provider.name);
    }
}

/// Provider name for a `host/path` URL without the scheme.
fn provider_for(registry: &ProviderRegistry, url: &str) -> Option<String> {
    let (host, path) = url.split_at(url.find('/').unwrap_or(url.len()));
    registry.find_provider(host, path).map(|p| p.name.clone())
}

#[test]
fn test_find_provider_url_fixtures() {
    let fixture: ProviderUrls = toml::from_str(PROVIDER_URLS)
        .unwrap_or_else(|e| panic!("fixtures/provider_urls.toml: {e}"));
    let registry = ProviderRegistry::with_defaults();
    let groups = [
        ("openai", &fixture.openai),
        ("anthropic", &fixture.anthropic),
        ("bedrock", &fixture.bedrock),
        ("gemini", &fixture.gemini),
        ("none", &fixture.none),
    ];
    let mut cases = 0;
    let mut failures = Vec::new();
    for (provider, group) in groups {
        let expected = (provider != "none").then_some(provider);
        for UrlCase { url, reason } in group {
            cases += 1;
            if reason.trim().is_empty() {
                failures.push(format!("{provider}: {url}: empty reason"));
            }
            let actual = provider_for(&registry, url);
            if actual.as_deref() != expected {
                failures.push(format!("{provider}: {url} ({reason}): got {actual:?}"));
            }
        }
    }
    assert!(cases > 0, "no cases in fixtures/provider_urls.toml");
    assert!(
        failures.is_empty(),
        "{} of {cases} cases failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn test_host_matches_wildcard_spans_whole_host() {
    let pattern = "bedrock-mantle.*.api.aws";
    for host in [
        "bedrock-mantle.us-east-1.api.aws",
        "bedrock-mantle.us-east-1.api.aws:443",
    ] {
        assert!(host_matches(host, pattern), "{host}");
    }
    for host in [
        "bedrock-mantle.api.aws",
        "bedrock-mantle.us-east-1.api.aws.example.com",
        "x.bedrock-mantle.us-east-1.api.aws",
        "ec2.us-east-1.api.aws",
    ] {
        assert!(!host_matches(host, pattern), "{host}");
    }
    assert!(host_matches("us.api.openai.com", "api.openai.com"));
}

#[test]
fn test_path_matches_wildcard_before_anchor() {
    let vertex = "/v1/projects/p/locations/global/publishers/anthropic/models";
    let pattern = "/publishers/anthropic/models/claude-*:rawPredict$";
    assert!(path_matches(
        &format!("{vertex}/claude-opus-5-5:rawPredict"),
        pattern
    ));
    assert!(!path_matches(
        &format!("{vertex}/count-tokens:rawPredict"),
        pattern
    ));
    assert!(!path_matches(
        &format!("{vertex}/claude-opus-5-5:streamRawPredict"),
        pattern
    ));
    assert!(path_matches(
        "/model/us.anthropic.claude-opus-5-5/invoke",
        "anthropic.*/invoke$"
    ));
    assert!(!path_matches(
        "/model/amazon.titan-text-express-v1/invoke",
        "anthropic.*/invoke$"
    ));
}

// Foundry serves OpenAI and Anthropic routes on one host, so only the path picks the provider.
#[test]
fn test_find_provider_foundry_host_split_by_path() {
    let registry = ProviderRegistry::with_defaults();

    let anthropic = registry
        .find_provider("myres.services.ai.azure.com", "/anthropic/v1/messages")
        .unwrap();
    assert!(anthropic.response.cache.prompt_excludes_cache);
    let openai = registry
        .find_provider("myres.services.ai.azure.com", "/openai/v1/chat/completions")
        .unwrap();
    assert!(!openai.response.cache.prompt_excludes_cache);
}

#[test]
fn test_find_provider_openai_responses() {
    let registry = ProviderRegistry::with_defaults();

    for path in [
        "/v1/responses",
        "/v1/responses?stream=true",
        "/openai/v1/responses",
        "/openai/responses?api-version=2025-04-01-preview",
        "/openai/deployments/gpt-4o/responses?api-version=2025-04-01-preview",
        "/v1/responses/compact",
        "/v1/responses/compact/",
        "/openai/v1/responses/compact",
        "/openai/responses/compact?api-version=2025-04-01-preview",
        "/openai/deployments/gpt-4o/responses/compact?api-version=2025-04-01-preview",
    ] {
        let p = registry.find_provider("api.openai.com", path);
        assert_eq!(p.map(|p| p.name.as_str()), Some("openai"), "{path}");
        assert!(matches!(
            p.map(|p| &p.request_extractor),
            Some(RequestExtractorType::Input)
        ));
    }
}

#[test]
fn test_path_matches_anchor() {
    assert!(path_matches("/v1/chat/completions", "/chat/completions"));
    assert!(path_matches("/v1/responses?x=1", "/v1/responses$"));
    assert!(path_matches("/v1/messages/", "/v1/messages$"));
    assert!(path_matches("/v1/chat/completions/", "/chat/completions$"));
    assert!(path_matches("/api/chat/", "/api/chat/$"));
    assert!(!path_matches("/v1/responses/r1", "/v1/responses$"));
    assert!(!path_matches("/v1/messages//", "/v1/messages$"));
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

fn names(registry: &ProviderRegistry) -> Vec<&str> {
    registry.providers.iter().map(|p| p.name.as_str()).collect()
}

#[test]
fn test_user_providers_are_added_to_the_defaults() {
    let user = r#"{"providers": [
        {"name": "ollama", "hosts": ["ollama.internal"], "paths": ["/api/chat$"]},
        {"name": "vllm", "hosts": ["vllm.internal"], "paths": ["/v1/chat/completions$"]}
    ]}"#;
    let registry =
        ProviderRegistry::with_defaults().merge(ProviderRegistry::from_json(user).unwrap());
    assert_eq!(
        names(&registry),
        [
            "openai",
            "openai",
            "anthropic",
            "gemini",
            "anthropic",
            "bedrock",
            "ollama",
            "vllm"
        ]
    );
    assert_eq!(
        provider_for(&registry, "api.openai.com/v1/chat/completions").as_deref(),
        Some("openai")
    );
    assert_eq!(
        provider_for(&registry, "ollama.internal/api/chat").as_deref(),
        Some("ollama")
    );

    let empty = ProviderRegistry::from_json(r#"{"providers": []}"#).unwrap();
    assert_eq!(
        names(&ProviderRegistry::with_defaults().merge(empty)),
        names(&ProviderRegistry::with_defaults())
    );
}

// Usage parsing tries providers in order, so a replacement keeps the place of the defaults it replaces.
#[test]
fn test_user_provider_replaces_defaults_with_its_name_in_their_place() {
    let user = r#"{"providers": [
        {"name": "anthropic", "hosts": ["claude.proxy.internal"], "paths": ["/v1/messages$"]},
        {"name": "ollama", "hosts": ["ollama.internal"]},
        {"name": "anthropic", "hosts": ["claude.proxy.internal"], "paths": [":rawPredict$"]}
    ]}"#;
    let registry =
        ProviderRegistry::with_defaults().merge(ProviderRegistry::from_json(user).unwrap());
    assert_eq!(
        names(&registry),
        [
            "openai",
            "openai",
            "anthropic",
            "anthropic",
            "gemini",
            "bedrock",
            "ollama"
        ]
    );
    assert_eq!(registry.providers[3].paths, [":rawPredict$"]);
    assert_eq!(
        provider_for(&registry, "api.anthropic.com/v1/messages"),
        None
    );
    assert_eq!(
        provider_for(&registry, "claude.proxy.internal/v1/messages").as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        provider_for(&registry, "api.openai.com/v1/responses").as_deref(),
        Some("openai")
    );
}

#[test]
fn test_may_serve_host() {
    let registry = ProviderRegistry::with_defaults();
    assert!(registry.may_serve_host("api.openai.com"));
    assert!(registry.may_serve_host("bedrock-mantle.us-east-2.api.aws:443"));
    assert!(!registry.may_serve_host("noise.internal"));
    assert!(!registry.may_serve_host("ec2.us-west-2.api.aws"));

    // A provider without hosts matches every host, so no host can be ruled out.
    let any_host =
        r#"{"providers": [{"name": "gw", "hosts": [], "paths": ["/v1/chat/completions$"]}]}"#;
    let registry = ProviderRegistry::from_json(any_host).unwrap();
    assert!(registry.may_serve_host("noise.internal"));
}
