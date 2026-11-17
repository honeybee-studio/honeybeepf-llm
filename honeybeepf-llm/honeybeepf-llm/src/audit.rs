//! Audit events: what the agent did on this node. Each is logged under target `audit` with a fixed `event` name and
//! its details as fields, so `RUST_LOG=warn,audit=info` keeps only these and `LOG_FORMAT=json` makes the fields queryable.

use std::fmt::Display;
use tracing::{info, warn};

/// The log target of every audit event.
pub const TARGET: &str = "audit";

/// The agent started, on this kernel.
pub fn startup(version: &str) {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let kernel = kernel.trim();
    info!(target: TARGET, event = "startup", version, kernel, "honeybeepf-llm starting");
}

/// LLM probes were attached to a library or executable; `kind` is `libssl`, `exe_openssl` or `exe_go`.
pub fn attach(kind: &str, path: &str, ino: u64, detail: &str) {
    info!(target: TARGET, event = "attach", kind, path, ino, detail, "LLM probes attached");
}

/// A library or executable that has TLS code could not be probed.
pub fn attach_failed(path: &str, error: &dyn Display) {
    warn!(target: TARGET, event = "attach_failed", path, error = %error, "LLM probes not attached");
}

/// Plaintext capture is on: socket programs attached at `cgroup` for `endpoints` configured endpoints.
pub fn plaintext_capture_on(cgroup: &str, endpoints: usize) {
    info!(target: TARGET, event = "plaintext_capture", enabled = true, cgroup, endpoints, "Plaintext capture enabled");
}

/// Plaintext capture was configured but stays off.
pub fn plaintext_capture_off(reason: &dyn Display) {
    warn!(target: TARGET, event = "plaintext_capture", enabled = false, reason = %reason, "Plaintext capture disabled");
}

/// The provider set the probe runs with; `summary` is the line people read.
pub fn providers_loaded(summary: &str, providers: &str, entries: usize, config_hash: &str) {
    info!(target: TARGET, event = "providers_loaded", providers, entries, config_hash, "{summary}");
}

/// The price table costs are computed with, and what a user price file changed in it.
pub fn prices_loaded(version: &str, replaced: &[String], added: &[String]) {
    let (replaced, added) = (replaced.join(","), added.join(","));
    info!(target: TARGET, event = "prices_loaded", version, replaced, added, "Price table {version} loaded");
}

/// A user price file changed while running and was taken.
pub fn prices_reloaded(from: &str, to: &str, replaced: &[String], added: &[String]) {
    let (replaced, added) = (replaced.join(","), added.join(","));
    info!(target: TARGET, event = "prices_reloaded", from, to, replaced, added, "Price table {from} -> {to}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// What `emit` logs through a subscriber in `json` or text format.
    fn capture(json: bool, emit: impl FnOnce()) -> String {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = {
            let buf = buf.clone();
            move || SharedBuf(buf.clone())
        };
        let builder = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false);
        if json {
            tracing::subscriber::with_default(builder.json().finish(), emit);
        } else {
            tracing::subscriber::with_default(builder.finish(), emit);
        }
        let bytes = buf.lock().map(|b| b.clone()).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut b) = self.0.lock() {
                b.extend_from_slice(data);
            }
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn json_audit_events_carry_their_fields() -> Result<(), serde_json::Error> {
        let out = capture(true, || attach("exe_go", "/usr/bin/app", 42, "go1.26"));
        let line: serde_json::Value = serde_json::from_str(out.trim())?;
        assert_eq!(line["target"], "audit");
        assert_eq!(line["level"], "INFO");
        assert_eq!(line["fields"]["event"], "attach");
        assert_eq!(line["fields"]["kind"], "exe_go");
        assert_eq!(line["fields"]["path"], "/usr/bin/app");
        assert_eq!(line["fields"]["ino"], 42);
        Ok(())
    }

    #[test]
    fn a_newline_in_a_path_cannot_forge_a_log_line() {
        let forged = "/tmp/x\n2026-01-01T00:00:00Z  INFO audit: fake";
        for json in [false, true] {
            let out = capture(json, || attach_failed(forged, &"denied"));
            assert_eq!(out.lines().count(), 1, "{out}");
        }
    }
}
