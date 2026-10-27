use honeybeepf_llm_core::providers::{
    ConfigurableProvider, ProviderConfig, ProviderRegistry, RequestExtractorType, ResponseConfig,
};
use honeybeepf_llm_core::types::UsageInfo;
use serde_json::json;

fn openai_config() -> ProviderConfig {
    ProviderConfig {
        name: "openai".to_string(),
        hosts: vec!["api.openai.com".to_string()],
        paths: vec!["/chat/completions".to_string()],
        response: ResponseConfig {
            usage_path: "usage".to_string(),
            prompt_tokens: "prompt_tokens".to_string(),
            completion_tokens: "completion_tokens".to_string(),
            thoughts_tokens: None,
            model_path: "model".to_string(),
            ..ResponseConfig::default()
        },
        request_extractor: RequestExtractorType::Messages,
    }
}

fn gemini_config() -> ProviderConfig {
    ProviderConfig {
        name: "gemini".to_string(),
        hosts: vec!["generativelanguage.googleapis.com".to_string()],
        paths: vec!["generateContent".to_string()],
        response: ResponseConfig {
            usage_path: "usageMetadata".to_string(),
            prompt_tokens: "promptTokenCount".to_string(),
            completion_tokens: "candidatesTokenCount".to_string(),
            thoughts_tokens: Some("thoughtsTokenCount".to_string()),
            model_path: "modelVersion".to_string(),
            ..ResponseConfig::default()
        },
        request_extractor: RequestExtractorType::Contents,
    }
}

#[test]
fn test_openai_matching() {
    let provider = ConfigurableProvider::new(openai_config());
    assert!(provider.matches("api.openai.com", "/v1/chat/completions"));
    assert!(!provider.matches("api.anthropic.com", "/v1/messages"));
}

#[test]
fn test_openai_parse_usage() {
    let provider = ConfigurableProvider::new(openai_config());
    let response = json!({
        "id": "chatcmpl-123",
        "model": "gpt-4",
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 20,
            "total_tokens": 30
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 20);
    assert_eq!(usage.model, Some("gpt-4".to_string()));
}

#[test]
fn test_gemini_parse_usage() {
    let provider = ConfigurableProvider::new(gemini_config());
    let response = json!({
        "candidates": [{"content": {"parts": [{"text": "Hello!"}]}}],
        "usageMetadata": {
            "promptTokenCount": 15,
            "candidatesTokenCount": 25,
            "thoughtsTokenCount": 100
        },
        "modelVersion": "gemini-1.5-pro"
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 15);
    assert_eq!(usage.completion_tokens, 25);
    assert_eq!(usage.thoughts_tokens, Some(100));
    assert_eq!(usage.model, Some("gemini-1.5-pro".to_string()));
}

fn openai_responses_provider() -> ConfigurableProvider {
    let registry = ProviderRegistry::with_defaults();
    let config = registry
        .find_provider("api.openai.com", "/v1/responses")
        .unwrap()
        .clone();
    ConfigurableProvider::new(config)
}

#[test]
fn test_openai_responses_parse_usage() {
    let provider = openai_responses_provider();
    let response = json!({
        "id": "resp_1",
        "object": "response",
        "model": "gpt-6",
        "status": "completed",
        "error": null,
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "hi"}]}],
        "usage": {
            "input_tokens": 30,
            "input_tokens_details": {"cached_tokens": 12},
            "output_tokens": 9,
            "output_tokens_details": {"reasoning_tokens": 2},
            "total_tokens": 39
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 30);
    assert_eq!(usage.completion_tokens, 9);
    assert_eq!(usage.thoughts_tokens, Some(2));
    assert_eq!(usage.cache_read_tokens, Some(12));
    assert_eq!(usage.model, Some("gpt-6".to_string()));
}

