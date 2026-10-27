//! Price table and the cost of one call from its parsed usage.

use std::collections::HashMap;

use serde::Deserialize;

use crate::types::UsageInfo;

/// Prices in USD per million tokens; a call that needs a missing price stays unpriced.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rates {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    cache_write_long: Option<f64>,
    /// Prompts over this many tokens (cache included) pay `above` for every category.
    above_tokens: Option<u64>,
    above: Option<Box<Rates>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    version: String,
    providers: Vec<RawProvider>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    name: String,
    source: String,
    checked: String,
    models: HashMap<String, Rates>,
    /// Served service tier (`flex`, `priority`) -> model -> prices.
    #[serde(default)]
    tiers: HashMap<String, HashMap<String, Rates>>,
    /// Model -> speed mode (`fast`) -> multiplier on every price.
    #[serde(default)]
    speed: HashMap<String, HashMap<String, f64>>,
    /// Inference geography (`us`) -> multiplier on every price.
    #[serde(default)]
    geo: HashMap<String, f64>,
}

struct ModelPrices {
    standard: Rates,
    tiers: HashMap<String, Rates>,
    speed: HashMap<String, f64>,
    geo: HashMap<String, f64>,
}

/// Where one provider's prices were copied from, and when.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceSource {
    pub provider: String,
    pub url: String,
    pub checked: String,
}

/// Why a call has no cost; its tokens are counted as unpriced instead of costing $0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unpriced {
    UnknownModel,
    MissingPrice,
    UnsupportedModifier,
    /// Cache counts larger than the prompt total they are part of.
    InconsistentUsage,
}

/// Cost of one call in USD, split the way providers bill it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    /// Prompt tokens that were neither read from nor written to the cache.
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    /// All output tokens; reasoning is billed at the output price.
    pub output: f64,
}

pub struct PriceTable {
    version: String,
    sources: Vec<PriceSource>,
    models: HashMap<String, ModelPrices>,
}

impl PriceTable {
    /// Load a table; a misspelled field name is an error, not a silently missing price.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let raw: RawTable = serde_json::from_str(json)?;
        let mut models = HashMap::new();
        let mut sources = Vec::new();
        for p in raw.providers {
            for (name, standard) in p.models {
                let tiers = p
                    .tiers
                    .iter()
                    .filter_map(|(tier, m)| Some((tier.clone(), m.get(&name)?.clone())))
                    .collect();
                let prices = ModelPrices {
                    standard,
                    tiers,
                    speed: p.speed.get(&name).cloned().unwrap_or_default(),
                    geo: p.geo.clone(),
                };
                models.insert(name, prices);
            }
            sources.push(PriceSource {
                provider: p.name,
                url: p.source,
                checked: p.checked,
            });
        }
        Ok(Self {
            version: raw.version,
            sources,
            models,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn sources(&self) -> &[PriceSource] {
        &self.sources
    }

    /// Cost of one call; an amount only when every price it needs is known.
    pub fn cost(&self, usage: &UsageInfo) -> Result<Cost, Unpriced> {
        let name = usage.model.as_deref().ok_or(Unpriced::UnknownModel)?;
        let model = self
            .models
            .get(name)
            .or_else(|| without_date(name).and_then(|n| self.models.get(n)))
            .ok_or(Unpriced::UnknownModel)?;
        let rates = match usage.service_tier.as_deref() {
            None | Some("default" | "auto" | "standard") => Some(&model.standard),
            // OpenAI's Fast tier is reported as `priority`; accept either name.
            Some("fast") => model.tiers.get("priority"),
            Some(tier) => model.tiers.get(tier),
        }
        .ok_or(Unpriced::UnsupportedModifier)?;
        // A tier without its own threshold still switches at the standard one.
        let rates = match rates.above_tokens.or(model.standard.above_tokens) {
            Some(limit) if usage.prompt_tokens > limit => {
                rates.above.as_deref().ok_or(Unpriced::MissingPrice)?
            }
            _ => rates,
        };
        let factor = multiplier(&model.speed, usage.speed.as_deref(), "standard")?
            * multiplier(&model.geo, usage.inference_geo.as_deref(), "global")?;
        let read = usage.cache_read_tokens.unwrap_or(0);
        let write = usage.cache_write_tokens.unwrap_or(0);
        let long = usage.cache_write_long_ttl_tokens.unwrap_or(0).min(write);
        let uncached = read
            .checked_add(write)
            .and_then(|cached| usage.prompt_tokens.checked_sub(cached))
            .ok_or(Unpriced::InconsistentUsage)?;
        let price = |tokens: u64, rate: Option<f64>| match (tokens, rate) {
            (0, _) => Ok(0.0),
            (_, Some(rate)) => Ok(tokens as f64 * rate * factor / 1_000_000.0),
            (_, None) => Err(Unpriced::MissingPrice),
        };
        Ok(Cost {
            input: price(uncached, rates.input)?,
            cache_read: price(read, rates.cache_read)?,
            cache_write: price(write - long, rates.cache_write)?
                + price(long, rates.cache_write_long)?,
            output: price(usage.completion_tokens, rates.output)?,
        })
    }
}

fn multiplier(map: &HashMap<String, f64>, v: Option<&str>, neutral: &str) -> Result<f64, Unpriced> {
    match v {
        None => Ok(1.0),
        Some(v) if v == neutral => Ok(1.0),
        Some(v) => map.get(v).copied().ok_or(Unpriced::UnsupportedModifier),
    }
}

/// Model name without a snapshot date (`-2024-08-06`, `-20250929`, `@20251101`).
fn without_date(name: &str) -> Option<&str> {
    let num = |s: &str, n: usize| -> Option<u32> {
        (s.len() == n && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
    };
    let is_date = |y: &str, m: &str, d: &str| {
        num(y, 4).is_some()
            && matches!(num(m, 2), Some(1..=12))
            && matches!(num(d, 2), Some(1..=31))
    };
    if let Some((base, date)) = name.rsplit_once(['-', '@'])
        && let (Some(y), Some(m), Some(d)) = (date.get(..4), date.get(4..6), date.get(6..))
        && is_date(y, m, d)
    {
        return Some(base);
    }
    let (rest, dd) = name.rsplit_once('-')?;
    let (rest, mm) = rest.rsplit_once('-')?;
    let (base, yyyy) = rest.rsplit_once('-')?;
    is_date(yyyy, mm, dd).then_some(base)
}
