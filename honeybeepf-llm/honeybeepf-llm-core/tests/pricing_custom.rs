mod common;

use common::{CallExt, MISSING, call, total};
use honeybeepf_llm_core::pricing::{Overrides, PriceTable};

const TABLE: &str = include_str!("fixtures/prices_test.json");

fn custom(providers: &[String]) -> String {
    let list = providers.join(", ");
    format!(r#"{{"version": "acme-1", "providers": [{list}]}}"#)
}

fn provider(name: &str, fields: &str) -> String {
    format!(r#"{{"name": "{name}", "source": "contract", "checked": "2026-10-01", {fields}}}"#)
}

#[test]
fn test_user_prices_replace_add_and_discount_per_model() {
    let mut table = PriceTable::from_json(TABLE).expect("test table loads");
    let openai = r#""models": {"gpt-5": {"input": 1, "output": 8}}, "discount": 0.5"#;
    let vllm = r#""models": {"my-vllm-llama": {"input": 0.1, "output": 0.2}}"#;
    let anthropic = r#""models": {}, "discount": 0.25"#;
    let json = custom(&[
        provider("openai", openai),
        provider("vllm", vllm),
        provider("anthropic", anthropic),
    ]);
    let overrides = table.merge_custom(&json).expect("user table loads");
    let names = |n: &str| vec![n.to_string()];
    let want = Overrides {
        replaced: names("gpt-5"),
        added: names("my-vllm-llama"),
    };
    assert_eq!(overrides, want);
    let version = table.version();
    let (base, hash) = version.split_once("+custom-").expect("custom version");
    assert!(base == "test-1" && hash.len() == 8 && hash.bytes().all(|b| b.is_ascii_hexdigit()));
    #[rustfmt::skip]
    let cases = [
        // The user entry replaces the whole model, so its missing cache price stays missing.
        (call("gpt-5", 1_000_000, 1_000_000), Ok(9.0)),
        (call("gpt-5", 1000, 0).read(500), MISSING),
        (call("my-vllm-llama", 1_000_000, 1_000_000), Ok(0.3)),
        // A discount scales every built-in price of its provider: base, dated name, long context, tier, speed.
        (call("gpt-4o", 1_000_000, 0), Ok(1.25)),
        (call("gpt-4o-2024-08-06", 1_000_000, 0), Ok(1.25)),
        (call("gpt-6.1-sol", 300_000, 0), Ok(0.6)),
        (call("gpt-6.1-sol", 100_000, 0).tier("flex"), Ok(0.05)),
        (call("claude-opus-5-5", 1_000_000, 0), Ok(1.0)),
        (call("claude-opus-5-5", 1_000_000, 0).speed("fast"), Ok(2.0)),
    ];
    for (call, want) in cases {
        assert_eq!(table.cost(&call).map(total), want, "{call:?}");
    }
}

#[test]
fn test_rejected_user_files_leave_the_table_unchanged() {
    let openai = |models: &str| provider("openai", &format!(r#""models": {models}"#));
    let m = || provider("vllm", r#""models": {"m": {}}"#);
    #[rustfmt::skip]
    let cases = [
        (custom(&[openai(r#"{"gpt-5": {"input": -1}}"#)]), "openai: gpt-5: prices must be between 0 and 1e6"),
        (custom(&[openai(r#"{"gpt-5": {"input_price": 1}}"#)]), "unknown field `input_price`"),
        (custom(&[openai(r#"{"gpt-5": {}, "gpt-5": {}}"#)]), "gpt-5 is listed twice"),
        (custom(&[m(), m()]), "vllm: m is listed twice"),
        (custom(&[provider("openai", r#""models": {}, "discount": 0"#)]), "multipliers must be above 0"),
        (custom(&[provider("openai", r#""models": {}, "discount": 0.8"#), provider("openai", r#""models": {}, "discount": 0.5"#)]), "openai: discount is listed twice"),
        (custom(&[provider("mistral", r#""models": {}, "discount": 0.8"#)]), "discount matches no built-in provider"),
        (r#"{"version": "acme-1"}"#.to_string(), "missing field `providers`"),
    ];
    for (json, why) in cases {
        let mut table = PriceTable::from_json(TABLE).expect("test table loads");
        let err = table.merge_custom(&json).err();
        assert!(err.is_some_and(|e| e.to_string().contains(why)), "{json}");
        assert_eq!(table.version(), "test-1");
        let gpt_5 = table.cost(&call("gpt-5", 1_000_000, 0)).map(total);
        assert_eq!(gpt_5, Ok(1.25), "{json}");
    }
}
