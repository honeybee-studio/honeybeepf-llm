use honeybeepf_llm_core::providers::{ProviderRegistry, RequestExtractorType, path_matches};

#[test]
fn test_default_providers() {
    let registry = ProviderRegistry::with_defaults();
    assert_eq!(registry.providers.len(), 4);
}

#[test]
fn test_find_provider_openai_responses() {
    let registry = ProviderRegistry::with_defaults();

    for path in [
        "/v1/responses",
        "/v1/responses?stream=true",
        "/openai/v1/responses",
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
fn test_find_provider_skips_responses_retrieval() {
    let registry = ProviderRegistry::with_defaults();

    for path in [
        "/v1/responses/resp_123",
        "/v1/responses/resp_123/input_items",
        "/v1/responses/resp_123/cancel",
    ] {
        assert!(
            registry.find_provider("api.openai.com", path).is_none(),
            "{path}"
        );
    }
}

#[test]
fn test_path_matches_anchor() {
    assert!(path_matches("/v1/chat/completions", "/chat/completions"));
    assert!(path_matches("/v1/responses?x=1", "/v1/responses$"));
    assert!(path_matches("/v1/messages/", "/v1/messages$"));
    assert!(!path_matches("/v1/responses/r1", "/v1/responses$"));
    assert!(!path_matches("/v1/messages//", "/v1/messages$"));
}

#[test]
fn test_find_provider_generation_endpoints() {
    let registry = ProviderRegistry::with_defaults();

    for (host, path, name) in [
        ("api.anthropic.com", "/v1/messages", "anthropic"),
        ("api.anthropic.com", "/v1/messages?beta=true", "anthropic"),
        ("api.anthropic.com", "/v1/messages/", "anthropic"),
        ("api.anthropic.com", "/anthropic/v1/messages", "anthropic"),
        ("api.openai.com", "/v1/chat/completions", "openai"),
        ("api.openai.com", "/v1/completions", "openai"),
        (
            "api.openai.com",
            "/openai/deployments/d1/chat/completions?api-version=2026-10-01",
            "openai",
        ),
    ] {
        let p = registry.find_provider(host, path);
        assert_eq!(p.map(|p| p.name.as_str()), Some(name), "{path}");
    }
}

#[test]
fn test_find_provider_skips_non_generation_endpoints() {
    let registry = ProviderRegistry::with_defaults();

    for (host, path) in [
        ("api.anthropic.com", "/v1/messages/count_tokens"),
        ("api.anthropic.com", "/v1/messages/count_tokens?beta=true"),
        ("api.anthropic.com", "/v1/messages/batches"),
        ("api.anthropic.com", "/v1/messages/batches/msgbatch_01"),
        (
            "api.anthropic.com",
            "/v1/messages/batches/msgbatch_01/results",
        ),
        (
            "api.anthropic.com",
            "/v1/messages/batches/msgbatch_01/cancel",
        ),
        ("api.openai.com", "/v1/chat/completions/chatcmpl-abc123"),
        (
            "api.openai.com",
            "/v1/chat/completions/chatcmpl-abc123/messages",
        ),
    ] {
        assert!(registry.find_provider(host, path).is_none(), "{path}");
    }
}

#[test]
fn test_find_provider() {
    let registry = ProviderRegistry::with_defaults();

    let openai = registry.find_provider("api.openai.com", "/v1/chat/completions");
    assert!(openai.is_some());
    assert_eq!(openai.unwrap().name, "openai");

    let gemini = registry.find_provider(
        "generativelanguage.googleapis.com",
        "/v1/models/gemini:generateContent",
    );
    assert!(gemini.is_some());
    assert_eq!(gemini.unwrap().name, "gemini");
}

#[test]
fn test_find_provider_gemini_stream_generate_content() {
    let registry = ProviderRegistry::with_defaults();

    let gemini = registry.find_provider(
        "generativelanguage.googleapis.com",
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
    );
    assert_eq!(gemini.map(|p| p.name.as_str()), Some("gemini"));
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
