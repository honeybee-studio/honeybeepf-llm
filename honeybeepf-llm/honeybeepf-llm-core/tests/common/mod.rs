//! Calls and totals shared by the pricing tests.
#![allow(dead_code)] // Each test file uses a different subset of these helpers.

use honeybeepf_llm_core::pricing::{Cost, Unpriced};
use honeybeepf_llm_core::types::UsageInfo;

/// Expected total in USD, or why the call stays unpriced.
pub type Want = Result<f64, Unpriced>;
pub const UNKNOWN: Want = Err(Unpriced::UnknownModel);
pub const MISSING: Want = Err(Unpriced::MissingPrice);
pub const UNSUPPORTED: Want = Err(Unpriced::UnsupportedModifier);

/// A call to `model` with these prompt (cache included) and completion counts.
pub fn call(model: &str, prompt: u64, completion: u64) -> UsageInfo {
    UsageInfo {
        prompt_tokens: prompt,
        completion_tokens: completion,
        model: Some(model.to_string()),
        ..UsageInfo::default()
    }
}

/// Cache counts and price modifiers on top of [`call`]; `long` is the long-TTL part of `write`.
pub trait CallExt {
    fn read(self, n: u64) -> Self;
    fn write(self, n: u64) -> Self;
    fn long(self, n: u64) -> Self;
    fn thoughts(self, n: u64) -> Self;
    fn tier(self, name: &str) -> Self;
    fn speed(self, mode: &str) -> Self;
    fn geo(self, geo: &str) -> Self;
}

#[rustfmt::skip]
impl CallExt for UsageInfo {
    fn read(self, n: u64) -> Self { UsageInfo { cache_read_tokens: Some(n), ..self } }
    fn write(self, n: u64) -> Self { UsageInfo { cache_write_tokens: Some(n), ..self } }
    fn long(self, n: u64) -> Self { UsageInfo { cache_write_long_ttl_tokens: Some(n), ..self } }
    fn thoughts(self, n: u64) -> Self { UsageInfo { thoughts_tokens: Some(n), ..self } }
    fn tier(self, name: &str) -> Self { UsageInfo { service_tier: Some(name.into()), ..self } }
    fn speed(self, mode: &str) -> Self { UsageInfo { speed: Some(mode.into()), ..self } }
    fn geo(self, geo: &str) -> Self { UsageInfo { inference_geo: Some(geo.into()), ..self } }
}

/// Total USD rounded to 1e-12, so hand-computed amounts compare exactly.
pub fn total(c: Cost) -> f64 {
    ((c.input + c.cache_read + c.cache_write + c.output) * 1e12).round() / 1e12
}
