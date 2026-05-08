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
