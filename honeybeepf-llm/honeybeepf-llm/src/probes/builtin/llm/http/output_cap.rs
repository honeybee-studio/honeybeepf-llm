//! The output-token cap a request sets, to flag a response that reports more output than it allowed: a
//! provider counting (and billing) tokens it could not have generated.

use honeybeepf_llm_core::types::UsageInfo;
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use serde_json::Value;

/// The most output tokens a request allows, over all the choices it asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputCap {
    pub(crate) tokens: u64,
    /// The cap is documented to bound reasoning too (OpenAI `max_completion_tokens`, `max_output_tokens`).
    /// Elsewhere only the visible output is compared, as providers differ on whether reasoning counts.
    pub(crate) with_reasoning: bool,
}

impl OutputCap {
    /// The reported output tokens over the cap, if any. Non-text output is left out: not every provider
    /// counts it toward the cap.
    pub(crate) fn excess(self, usage: &UsageInfo) -> Option<u64> {
        let mut output = usage
            .completion_tokens
            .saturating_sub(usage.output_modalities.non_text());
        if !self.with_reasoning {
            output = output.saturating_sub(usage.thoughts_tokens.unwrap_or(0));
        }
        output.checked_sub(self.tokens).filter(|&over| over > 0)
    }
}

#[derive(Deserialize)]
struct Request {
    #[serde(default, deserialize_with = "lenient")]
    max_completion_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    max_output_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    max_tokens: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    n: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    best_of: Option<u64>,
    #[serde(rename = "generationConfig", default, deserialize_with = "lenient")]
    generation_config: Option<GenerationConfig>,
    #[serde(rename = "inferenceConfig", default, deserialize_with = "lenient")]
    inference_config: Option<InferenceConfig>,
    #[serde(default, deserialize_with = "lenient")]
    tools: Option<Vec<Value>>,
    /// Predicted Outputs bill rejected prediction tokens as output, beyond what was generated.
    prediction: Option<serde::de::IgnoredAny>,
    /// Anthropic's MCP connector and context management run more model work under one request.
    mcp_servers: Option<serde::de::IgnoredAny>,
    context_management: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct GenerationConfig {
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: Option<u64>,
    #[serde(rename = "candidateCount")]
    candidate_count: Option<u64>,
}

#[derive(Deserialize)]
struct InferenceConfig {
    #[serde(rename = "maxTokens")]
    max_tokens: Option<u64>,
}

/// A field of an unexpected shape reads as absent instead of failing the whole body.
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Option<T>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?.and_then(|v| serde_json::from_value(v).ok()))
}

/// A tool the app runs itself; any other kind (web search, code execution, an MCP server) may let the
/// provider sample more than once under one cap.
fn client_tool(tool: &Value) -> bool {
    match tool.get("type").and_then(Value::as_str) {
        Some(kind) => matches!(kind, "function" | "custom"),
        // Anthropic custom tools have no type; Gemini lists functions under `functionDeclarations`.
        None => {
            tool.get("name").is_some()
                || tool
                    .as_object()
                    .is_some_and(|o| o.keys().all(|k| k == "functionDeclarations"))
        }
    }
}

/// The output cap of an HTTP/1.1 request's JSON body; None without one, or when the provider may run
/// more than one generation under it.
pub(crate) fn from_h1(request: &[u8]) -> Option<OutputCap> {
    from_body(super::request_model::h1_parts(request)?.1?)
}

