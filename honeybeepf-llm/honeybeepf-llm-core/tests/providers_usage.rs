use honeybeepf_llm_core::pricing::PriceTable;
use honeybeepf_llm_core::providers::{
    ConfigurableProvider, IdentityConfig, ProviderConfig, ProviderRegistry, RequestExtractorType,
    ResponseConfig, TokenConfig,
};
use honeybeepf_llm_core::types::{ModalityTokens, Operation, UsageInfo};
use serde_json::json;

fn openai_config() -> ProviderConfig {
    ProviderConfig {
        name: "openai".to_string(),
        hosts: vec!["api.openai.com".to_string()],
        paths: vec!["/chat/completions".to_string()],
        response: ResponseConfig::default(),
        request_extractor: RequestExtractorType::Messages,
        operation: Operation::Chat,
        plaintext: None,
        record_only: false,
    }
}

fn gemini_config() -> ProviderConfig {
    ProviderConfig {
        name: "gemini".to_string(),
        hosts: vec!["generativelanguage.googleapis.com".to_string()],
        paths: vec!["generateContent".to_string()],
        response: ResponseConfig {
            tokens: TokenConfig {
                usage_path: "usageMetadata".to_string(),
                prompt_tokens: "promptTokenCount".to_string(),
                completion_tokens: "candidatesTokenCount".to_string(),
                thoughts_tokens: Some("thoughtsTokenCount".to_string()),
                ..TokenConfig::default()
            },
            identity: IdentityConfig {
                model_path: "modelVersion".to_string(),
                ..IdentityConfig::default()
            },
            ..ResponseConfig::default()
        },
        request_extractor: RequestExtractorType::Contents,
        operation: Operation::Chat,
        plaintext: None,
        record_only: false,
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
    assert_eq!(usage.cache_write_tokens, Some(0));
    assert_eq!(usage.model, None);
}

fn default_provider(host: &str, path: &str) -> ConfigurableProvider {
    let registry = ProviderRegistry::with_defaults();
    ConfigurableProvider::new(registry.find_provider(host, path).unwrap().clone())
}

#[test]
fn test_openai_chat_splits_audio_tokens_by_modality() {
    let provider = default_provider("api.openai.com", "/v1/chat/completions");
    let response = json!({"model": "gpt-4o-audio", "usage": {
        "prompt_tokens": 1200, "completion_tokens": 500,
        "prompt_tokens_details": {"audio_tokens": 300, "cached_tokens": 0},
        "completion_tokens_details": {"audio_tokens": 200, "reasoning_tokens": 0}}});
    let u = provider.parse_usage(&response).expect("usage");
    assert_eq!(u.input_modalities, audio(300));
    assert_eq!(u.output_modalities, audio(200));
    // The totals stay whole; text is the remainder (900 in, 300 out).
    assert_eq!((u.prompt_tokens, u.completion_tokens), (1200, 500));
}

#[test]
fn test_openai_image_splits_image_tokens_by_modality() {
    let provider = default_provider("api.openai.com", "/v1/images/generations");
    let response = json!({"model": "gpt-image-1", "usage": {
        "input_tokens": 120, "output_tokens": 1600, "total_tokens": 1720,
        "input_tokens_details": {"text_tokens": 20, "image_tokens": 100},
        "output_tokens_details": {"text_tokens": 0, "image_tokens": 1600}}});
    let u = provider.parse_usage(&response).expect("usage");
    assert_eq!(u.input_modalities.image, 100);
    assert_eq!(u.output_modalities.image, 1600);
    assert_eq!(u.input_modalities.audio, 0);
}

#[test]
fn test_openai_transcription_splits_input_audio() {
    let provider = default_provider("api.openai.com", "/v1/audio/transcriptions");
    let response = json!({"model": "gpt-4o-transcribe", "usage": {
        "type": "tokens", "input_tokens": 58, "output_tokens": 12, "total_tokens": 70,
        "input_token_details": {"audio_tokens": 50, "text_tokens": 8}}});
    let u = provider.parse_usage(&response).expect("usage");
    assert_eq!(u.input_modalities, audio(50));
    assert_eq!(u.output_modalities, ModalityTokens::default());
}

#[test]
fn test_gemini_splits_modalities_from_detail_arrays() {
    let provider = default_provider(
        "generativelanguage.googleapis.com",
        "/v1/models/g:generateContent",
    );
    let response = json!({"modelVersion": "gemini-omni", "usageMetadata": {
        "promptTokenCount": 1000, "candidatesTokenCount": 800, "totalTokenCount": 1800,
        "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 600},
            {"modality": "AUDIO", "tokenCount": 300}, {"modality": "DOCUMENT", "tokenCount": 100}],
        "candidatesTokensDetails": [{"modality": "TEXT", "tokenCount": 500},
            {"modality": "IMAGE", "tokenCount": 200}, {"modality": "VIDEO", "tokenCount": 100}]}});
    let u = provider.parse_usage(&response).expect("usage");
    // AUDIO/IMAGE/VIDEO are read; TEXT and DOCUMENT fall into the text remainder.
    assert_eq!(u.input_modalities, audio(300));
    assert_eq!(
        u.output_modalities,
        ModalityTokens {
            image: 200,
            video: 100,
            ..Default::default()
        }
    );
    assert_eq!((u.prompt_tokens, u.completion_tokens), (1000, 800));
}