// The compacted response carries Responses usage but no `model`; its output items have their own `status`.
#[test]
fn test_openai_responses_compact_parse_usage() {
    let registry = ProviderRegistry::with_defaults();
    let config = registry
        .find_provider("api.openai.com", "/v1/responses/compact")
        .unwrap()
        .clone();
    let provider = ConfigurableProvider::new(config);
    let response = json!({
        "id": "resp_001",
        "object": "response.compaction",
        "output": [
            {"id": "msg_001", "status": "completed", "type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "usage: {\"input_tokens\": 1}"}]},
            {"type": "compaction", "id": "cmp_001", "encrypted_content": "encrypted-summary"}
        ],
        "created_at": 1731459200,
        "usage": {
            "input_tokens": 1200,
            "input_tokens_details": {"cached_tokens": 1024, "cache_write_tokens": 0},
            "output_tokens": 300,
            "output_tokens_details": {"reasoning_tokens": 64},
            "total_tokens": 1500
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 1200);
    assert_eq!(usage.completion_tokens, 300);
    assert_eq!(usage.thoughts_tokens, Some(64));
    assert_eq!(usage.cache_read_tokens, Some(1024));
    assert_eq!(usage.cache_write_tokens, None);
    assert_eq!(usage.model, None);
}

fn default_provider(host: &str, path: &str) -> ConfigurableProvider {
    let registry = ProviderRegistry::with_defaults();
    ConfigurableProvider::new(registry.find_provider(host, path).unwrap().clone())
}

#[test]
fn test_openai_chat_cached_tokens_are_part_of_prompt() {
    let provider = default_provider("api.openai.com", "/v1/chat/completions");
    let response = json!({
        "model": "gpt-4o",
        "usage": {
            "prompt_tokens": 2000,
            "completion_tokens": 40,
            "prompt_tokens_details": {"cached_tokens": 1536}
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 2000);
    assert_eq!(usage.cache_read_tokens, Some(1536));
    assert_eq!(usage.cache_write_tokens, None);
}

#[test]
fn test_anthropic_prompt_adds_cache_read_and_write() {
    let provider = default_provider("api.anthropic.com", "/v1/messages");
    let response = json!({
        "type": "message",
        "model": "claude-test",
        "usage": {
            "input_tokens": 50,
            "cache_creation_input_tokens": 2000,
            "cache_read_input_tokens": 100000,
            "output_tokens": 300
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 102_050);
    assert_eq!(usage.completion_tokens, 300);
    assert_eq!(usage.cache_read_tokens, Some(100_000));
    assert_eq!(usage.cache_write_tokens, Some(2000));
}

#[test]
fn test_anthropic_without_cache_fields_keeps_input_tokens() {
    let provider = default_provider("api.anthropic.com", "/v1/messages");
    let response = json!({"usage": {"input_tokens": 25, "output_tokens": 15}});

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 25);
    assert_eq!(
        (usage.cache_read_tokens, usage.cache_write_tokens),
        (None, None)
    );
}

#[test]
fn test_gemini_cached_content_tokens_are_part_of_prompt() {
    let provider = default_provider("generativelanguage.googleapis.com", ":generateContent");
    let response = json!({
        "usageMetadata": {
            "promptTokenCount": 5000,
            "candidatesTokenCount": 20,
            "cachedContentTokenCount": 4096
        },
        "modelVersion": "gemini-test"
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 5000);
    assert_eq!(usage.cache_read_tokens, Some(4096));
}

fn gemini_default() -> ConfigurableProvider {
    default_provider("generativelanguage.googleapis.com", ":generateContent")
}

#[test]
fn test_gemini_missing_candidates_count_is_zero_output() {
    for finish in ["MAX_TOKENS", "STOP"] {
        let response = json!({
            "candidates": [{"content": {"role": "model"}, "finishReason": finish, "index": 0}],
            "usageMetadata": {
                "promptTokenCount": 1,
                "totalTokenCount": 1,
                "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 1}],
                "serviceTier": "standard"
            },
            "modelVersion": "gemini-test"
        });

        let usage = gemini_default().parse_usage(&response).unwrap();
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (1, 0));
        assert_eq!(usage.thoughts_tokens, None);
    }
}

#[test]
fn test_gemini_thoughts_without_candidates_count() {
    let response = json!({
        "usageMetadata": {"promptTokenCount": 5, "totalTokenCount": 9, "thoughtsTokenCount": 4}
    });

    let usage = gemini_default().parse_usage(&response).unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (5, 4));
    assert_eq!(usage.thoughts_tokens, Some(4));
}

#[test]
fn test_gemini_thoughts_are_part_of_completion() {
    let response = json!({
        "usageMetadata": {"promptTokenCount": 15, "candidatesTokenCount": 25,
            "thoughtsTokenCount": 100, "totalTokenCount": 140},
        "modelVersion": "gemini-2.5-pro"
    });

    let usage = gemini_default().parse_usage(&response).unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (15, 125));
    assert_eq!(usage.thoughts_tokens, Some(100));
}

#[test]
fn test_openai_reasoning_is_not_added_to_completion_again() {
    let chat = json!({"model": "gpt-5", "usage": {"prompt_tokens": 1200, "completion_tokens": 800,
        "completion_tokens_details": {"reasoning_tokens": 600}}});
    let responses = json!({"model": "gpt-5", "usage": {"input_tokens": 1200,
        "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 800,
        "output_tokens_details": {"reasoning_tokens": 600}}});

    for (path, response) in [("/v1/chat/completions", chat), ("/v1/responses", responses)] {
        let usage = default_provider("api.openai.com", path)
            .parse_usage(&response)
            .unwrap();
        assert_eq!(usage.completion_tokens, 800, "{path}");
        assert_eq!(usage.thoughts_tokens, Some(600), "{path}");
    }
}

#[test]
fn test_custom_provider_can_add_thoughts_to_completion() -> Result<(), serde_json::Error> {
    let registry = ProviderRegistry::from_json(
        r#"{"providers":[{"name":"gemini-proxy","response":{"usage_path":"usageMetadata",
            "prompt_tokens":"promptTokenCount","completion_tokens":"candidatesTokenCount",
            "thoughts_tokens":"thoughtsTokenCount","completion_excludes_thoughts":true}}]}"#,
    )?;
    let provider = ConfigurableProvider::new(registry.providers[0].clone());
    let usage = provider
        .parse_usage(&json!({"usageMetadata": {"promptTokenCount": 3,
            "candidatesTokenCount": 2, "thoughtsTokenCount": 7}}))
        .unwrap();
    assert_eq!(usage.completion_tokens, 9);
    Ok(())
}

