use serde_json::Value;

use super::config::RequestExtractorType;

/// Trait for extracting text from request JSON
pub trait RequestExtractor: Send + Sync {
    fn extract(&self, json: &Value) -> String;
}

/// Get extractor for the given type
pub fn get_extractor(extractor_type: &RequestExtractorType) -> Box<dyn RequestExtractor> {
    match extractor_type {
        RequestExtractorType::Messages => Box::new(MessagesExtractor),
        RequestExtractorType::Contents => Box::new(ContentsExtractor),
        RequestExtractorType::Prompt => Box::new(PromptExtractor),
        RequestExtractorType::None => Box::new(NoOpExtractor),
    }
}

/// OpenAI/Anthropic style: messages[].content
struct MessagesExtractor;

impl RequestExtractor for MessagesExtractor {
    fn extract(&self, json: &Value) -> String {
        let mut texts = Vec::new();
        if let Some(messages) = json.get("messages").and_then(|m| m.as_array()) {
            for msg in messages {
                if let Some(content) = msg.get("content") {
                    if let Some(s) = content.as_str() {
                        texts.push(s.to_string());
                    } else if let Some(arr) = content.as_array() {
                        // Handle array of content blocks (e.g., with images)
                        for block in arr {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                texts.push(text.to_string());
                            }
                        }
                    }
                }
            }
        }
        texts.join(" ")
    }
}

/// Gemini style: contents[].parts[].text
struct ContentsExtractor;

impl RequestExtractor for ContentsExtractor {
    fn extract(&self, json: &Value) -> String {
        let mut texts = Vec::new();
        if let Some(contents) = json.get("contents").and_then(|c| c.as_array()) {
            for content in contents {
                if let Some(parts) = content.get("parts").and_then(|p| p.as_array()) {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                            texts.push(text.to_string());
                        }
                    }
                }
            }
        }
        texts.join(" ")
    }
}

/// Simple prompt field
struct PromptExtractor;

impl RequestExtractor for PromptExtractor {
    fn extract(&self, json: &Value) -> String {
        json.get("prompt")
            .and_then(|p| p.as_str())
            .unwrap_or("")
            .to_string()
    }
}

/// No-op extractor (returns empty string)
struct NoOpExtractor;

impl RequestExtractor for NoOpExtractor {
    fn extract(&self, _json: &Value) -> String {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_messages_extractor() {
        let extractor = MessagesExtractor;
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
        let extractor = MessagesExtractor;
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
        let extractor = ContentsExtractor;
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
        let extractor = PromptExtractor;
        let json = json!({
            "prompt": "Complete this sentence:"
        });
        let result = extractor.extract(&json);
        assert_eq!(result, "Complete this sentence:");
    }
}
