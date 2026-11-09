mod common;

use common::{CallExt, MISSING, UNKNOWN, UNSUPPORTED, call, total};
use honeybeepf_llm_core::pricing::{PriceTable, Unpriced};
use honeybeepf_llm_core::types::{ModalityTokens, UsageInfo};

// Prices from the providers' pricing pages (2026-10-26), in USD per million tokens.
const TABLE: &str = include_str!("fixtures/prices_test.json");

#[test]
fn test_calls_are_priced_or_counted_as_unpriced() {
    let table = PriceTable::from_json(TABLE).expect("test table loads");
    // Uncached 100, read 2000, write 500 (prompt counts all three), dated model id.
    let sonnet = call("claude-sonnet-4-5-20250929", 2600, 300).read(2000);
    let cost = table.cost(&sonnet.write(500)).expect("priced");
    let parts = [cost.input, cost.cache_read, cost.cache_write, cost.output];
    let micro_usd = parts.map(|c| (c * 1e6).round());
    assert_eq!(micro_usd, [300.0, 600.0, 1875.0, 4500.0]);
    let mut no_model = call("", 10, 2);
    no_model.model = None;
    let sol = |tier: &str, prompt: u64| call("gpt-6.1-sol", prompt, 2000).tier(tier);
    let opus = || call("claude-opus-5-5", 11000, 2000).read(1000);
    #[rustfmt::skip]
    let cases = [
        // OpenAI prompt caching guide: 1000 plain, 4000 read, 3000 30m writes, 4000 12h writes.
        (call("gpt-6.1-sol", 12000, 500).read(4000).write(7000).long(4000), Ok(0.0349)),
        // Reasoning is inside completion and billed once.
        (call("gpt-5", 1200, 800).read(1000).thoughts(600), Ok(0.008375)),
        // Unknown models are unpriced; a snapshot id matches exactly before its date is dropped.
        (call("my-vllm-llama", 1200, 300), UNKNOWN),
        (call("gpt-9-2026-01-01", 10, 2), UNKNOWN),
        (call("gpt-5-12345678", 10, 2), UNKNOWN),
        (call("gpt-5-2024-13-45", 10, 2), UNKNOWN),
        (no_model, UNKNOWN),
        (call("gpt-4o-2024-05-13", 1_000_000, 0), Ok(5.0)),
        (call("gpt-4o-2024-08-06", 1_000_000, 0), Ok(2.5)),
        (call("claude-opus-5-5@20260901", 1_000_000, 0), Ok(4.0)),
        // A missing price makes the whole call unpriced, unless nothing in the call needs it.
        (call("gpt-5.5-pro", 100, 10).read(10), MISSING),
        (call("gpt-5", 100, 10).write(10), MISSING),
        (call("gpt-5.5-pro", 100_000, 0), Ok(3.0)),
        // Long-context prices start above the threshold; cache reads count toward it.
        (call("gpt-6.1-sol", 272_000, 0), Ok(0.544)),
        (call("gpt-6.1-sol", 272_001, 0), Ok(1.088004)),
        (call("claude-haiku-5-5", 100_001, 0).read(100_000), Ok(0.0050005)),
        // The served service tier picks its prices.
        (sol("flex", 10000), Ok(0.02)),
        (sol("priority", 10000), Ok(0.08)),
        (sol("fast", 10000), Ok(0.08)),
        (sol("default", 10000), Ok(0.04)),
        (sol("auto", 10000), Ok(0.04)),
        (sol("scale", 10000), UNSUPPORTED),
        (sol("", 10000), UNSUPPORTED),
        (call("gpt-5", 10000, 2000).tier("flex"), UNSUPPORTED),
        // Tier rates without long-context prices inherit the standard threshold, not short prices.
        (sol("flex", 300_000), MISSING),
        (sol("priority", 300_000), MISSING),
        // Speed and geo multiply every price: 10000×$4 + 1000×$0.20 + 2000×$20 per million.
        (opus(), Ok(0.0802)),
        (opus().speed("standard").geo("global"), Ok(0.0802)),
        (opus().speed("fast").geo("us"), Ok(0.17644)),
        (call("claude-sonnet-4-5", 11000, 2000).read(1000).speed("fast"), UNSUPPORTED),
        // Models before 4.6 reject inference_geo, so the table gives them no geo price.
        (call("claude-sonnet-4-5", 11000, 2000).read(1000).geo("us"), UNSUPPORTED),
        (opus().geo("eu"), UNSUPPORTED),
        (call("gpt-5", 11000, 2000).read(1000).geo("us"), UNSUPPORTED),
        // Cache counts above the prompt total mean the usage was not normalized; no guessed amount.
        (call("claude-opus-5-5", 100, 0).read(50).write(80), Err(Unpriced::InconsistentUsage)),
    ];
    for (call, want) in cases {
        assert_eq!(table.cost(&call).map(total), want, "{call:?}");
    }
    let huge = call("gpt-5", u64::MAX, u64::MAX).read(u64::MAX);
    assert!(table.cost(&huge).map(total).is_ok_and(f64::is_finite));
}