#[test]
fn test_gemini_without_prompt_count_is_not_usage() {
    let provider = gemini_default();
    assert!(
        provider
            .parse_usage(&json!({"usageMetadata": {}}))
            .is_none()
    );
    assert!(
        provider
            .parse_usage(&json!({"usageMetadata": {"candidatesTokenCount": 3}}))
            .is_none()
    );
    assert!(provider.parse_usage(&json!({"candidates": []})).is_none());
}

#[test]
fn test_missing_completion_is_still_not_usage_for_other_providers() {
    let registry = ProviderRegistry::with_defaults();
    let usages = [
        json!({"usage": {"prompt_tokens": 8, "total_tokens": 8}}),
        json!({"usage": {"input_tokens": 25}}),
        json!({"usage": {"input_tokens": 25, "input_tokens_details": {"cached_tokens": 0}}}),
    ];
    for config in registry.providers.iter().filter(|p| p.name != "gemini") {
        let provider = ConfigurableProvider::new(config.clone());
        for usage in &usages {
            assert!(
                provider.parse_usage(usage).is_none(),
                "{} {usage}",
                config.name
            );
        }
    }
}

#[test]
fn test_custom_provider_requires_completion_by_default() -> Result<(), serde_json::Error> {
    let registry = ProviderRegistry::from_json(
        r#"{"providers":[{"name":"bedrock-like","response":{"prompt_tokens":"inputTokens","completion_tokens":"outputTokens"}},
            {"name":"gemini-proxy","response":{"usage_path":"usageMetadata","prompt_tokens":"promptTokenCount",
            "completion_tokens":"candidatesTokenCount","completion_optional":true}}]}"#,
    )?;
    let strict = ConfigurableProvider::new(registry.providers[0].clone());
    assert!(
        strict
            .parse_usage(&json!({"usage": {"inputTokens": 12}}))
            .is_none()
    );
    let optional = ConfigurableProvider::new(registry.providers[1].clone());
    let usage = optional
        .parse_usage(&json!({"usageMetadata": {"promptTokenCount": 12}}))
        .unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (12, 0));
    Ok(())
}

