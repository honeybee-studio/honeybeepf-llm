mod common;

use common::{CallExt, MISSING, call};
use honeybeepf_llm_core::pricing::{Cost, PriceTable};

const TABLE: &str = include_str!("fixtures/prices_test.json");

fn one_provider(models: &str, extra: &str) -> String {
    format!(
        r#"{{"version": "v", "providers": [{{"name": "p", "source": "s", "checked": "2026-10-26", "models": {models}{extra}}}]}}"#
    )
}

#[test]
fn test_table_keeps_its_sources() {
    let table = PriceTable::from_json(TABLE).expect("test table loads");
    assert_eq!(table.version(), "test-1");
    let openai = &table.sources()[1];
    assert_eq!(openai.url, "https://developers.openai.com/api/docs/pricing");
    assert_eq!(openai.checked, "2026-10-26");
}

#[test]
fn test_dated_prices_switch_after_their_last_day() {
    // Gemini pricing page: input $2 -> $4 and output $10 -> $20 after 2026-11-30.
    let json = one_provider(
        r#"{"gemini-4-argon": {"input": 2, "output": 10, "until": "2026-11-30", "then": {"input": 4, "output": 20}},
            "promo": {"input": 1, "until": "2026-11-30"},
            "old": {"input": 9, "until": "2000-01-01", "then": {"input": 1}}}"#,
        r#", "tiers": {"flex": {"promo": {"input": 0.5, "above_tokens": 10, "above": {"input": 1}}}}"#,
    );
    let table = PriceTable::from_json(&json).expect("dated table loads");
    let nov_30 = 20_787; // 2026-11-30
    let million = |model: &str| call(model, 1_000_000, 0);
    // Unrounded, so the switch must give the listed price exactly.
    let total = |c: Cost| c.input + c.cache_read + c.cache_write + c.output;
    #[rustfmt::skip]
    let cases = [
        (million("gemini-4-argon"), nov_30, Ok(2.0)),
        (million("gemini-4-argon"), nov_30 + 1, Ok(4.0)),
        (million("promo"), nov_30, Ok(1.0)),
        (million("promo"), nov_30 + 1, MISSING),
        // A tier with its own threshold does not need the expired standard prices.
        (million("promo").tier("flex"), nov_30 + 1, Ok(1.0)),
    ];
    for (call, day, want) in cases {
        assert_eq!(table.cost_on(&call, day).map(total), want, "{call:?}");
    }
    assert_eq!(table.cost(&million("old")).map(total), Ok(1.0));
}

