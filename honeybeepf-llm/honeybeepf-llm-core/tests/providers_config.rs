use honeybeepf_llm_core::providers::ProviderRegistry;

#[test]
fn test_default_providers() {
    let registry = ProviderRegistry::with_defaults();
    assert_eq!(registry.providers.len(), 3);
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
