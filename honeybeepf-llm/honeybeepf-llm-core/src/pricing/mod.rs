//! Price table and the cost of one call from its parsed usage.

mod model_name;
mod table;

use std::collections::HashMap;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::Error as _;

use crate::types::UsageInfo;
use model_name::without_date;
use table::{Rates, RawTable};

struct ModelPrices {
    provider: String,
    standard: Rates,
    tiers: HashMap<String, Rates>,
    speed: HashMap<String, f64>,
    geo: HashMap<String, f64>,
    /// A user file's provider discount on this built-in entry, else 1.
    discount: f64,
}

/// Models a user price file replaced in the table, and models it added.
#[derive(Debug, Default, PartialEq)]
pub struct Overrides {
    pub replaced: Vec<String>,
    pub added: Vec<String>,
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
    /// A price the call needs is absent or past its last day.
    MissingPrice,
    UnsupportedModifier,
    /// Cache counts larger than the prompt total they are part of.
    InconsistentUsage,
}

impl Unpriced {
    /// Value of the `reason` label on the unpriced token counter.
    pub fn as_str(self) -> &'static str {
        match self {
            Unpriced::UnknownModel => "unknown_model",
            Unpriced::MissingPrice => "missing_price",
            Unpriced::UnsupportedModifier => "unsupported_modifier",
            Unpriced::InconsistentUsage => "inconsistent_usage",
        }
    }
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
    discounts: HashMap<String, f64>,
}

impl PriceTable {
    /// The table shipped with the agent, copied from the providers' pricing pages.
    pub fn builtin() -> Result<Self, serde_json::Error> {
        Self::from_json(include_str!("../prices.json"))
    }

    /// Load and check a table; a typo, duplicate, bad date or negative price is an error.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let raw: RawTable = serde_json::from_str(json)?;
        let mut models = HashMap::new();
        let mut sources = Vec::new();
        let mut discounts = HashMap::new();
        for p in raw.providers {
            p.check().map_err(|why| invalid(&p.name, why))?;
            if let Some(discount) = p.discount
                && discounts.insert(p.name.clone(), discount).is_some()
            {
                return Err(invalid(&p.name, "discount is listed twice"));
            }
            for (name, standard) in p.models {
                let tiers = p
                    .tiers
                    .iter()
                    .filter_map(|(tier, m)| Some((tier.clone(), m.get(&name)?.clone())))
                    .collect();
                let prices = ModelPrices {
                    provider: p.name.clone(),
                    standard,
                    tiers,
                    speed: p.speed.get(&name).cloned().unwrap_or_default(),
                    geo: p.geo.get(&name).cloned().unwrap_or_default(),
                    discount: 1.0,
                };
                if models.contains_key(&name) {
                    return Err(invalid(&p.name, format!("{name} is listed twice")));
                }
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
            discounts,
        })
    }

    /// Merge a user price file over this table per model; a rejected file changes nothing.
    pub fn merge_custom(&mut self, json: &str) -> Result<Overrides, serde_json::Error> {
        let custom = Self::from_json(json)?;
        let known = |p: &&String| self.sources.iter().any(|s| s.provider == **p);
        if let Some(p) = custom.discounts.keys().find(|p| !known(p)) {
            return Err(invalid(p, "discount matches no built-in provider"));
        }
        for m in self.models.values_mut() {
            m.discount = custom.discounts.get(&m.provider).copied().unwrap_or(1.0);
        }
        let mut overrides = Overrides::default();
        for (name, prices) in custom.models {
            match self.models.insert(name.clone(), prices) {
                Some(_) => overrides.replaced.push(name),
                None => overrides.added.push(name),
            }
        }
        overrides.replaced.sort();
        overrides.added.sort();
        self.sources.extend(custom.sources);
        self.version = format!("{}+custom-{:08x}", self.version, fnv1a(json));
        Ok(overrides)
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn sources(&self) -> &[PriceSource] {
        &self.sources
    }

    /// Cost of one call at today's (UTC) prices; see [`PriceTable::cost_on`].
    pub fn cost(&self, usage: &UsageInfo) -> Result<Cost, Unpriced> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.cost_on(usage, u32::try_from(secs / 86_400).unwrap_or(u32::MAX))
    }

    /// Cost of one call on `day` (days since 1970-01-01 UTC); an amount only when every price it needs is known.
    pub fn cost_on(&self, usage: &UsageInfo, day: u32) -> Result<Cost, Unpriced> {
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
        .ok_or(Unpriced::UnsupportedModifier)?
        .on(day)?;
        // A tier without its own threshold still switches at the standard one.
        let limit = match rates.above_tokens {
            Some(limit) => Some(limit),
            None => model.standard.on(day)?.above_tokens,
        };
        let rates = match limit {
            Some(limit) if usage.prompt_tokens > limit => {
                rates.above.as_deref().ok_or(Unpriced::MissingPrice)?
            }
            _ => rates,
        };
        let factor = model.discount
            * multiplier(&model.speed, usage.speed.as_deref(), "standard")?
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

fn invalid(provider: &str, why: impl fmt::Display) -> serde_json::Error {
    serde_json::Error::custom(format!("{provider}: {why}"))
}

/// FNV-1a: stable across builds, so the version label changes only with the file.
fn fnv1a(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

fn multiplier(map: &HashMap<String, f64>, v: Option<&str>, neutral: &str) -> Result<f64, Unpriced> {
    match v {
        None => Ok(1.0),
        Some(v) if v == neutral => Ok(1.0),
        Some(v) => map.get(v).copied().ok_or(Unpriced::UnsupportedModifier),
    }
}
