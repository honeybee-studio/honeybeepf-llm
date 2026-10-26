use honeybeepf_llm_core::providers::{
    ProviderRegistry, RequestExtractorType, host_matches, path_matches,
};

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

#[test]
fn test_find_provider_openai_regional_domains() {
    let registry = ProviderRegistry::with_defaults();

    for url in [
        "us.api.openai.com/v1/chat/completions",
        "eu.api.openai.com/v1/responses",
        "ae.api.openai.com/v1/chat/completions",
    ] {
        assert_eq!(
            provider_for(&registry, url).as_deref(),
            Some("openai"),
            "{url}"
        );
    }
}

#[test]
fn test_find_provider_cloud_endpoints() {
    let registry = ProviderRegistry::with_defaults();

    let cases: [(&str, &[&str]); 4] = [
        (
            "anthropic",
            &[
                "aws-external-anthropic.us-west-2.api.aws/v1/messages",
                "bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages",
                "aiplatform.googleapis.com/v1/projects/p/locations/global/publishers/anthropic/models/claude-opus-5-5:rawPredict",
                "us-east5-aiplatform.googleapis.com/v1/projects/p/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-6:streamRawPredict",
                "aiplatform.eu.rep.googleapis.com/v1/projects/p/locations/eu/publishers/anthropic/models/claude-opus-5-5:rawPredict",
                "claude.googleapis.com/v1/projects/p/locations/global/workspaces/wrkspc_01/invoke/v1/messages",
                "myres.services.ai.azure.com/anthropic/v1/messages",
            ],
        ),
        (
            "openai",
            &[
                "bedrock-mantle.us-east-2.api.aws/openai/v1/responses",
                "bedrock-mantle.us-east-1.api.aws/v1/chat/completions",
                "bedrock-mantle.us-east-1.api.aws/openai/v1/chat/completions",
                "bedrock-runtime.us-east-1.amazonaws.com/openai/v1/chat/completions",
                "bedrock-runtime.us-east-1.amazonaws.com/openai/v1/responses",
                "myres.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2024-10-21",
                "myres.openai.azure.com/openai/deployments/gpt-35-turbo-instruct/completions?api-version=2024-10-21",
                "myres.openai.azure.com/openai/responses?api-version=2025-04-01-preview",
                "myres.openai.azure.com/openai/deployments/gpt-4o/responses?api-version=2025-04-01-preview",
                "myres.openai.azure.com/openai/v1/chat/completions",
                "myres.openai.azure.com/openai/v1/responses",
                "myres.services.ai.azure.com/openai/v1/responses",
            ],
        ),
        (
            "bedrock",
            &[
                "bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-sonnet-20240229-v1:0/converse",
                "bedrock-runtime.us-east-1.amazonaws.com/model/us.anthropic.claude-3-5-sonnet-20240620-v1%3A0/invoke",
            ],
        ),
        (
            "gemini",
            &[
                "us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google/models/gemini-3.5-flash:generateContent",
                "aiplatform.googleapis.com/v1/projects/p/locations/global/publishers/google/models/gemini-3.5-flash:streamGenerateContent?alt=sse",
                "aiplatform.us.rep.googleapis.com/v1/projects/p/locations/us/publishers/google/models/gemini-3.5-flash:generateContent",
            ],
        ),
    ];
    for (name, urls) in cases {
        for url in urls {
            assert_eq!(provider_for(&registry, url).as_deref(), Some(name), "{url}");
        }
    }
}

#[test]
fn test_find_provider_skips_similar_cloud_calls() {
    let registry = ProviderRegistry::with_defaults();

    for url in [
        // Other services under the same parent domains
        "ec2.us-east-1.api.aws/v1/messages",
        "bedrock-mantle.us-east-1.api.aws.example.com/v1/messages",
        "bedrock.us-east-1.amazonaws.com/model/anthropic.claude-x/invoke",
        "bedrock-agent-runtime.us-east-1.amazonaws.com/model/anthropic.claude-x/converse",
        "storage.googleapis.com/v1/b/models:generateContent",
        // Non-generation routes on LLM hosts
        "bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages/count_tokens",
        "bedrock-mantle.us-east-1.api.aws/v1/models",
        "aws-external-anthropic.us-west-2.api.aws/v1/messages/batches",
        "bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-x/invoke-with-response-stream",
        "bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-x/converse-stream",
        "bedrock-runtime.us-east-1.amazonaws.com/model/amazon.titan-text-express-v1/invoke",
        "bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-x/count-tokens",
        "us-east5-aiplatform.googleapis.com/v1/projects/p/locations/us-east5/publishers/anthropic/models/count-tokens:rawPredict",
        "us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/endpoints/123:rawPredict",
        "us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google/models/gemini-3.5-flash:countTokens",
        "myres.openai.azure.com/openai/deployments/d1/embeddings?api-version=2024-10-21",
        "myres.openai.azure.com/openai/deployments/d1/audio/transcriptions?api-version=2024-10-21",
        "myres.openai.azure.com/openai/v1/responses/resp_1",
        "myres.openai.azure.com/openai/responses/resp_1?api-version=2025-04-01-preview",
        "myres.openai.azure.com/openai/deployments/gpt-4o/responses/resp_1/cancel?api-version=2025-04-01-preview",
        "myres.openai.azure.com/openai/deployments/d1/completions/x?api-version=2024-10-21",
        "claude.googleapis.com/v1/projects/p/locations/global/workspaces/wrkspc_01/invoke/v1/messages/count_tokens",
        "claude.googleapis.com/v1/projects/p/locations/global/workspaces/wrkspc_01/invoke/v1/messages/batches",
        "claude.googleapis.com/v1/projects/p/locations/global/workspaces/wrkspc_01/invoke/v1/models",
        "claude.googleapis.com/v1/projects/p/locations/global/claudeOrganization/invoke/v1/organizations/users",
        "claude.googleapis.com/v1/projects/p/locations/global/publishers/google/models/gemini-3.5-flash:generateContent",
        "myres.services.ai.azure.com/anthropic/v1/messages/count_tokens",
    ] {
        assert_eq!(provider_for(&registry, url), None, "{url}");
    }
}

// Foundry serves OpenAI and Anthropic routes on one host, so only the path picks the provider.
#[test]
fn test_find_provider_foundry_host_split_by_path() {
    let registry = ProviderRegistry::with_defaults();

    for (url, name) in [
        (
            "myres.services.ai.azure.com/anthropic/v1/messages",
            "anthropic",
        ),
        (
            "myres.services.ai.azure.com/anthropic/v1/messages/",
            "anthropic",
        ),
        (
            "myres.services.ai.azure.com/openai/v1/chat/completions",
            "openai",
        ),
        ("myres.services.ai.azure.com/openai/v1/responses", "openai"),
    ] {
        assert_eq!(provider_for(&registry, url).as_deref(), Some(name), "{url}");
    }
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
    assert!(path_matches("/v1/chat/completions/", "/chat/completions$"));
    assert!(path_matches("/api/chat/", "/api/chat/$"));
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
