use honeybeepf_llm_core::pricing::{Cost, PriceTable, Unpriced};
use honeybeepf_llm_core::types::UsageInfo;

// Prices from the providers' pricing pages (2026-10-26), in USD per million tokens.
const TABLE: &str = r#"{"version": "test-1", "providers": [
  {"name": "anthropic", "source": "https://platform.claude.com/docs/en/about-claude/pricing", "checked": "2026-10-26",
   "models": {
     "claude-opus-5-5": {"input": 4, "output": 20, "cache_read": 0.2, "cache_write": 5, "cache_write_long": 8},
     "claude-sonnet-4-5": {"input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75, "cache_write_long": 6},
     "claude-haiku-5-5": {"input": 0.1, "output": 0.5, "cache_read": 0.01, "above_tokens": 100000,
       "above": {"input": 0.5, "output": 2.5, "cache_read": 0.05}}},
   "speed": {"claude-opus-5-5": {"fast": 2}},
   "geo": {"us": 1.1}},
  {"name": "openai", "source": "https://developers.openai.com/api/docs/pricing", "checked": "2026-10-26",
   "models": {
     "gpt-6.1-sol": {"input": 2, "output": 10, "cache_read": 0.1, "cache_write": 2.5, "cache_write_long": 5,
       "above_tokens": 272000, "above": {"input": 4, "output": 15}},
     "gpt-5.5-pro": {"input": 30, "output": 180},
     "gpt-5": {"input": 1.25, "output": 10, "cache_read": 0.125},
     "gpt-4o": {"input": 2.5, "output": 10},
     "gpt-4o-2024-05-13": {"input": 5, "output": 15}},
   "tiers": {
     "flex": {"gpt-6.1-sol": {"input": 1, "output": 5}},
     "priority": {"gpt-6.1-sol": {"input": 4, "output": 20}}}}
]}"#;

fn usage(model: &str, prompt: u64, completion: u64) -> UsageInfo {
    UsageInfo {
        prompt_tokens: prompt,
        completion_tokens: completion,
        model: Some(model.to_string()),
        ..UsageInfo::default()
    }
}

/// Total USD rounded to 1e-12, so hand-computed amounts compare exactly.
fn usd(usage: &UsageInfo) -> Result<f64, Unpriced> {
    let table = PriceTable::from_json(TABLE).expect("test table loads");
    let total = |c: Cost| c.input + c.cache_read + c.cache_write + c.output;
    table.cost(usage).map(|c| (total(c) * 1e12).round() / 1e12)
}

#[test]
fn test_table_keeps_its_sources_and_rejects_misspelled_fields() {
    let table = PriceTable::from_json(TABLE).expect("test table loads");
    assert_eq!(table.version(), "test-1");
    let openai = &table.sources()[1];
    assert_eq!(openai.url, "https://developers.openai.com/api/docs/pricing");
    assert_eq!(openai.checked, "2026-10-26");
    let typo = TABLE.replace("\"output\": 180", "\"output_price\": 180");
    assert!(PriceTable::from_json(&typo).is_err());
}

#[test]
fn test_cache_reads_and_writes_get_their_own_prices() {
    // Uncached 100, read 2000, write 500 (prompt counts all three), dated model id.
    let u = UsageInfo {
        cache_read_tokens: Some(2000),
        cache_write_tokens: Some(500),
        ..usage("claude-sonnet-4-5-20250929", 2600, 300)
    };
    let table = PriceTable::from_json(TABLE).expect("test table loads");
    let cost = table.cost(&u).expect("priced");
    assert_eq!(
        [cost.input, cost.cache_read, cost.cache_write, cost.output].map(|c| (c * 1e6).round()),
        [300.0, 600.0, 1875.0, 4500.0]
    );
    // OpenAI prompt caching guide: 1000 plain, 4000 read, 3000 30m writes, 4000 12h writes.
    let ttl = UsageInfo {
        cache_read_tokens: Some(4000),
        cache_write_tokens: Some(7000),
        cache_write_long_ttl_tokens: Some(4000),
        ..usage("gpt-6.1-sol", 12000, 500)
    };
    assert_eq!(usd(&ttl), Ok(0.0349));
    // Reasoning is inside completion and billed once.
    let reasoning = UsageInfo {
        cache_read_tokens: Some(1000),
        thoughts_tokens: Some(600),
        ..usage("gpt-5", 1200, 800)
    };
    assert_eq!(usd(&reasoning), Ok(0.008375));
}

