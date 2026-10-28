mod common;

use common::{CallExt, MISSING, UNKNOWN, UNSUPPORTED, call, total};
use honeybeepf_llm_core::pricing::PriceTable;

const NOV_21: u32 = 20_778; // 2026-11-21
const NOV_30: u32 = 20_787; // 2026-11-30

#[test]
fn test_builtin_prices_match_the_pricing_pages() {
    let table = PriceTable::builtin().expect("built-in table loads");
    assert_eq!(table.version(), "2026-10-28");
    let sources: Vec<_> = table
        .sources()
        .iter()
        .map(|s| (s.provider.as_str(), s.checked.as_str()))
        .collect();
    assert_eq!(
        sources,
        [
            ("anthropic", "2026-10-26"),
            ("openai", "2026-10-28"),
            ("gemini", "2026-10-26")
        ]
    );
    // Opus 5.5 fast in the US: (10000×$4 + 500×$5 + 1500×$8 + 2000×$20) × 2 × 1.1.
    let opus = call("claude-opus-5-5", 12000, 2000).write(2000).long(1500);
    let million = |model: &str| call(model, 1_000_000, 0);
    let sol = || call("gpt-5.6-sol", 100_000, 0);
    let terra = || call("gpt-6.1-terra", 10000, 2000);
    let terra_long = || {
        call("gpt-6.1-terra", 300_000, 1000)
            .read(100_000)
            .write(50_000)
            .long(20_000)
    };
    #[rustfmt::skip]
    let cases = [
        (NOV_30, opus.speed("fast").geo("us"), Ok(0.2079)),
        // OpenAI prompt caching guide: 1000 plain, 4000 read, 3000 30m and 4000 12h writes.
        (NOV_30, call("gpt-6.1-sol", 12000, 500).read(4000).write(7000).long(4000), Ok(0.0349)),
        // Fast tier long context: 300000×$8 + 1000×$30.
        (NOV_30, call("gpt-6.1-sol", 300_000, 1000).tier("priority"), Ok(2.43)),
        // GPT-6.1 Terra: 10000×$1 + 2000×$5, Flex half, Fast double.
        (NOV_30, terra(), Ok(0.02)),
        (NOV_30, terra().tier("flex"), Ok(0.01)),
        (NOV_30, terra().tier("priority"), Ok(0.04)),
        // Long context: 150000×$2 + 100000×$0.10 + 30000×$2.50 + 20000×$5 + 1000×$7.50,
        // Flex half, Fast double.
        (NOV_30, terra_long(), Ok(0.4925)),
        (NOV_30, terra_long().tier("flex"), Ok(0.24625)),
        (NOV_30, terra_long().tier("priority"), Ok(0.985)),
        // Haiku 5.5 above 100k in the US: 100001×$0.50×1.1.
        (NOV_30, call("claude-haiku-5-5", 100_001, 0).geo("us"), Ok(0.05500055)),
        // Gemini 2.5 Pro above 200k: 110000×$2.50 + 100000×$0.25 + 4000×$15.
        (NOV_30, call("gemini-2.5-pro", 210_000, 4000).read(100_000), Ok(0.36)),
        (NOV_30, million("claude-sonnet-4-5-20250929"), Ok(3.0)),
        (NOV_30, million("gpt-4o-2024-05-13").tier("priority"), Ok(8.75)),
        (NOV_30, million("gemini-4-argon"), Ok(2.0)),
        (NOV_30 + 1, million("gemini-4-argon"), Ok(4.0)),
        // The gpt-5.6-sol promotion runs "at least through November 21" without naming a tier.
        (NOV_21, sol(), Ok(0.4)),
        (NOV_21, sol().tier("flex"), Ok(0.2)),
        (NOV_21 + 1, sol(), MISSING),
        (NOV_21 + 1, sol().tier("flex"), MISSING),
        (NOV_21 + 1, sol().tier("priority"), MISSING),
        // US inference is 1.1x from Claude 4.6 on; earlier models reject the setting.
        (NOV_30, million("claude-sonnet-4-6").geo("us"), Ok(3.3)),
        (NOV_30, million("claude-sonnet-4-5-20250929").geo("us"), UNSUPPORTED),
        // Fast has no long-context price for gpt-5.5, and gpt-5 has no cache write price.
        (NOV_30, call("gpt-5.5", 300_000, 0).tier("priority"), MISSING),
        (NOV_30, call("gpt-5", 1000, 0).write(100), MISSING),
        (NOV_30, call("claude-3-opus-20240229", 1000, 10), UNKNOWN),
        (NOV_30, call("gpt-4-turbo-2024-04-09", 10000, 1000), Ok(0.13)),
        (NOV_30, call("gpt-4-0613", 1000, 1000), Ok(0.09)),
        (NOV_30, million("gpt-3.5-turbo-0125"), Ok(0.5)),
        // A four-digit suffix is not a date, so an unlisted snapshot stays unknown.
        (NOV_30, million("gpt-3.5-turbo-1106"), UNKNOWN),
        // These models have no cached-input price and no Flex tier.
        (NOV_30, call("gpt-3.5-turbo", 1000, 0).read(500), MISSING),
        (NOV_30, million("gpt-4-0613").tier("flex"), UNSUPPORTED),
    ];
    for (day, call, want) in cases {
        assert_eq!(table.cost_on(&call, day).map(total), want, "{call:?}");
    }
}