#[test]
fn test_text_only_call_has_no_modality_split() {
    let provider = default_provider("api.openai.com", "/v1/chat/completions");
    let response =
        json!({"model": "gpt-4o", "usage": {"prompt_tokens": 40, "completion_tokens": 10}});
    let u = provider.parse_usage(&response).expect("usage");
    assert_eq!(u.input_modalities, ModalityTokens::default());
    assert_eq!(u.output_modalities, ModalityTokens::default());
}

#[test]
fn test_modalities_over_the_total_are_dropped() {
    let provider = default_provider("api.openai.com", "/v1/chat/completions");
    // Bad data: audio exceeds prompt_tokens, so the split is dropped and text stays the whole total.
    let response = json!({"model": "m", "usage": {"prompt_tokens": 100, "completion_tokens": 10,
        "prompt_tokens_details": {"audio_tokens": 500}}});
    let u = provider.parse_usage(&response).expect("usage");
    assert_eq!(u.input_modalities, ModalityTokens::default());
}

fn audio(n: u64) -> ModalityTokens {
    ModalityTokens {
        audio: n,
        ..Default::default()
    }
}

// A custom provider that opts into fallback iterations but declares no billed_categories.
fn provider_without_billed_categories() -> ConfigurableProvider {
    ConfigurableProvider::new(ProviderConfig {
        name: "custom".to_string(),
        hosts: vec![],
        paths: vec![],
        response: ResponseConfig {
            tokens: TokenConfig {
                prompt_tokens: "input_tokens".to_string(),
                completion_tokens: "output_tokens".to_string(),
                ..TokenConfig::default()
            },
            identity: IdentityConfig {
                iterations_path: Some("iterations".to_string()),
                ..IdentityConfig::default()
            },
            ..ResponseConfig::default()
        },
        request_extractor: RequestExtractorType::Messages,
        operation: Operation::Chat,
        plaintext: None,
        record_only: false,
    })
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
    assert!(usage.iterations.is_empty());
}

