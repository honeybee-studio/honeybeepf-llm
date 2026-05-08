use honeybeepf_llm_core::byte_utils::get_nested_value;
use serde_json::json;

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