#[test]
fn test_bad_tables_are_rejected() {
    let model = |rates: &str| one_provider(&format!(r#"{{"m": {rates}}}"#), "");
    let with = |extra: &str| one_provider(r#"{"m": {"input": 1, "above_tokens": 10}}"#, extra);
    let flex = |rates: &str| with(&format!(r#", "tiers": {{"flex": {{"m": {rates}}}}}"#));
    let until = |day: &str| model(&format!(r#"{{"until": "{day}"}}"#));
    let dated = r#"{"until": "2026-01-01", "then": "#.repeat(200);
    let deep = model(&format!("{dated}{{}}{}", "}".repeat(200)));
    #[rustfmt::skip]
    let cases = [
        (TABLE.replace("\"output\": 180", "\"output_price\": 180"), "unknown field `output_price`"),
        (model(r#"{"input": -1}"#), "p: m: prices must be between 0 and 1e6"),
        (model(r#"{"input": -0.0}"#), "between 0 and 1e6"),
        (model(r#"{"input": 1e7}"#), "between 0 and 1e6"),
        (model(r#"{"input": 1e400}"#), "number out of range"),
        (model(r#"{"above_tokens": 5, "above": {"input": -2}}"#), "between 0 and 1e6"),
        (model(r#"{"until": "2026-11-30", "then": {"input": -2}}"#), "between 0 and 1e6"),
        (model(r#"{"input": 1, "above": {"input": 2}}"#), "p: m: above needs above_tokens"),
        (flex(r#"{"input": 1, "above": {"input": 2}}"#), "p: m: above needs above_tokens"),
        (model(r#"{"above_tokens": 5, "above": {"above_tokens": 9}}"#), "cannot have its own"),
        (model(r#"{"above_tokens": 5, "above": {"then": {}}}"#), "cannot have its own"),
        (model(r#"{"input": 1, "then": {"input": 2}}"#), "p: m: then needs until"),
        (model(r#"{"until": "2026-11-30", "then": {"until": "2026-11-01"}}"#), "must increase"),
        (model("{}").replace("2026-10-26", "today"), "p: checked is not a YYYY-MM-DD date"),
        (model("{}").replace("2026-10-26", "2026-04-31"), "p: checked is not"),
        (until("2026-02-29"), "is not a YYYY-MM-DD date from 1970 on"),
        (until("2100-02-29"), "date from 1970 on"),
        (until("2026-02-30"), "date from 1970 on"),
        (until("2026-13-01"), "date from 1970 on"),
        (until("2026-11-30T00:00"), "date from 1970 on"),
        (until("1969-12-31"), "date from 1970 on"),
        (until("26-11-30"), "date from 1970 on"),
        (until("2026-1-30"), "date from 1970 on"), // chrono alone would accept these five.
        (until(" 2026-11-30"), "date from 1970 on"),
        (until("+2026-11-30"), "date from 1970 on"),
        (until("2026- 1-30"), "date from 1970 on"),
        (until("2026-01- 1"), "date from 1970 on"),
        (with(r#", "tiers": {"flex": {"x": {}}}"#), "p: x has no standard prices"),
        (with(r#", "speed": {"x": {"fast": 2}}"#), "p: x has no standard prices"),
        (with(r#", "geo": {"x": {"us": 1.1}}"#), "p: x has no standard prices"),
        (with(r#", "tiers": {"default": {"m": {}}}"#), "p: default is never looked up"),
        (with(r#", "tiers": {"fast": {"m": {}}}"#), "p: fast is never looked up"),
        (with(r#", "speed": {"m": {"standard": 2}}"#), "p: standard is never looked up"),
        (with(r#", "geo": {"m": {"global": 2}}"#), "p: global is never looked up"),
        (with(r#", "speed": {"m": {"fast": 0}}"#), "multipliers must be above 0 and at most 100"),
        (with(r#", "geo": {"m": {"us": -1}}"#), "multipliers must be above 0"),
        (with(r#", "geo": {"m": {"us": 1000}}"#), "multipliers must be above 0"),
        (model(r#"{}, "m": {}"#), "m is listed twice"),
        (flex(r#"{}, "m": {}"#), "m is listed twice"),
        (with(r#", "tiers": {"flex": {}, "flex": {}}"#), "flex is listed twice"),
        (with(r#", "speed": {"m": {}, "m": {}}"#), "m is listed twice"),
        (with(r#", "speed": {"m": {"fast": 2, "fast": 3}}"#), "fast is listed twice"),
        (with(r#", "geo": {"m": {"us": 1.1, "us": 1.2}}"#), "us is listed twice"),
        (TABLE.replace("gpt-5.5-pro", "claude-opus-5-5"), "openai: claude-opus-5-5 is listed twice"),
        (deep, "recursion limit exceeded"),
    ];
    for (json, why) in cases {
        let err = PriceTable::from_json(&json).err();
        assert!(err.is_some_and(|e| e.to_string().contains(why)), "{json}");
    }
    // Leap days, a null date and a tier with its own threshold load.
    #[rustfmt::skip]
    let fine = [until("2028-02-29"), until("2000-02-29"), model(r#"{"until": null}"#),
        flex(r#"{"above_tokens": 5, "above": {}}"#)];
    for json in fine {
        assert!(PriceTable::from_json(&json).is_ok(), "{json}");
    }
    // Prices and multipliers at the caps load, and the largest cost stays finite.
    let caps = r#", "speed": {"m": {"fast": 100}}, "geo": {"m": {"us": 100}}"#;
    let json = one_provider(r#"{"m": {"input": 1e6, "output": 1e6}}"#, caps);
    let table = PriceTable::from_json(&json).expect("table at the caps loads");
    let huge = call("m", u64::MAX, u64::MAX).speed("fast").geo("us");
    let total = table.cost(&huge).map(|c| c.input + c.output);
    assert!(total.is_ok_and(f64::is_finite));
}
