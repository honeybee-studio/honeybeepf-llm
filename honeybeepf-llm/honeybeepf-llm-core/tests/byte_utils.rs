use honeybeepf_llm_core::byte_utils::{decode_chunked_body, find_pattern, get_nested_value};
use serde_json::json;

#[test]
fn test_decode_chunked_body_size_past_the_buffer_keeps_the_rest() {
    let cases: [(&[u8], &[u8]); 4] = [
        (b"10\r\nok", b"ok"),
        (b"ffffffffffffffff\r\n{}\r\n0\r\n\r\n", b"{}\r\n0\r\n\r\n"),
        (b"2\r\nok\r\nffffffffffffffff\r\n{}", b"ok{}"),
        (b"2\r\nok\r\nfffffffffffffff9\r\n{}", b"ok{}"),
    ];
    for (body, want) in cases {
        let line = String::from_utf8_lossy(body);
        assert_eq!(&*decode_chunked_body(body), want, "{line}");
    }
}

#[test]
fn test_find_pattern_first_and_repeated_prefix() {
    assert_eq!(find_pattern(b"aaab", b"aab"), Some(1));
    assert_eq!(find_pattern(b"x api.openai.com", b"openai"), Some(6));
    assert_eq!(find_pattern(b"api.openai.co", b"api.openai.com"), None);
    assert_eq!(find_pattern(b"abc", b""), Some(0));
}

#[test]
fn test_nested_path() {
    let json = json!({
        "outer": {
            "inner": {
                "value": 42
            }
        }
    });

    let value = get_nested_value(&json, "outer.inner.value").unwrap();
    assert_eq!(value.as_u64(), Some(42));
}
