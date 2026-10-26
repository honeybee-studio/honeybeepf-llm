//! Price table and the cost of one call from its parsed usage.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::marker::PhantomData;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

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
    /// Last day (UTC) these prices apply; `then` holds the prices after it.
    #[serde(default, deserialize_with = "day")]
    until: Option<u32>,
    then: Option<Box<Rates>>,
}

impl Rates {
    /// Prices in effect on `day`; none once the last `until` has passed.
    fn on(&self, day: u32) -> Result<&Rates, Unpriced> {
        let mut rates = self;
        while let Some(until) = rates.until
            && day > until
        {
            rates = rates.then.as_deref().ok_or(Unpriced::MissingPrice)?;
        }
        Ok(rates)
    }

    /// Why these prices cannot be applied as written.
    fn check(&self) -> Result<(), &'static str> {
        let prices = [
            self.input,
            self.output,
            self.cache_read,
            self.cache_write,
            self.cache_write_long,
        ];
        if !prices
            .iter()
            .flatten()
            .all(|p| p.is_sign_positive() && *p <= MAX_PRICE)
        {
            return Err("prices must be between 0 and 1e6");
        }
        if let Some(above) = &self.above {
            // Tiers too, so their long prices never hang on the standard's dates.
            if self.above_tokens.is_none() {
                return Err("above needs above_tokens");
            }
            // cost() switches once, so anything nested here would never apply.
            if above.above_tokens.is_some()
                || above.above.is_some()
                || above.until.is_some()
                || above.then.is_some()
            {
                return Err("above cannot have its own threshold or dates");
            }
            above.check()?;
        }
        match (self.until, self.then.as_deref()) {
            (None, Some(_)) => Err("then needs until"),
            (Some(until), Some(then)) if then.until.is_some_and(|next| next <= until) => {
                Err("until dates must increase")
            }
            (_, Some(then)) => then.check(),
            (_, None) => Ok(()),
        }
    }
}

/// Caps that keep every cost finite; real prices and multipliers are far below them.
const MAX_PRICE: f64 = 1e6;
const MAX_FACTOR: f64 = 100.0;

/// A JSON object with unique keys; serde_json would keep the last duplicate silently.
struct Unique<V>(HashMap<String, V>);

impl<V> Default for Unique<V> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for Unique<V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Keys<V>(PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for Keys<V> {
            type Value = Unique<V>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object with unique keys")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = HashMap::new();
                while let Some((key, value)) = map.next_entry::<String, V>()? {
                    if out.contains_key(&key) {
                        return Err(A::Error::custom(format!("{key} is listed twice")));
                    }
                    out.insert(key, value);
                }
                Ok(Unique(out))
            }
        }
        d.deserialize_map(Keys(PhantomData))
    }
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
    models: Unique<Rates>,
    /// Served service tier (`flex`, `priority`) -> model -> prices.
    #[serde(default)]
    tiers: Unique<Unique<Rates>>,
    /// Model -> speed mode (`fast`) -> multiplier on every price.
    #[serde(default)]
    speed: Unique<Unique<f64>>,
    /// Model -> inference geography (`us`) -> multiplier, only for models that accept the setting.
    #[serde(default)]
    geo: Unique<Unique<f64>>,
}

impl RawProvider {
    /// Why this provider's entries cannot be priced as written.
    fn check(&self) -> Result<(), String> {
        if parse_day(&self.checked).is_none() {
            return Err("checked is not a YYYY-MM-DD date from 1970 on".into());
        }
        let models = &self.models.0;
        let speed_modes = self.speed.0.values().flat_map(|m| m.0.iter());
        let geos = self.geo.0.values().flat_map(|m| m.0.iter());
        let unknown = self
            .tiers
            .0
            .values()
            .flat_map(|m| m.0.keys())
            .chain(self.speed.0.keys())
            .chain(self.geo.0.keys())
            .find(|name| !models.contains_key(*name));
        if let Some(name) = unknown {
            return Err(format!("{name} has no standard prices"));
        }
        // cost() maps these names before any lookup, so their entries would be dead.
        let dead = self
            .tiers
            .0
            .keys()
            .find(|t| matches!(t.as_str(), "default" | "auto" | "standard" | "fast"))
            .or_else(|| {
                speed_modes
                    .clone()
                    .map(|(s, _)| s)
                    .find(|s| *s == "standard")
            })
            .or_else(|| geos.clone().map(|(g, _)| g).find(|g| *g == "global"));
        if let Some(key) = dead {
            return Err(format!("{key} is never looked up"));
        }
        // A zero factor would turn a priced call into $0.
        if !speed_modes
            .chain(geos)
            .all(|(_, f)| *f > 0.0 && *f <= MAX_FACTOR)
        {
            return Err("multipliers must be above 0 and at most 100".into());
        }
        models.iter().try_for_each(|(name, standard)| {
            let mut tiers = self.tiers.0.values().filter_map(|m| m.0.get(name));
            let checked = standard
                .check()
                .and_then(|()| tiers.try_for_each(Rates::check));
            checked.map_err(|why| format!("{name}: {why}"))
        })
    }
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
}

impl PriceTable {
    /// The table shipped with the agent, copied from the providers' pricing pages.
    pub fn builtin() -> Result<Self, serde_json::Error> {
        Self::from_json(include_str!("prices.json"))
    }

    /// Load and check a table; a typo, duplicate, bad date or negative price is an error.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let raw: RawTable = serde_json::from_str(json)?;
        let mut models = HashMap::new();
        let mut sources = Vec::new();
        for p in raw.providers {
            p.check().map_err(|why| invalid(&p.name, why))?;
            let (Unique(tiers), Unique(speed), Unique(geo)) = (p.tiers, p.speed, p.geo);
            for (name, standard) in p.models.0 {
                let tiers = tiers
                    .iter()
                    .filter_map(|(tier, m)| Some((tier.clone(), m.0.get(&name)?.clone())))
                    .collect();
                let prices = ModelPrices {
                    standard,
                    tiers,
                    speed: speed.get(&name).map(|m| m.0.clone()).unwrap_or_default(),
                    geo: geo.get(&name).map(|m| m.0.clone()).unwrap_or_default(),
                };
                match models.entry(name) {
                    Entry::Occupied(e) => {
                        return Err(invalid(&p.name, format!("{} is listed twice", e.key())));
                    }
                    Entry::Vacant(e) => {
                        e.insert(prices);
                    }
                }
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

fn invalid(provider: &str, why: impl fmt::Display) -> serde_json::Error {
    serde_json::Error::custom(format!("{provider}: {why}"))
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

fn day<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    let Some(s) = Option::<String>::deserialize(d)? else {
        return Ok(None);
    };
    parse_day(&s)
        .map(Some)
        .ok_or_else(|| D::Error::custom(format!("{s:?} is not a YYYY-MM-DD date from 1970 on")))
}

/// Days since 1970-01-01 for a real `YYYY-MM-DD` date (Hinnant's days_from_civil).
fn parse_day(s: &str) -> Option<u32> {
    let (y, rest) = s.split_once('-')?;
    let (m, d) = rest.split_once('-')?;
    let digits = |p: &str, n: usize| p.len() == n && p.bytes().all(|b| b.is_ascii_digit());
    if !(digits(y, 4) && digits(m, 2) && digits(d, 2)) {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let last = match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => return None,
    };
    if !(1..=last).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    u32::try_from(era * 146_097 + doe - 719_468).ok()
}
