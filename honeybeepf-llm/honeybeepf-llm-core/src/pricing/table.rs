//! Price table file format and the checks a table must pass before it is used.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

use chrono::NaiveDate;
use serde::de::{DeserializeSeed, Error as _, MapAccess, Visitor};
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
        let in_range = |p: &f64| p.is_sign_positive() && *p <= MAX_PRICE;
        if !prices.iter().flatten().all(in_range) {
            return Err("prices must be between 0 and 1e6");
        }
        if let Some(above) = &self.above {
            // Tiers too, so their long prices never hang on the standard's dates.
            if self.above_tokens.is_none() {
                return Err("above needs above_tokens");
            }
            // cost() switches once, so anything nested here would never apply.
            let threshold = above.above_tokens.is_some() || above.above.is_some();
            if threshold || above.until.is_some() || above.then.is_some() {
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

/// Reads a JSON object into a map, rejecting a key that appears twice.
#[derive(Clone)]
struct Keys<S>(S);

impl<'de, S: DeserializeSeed<'de> + Clone> Visitor<'de> for Keys<S> {
    type Value = HashMap<String, S::Value>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object with unique keys")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut out = HashMap::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(self.0.clone())?;
            if out.contains_key(&key) {
                return Err(A::Error::custom(format!("{key} is listed twice")));
            }
            out.insert(key, value);
        }
        Ok(out)
    }
}

impl<'de, S: DeserializeSeed<'de> + Clone> DeserializeSeed<'de> for Keys<S> {
    type Value = HashMap<String, S::Value>;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_map(self)
    }
}

/// serde_json would keep the last of two equal keys silently.
fn unique_keys<'de, D: Deserializer<'de>, V: Deserialize<'de>>(
    d: D,
) -> Result<HashMap<String, V>, D::Error> {
    Keys(PhantomData::<V>).deserialize(d)
}

/// [`unique_keys`] for the outer and every inner object.
fn unique_nested_keys<'de, D: Deserializer<'de>, V: Deserialize<'de>>(
    d: D,
) -> Result<HashMap<String, HashMap<String, V>>, D::Error> {
    Keys(Keys(PhantomData::<V>)).deserialize(d)
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
    #[serde(deserialize_with = "unique_keys")]
    pub(super) models: HashMap<String, Rates>,
    /// Served service tier (`flex`, `priority`) -> model -> prices.
    #[serde(default, deserialize_with = "unique_nested_keys")]
    pub(super) tiers: HashMap<String, HashMap<String, Rates>>,
    /// Model -> speed mode (`fast`) -> multiplier on every price.
    #[serde(default, deserialize_with = "unique_nested_keys")]
    pub(super) speed: HashMap<String, HashMap<String, f64>>,
    /// Model -> inference geography (`us`) -> multiplier, only for models that accept the setting.
    #[serde(default, deserialize_with = "unique_nested_keys")]
    pub(super) geo: HashMap<String, HashMap<String, f64>>,
}

impl RawProvider {
    /// Why this provider's entries cannot be priced as written.
    pub(super) fn check(&self) -> Result<(), String> {
        if parse_day(&self.checked).is_none() {
            return Err("checked is not a YYYY-MM-DD date from 1970 on".into());
        }
        if let Some(name) = self.unknown_model() {
            return Err(format!("{name} has no standard prices"));
        }
        if let Some(key) = self.dead_key() {
            return Err(format!("{key} is never looked up"));
        }
        // A zero factor would turn a priced call into $0.
        let factors = self.speed.values().chain(self.geo.values());
        let in_range = |f: &f64| *f > 0.0 && *f <= MAX_FACTOR;
        if !factors.flat_map(HashMap::values).all(in_range) {
            return Err("multipliers must be above 0 and at most 100".into());
        }
        self.models.iter().try_for_each(|(name, standard)| {
            let mut tiers = self.tiers.values().filter_map(|m| m.get(name));
            let checked = standard
                .check()
                .and_then(|()| tiers.try_for_each(Rates::check));
            checked.map_err(|why| format!("{name}: {why}"))
        })
    }

    /// A model with tier, speed or geo entries but no standard prices.
    fn unknown_model(&self) -> Option<&String> {
        let tiered = self.tiers.values().flat_map(HashMap::keys);
        let mut named = tiered.chain(self.speed.keys()).chain(self.geo.keys());
        named.find(|name| !self.models.contains_key(*name))
    }

    /// A key cost() maps before any lookup, so its entry would be dead.
    fn dead_key(&self) -> Option<&String> {
        let mut speeds = self.speed.values().flat_map(HashMap::keys);
        let mut geos = self.geo.values().flat_map(HashMap::keys);
        self.tiers
            .keys()
            .find(|t| matches!(t.as_str(), "default" | "auto" | "standard" | "fast"))
            .or_else(|| speeds.find(|s| *s == "standard"))
            .or_else(|| geos.find(|g| *g == "global"))
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

/// Days since 1970-01-01 for a real `YYYY-MM-DD` date.
fn parse_day(s: &str) -> Option<u32> {
    // chrono alone would also take `2026-1-30`, a sign or leading spaces.
    let digits = s.bytes().all(|b| b == b'-' || b.is_ascii_digit());
    if !(digits && s.split('-').map(str::len).eq([4, 2, 2])) {
        return None;
    }
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    u32::try_from(date.to_epoch_days()).ok()
}
