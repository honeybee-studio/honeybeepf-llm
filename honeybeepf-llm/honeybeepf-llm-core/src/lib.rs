//! Pure analysis primitives for the honeybeepf agent.
//!
//! This crate contains stateless logic (types, byte utilities, parsers, matchers)
//! that does not depend on aya, tokio, OpenTelemetry, or `/proc`. It can be
//! built and tested on any platform without an eBPF toolchain.

pub mod byte_utils;
pub mod types;
