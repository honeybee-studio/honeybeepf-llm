use honeybeepf_llm_core::providers::{
    ProviderRegistry, RequestExtractorType, host_matches, path_matches,
};

/// `<provider|none>  <host/path>  # reason` lines checked against the default registry.
const PROVIDER_URLS: &str = include_str!("fixtures/provider_urls.txt");

#[test]
fn test_default_providers() {
    let registry = ProviderRegistry::with_defaults();
    assert_eq!(registry.providers.len(), 6);
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
    let registry = ProviderRegistry::with_defaults();
    let mut cases = 0;
    let mut failures = Vec::new();
    for (i, line) in PROVIDER_URLS.lines().enumerate() {
        let line_no = i + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (case, reason) = line.split_once(" #").unwrap_or((line, ""));
        let mut fields = case.split_whitespace();
        let (Some(expected), Some(url), None) = (fields.next(), fields.next(), fields.next())
        else {
            panic!("line {line_no}: want `<provider|none>  <host/path>  # reason`, got {line:?}");
        };
        assert!(
            !reason.trim().is_empty(),
            "line {line_no}: missing `# reason`"
        );
        let expected = (expected != "none").then_some(expected);
        let actual = provider_for(&registry, url);
        if actual.as_deref() != expected {
            failures.push(format!(
                "line {line_no}: {url}: expected {expected:?}, got {actual:?}"
            ));
        }
        cases += 1;
    }
    assert!(cases > 0, "no cases in fixtures/provider_urls.txt");
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
    assert!(anthropic.response.prompt_excludes_cache);
    let openai = registry
        .find_provider("myres.services.ai.azure.com", "/openai/v1/chat/completions")
        .unwrap();
    assert!(!openai.response.prompt_excludes_cache);
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
