use std::collections::HashSet;

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::probes::discovery as probe_discovery;

/// SSL library pattern for libssl and libcrypto.
static SSL_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"libssl\.so\..*|libcrypto\.so\..*").unwrap());

/// Find all SSL libraries across running processes and system defaults.
pub fn find_all_targets() -> Result<HashSet<String>> {
    probe_discovery::find_libraries_all(&SSL_PATTERN, Some("libssl.so"))
}
