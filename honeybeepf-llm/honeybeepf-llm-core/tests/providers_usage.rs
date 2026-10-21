use honeybeepf_llm_core::providers::{
    ConfigurableProvider, ProviderConfig, ProviderRegistry, RequestExtractorType, ResponseConfig,
};
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
            cached_tokens: None,
            model_path: "model".to_string(),
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
            cached_tokens: None,
            model_path: "modelVersion".to_string(),
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
    assert_eq!(usage.cached_tokens, Some(12));
    assert_eq!(usage.model, Some("gpt-6".to_string()));
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