/// The output cap of a JSON request body.
pub(crate) fn from_body(body: &[u8]) -> Option<OutputCap> {
    let req: Request = serde_json::from_slice(body).ok()?;
    if req.prediction.is_some() || req.mcp_servers.is_some() || req.context_management.is_some() {
        return None;
    }
    let generation = req.generation_config.as_ref();
    let choices = [
        req.n,
        req.best_of,
        generation.and_then(|g| g.candidate_count),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(1)
    .max(1);
    let (tokens, with_reasoning) = match (req.max_completion_tokens, req.max_output_tokens) {
        (Some(cap), _) | (None, Some(cap)) => (cap, true),
        (None, None) => {
            if !req.tools.iter().flatten().all(client_tool) {
                return None;
            }
            let cap = req
                .max_tokens
                .or_else(|| generation.and_then(|g| g.max_output_tokens))
                .or_else(|| req.inference_config.and_then(|c| c.max_tokens))?;
            (cap, false)
        }
    };
    Some(OutputCap {
        tokens: tokens.checked_mul(choices)?,
        with_reasoning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(tokens: u64, with_reasoning: bool) -> OutputCap {
        OutputCap {
            tokens,
            with_reasoning,
        }
    }

    #[test]
    fn each_provider_names_its_cap() {
        let cases = [
            (
                r#"{"model":"gpt-5.6","max_output_tokens":16384}"#,
                Some(cap(16384, true)),
            ),
            (
                r#"{"messages":[],"max_completion_tokens":100,"n":3}"#,
                Some(cap(300, true)),
            ),
            (
                r#"{"max_tokens":50,"max_completion_tokens":100}"#,
                Some(cap(100, true)),
            ),
            (
                r#"{"max_tokens":1024,"messages":[]}"#,
                Some(cap(1024, false)),
            ),
            (
                r#"{"prompt":"x","max_tokens":16,"best_of":4,"n":2}"#,
                Some(cap(64, false)),
            ),
            (
                r#"{"contents":[],"generationConfig":{"maxOutputTokens":200,"candidateCount":2}}"#,
                Some(cap(400, false)),
            ),
            (
                r#"{"messages":[],"inferenceConfig":{"maxTokens":512}}"#,
                Some(cap(512, false)),
            ),
        ];
        for (body, want) in cases {
            assert_eq!(from_body(body.as_bytes()), want, "{body}");
        }
    }

    #[test]
    fn no_cap_or_a_server_side_loop_reads_none() {
        for body in [
            r#"{"model":"gpt-4o","messages":[]}"#,
            r#"{"max_tokens":"lots"}"#,
            r#"{"max_tokens":-1}"#,
            r#"{"max_completion_tokens":10,"prediction":{"type":"content","content":"x"}}"#,
            r#"{"max_tokens":10,"tools":[{"type":"web_search_20250305","name":"web_search"}]}"#,
            r#"{"max_tokens":10,"mcp_servers":[{"type":"url","url":"https://x"}]}"#,
            r#"{"generationConfig":{"maxOutputTokens":10},"tools":[{"googleSearch":{}}]}"#,
            "not json",
        ] {
            assert_eq!(from_body(body.as_bytes()), None, "{body}");
        }
    }

    #[test]
    fn client_tools_keep_the_cap() {
        for body in [
            r#"{"max_tokens":10,"tools":[{"type":"function","function":{"name":"f"}}]}"#,
            r#"{"max_tokens":10,"tools":[{"name":"f","input_schema":{"type":"object"}}]}"#,
            r#"{"generationConfig":{"maxOutputTokens":10},"tools":[{"functionDeclarations":[]}]}"#,
        ] {
            assert_eq!(from_body(body.as_bytes()), Some(cap(10, false)), "{body}");
        }
    }

    #[test]
    fn a_shape_mismatch_in_one_field_keeps_the_others() {
        let body = r#"{"max_tokens":10,"n":"two","generationConfig":"x","tools":{}}"#;
        assert_eq!(from_body(body.as_bytes()), Some(cap(10, false)));
    }

    #[test]
    fn excess_compares_what_the_cap_bounds() {
        let usage = UsageInfo {
            completion_tokens: 273_250,
            thoughts_tokens: Some(273_000),
            ..UsageInfo::default()
        };
        assert_eq!(cap(16384, true).excess(&usage), Some(256_866));
        // Visible output alone (250) is under the cap.
        assert_eq!(cap(16384, false).excess(&usage), None);
        assert_eq!(cap(273_250, true).excess(&usage), None);

        let mut image = UsageInfo {
            completion_tokens: 1300,
            ..UsageInfo::default()
        };
        image.output_modalities.image = 1290;
        assert_eq!(cap(10, false).excess(&image), None);
    }

    #[test]
    fn h1_requests_read_the_body_within_content_length() {
        let body = r#"{"max_output_tokens":8}"#;
        let request = format!(
            "POST /v1/responses HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}GET /next",
            body.len()
        );
        assert_eq!(from_h1(request.as_bytes()), Some(cap(8, true)));
        let short = format!("POST /v1/responses HTTP/1.1\r\nContent-Length: 99\r\n\r\n{body}");
        assert_eq!(from_h1(short.as_bytes()), None);
    }
}
