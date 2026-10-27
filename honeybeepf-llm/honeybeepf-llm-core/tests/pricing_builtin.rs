use honeybeepf_llm_core::pricing::{Cost, PriceTable, Unpriced};
use honeybeepf_llm_core::types::UsageInfo;

const NOV_21: u32 = 20_778; // 2026-11-21
const NOV_30: u32 = 20_787; // 2026-11-30

fn usage(model: &str, prompt: u64, completion: u64) -> UsageInfo {
    UsageInfo {
        prompt_tokens: prompt,
        completion_tokens: completion,
        model: Some(model.to_string()),
        ..UsageInfo::default()
    }
}

/// Total USD on `day`, rounded to 1e-12 so amounts from the pricing pages compare exactly.
fn usd(u: &UsageInfo, day: u32) -> Result<f64, Unpriced> {
    let table = PriceTable::builtin().expect("built-in table loads");
    let total = |c: Cost| c.input + c.cache_read + c.cache_write + c.output;
    table
        .cost_on(u, day)
        .map(|c| (total(c) * 1e12).round() / 1e12)
}

fn tier(name: &str, u: UsageInfo) -> UsageInfo {
    UsageInfo {
        service_tier: Some(name.to_string()),
        ..u
    }
}

#[test]
fn test_builtin_table_names_its_sources() {
    let table = PriceTable::builtin().expect("built-in table loads");
    assert_eq!(table.version(), "2026-10-26");
    let sources: Vec<_> = table
        .sources()
        .iter()
        .map(|s| (s.provider.as_str(), s.checked.as_str()))
        .collect();
    assert_eq!(
        sources,
        [
            ("anthropic", "2026-10-26"),
            ("openai", "2026-10-26"),
            ("gemini", "2026-10-26")
        ]
    );
}

#[test]
fn test_builtin_prices_match_the_pricing_pages() {
    // Opus 5.5 fast in the US: (10000×$4 + 500×$5 + 1500×$8 + 2000×$20) × 2 × 1.1.
    let opus = UsageInfo {
        cache_write_tokens: Some(2000),
        cache_write_long_ttl_tokens: Some(1500),
        speed: Some("fast".to_string()),
        inference_geo: Some("us".to_string()),
        ..usage("claude-opus-5-5", 12000, 2000)
    };
    assert_eq!(usd(&opus, NOV_30), Ok(0.2079));
    // OpenAI prompt caching guide: 1000 plain, 4000 read, 3000 30m and 4000 12h writes.
    let guide = UsageInfo {
        cache_read_tokens: Some(4000),
        cache_write_tokens: Some(7000),
        cache_write_long_ttl_tokens: Some(4000),
        ..usage("gpt-6.1-sol", 12000, 500)
    };
    assert_eq!(usd(&guide, NOV_30), Ok(0.0349));
    // Fast tier long context: 300000×$8 + 1000×$30.
    let fast = tier("priority", usage("gpt-6.1-sol", 300_000, 1000));
    assert_eq!(usd(&fast, NOV_30), Ok(2.43));
    // Haiku 5.5 above 100k in the US: 100001×$0.50×1.1.
    let haiku = UsageInfo {
        inference_geo: Some("us".to_string()),
        ..usage("claude-haiku-5-5", 100_001, 0)
    };
    assert_eq!(usd(&haiku, NOV_30), Ok(0.05500055));
    // Gemini 2.5 Pro above 200k: 110000×$2.50 + 100000×$0.25 + 4000×$15.
    let gemini = UsageInfo {
        cache_read_tokens: Some(100_000),
        ..usage("gemini-2.5-pro", 210_000, 4000)
    };
    assert_eq!(usd(&gemini, NOV_30), Ok(0.36));
    let sonnet = usage("claude-sonnet-4-5-20250929", 1_000_000, 0);
    assert_eq!(usd(&sonnet, NOV_30), Ok(3.0));
    let snapshot = tier("priority", usage("gpt-4o-2024-05-13", 1_000_000, 0));
    assert_eq!(usd(&snapshot, NOV_30), Ok(8.75));
}

#[test]
fn test_builtin_dated_prices_and_gaps_stay_unpriced() {
    let argon = usage("gemini-4-argon", 1_000_000, 0);
    assert_eq!(usd(&argon, NOV_30), Ok(2.0));
    assert_eq!(usd(&argon, NOV_30 + 1), Ok(4.0));
    // The gpt-5.6-sol promotion runs "at least through November 21" without naming a tier.
    let sol = || usage("gpt-5.6-sol", 100_000, 0);
    assert_eq!(usd(&sol(), NOV_21), Ok(0.4));
    assert_eq!(usd(&tier("flex", sol()), NOV_21), Ok(0.2));
    for u in [sol(), tier("flex", sol()), tier("priority", sol())] {
        assert_eq!(usd(&u, NOV_21 + 1), Err(Unpriced::MissingPrice));
    }
    // US inference is 1.1x from Claude 4.6 on; earlier models reject the setting.
    let geo = |model: &str| UsageInfo {
        inference_geo: Some("us".to_string()),
        ..usage(model, 1_000_000, 0)
    };
    assert_eq!(usd(&geo("claude-sonnet-4-6"), NOV_30), Ok(3.3));
    let old_geo = usd(&geo("claude-sonnet-4-5-20250929"), NOV_30);
    assert_eq!(old_geo, Err(Unpriced::UnsupportedModifier));
    // Fast has no long-context price for gpt-5.5, and gpt-5 has no cache write price.
    let long = tier("priority", usage("gpt-5.5", 300_000, 0));
    assert_eq!(usd(&long, NOV_30), Err(Unpriced::MissingPrice));
    let write = UsageInfo {
        cache_write_tokens: Some(100),
        ..usage("gpt-5", 1000, 0)
    };
    assert_eq!(usd(&write, NOV_30), Err(Unpriced::MissingPrice));
    let old = usage("claude-3-opus-20240229", 1000, 10);
    assert_eq!(usd(&old, NOV_30), Err(Unpriced::UnknownModel));
}