#[test]
fn test_custom_provider_cached_tokens_key_still_reads_cache() -> Result<(), serde_json::Error> {
    let registry = ProviderRegistry::from_json(
        r#"{"providers":[{"name":"proxy","response":{"cached_tokens":"prompt_tokens_details.cached_tokens"}}]}"#,
    )?;
    let provider = ConfigurableProvider::new(registry.providers[0].clone());
    let response = json!({
        "usage": {"prompt_tokens": 10, "completion_tokens": 2, "prompt_tokens_details": {"cached_tokens": 8}}
    });
    assert_eq!(
        provider.parse_usage(&response).unwrap().cache_read_tokens,
        Some(8)
    );
    Ok(())
}

#[test]
fn test_responses_config_skips_anthropic_usage() {
    let provider = openai_responses_provider();
    let anthropic = json!({
        "usage": {"input_tokens": 50, "cache_read_input_tokens": 100000, "output_tokens": 300}
    });
    assert!(provider.parse_usage(&anthropic).is_none());
}

#[test]
fn test_openai_responses_reads_cache_writes_their_ttl_and_service_tier() {
    // Usage example from the OpenAI prompt caching guide (12-hour retention).
    let response = json!({"model": "gpt-6.1-sol", "service_tier": "priority", "usage": {
        "input_tokens": 12000, "output_tokens": 500,
        "input_tokens_details": {"cached_tokens": 4000, "cache_write_tokens": 7000,
            "cache_write_tokens_details": {"ttl_30m_tokens": 3000, "ttl_12h_tokens": 4000}}}});

    let u = openai_responses_provider().parse_usage(&response).unwrap();
    assert_eq!((u.prompt_tokens, u.completion_tokens), (12000, 500));
    assert_eq!(
        (
            u.cache_read_tokens,
            u.cache_write_tokens,
            u.cache_write_long_ttl_tokens
        ),
        (Some(4000), Some(7000), Some(4000))
    );
    assert_eq!(u.service_tier.as_deref(), Some("priority"));
}

