//! Provider request extractor tests.
//!
//! These go through the `get_extractor()` factory instead of constructing the
//! individual extractor structs directly, so `MessagesExtractor`, `ContentsExtractor`,
//! `PromptExtractor`, and `NoOpExtractor` can stay private to the `request` module.

use honeybeepf_llm_core::providers::{RequestExtractorType, get_extractor};
use serde_json::json;

#[test]
fn test_messages_extractor() {
    let extractor = get_extractor(&RequestExtractorType::Messages);
    let json = json!({
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi there"},
            {"role": "user", "content": "How are you?"}
        ]
    });
    let result = extractor.extract(&json);
    assert_eq!(result, "Hello Hi there How are you?");
}

#[test]
fn test_messages_extractor_with_content_blocks() {
    let extractor = get_extractor(&RequestExtractorType::Messages);
    let json = json!({
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "What's in this image?"},
                    {"type": "image_url", "image_url": {"url": "..."}}
                ]
            }
        ]
    });
    let result = extractor.extract(&json);
    assert_eq!(result, "What's in this image?");
}

#[test]
fn test_contents_extractor() {
    let extractor = get_extractor(&RequestExtractorType::Contents);
    let json = json!({
        "contents": [
            {
                "parts": [
                    {"text": "Hello from Gemini"}
                ]
            }
        ]
    });
    let result = extractor.extract(&json);
    assert_eq!(result, "Hello from Gemini");
}

#[test]
fn test_prompt_extractor() {
    let extractor = get_extractor(&RequestExtractorType::Prompt);
    let json = json!({
        "prompt": "Complete this sentence:"
    });
    let result = extractor.extract(&json);
    assert_eq!(result, "Complete this sentence:");
}

#[test]
fn test_input_extractor_string() {
    let extractor = get_extractor(&RequestExtractorType::Input);
    let json = json!({"model": "gpt-6", "input": "Write a haiku"});
    assert_eq!(extractor.extract(&json), "Write a haiku");
}

#[test]
fn test_input_extractor_message_items() {
    let extractor = get_extractor(&RequestExtractorType::Input);
    let json = json!({
        "input": [
            {"role": "developer", "content": "Be brief"},
            {
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "What is this?"},
                    {"type": "input_image", "image_url": "..."}
                ]
            },
            {"type": "function_call_output", "call_id": "c1", "output": "42"}
        ]
    });
    assert_eq!(extractor.extract(&json), "Be brief What is this?");
}

#[test]
fn test_noop_extractor_returns_empty() {
    let extractor = get_extractor(&RequestExtractorType::None);
    let json = json!({
        "messages": [{"role": "user", "content": "should be ignored"}],
        "prompt": "should also be ignored"
    });
    assert_eq!(extractor.extract(&json), "");
}
