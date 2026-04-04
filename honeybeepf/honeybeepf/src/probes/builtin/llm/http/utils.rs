use std::borrow::Cow;

/// Find byte pattern in haystack
pub fn find_pattern(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
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
    let mut pos = 0;

    while pos < buffer.len() {
        let size_end = match find_pattern(&buffer[pos..], b"\r\n") {
            Some(i) => pos + i,
            None => break,
        };

        let size_str = String::from_utf8_lossy(&buffer[pos..size_end]);
        let chunk_size = match usize::from_str_radix(size_str.trim(), 16) {
            Ok(n) => n,
            Err(_) => break,
        };

        if chunk_size == 0 {
            break;
        }

        let data_start = size_end + 2;
        let data_end = data_start + chunk_size;

        if data_end > buffer.len() {
            result.extend_from_slice(&buffer[data_start..]);
            break;
        }

        result.extend_from_slice(&buffer[data_start..data_end]);
        pos = data_end + 2;
    }

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