#[test]
fn test_openai_responses_write_count_without_ttl_details_has_no_long_ttl() {
    let response = json!({"model": "gpt-5.5", "service_tier": "default", "usage": {
        "input_tokens": 30, "output_tokens": 9,
        "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0}}});

    let u = openai_responses_provider().parse_usage(&response).unwrap();
    assert_eq!(u.prompt_tokens, 30);
    assert_eq!(
        (u.cache_write_tokens, u.cache_write_long_ttl_tokens),
        (Some(0), None)
    );
}

#[test]
fn test_anthropic_reads_1h_cache_writes_speed_and_inference_geo() {
    let provider = default_provider("api.anthropic.com", "/v1/messages");
    let response = json!({"type": "message", "model": "claude-opus-5-5", "usage": {
        "input_tokens": 10000, "output_tokens": 2000,
        "cache_creation_input_tokens": 2000, "cache_read_input_tokens": 0,
        "cache_creation": {"ephemeral_5m_input_tokens": 500, "ephemeral_1h_input_tokens": 1500},
        "service_tier": "standard", "speed": "fast", "inference_geo": "us"}});

    let u = provider.parse_usage(&response).unwrap();
    assert_eq!(u.prompt_tokens, 12_000);
    assert_eq!(
        (u.cache_write_tokens, u.cache_write_long_ttl_tokens),
        (Some(2000), Some(1500))
    );
    assert_eq!(
        (u.speed.as_deref(), u.inference_geo.as_deref()),
        (Some("fast"), Some("us"))
    );
    // Anthropic usage.service_tier is the commitment tier, not an OpenAI-style price tier.
    assert_eq!(u.service_tier, None);
}

#[test]
fn test_long_ttl_writes_never_exceed_the_write_total() {
    let provider = default_provider("api.anthropic.com", "/v1/messages");
    let long_ttl = |usage: serde_json::Value| {
        let u = provider
            .parse_usage(&json!({"model": "claude-opus-5-5", "usage": usage}))
            .unwrap();
        (u.cache_write_tokens, u.cache_write_long_ttl_tokens)
    };

    assert_eq!(
        long_ttl(
            json!({"input_tokens": 10, "output_tokens": 2, "cache_creation_input_tokens": 100,
            "cache_creation": {"ephemeral_1h_input_tokens": 999_999}})
        ),
        (Some(100), Some(100))
    );
    assert_eq!(
        long_ttl(json!({"input_tokens": 10, "output_tokens": 2,
            "cache_creation": {"ephemeral_1h_input_tokens": 1500}})),
        (None, None)
    );
}

#[test]
fn test_openai_chat_reads_served_service_tier() {
    let provider = default_provider("api.openai.com", "/v1/chat/completions");
    let response = json!({"model": "gpt-6.1-sol", "service_tier": "flex",
        "usage": {"prompt_tokens": 10000, "completion_tokens": 2000}});

    let u = provider.parse_usage(&response).unwrap();
    assert_eq!(u.service_tier.as_deref(), Some("flex"));
    assert_eq!((u.speed, u.inference_geo), (None, None));
}

#[test]
fn test_pricing_fields_are_read_only_from_their_paths() {
    let registry = ProviderRegistry::with_defaults();
    let lookalike = r#"{"service_tier":"flex","speed":"fast","inference_geo":"us"}"#;
    for response in [
        // Model output quoting the fields, and a non-string or null tier.
        json!({"model": "gpt-4o", "service_tier": 5,
            "choices": [{"message": {"content": lookalike}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2}}),
        json!({"model": "claude-opus-5-5", "content": [{"type": "text", "text": lookalike}],
            "usage": {"input_tokens": 10, "output_tokens": 2, "speed": null, "inference_geo": 1}}),
        json!({"model": "gpt-6", "service_tier": null, "output": [{"content": [{"text": lookalike}]}],
            "usage": {"input_tokens": 10, "output_tokens": 2, "input_tokens_details": {"cached_tokens": 0}}}),
    ] {
        let u = parse_in_order(&registry.providers, &response).unwrap();
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens),
            (10, 2),
            "{response}"
        );
        assert_eq!(
            (u.service_tier, u.speed, u.inference_geo),
            (None, None, None),
            "{response}"
        );
    }
}

#[test]
fn test_custom_provider_can_read_pricing_fields() -> Result<(), serde_json::Error> {
    let registry = ProviderRegistry::from_json(
        r#"{"providers":[{"name":"proxy","response":{"service_tier_path":"meta.tier","speed":"mode",
            "cache_write_tokens":"writes","cache_write_long_ttl_tokens":"writes_1h"}}]}"#,
    )?;
    let provider = ConfigurableProvider::new(registry.providers[0].clone());
    let response = json!({"meta": {"tier": "flex"}, "usage": {"prompt_tokens": 10,
        "completion_tokens": 2, "mode": "fast", "writes": 6, "writes_1h": 4}});

    let u = provider.parse_usage(&response).unwrap();
    assert_eq!(
        (u.service_tier.as_deref(), u.speed.as_deref()),
        (Some("flex"), Some("fast"))
    );
    assert_eq!(
        (u.cache_write_tokens, u.cache_write_long_ttl_tokens),
        (Some(6), Some(4))
    );
    Ok(())
}

