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