#[test]
fn test_modality_prices_split_each_direction() {
    let table = PriceTable::from_json(TABLE).expect("test table loads");
    let audio = |tokens| ModalityTokens {
        audio: tokens,
        ..Default::default()
    };
    let image = |tokens| ModalityTokens {
        image: tokens,
        ..Default::default()
    };
    let video = |tokens| ModalityTokens {
        video: tokens,
        ..Default::default()
    };
    let media = |prompt, completion| call("gpt-media-test", prompt, completion);
    #[rustfmt::skip]
    let cases = [
        // input: 900 text ×$1 + 100 audio ×$40; output: 300 text ×$5 + 200 image ×$30.
        (media(1000, 500).in_modalities(audio(100)).out_modalities(image(200)), Ok(0.0124)),
        // A modality without its own rate (image input) falls back to the base input price.
        (media(1000, 0).in_modalities(image(100)), Ok(0.001)),
        // Video output has its own rate; text output stays at the base.
        (media(0, 1000).out_modalities(video(100)), Ok(0.0057)),
        // The speed multiplier applies to the modality rates too: (900×$1 + 100×$40) × 2.
        (media(1000, 0).in_modalities(audio(100)).speed("fast"), Ok(0.0098)),
        // No modalities: the base rates, unchanged.
        (media(1000, 500), Ok(0.0035)),
        // Cache and a modality together: the modality takes from the uncached input, cache is separate.
        (media(1000, 0).in_modalities(audio(300)).read(400), Ok(0.01234)),
        // A modality above the uncached input is capped there (the rest is cached text), never rejected.
        (media(100, 0).in_modalities(audio(80)).read(50), Ok(0.002005)),
    ];
    for (call, want) in cases {
        assert_eq!(table.cost(&call).map(total), want, "{call:?}");
    }
}

#[test]
fn cache_savings_ceiling_uses_the_model_cache_price_or_a_conservative_bound() {
    let table = PriceTable::from_json(TABLE).expect("test table loads");
    let micro = |u: &UsageInfo| {
        table
            .cache_savings_ceiling(u)
            .map(|(usd, exact)| ((usd * 1e6).round(), exact))
    };
    // sonnet: input $3, cache read $0.30 per million → 1000 uncached could save 1000×$2.70.
    assert_eq!(
        micro(&call("claude-sonnet-4-5", 1000, 0)),
        Ok((2700.0, true))
    );
    // Tokens already read from cache are not counted; only the 300 uncached can still be saved.
    assert_eq!(
        micro(&call("claude-sonnet-4-5", 1000, 0).read(700)),
        Ok((810.0, true))
    );
    // gpt-5.5-pro has an input price but no cache-read price → full input cost, flagged inexact.
    assert_eq!(micro(&call("gpt-5.5-pro", 1000, 0)), Ok((30000.0, false)));
    // Unknown model and inconsistent cache counts give the same reasons as cost().
    assert_eq!(
        table.cache_savings_ceiling(&call("my-vllm-llama", 1000, 0)),
        Err(Unpriced::UnknownModel)
    );
    assert_eq!(
        table.cache_savings_ceiling(&call("claude-opus-5-5", 100, 0).read(50).write(80)),
        Err(Unpriced::InconsistentUsage)
    );
    // A u64::MAX prompt stays finite and non-negative, never a panic.
    let huge = call("gpt-5", u64::MAX, 0);
    assert!(
        table
            .cache_savings_ceiling(&huge)
            .is_ok_and(|(usd, _)| usd.is_finite() && usd >= 0.0)
    );
}