#[test]
fn test_unknown_model_is_unpriced_and_snapshots_match_exactly_first() {
    let unknown = Err(Unpriced::UnknownModel);
    assert_eq!(usd(&usage("my-vllm-llama", 1200, 300)), unknown);
    for name in ["gpt-9-2026-01-01", "gpt-5-12345678", "gpt-5-2024-13-45"] {
        assert_eq!(usd(&usage(name, 10, 2)), unknown, "{name}");
    }
    let mut no_model = usage("", 10, 2);
    no_model.model = None;
    assert_eq!(usd(&no_model), unknown);
    assert_eq!(usd(&usage("gpt-4o-2024-05-13", 1_000_000, 0)), Ok(5.0));
    assert_eq!(usd(&usage("gpt-4o-2024-08-06", 1_000_000, 0)), Ok(2.5));
    assert_eq!(
        usd(&usage("claude-opus-5-5@20260901", 1_000_000, 0)),
        Ok(4.0)
    );
}

#[test]
fn test_missing_price_makes_the_whole_call_unpriced() {
    let read = UsageInfo {
        cache_read_tokens: Some(10),
        ..usage("gpt-5.5-pro", 100, 10)
    };
    assert_eq!(usd(&read), Err(Unpriced::MissingPrice));
    let write = UsageInfo {
        cache_write_tokens: Some(10),
        ..usage("gpt-5", 100, 10)
    };
    assert_eq!(usd(&write), Err(Unpriced::MissingPrice));
    // Nothing in this call needs the missing prices.
    assert_eq!(usd(&usage("gpt-5.5-pro", 100_000, 0)), Ok(3.0));
}

#[test]
fn test_long_context_prices_start_above_the_threshold() {
    assert_eq!(usd(&usage("gpt-6.1-sol", 272_000, 0)), Ok(0.544));
    assert_eq!(usd(&usage("gpt-6.1-sol", 272_001, 0)), Ok(1.088004));
    // Cache reads count toward the threshold and pay the higher read price.
    let haiku = UsageInfo {
        cache_read_tokens: Some(100_000),
        ..usage("claude-haiku-5-5", 100_001, 0)
    };
    assert_eq!(usd(&haiku), Ok(0.0050005));
}

#[test]
fn test_served_service_tier_picks_its_prices() {
    let tier = |model: &str, tier: &str, prompt: u64| {
        usd(&UsageInfo {
            service_tier: Some(tier.to_string()),
            ..usage(model, prompt, 2000)
        })
    };
    assert_eq!(tier("gpt-6.1-sol", "flex", 10000), Ok(0.02));
    assert_eq!(tier("gpt-6.1-sol", "priority", 10000), Ok(0.08));
    assert_eq!(tier("gpt-6.1-sol", "fast", 10000), Ok(0.08));
    assert_eq!(tier("gpt-6.1-sol", "default", 10000), Ok(0.04));
    assert_eq!(tier("gpt-6.1-sol", "auto", 10000), Ok(0.04));
    let unsupported = Err(Unpriced::UnsupportedModifier);
    assert_eq!(tier("gpt-6.1-sol", "scale", 10000), unsupported);
    assert_eq!(tier("gpt-6.1-sol", "", 10000), unsupported);
    assert_eq!(tier("gpt-5", "flex", 10000), unsupported);
    // Tier rates without long-context prices inherit the standard threshold, not short prices.
    let missing = Err(Unpriced::MissingPrice);
    for name in ["flex", "priority"] {
        assert_eq!(tier("gpt-6.1-sol", name, 300_000), missing);
    }
}

#[test]
fn test_speed_and_geo_multiply_every_price() {
    let call = |model: &str, speed: Option<&str>, geo: Option<&str>| {
        usd(&UsageInfo {
            cache_read_tokens: Some(1000),
            speed: speed.map(str::to_string),
            inference_geo: geo.map(str::to_string),
            ..usage(model, 11000, 2000)
        })
    };
    // 10000×$4 + 1000×$0.20 + 2000×$20 per million.
    let opus = "claude-opus-5-5";
    assert_eq!(call(opus, None, None), Ok(0.0802));
    assert_eq!(call(opus, Some("standard"), Some("global")), Ok(0.0802));
    assert_eq!(call(opus, Some("fast"), Some("us")), Ok(0.17644));
    let unsupported = Err(Unpriced::UnsupportedModifier);
    assert_eq!(call("claude-sonnet-4-5", Some("fast"), None), unsupported);
    assert_eq!(call(opus, None, Some("eu")), unsupported);
    assert_eq!(call("gpt-5", None, Some("us")), unsupported);
}

#[test]
fn test_huge_and_inconsistent_counts_stay_finite() {
    let huge = UsageInfo {
        cache_read_tokens: Some(u64::MAX),
        ..usage("gpt-5", u64::MAX, u64::MAX)
    };
    assert!(usd(&huge).is_ok_and(f64::is_finite));
    // Cache counts above the prompt total mean the usage was not normalized; no guessed amount.
    let odd = UsageInfo {
        cache_read_tokens: Some(50),
        cache_write_tokens: Some(80),
        ..usage("claude-opus-5-5", 100, 0)
    };
    assert_eq!(usd(&odd), Err(Unpriced::InconsistentUsage));
}
