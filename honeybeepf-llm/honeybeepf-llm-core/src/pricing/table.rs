//! Price table file format and the checks a table must pass before it is used.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::Unpriced;

/// Prices in USD per million tokens; a call that needs a missing price stays unpriced.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Rates {
    pub(super) input: Option<f64>,
    pub(super) output: Option<f64>,
    pub(super) cache_read: Option<f64>,
    pub(super) cache_write: Option<f64>,
    pub(super) cache_write_long: Option<f64>,
    /// Prompts over this many tokens (cache included) pay `above` for every category.
    pub(super) above_tokens: Option<u64>,
    pub(super) above: Option<Box<Rates>>,
    /// Last day (UTC) these prices apply; `then` holds the prices after it.
    #[serde(default, deserialize_with = "day")]
    until: Option<u32>,
    then: Option<Box<Rates>>,
}

impl Rates {
    /// Prices in effect on `day`; none once the last `until` has passed.
    pub(super) fn on(&self, day: u32) -> Result<&Rates, Unpriced> {
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
pub(super) struct Unique<V>(pub(super) HashMap<String, V>);

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
pub(super) struct RawTable {
    pub(super) version: String,
    pub(super) providers: Vec<RawProvider>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawProvider {
    pub(super) name: String,
    pub(super) source: String,
    pub(super) checked: String,
    pub(super) models: Unique<Rates>,
    /// Served service tier (`flex`, `priority`) -> model -> prices.
    #[serde(default)]
    pub(super) tiers: Unique<Unique<Rates>>,
    /// Model -> speed mode (`fast`) -> multiplier on every price.
    #[serde(default)]
    pub(super) speed: Unique<Unique<f64>>,
    /// Model -> inference geography (`us`) -> multiplier, only for models that accept the setting.
    #[serde(default)]
    pub(super) geo: Unique<Unique<f64>>,
}

impl RawProvider {
    /// Why this provider's entries cannot be priced as written.
    pub(super) fn check(&self) -> Result<(), String> {
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
