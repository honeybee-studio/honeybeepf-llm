use honeybeepf_llm_core::byte_utils::{find_pattern, get_nested_value};
use serde_json::json;

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