/// Parse like the agent does: try providers in order, without a host check.
fn parse_in_order(providers: &[ProviderConfig], response: &serde_json::Value) -> Option<UsageInfo> {
    providers
        .iter()
        .find_map(|c| ConfigurableProvider::new(c.clone()).parse_usage(response))
}

#[test]
fn test_bedrock_converse_prompt_adds_cache_read_and_write() {
    let registry = ProviderRegistry::with_defaults();
    let response = json!({"stopReason": "end_turn", "metrics": {"latencyMs": 1275},
        "usage": {"inputTokens": 30, "outputTokens": 628, "totalTokens": 1858,
            "cacheReadInputTokens": 1000, "cacheWriteInputTokens": 200}});

    let u = parse_in_order(&registry.providers, &response).unwrap();
    let cache = (u.cache_read_tokens, u.cache_write_tokens);
    assert_eq!(
        (u.prompt_tokens, u.completion_tokens, u.model),
        (1230, 628, None)
    );
    assert_eq!(cache, (Some(1000), Some(200)));
}

#[test]
fn test_new_defaults_parse_existing_bodies_like_before() {
    let registry = ProviderRegistry::with_defaults();
    // Entries 0-3 are the OpenAI (chat, Responses), Anthropic and Gemini first-party providers.
    let (before, _) = registry.providers.split_at(4);

    for response in [
        json!({"model": "gpt-4o", "usage": {"prompt_tokens": 2000, "completion_tokens": 40,
            "prompt_tokens_details": {"cached_tokens": 1536}, "completion_tokens_details": {"reasoning_tokens": 8}}}),
        json!({"model": "gpt-6", "usage": {"input_tokens": 30, "input_tokens_details": {"cached_tokens": 12},
            "output_tokens": 9, "output_tokens_details": {"reasoning_tokens": 2}}}),
        json!({"model": "claude-opus-5-5", "usage": {"input_tokens": 50, "cache_creation_input_tokens": 2000,
            "cache_read_input_tokens": 100000, "output_tokens": 300}}),
        json!({"modelVersion": "gemini-3.5-flash", "usageMetadata": {"promptTokenCount": 5000,
            "candidatesTokenCount": 20, "thoughtsTokenCount": 7, "cachedContentTokenCount": 4096}}),
        // Azure OpenAI chat example (no model field) and Responses example (no input_tokens_details)
        json!({"usage": {"completion_tokens": 557, "prompt_tokens": 33, "total_tokens": 590}}),
        json!({"model": "deepseek-v3-0324", "status": "completed",
            "usage": {"input_tokens": 28, "output_tokens": 52, "total_tokens": 80}}),
    ] {
        let now = parse_in_order(&registry.providers, &response);
        assert!(now.is_some(), "{response}");
        assert_eq!(now, parse_in_order(before, &response), "{response}");
    }
}

#[test]
fn test_openai_responses_extract_request_text() {
    let provider = openai_responses_provider();
    let request = json!({"model": "gpt-6", "input": "Tell me a joke"});
    assert_eq!(provider.extract_request_text(&request), "Tell me a joke");
}

#[test]
fn test_extract_request_text() {
    let provider = ConfigurableProvider::new(openai_config());
    let request = json!({
        "model": "gpt-4",
        "messages": [
            {"role": "user", "content": "Hello, world!"}
        ]
    });

    let text = provider.extract_request_text(&request);
    assert_eq!(text, "Hello, world!");
}
