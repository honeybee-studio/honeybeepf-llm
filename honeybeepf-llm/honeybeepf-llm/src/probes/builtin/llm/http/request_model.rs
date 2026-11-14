//! The model a request names, for responses that carry none (Gemini Decisions, Bedrock Converse).

use serde::Deserialize;

/// Longest model name taken from a request; a longer value is not a model name and would bloat the label.
const MAX_MODEL_LEN: usize = 128;
/// Bedrock runtime operations whose path names the model: `/model/{modelId}/{operation}`.
const BEDROCK_OPERATIONS: [&str; 4] = [
    "converse",
    "converse-stream",
    "invoke",
    "invoke-with-response-stream",
];

#[derive(Deserialize)]
struct Named {
    model: Option<String>,
}

/// The model an HTTP/1.1 request names in its path or, failing that, in its JSON body.
pub(crate) fn from_h1(request: &[u8]) -> Option<String> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let httparse::Status::Complete(head) = req.parse(request).ok()? else {
        return None;
    };
    if let Some(model) = req.path.and_then(from_path) {
        return Some(model);
    }
    let body = &request[head..];
    let len = req
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("Content-Length"))
        .and_then(|h| {
            std::str::from_utf8(h.value)
                .ok()?
                .trim()
                .parse::<usize>()
                .ok()
        });
    from_body(match len {
        Some(len) => body.get(..len)?,
        None => body,
    })
}

/// The model in a Bedrock runtime path, percent-decoded. None for other paths and for ARNs, which name an
/// account's inference profile, not a model.
pub(crate) fn from_path(path: &str) -> Option<String> {
    let path = path.split_once('?').map_or(path, |(p, _)| p);
    let (id, operation) = path.strip_prefix("/model/")?.rsplit_once('/')?;
    if !BEDROCK_OPERATIONS.contains(&operation) {
        return None;
    }
    let id = percent_decode(id)?;
    if id.starts_with("arn:") {
        return None;
    }
    checked(id)
}

/// The top-level `model` string of a JSON request body.
pub(crate) fn from_body(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Named>(body)
        .ok()?
        .model
        .and_then(checked)
}

fn checked(model: String) -> Option<String> {
    (!model.is_empty()
        && model.len() <= MAX_MODEL_LEN
        && model.bytes().all(|b| b.is_ascii_graphic()))
    .then_some(model)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bedrock_paths_name_the_model() {
        let id = "anthropic.claude-sonnet-4-5-20250929-v1:0";
        for op in BEDROCK_OPERATIONS {
            assert_eq!(
                from_path(&format!("/model/{id}/{op}")).as_deref(),
                Some(id),
                "{op}"
            );
        }
        assert_eq!(
            from_path("/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse").as_deref(),
            Some("us.anthropic.claude-sonnet-4-5-20250929-v1:0")
        );
    }

    #[test]
    fn other_paths_and_arns_name_none() {
        for path in [
            "/v1/chat/completions",
            "/model/m/count-tokens",
            "/model//converse/extra",
            "/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Aapplication-inference-profile%2Fabc/converse",
            "/model/bad%zz/converse",
        ] {
            assert_eq!(from_path(path), None, "{path}");
        }
    }

    #[test]
    fn body_model_is_read_from_the_top_level_only() {
        let decisions = br#"{"model":"gemini-decision-1","input":"Is this a refund request?","questions":{"q":{"type":"noul","instructions":"?"}}}"#;
        assert_eq!(from_body(decisions).as_deref(), Some("gemini-decision-1"));
        assert_eq!(from_body(br#"{"input":{"model":"nested"}}"#), None);
        assert_eq!(from_body(br#"{"model":{"id":"x"}}"#), None);
        assert_eq!(from_body(b"not json"), None);
    }

    #[test]
    fn odd_model_values_are_dropped() {
        let long = format!(r#"{{"model":"{}"}}"#, "m".repeat(MAX_MODEL_LEN + 1));
        for body in [
            long.as_str(),
            r#"{"model":""}"#,
            r#"{"model":"a b"}"#,
            r#"{"model":"a\nb"}"#,
        ] {
            assert_eq!(from_body(body.as_bytes()), None, "{body}");
        }
    }

    #[test]
    fn h1_request_uses_the_path_then_the_body() {
        let body = r#"{"model":"gemini-decision-1","input":"hi"}"#;
        let decisions = format!(
            "POST /v1beta/decisions HTTP/1.1\r\nHost: generativelanguage.googleapis.com\r\nContent-Length: {}\r\n\r\n{body}POST /next HTTP/1.1\r\n",
            body.len()
        );
        assert_eq!(
            from_h1(decisions.as_bytes()).as_deref(),
            Some("gemini-decision-1")
        );

        let no_length = format!("POST /v1beta/decisions HTTP/1.1\r\nHost: g\r\n\r\n{body}");
        assert_eq!(
            from_h1(no_length.as_bytes()).as_deref(),
            Some("gemini-decision-1")
        );

        let converse = "POST /model/amazon.nova-pro-v1%3A0/converse HTTP/1.1\r\nHost: bedrock-runtime.us-east-1.amazonaws.com\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(
            from_h1(converse.as_bytes()).as_deref(),
            Some("amazon.nova-pro-v1:0")
        );

        let partial = "POST /v1beta/decisions HTTP/1.1\r\nContent-Length: 500\r\n\r\n{\"model\":\"gemini-decision-1\"";
        assert_eq!(from_h1(partial.as_bytes()), None);
    }
}