// Server-side fallback example from the refusals-and-fallback docs: Fable 5 declined, Opus 4.8 served.
#[test]
fn test_anthropic_fallback_bills_the_declined_attempt_under_its_model() {
    let provider = default_provider("api.anthropic.com", "/v1/messages");
    let response = json!({
        "type": "message",
        "model": "claude-opus-4-8",
        "content": [
            {"type": "fallback", "from": {"model": "claude-fable-5"}, "to": {"model": "claude-opus-4-8"},
             "trigger": {"type": "refusal", "category": "cyber"}},
            {"type": "text", "text": "Hi!"}
        ],
        "usage": {
            "input_tokens": 412, "output_tokens": 264,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0,
            "iterations": [
                {"type": "message", "model": "claude-fable-5",
                 "input_tokens": 535, "output_tokens": 0,
                 "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
                {"type": "fallback_message", "model": "claude-opus-4-8",
                 "input_tokens": 412, "output_tokens": 264,
                 "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
            ]
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    // Top-level counts the served attempt only; the declined one must not be summed in.
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (412, 264));
    assert_eq!(usage.model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(usage.iterations.len(), 1);
    let declined = &usage.iterations[0];
    assert_eq!(
        (declined.prompt_tokens, declined.completion_tokens),
        (535, 0)
    );
    assert_eq!(declined.model.as_deref(), Some("claude-fable-5"));
    // Declined before output under `cyber`, which is not in billed_categories: tokens, no cost.
    assert!(declined.cost_suppressed);
}

// A before-output decline whose category is billed (here `bio`) keeps its cost.
#[test]
fn test_anthropic_fallback_before_output_in_billed_category_keeps_cost() {
    let provider = default_provider("api.anthropic.com", "/v1/messages");
    let response = json!({
        "type": "message",
        "model": "claude-opus-4-8",
        "content": [
            {"type": "fallback", "from": {"model": "claude-fable-5"}, "to": {"model": "claude-opus-4-8"},
             "trigger": {"type": "refusal", "category": "bio"}},
            {"type": "text", "text": "Hi!"}
        ],
        "usage": {
            "input_tokens": 412, "output_tokens": 264,
            "iterations": [
                {"type": "message", "model": "claude-fable-5", "input_tokens": 535, "output_tokens": 0},
                {"type": "fallback_message", "model": "claude-opus-4-8", "input_tokens": 412, "output_tokens": 264}
            ]
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.iterations.len(), 1);
    assert!(!usage.iterations[0].cost_suppressed);
}

// A mid-output decline (output tokens already spent) always bills, whatever the category.
#[test]
fn test_anthropic_fallback_mid_output_decline_always_bills() {
    let provider = default_provider("api.anthropic.com", "/v1/messages");
    let response = json!({
        "type": "message",
        "model": "claude-opus-4-8",
        "content": [
            {"type": "fallback", "from": {"model": "claude-fable-5"}, "to": {"model": "claude-opus-4-8"},
             "trigger": {"type": "refusal", "category": "cyber"}},
            {"type": "text", "text": "Hi!"}
        ],
        "usage": {
            "input_tokens": 412, "output_tokens": 264,
            "iterations": [
                {"type": "message", "model": "claude-fable-5", "input_tokens": 535, "output_tokens": 40},
                {"type": "fallback_message", "model": "claude-opus-4-8", "input_tokens": 412, "output_tokens": 264}
            ]
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.iterations.len(), 1);
    assert_eq!(usage.iterations[0].completion_tokens, 40);
    assert!(!usage.iterations[0].cost_suppressed);
}

// Block and attempt counts can differ, so the category is matched to the declined model
// (`from.model`), not by position. Here a mid-output decline has no block, and the billed `bio`
// block belongs to the later before-output decline.
#[test]
fn test_anthropic_fallback_matches_category_to_the_declined_model_not_position() {
    let provider = default_provider("api.anthropic.com", "/v1/messages");
    let response = json!({
        "type": "message",
        "model": "claude-opus-4-8",
        "content": [
            {"type": "fallback", "from": {"model": "claude-sonnet-x"}, "to": {"model": "claude-opus-4-8"},
             "trigger": {"type": "refusal", "category": "bio"}},
            {"type": "text", "text": "Hi!"}
        ],
        "usage": {
            "input_tokens": 412, "output_tokens": 264,
            "iterations": [
                {"type": "message", "model": "claude-fable-5", "input_tokens": 300, "output_tokens": 40},
                {"type": "message", "model": "claude-sonnet-x", "input_tokens": 535, "output_tokens": 0},
                {"type": "fallback_message", "model": "claude-opus-4-8", "input_tokens": 412, "output_tokens": 264}
            ]
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.iterations.len(), 2);
    assert_eq!(usage.iterations[0].model.as_deref(), Some("claude-fable-5"));
    assert!(!usage.iterations[0].cost_suppressed); // mid-output: always billed
    assert_eq!(
        usage.iterations[1].model.as_deref(),
        Some("claude-sonnet-x")
    );
    assert_eq!(usage.iterations[1].completion_tokens, 0);
    assert!(!usage.iterations[1].cost_suppressed); // before output but `bio` is billed
}

// Without billed_categories the provider does not opt into the rule, so every declined attempt
// bills as it did before this feature, even a before-output decline.
#[test]
fn test_fallback_without_billed_categories_bills_every_declined_attempt() {
    let provider = provider_without_billed_categories();
    let response = json!({
        "model": "served",
        "content": [
            {"type": "fallback", "from": {"model": "declined"}, "to": {"model": "served"},
             "trigger": {"type": "refusal", "category": "cyber"}}
        ],
        "usage": {
            "input_tokens": 100, "output_tokens": 50,
            "iterations": [
                {"type": "message", "model": "declined", "input_tokens": 80, "output_tokens": 0},
                {"type": "fallback_message", "model": "served", "input_tokens": 100, "output_tokens": 50}
            ]
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.iterations.len(), 1);
    assert!(!usage.iterations[0].cost_suppressed);
}

// Sticky routing sends a later turn straight to the fallback model: one iteration, nothing extra.
#[test]
fn test_anthropic_single_iteration_adds_no_declined_attempt() {
    let provider = default_provider("api.anthropic.com", "/v1/messages");
    let response = json!({
        "type": "message",
        "model": "claude-opus-4-8",
        "usage": {
            "input_tokens": 412, "output_tokens": 264,
            "iterations": [
                {"type": "fallback_message", "model": "claude-opus-4-8",
                 "input_tokens": 412, "output_tokens": 264}
            ]
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (412, 264));
    assert!(usage.iterations.is_empty());
}

// Threshold compaction also uses usage.iterations, but its last entry is `message`, not
// `fallback_message`, and the top level excludes the compaction tokens. Only a fallback is handled
// here, so this response engages nothing and records just the served total (compaction is out of scope).
#[test]
fn test_anthropic_compaction_iterations_are_left_alone() {
    let provider = default_provider("api.anthropic.com", "/v1/messages");
    let response = json!({
        "type": "message",
        "model": "claude-opus-4-8",
        "usage": {
            "input_tokens": 23000, "output_tokens": 1000,
            "iterations": [
                {"type": "compaction", "input_tokens": 180000, "output_tokens": 3500},
                {"type": "message", "input_tokens": 23000, "output_tokens": 1000}
            ]
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(
        (usage.prompt_tokens, usage.completion_tokens),
        (23000, 1000)
    );
    assert!(usage.iterations.is_empty());
}

// The advisor tool sums its executor `message` entries into the top level and ends on a `message`
// entry, so billing them again would double-count. Only a fallback (last entry `fallback_message`)
// is handled, so this records just the served total.
#[test]
fn test_anthropic_advisor_iterations_are_not_double_counted() {
    let provider = default_provider("api.anthropic.com", "/v1/messages");
    let response = json!({
        "type": "message",
        "model": "claude-sonnet-5-5",
        "usage": {
            "input_tokens": 412, "output_tokens": 1701,
            "iterations": [
                {"type": "message", "model": "claude-sonnet-5-5", "input_tokens": 412, "output_tokens": 89},
                {"type": "advisor_message", "model": "claude-opus-5-5", "input_tokens": 823, "output_tokens": 1612},
                {"type": "message", "model": "claude-sonnet-5-5", "input_tokens": 0, "output_tokens": 1612}
            ]
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (412, 1701));
    assert!(usage.iterations.is_empty());
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

// URL context and code execution results go back to the model as input, outside promptTokenCount.
fn gemini_tool_use_response() -> serde_json::Value {
    json!({
        "candidates": [{"content": {"role": "model", "parts": [{"text": "summary"}]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 27, "candidatesTokenCount": 120,
            "toolUsePromptTokenCount": 10309, "totalTokenCount": 10456},
        "modelVersion": "gemini-3.8-flash"
    })
}

#[test]
fn test_gemini_tool_use_prompt_tokens_are_part_of_prompt_on_gemini_api_and_vertex() {
    let endpoints = [
        (
            "generativelanguage.googleapis.com",
            "/v1beta/models/gemini-3.8-flash:generateContent",
        ),
        (
            "generativelanguage.googleapis.com",
            "/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse",
        ),
        (
            "aiplatform.googleapis.com",
            "/v1/projects/p/locations/global/publishers/google/models/gemini-3.8-flash:generateContent",
        ),
        (
            "us-central1-aiplatform.googleapis.com",
            "/v1/projects/p/locations/us-central1/publishers/google/models/gemini-3.8-flash:streamGenerateContent",
        ),
        (
            "aiplatform.us.rep.googleapis.com",
            "/v1/projects/p/locations/us/publishers/google/models/gemini-3.8-flash:generateContent",
        ),
    ];
    for (host, path) in endpoints {
        let usage = default_provider(host, path)
            .parse_usage(&gemini_tool_use_response())
            .unwrap();
        assert_eq!(
            (usage.prompt_tokens, usage.completion_tokens),
            (27 + 10309, 120),
            "{host}{path}"
        );
    }
}

#[test]
fn test_gemini_input_and_output_add_up_to_total_with_every_part() {
    let response = json!({
        "usageMetadata": {"promptTokenCount": 5000, "cachedContentTokenCount": 4096,
            "candidatesTokenCount": 20, "thoughtsTokenCount": 300,
            "toolUsePromptTokenCount": 800, "totalTokenCount": 6120},
        "modelVersion": "gemini-test"
    });

    let usage = gemini_default().parse_usage(&response).unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (5800, 320));
    assert_eq!(usage.prompt_tokens + usage.completion_tokens, 6120);
    assert_eq!(usage.cache_read_tokens, Some(4096));
}

#[test]
fn test_gemini_bad_tool_use_count_keeps_the_prompt() {
    for tool_use in [json!(null), json!("10309"), json!(-1), json!(1.5)] {
        let response = json!({
            "usageMetadata": {"promptTokenCount": 27, "candidatesTokenCount": 120,
                "toolUsePromptTokenCount": tool_use}
        });
        let usage = gemini_default().parse_usage(&response).unwrap();
        assert_eq!(
            (usage.prompt_tokens, usage.completion_tokens),
            (27, 120),
            "{tool_use}"
        );
    }
}

#[test]
fn test_gemini_tool_use_prompt_tokens_are_priced_as_input() {
    let table = PriceTable::builtin().expect("builtin price table");
    let with_tools = gemini_default()
        .parse_usage(&gemini_tool_use_response())
        .unwrap();
    let plain = gemini_default()
        .parse_usage(&json!({
            "usageMetadata": {"promptTokenCount": 10336, "candidatesTokenCount": 120},
            "modelVersion": "gemini-3.8-flash"
        }))
        .unwrap();

    let (with_tools, plain) = (
        table.cost(&with_tools).expect("priced"),
        table.cost(&plain).expect("priced"),
    );
    assert!(plain.input > 0.0);
    assert_eq!(with_tools.input, plain.input);
}

#[test]
fn test_custom_provider_can_add_extra_prompt_tokens() -> Result<(), serde_json::Error> {
    let registry = ProviderRegistry::from_json(
        r#"{"providers":[{"name":"gemini-proxy","response":{"usage_path":"usageMetadata",
            "prompt_tokens":"promptTokenCount","completion_tokens":"candidatesTokenCount",
            "extra_prompt_tokens":"toolUsePromptTokenCount"}}]}"#,
    )?;
    let provider = ConfigurableProvider::new(registry.providers[0].clone());
    let usage = provider
        .parse_usage(&json!({"usageMetadata": {"promptTokenCount": 3,
            "candidatesTokenCount": 2, "toolUsePromptTokenCount": 7}}))
        .unwrap();
    assert_eq!(usage.prompt_tokens, 10);
    Ok(())
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
    // Only providers that require completion; the embeddings and Gemini entries opt into
    // completion_optional and legitimately record a prompt-only usage.
    for config in registry
        .providers
        .iter()
        .filter(|p| !p.response.tokens.completion_optional)
    {
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

// Every flat key a user config may set, so grouping the fields in Rust keeps the JSON shape.
#[test]
fn test_custom_provider_reads_every_flat_field() -> Result<(), serde_json::Error> {
    let registry = ProviderRegistry::from_json(
        r#"{"providers":[{"name":"proxy","response":{"usage_path":"meta.usage",
            "prompt_tokens":"in","completion_tokens":"out","completion_optional":true,
            "extra_prompt_tokens":"tool","thoughts_tokens":"think","completion_excludes_thoughts":true,"cached_tokens":"hit",
            "cache_write_tokens":"put","cache_write_long_ttl_tokens":"put_long","prompt_excludes_cache":true,
            "service_tier_path":"tier","speed":"mode","inference_geo":"geo",
            "model_path":"served.model","required_field":"marker"}}]}"#,
    )?;
    let provider = ConfigurableProvider::new(registry.providers[0].clone());
    let body = json!({"tier": "flex", "served": {"model": "m-1"}, "meta": {"usage": {
        "in": 10, "tool": 2, "think": 5, "hit": 3, "put": 4, "put_long": 9, "mode": "fast", "geo": "us", "marker": {}}}});
    let usage = provider.parse_usage(&body).unwrap();
    let expected = UsageInfo {
        prompt_tokens: 19,
        completion_tokens: 5,
        input_modalities: ModalityTokens::default(),
        output_modalities: ModalityTokens::default(),
        thoughts_tokens: Some(5),
        cache_read_tokens: Some(3),
        cache_write_tokens: Some(4),
        cache_write_long_ttl_tokens: Some(4),
        model: Some("m-1".to_string()),
        operation: Operation::Chat,
        service_tier: Some("flex".to_string()),
        speed: Some("fast".to_string()),
        inference_geo: Some("us".to_string()),
        iterations: Vec::new(),
        cost_suppressed: false,
    };
    assert_eq!(format!("{usage:?}"), format!("{expected:?}"));

    let mut no_marker = body;
    no_marker["meta"]["usage"]
        .as_object_mut()
        .unwrap()
        .remove("marker");
    assert!(provider.parse_usage(&no_marker).is_none());
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
    // Entries 0-7 are the OpenAI (embeddings, chat, image, Responses, transcription), Anthropic and Gemini (embeddings, chat) first-party providers.
    let (before, _) = registry.providers.split_at(8);

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

// Gemini Interactions non-streaming body, with the usage numbers from the API reference example.
#[test]
fn test_gemini_interactions_non_streaming_parses_usage() {
    let provider = default_provider("generativelanguage.googleapis.com", "/v1beta/interactions");
    let response = json!({
        "model": "gemini-3.6-flash",
        "status": "completed",
        "usage": {
            "total_input_tokens": 7,
            "total_output_tokens": 23,
            "total_thought_tokens": 49,
            "total_cached_tokens": 0,
            "total_tool_use_tokens": 0,
            "total_tokens": 79
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 7);
    // total_output_tokens excludes thoughts (7 + 23 + 49 = 79 = total_tokens), so output is 23 + 49.
    assert_eq!(usage.completion_tokens, 72);
    assert_eq!(usage.thoughts_tokens, Some(49));
    assert_eq!(usage.cache_read_tokens, Some(0));
    assert_eq!(usage.model, Some("gemini-3.6-flash".to_string()));
}

// total_tool_use_tokens (tool results billed as input, outside total_input_tokens) is added to the
// prompt as generateContent's toolUsePromptTokenCount is; a Vertex host/path reaches the same entry.
#[test]
fn test_gemini_interactions_tool_use_tokens_add_to_input() {
    let provider = default_provider(
        "aiplatform.googleapis.com",
        "/v1/projects/p/locations/global/interactions",
    );
    let response = json!({
        "model": "gemini-3.6-flash",
        "status": "completed",
        "usage": {
            "total_input_tokens": 11,
            "total_output_tokens": 90,
            "total_thought_tokens": 245,
            "total_cached_tokens": 0,
            "total_tool_use_tokens": 100,
            "total_tokens": 446
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    // Input grows by the tool-use tokens (11 + 100); output is total_output + thoughts (90 + 245).
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (111, 335));
}

// total_cached_tokens is part of total_input_tokens, so the prompt is not grown by the cache read.
#[test]
fn test_gemini_interactions_cached_is_part_of_input() {
    let provider = default_provider("generativelanguage.googleapis.com", "/v1/interactions");
    let response = json!({
        "model": "gemini-3.6-flash",
        "usage": {
            "total_input_tokens": 1000,
            "total_output_tokens": 40,
            "total_cached_tokens": 200,
            "total_tokens": 1240
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 1000);
    assert_eq!(usage.cache_read_tokens, Some(200));
}

// A zero-output interaction may omit total_output_tokens, as generateContent omits candidatesTokenCount;
// completion_optional keeps the call (output 0) instead of dropping it and losing the input tokens.
#[test]
fn test_gemini_interactions_missing_output_tokens_records_zero() {
    let provider = default_provider("generativelanguage.googleapis.com", "/v1beta/interactions");
    let response = json!({
        "model": "gemini-3.6-flash",
        "usage": {"total_input_tokens": 12, "total_tokens": 12}
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (12, 0));
}

// required_field keeps the Interactions entry from claiming another provider's top-level `usage`.
#[test]
fn test_gemini_interactions_requires_total_input_tokens() {
    let provider = default_provider("generativelanguage.googleapis.com", "/v1beta/interactions");
    // An OpenAI-shaped usage reaches this entry (usage_path "usage") but has no total_input_tokens.
    let openai = json!({
        "model": "gpt-4o",
        "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
    });
    assert!(provider.parse_usage(&openai).is_none());
}

// Image generation reports input_tokens/output_tokens, the Responses key names; the per-modality
// text/image split is a follow-up, so only the totals are read here. gpt-image-1 is not in the
// price table, so the call stays unpriced rather than recording $0.
#[test]
fn test_openai_image_generation_records_totals() {
    let response = json!({
        "created": 1731459200,
        "data": [{"b64_json": "iVBORw0KGgo="}],
        "usage": {
            "input_tokens": 50,
            "input_tokens_details": {"text_tokens": 50, "image_tokens": 0},
            "output_tokens": 1024,
            "output_tokens_details": {"text_tokens": 0, "image_tokens": 1024},
            "total_tokens": 1074
        }
    });
    let usage = default_provider("api.openai.com", "/v1/images/generations")
        .parse_usage(&response)
        .unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (50, 1024));
    assert_eq!(usage.model, None);
}

// output_tokens_details.image_tokens tells image responses apart from Responses, which has
// output_tokens_details.reasoning_tokens instead, so the image entry must not claim a Responses body.
#[test]
fn test_image_entry_does_not_claim_responses_usage() {
    let registry = ProviderRegistry::with_defaults();
    let responses = json!({
        "model": "gpt-6", "status": "completed", "error": null,
        "usage": {
            "input_tokens": 30, "input_tokens_details": {"cached_tokens": 12},
            "output_tokens": 9, "output_tokens_details": {"reasoning_tokens": 2}, "total_tokens": 39
        }
    });
    let usage = parse_in_order(&registry.providers, &responses).unwrap();
    // Parsed by the Responses entry (reasoning and cache read), not the image entry.
    assert_eq!(usage.thoughts_tokens, Some(2));
    assert_eq!(usage.cache_read_tokens, Some(12));
}

// The token-usage transcription variant carries usage.type=tokens with input_tokens/output_tokens.
#[test]
fn test_openai_transcription_tokens_records_totals() {
    let response = json!({
        "text": "hello world",
        "usage": {
            "type": "tokens",
            "input_tokens": 14,
            "input_token_details": {"text_tokens": 0, "audio_tokens": 14},
            "output_tokens": 4,
            "total_tokens": 18
        }
    });
    let usage = default_provider("api.openai.com", "/v1/audio/transcriptions")
        .parse_usage(&response)
        .unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (14, 4));
}

// The duration-billed variant has seconds, not tokens, so it records nothing here (a follow-up),
// and no other provider claims it either.
#[test]
fn test_openai_transcription_duration_is_not_recorded() {
    let registry = ProviderRegistry::with_defaults();
    let response = json!({"text": "hi", "usage": {"type": "duration", "seconds": 3.2}});
    assert!(
        default_provider("api.openai.com", "/v1/audio/transcriptions")
            .parse_usage(&response)
            .is_none()
    );
    assert!(parse_in_order(&registry.providers, &response).is_none());
}

// usage.type keeps the transcription entry off the anthropic body, which has no usage.type and must
// still be read by the anthropic entry (prompt includes cache).
#[test]
fn test_transcription_entry_does_not_claim_anthropic_usage() {
    let transcription = default_provider("api.openai.com", "/v1/audio/transcriptions");
    let anthropic = json!({
        "type": "message", "model": "claude-opus-5-5",
        "usage": {"input_tokens": 50, "cache_read_input_tokens": 100000, "output_tokens": 300}
    });
    assert!(transcription.parse_usage(&anthropic).is_none());
}

// An image response may omit output_tokens_details (optional), so the image entry's required_field is
// unmet; the Responses entry then reads the same input_tokens/output_tokens totals and the call is
// still recorded.
#[test]
fn test_image_without_output_details_is_recorded_via_responses() {
    let registry = ProviderRegistry::with_defaults();
    let response = json!({"created": 1, "data": [{"b64_json": "AA=="}], "usage": {
        "input_tokens": 50, "input_tokens_details": {"text_tokens": 50, "image_tokens": 0},
        "output_tokens": 1024, "total_tokens": 1074}});
    assert!(
        default_provider("api.openai.com", "/v1/images/generations")
            .parse_usage(&response)
            .is_none()
    );
    let usage = parse_in_order(&registry.providers, &response).unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (50, 1024));
}

// Audio tokens stay folded into the chat totals; splitting them by modality is a follow-up.
#[test]
fn test_chat_audio_tokens_stay_on_the_chat_entry() {
    let registry = ProviderRegistry::with_defaults();
    let response = json!({"model": "gpt-4o-audio-preview", "usage": {
        "prompt_tokens": 100, "completion_tokens": 200,
        "prompt_tokens_details": {"audio_tokens": 30, "cached_tokens": 10},
        "completion_tokens_details": {"audio_tokens": 150, "reasoning_tokens": 5}}});
    let usage = parse_in_order(&registry.providers, &response).unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (100, 200));
    assert_eq!(usage.thoughts_tokens, Some(5));
    assert_eq!(usage.cache_read_tokens, Some(10));
}

// The media entries' required_field keeps them from claiming another provider's usage shape that
// reaches them while parsing tries providers in order.
#[test]
fn test_media_entries_do_not_claim_other_providers_usage() {
    let image = default_provider("api.openai.com", "/v1/images/generations");
    let transcription = default_provider("api.openai.com", "/v1/audio/transcriptions");
    for body in [
        json!({"usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 3}}),
        json!({"usage": {"total_input_tokens": 7, "total_output_tokens": 2}}),
        json!({"usage": {"inputTokens": 30, "outputTokens": 628, "totalTokens": 658}}),
        json!({"amazon-bedrock-invocationMetrics": {"inputTokenCount": 3, "outputTokenCount": 4}}),
        json!({"type": "message", "usage": {"input_tokens": 5, "output_tokens": 6}}),
    ] {
        assert!(image.parse_usage(&body).is_none(), "image claimed {body}");
        assert!(
            transcription.parse_usage(&body).is_none(),
            "transcription claimed {body}"
        );
    }
}

// A token-variant transcription with a missing or non-integer count is not usable, so nothing records it.
#[test]
fn test_transcription_tokens_with_bad_counts_is_not_recorded() {
    let registry = ProviderRegistry::with_defaults();
    for usage in [
        json!({"type": "tokens", "output_tokens": 4}),
        json!({"type": "tokens", "input_tokens": 14}),
        json!({"type": "tokens", "input_tokens": "14", "output_tokens": 4}),
        json!({"type": "tokens", "input_tokens": 14.0, "output_tokens": 4}),
    ] {
        let body = json!({"text": "hi", "usage": usage});
        assert!(
            parse_in_order(&registry.providers, &body).is_none(),
            "{body}"
        );
    }
}

// An embeddings response (input tokens only) is recorded with operation=embeddings. The chat
// entry has no completion field as optional, so it cannot claim the same body, which is why the
// embeddings entry can sit after it without stealing chat responses.
#[test]
fn test_openai_embeddings_recorded_as_embedding_operation() {
    let embeddings = json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
        "model": "text-embedding-3-small",
        "usage": {"prompt_tokens": 410000, "total_tokens": 410000}
    });

    let provider = default_provider("api.openai.com", "/v1/embeddings");
    let usage = provider.parse_usage(&embeddings).unwrap();
    assert_eq!(usage.operation, Operation::Embeddings);
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (410000, 0));
    assert_eq!(usage.model.as_deref(), Some("text-embedding-3-small"));

    // The chat entry requires completion_tokens, so it leaves the embeddings body for this entry.
    let chat = default_provider("api.openai.com", "/v1/chat/completions");
    assert!(chat.parse_usage(&embeddings).is_none());

    // A chat response keeps operation=chat (the default for every other entry).
    let chat_body = json!({
        "model": "gpt-5.4-mini",
        "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}
    });
    assert_eq!(
        chat.parse_usage(&chat_body).unwrap().operation,
        Operation::Chat
    );
}

// Operation must not flip when a proxy embeddings body carries completion_tokens 0
// (LiteLLM/vLLM/Mistral), and a generation body missing its completion count must not be claimed as an
// embedding. First-match over the default order, as the agent parses a response body (http/body.rs).
#[test]
fn test_embeddings_entry_only_claims_embeddings_envelopes() {
    let providers: Vec<ConfigurableProvider> = ProviderRegistry::with_defaults()
        .providers
        .into_iter()
        .map(ConfigurableProvider::new)
        .collect();
    let first = |body: &serde_json::Value| providers.iter().find_map(|p| p.parse_usage(body));

    // Proxy embeddings: object "list" and completion_tokens 0. The embeddings entry claims it first.
    let proxy_embeddings = json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1]}],
        "model": "text-embedding-3-small",
        "usage": {"prompt_tokens": 8, "completion_tokens": 0, "total_tokens": 8}
    });
    let u = first(&proxy_embeddings).expect("proxy embeddings recorded");
    assert_eq!(u.operation, Operation::Embeddings);
    assert_eq!((u.prompt_tokens, u.completion_tokens), (8, 0));

    // A chat body missing its completion count is not an embeddings envelope, so no entry claims it,
    // rather than being mislabelled as an embedding.
    let malformed_chat = json!({
        "object": "chat.completion",
        "model": "gpt-5.4-mini",
        "choices": [],
        "usage": {"prompt_tokens": 8, "total_tokens": 8}
    });
    assert!(first(&malformed_chat).is_none());

    // A normal chat body stays chat.
    let chat = json!({
        "object": "chat.completion",
        "model": "gpt-5.4-mini",
        "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}
    });
    assert_eq!(first(&chat).unwrap().operation, Operation::Chat);
}

// A Gemini :embedContent response carries usageMetadata.promptTokenCount but no candidates and
// no model field; it is recorded with operation=embeddings and an unknown model.
#[test]
fn test_gemini_embeddings_recorded_as_embedding_operation() {
    let body = json!({
        "embedding": {"values": [0.1, 0.2]},
        "usageMetadata": {"promptTokenCount": 7, "promptTokenDetails": [{"modality": "TEXT", "tokenCount": 7}]}
    });

    let provider = default_provider(
        "generativelanguage.googleapis.com",
        "/v1beta/models/gemini-embedding-2:embedContent",
    );
    let usage = provider.parse_usage(&body).unwrap();
    assert_eq!(usage.operation, Operation::Embeddings);
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (7, 0));
    assert_eq!(usage.model, None);
}

// The chat entry reads the same usageMetadata keys with completion_optional, so the embeddings entry
// is guarded by the root `embedding`: a generateContent body (no such field) stays chat, first match
// over the default order (http/body.rs).
#[test]
fn test_gemini_embeddings_entry_only_claims_embedding_envelopes() {
    let providers: Vec<ConfigurableProvider> = ProviderRegistry::with_defaults()
        .providers
        .into_iter()
        .map(ConfigurableProvider::new)
        .collect();
    let first = |body: &serde_json::Value| providers.iter().find_map(|p| p.parse_usage(body));

    let chat = json!({
        "modelVersion": "gemini-3.5-flash",
        "candidates": [],
        "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 20, "totalTokenCount": 120}
    });
    let u = first(&chat).expect("chat recorded");
    assert_eq!(u.operation, Operation::Chat);
    assert_eq!(u.model.as_deref(), Some("gemini-3.5-flash"));

    let embed = json!({"embedding": {"values": [0.1]}, "usageMetadata": {"promptTokenCount": 8}});
    assert_eq!(first(&embed).unwrap().operation, Operation::Embeddings);
}

// gemini-embedding-001 and synchronous :batchEmbedContents carry no usageMetadata, so no entry
// records them; the agent counts the framed response as usage_absent instead.
#[test]
fn test_gemini_embeddings_without_usage_are_not_recorded() {
    let providers: Vec<ConfigurableProvider> = ProviderRegistry::with_defaults()
        .providers
        .into_iter()
        .map(ConfigurableProvider::new)
        .collect();
    let first = |body: &serde_json::Value| providers.iter().find_map(|p| p.parse_usage(body));

    assert!(first(&json!({"embedding": {"values": [0.1, 0.2]}})).is_none());
    assert!(first(&json!({"embeddings": [{"values": [0.1]}, {"values": [0.2]}]})).is_none());
}

// Meta Model API (Muse Spark, api.meta.ai): OpenAI chat/Responses and Anthropic Messages shapes on
// one host, read with the OpenAI chat, OpenAI Responses and anthropic response blocks respectively.
fn meta_provider(path: &str) -> ConfigurableProvider {
    let registry = ProviderRegistry::with_defaults();
    let config = registry.find_provider("api.meta.ai", path).unwrap().clone();
    assert_eq!(config.name, "meta", "{path}");
    ConfigurableProvider::new(config)
}

#[test]
fn test_meta_chat_reads_openai_shape_with_reasoning() {
    let provider = meta_provider("/v1/chat/completions");
    let response = json!({
        "model": "muse-spark-1",
        "usage": {
            "prompt_tokens": 30,
            "completion_tokens": 40,
            "completion_tokens_details": {"reasoning_tokens": 12},
            "prompt_tokens_details": {"cached_tokens": 8}
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 30);
    assert_eq!(usage.completion_tokens, 40);
    assert_eq!(usage.thoughts_tokens, Some(12));
    assert_eq!(usage.cache_read_tokens, Some(8));
    assert_eq!(usage.model, Some("muse-spark-1".to_string()));
    assert_eq!(usage.operation, Operation::Chat);
}

#[test]
fn test_meta_responses_reads_openai_responses_shape() {
    let provider = meta_provider("/v1/responses");
    let response = json!({
        "model": "muse-spark-1",
        "status": "completed",
        "error": null,
        "usage": {
            "input_tokens": 50,
            "input_tokens_details": {"cached_tokens": 10},
            "output_tokens": 25,
            "output_tokens_details": {"reasoning_tokens": 7}
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    assert_eq!(usage.prompt_tokens, 50);
    assert_eq!(usage.completion_tokens, 25);
    assert_eq!(usage.thoughts_tokens, Some(7));
    assert_eq!(usage.cache_read_tokens, Some(10));
    assert_eq!(usage.model, Some("muse-spark-1".to_string()));
}

#[test]
fn test_meta_messages_reads_anthropic_shape() {
    let provider = meta_provider("/v1/messages");
    let response = json!({
        "type": "message",
        "model": "muse-spark-1",
        "usage": {
            "input_tokens": 50,
            "cache_creation_input_tokens": 2000,
            "cache_read_input_tokens": 100000,
            "output_tokens": 300
        }
    });

    let usage = provider.parse_usage(&response).unwrap();
    // Anthropic shape: prompt = input + cache read + write.
    assert_eq!(usage.prompt_tokens, 102_050);
    assert_eq!(usage.completion_tokens, 300);
    assert_eq!(usage.cache_read_tokens, Some(100_000));
    assert_eq!(usage.cache_write_tokens, Some(2000));
    assert_eq!(usage.model, Some("muse-spark-1".to_string()));
}
