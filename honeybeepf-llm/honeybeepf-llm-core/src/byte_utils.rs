use std::borrow::Cow;

use serde_json::Value;

use crate::chunked::{ChunkRules, ChunkWalk};

/// Find byte pattern in haystack
pub fn find_pattern(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let Some((&first, rest)) = needle.split_first() else {
        return Some(0);
    };
    // Compare only where the first byte matches: a slice compare at every offset dominates the HTTP/2 host scan.
    let mut from = 0;
    while let Some(i) = haystack[from..].iter().position(|&b| b == first) {
        let at = from + i;
        if haystack[at + 1..].starts_with(rest) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// Check if haystack contains needle
pub fn contains_pattern(haystack: &[u8], needle: &[u8]) -> bool {
    find_pattern(haystack, needle).is_some()
}

/// Trim trailing ASCII whitespace
pub fn trim_trailing_whitespace(buf: &[u8]) -> &[u8] {
    let mut end = buf.len();
    while end > 0 && buf[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &buf[..end]
}

/// Decode HTTP Chunked Encoding
pub fn decode_chunked_body(buffer: &[u8]) -> Cow<'_, [u8]> {
    if !contains_pattern(buffer, b"\r\n") {
        return Cow::Borrowed(buffer);
    }

    let mut result = Vec::with_capacity(buffer.len());
    ChunkWalk::new(ChunkRules::BASIC).walk(buffer, |data| result.extend_from_slice(data));

    if result.is_empty() {
        Cow::Borrowed(buffer)
    } else {
        Cow::Owned(result)
    }
}

/// Extract the first balanced JSON object from an HTTP/2 byte stream.
pub fn extract_h2_json_payload(buffer: &[u8]) -> Cow<'_, [u8]> {
    extract_h2_json_nth(buffer, 0)
}

/// Extract all balanced JSON objects from an HTTP/2 byte stream.
pub fn extract_h2_json_all(buffer: &[u8]) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(buffer);
    let bytes = text.as_bytes();
    let mut results = Vec::new();
    let mut search_pos = 0;

    while let Some(offset) = text[search_pos..].find('{') {
        let start = search_pos + offset;
        if let Some(end) = find_balanced_brace(bytes, start) {
            results.push(text[start..=end].as_bytes().to_vec());
            search_pos = end + 1;
        } else {
            search_pos = start + 1;
        }
    }

    results
}

/// Look up a value in a nested JSON object using dotted-path notation.
pub fn get_nested_value<'a>(json: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = json;
    for key in path.split('.') {
        current = current.get(key)?;
    }
    Some(current)
}

fn extract_h2_json_nth(buffer: &[u8], n: usize) -> Cow<'_, [u8]> {
    let text = String::from_utf8_lossy(buffer);
    let bytes = text.as_bytes();
    let mut search_pos = 0;
    let mut count = 0;

    while let Some(offset) = text[search_pos..].find('{') {
        let start = search_pos + offset;
        if let Some(end) = find_balanced_brace(bytes, start) {
            if count == n {
                return Cow::Owned(text[start..=end].as_bytes().to_vec());
            }
            count += 1;
            search_pos = end + 1;
        } else {
            search_pos = start + 1;
        }
    }

    Cow::Borrowed(&[])
}

/// Find the closing brace index for balanced `{...}` starting at `start`.
fn find_balanced_brace(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0;
    let mut in_string = false;
    let mut escape = false;

    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if escape {
            escape = false;
            continue;
        }
        if b == b'\\' && in_string {
            escape = true;
            continue;
        }
        if b == b'"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }

        if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}
