//! Liveness heartbeat: the agent rewrites a file only while every registered loop keeps turning, so an
//! exec probe can tell a hung agent from a running one without a listening port.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::warn;
use once_cell::sync::Lazy;

use crate::probes::shutdown_flag;

/// Path of the heartbeat file; unset (a binary install) means no heartbeat.
const HEARTBEAT_FILE_ENV: &str = "HEARTBEAT_FILE";
/// A loop that has not beaten for this long counts as stuck, and the file stops being rewritten.
const STALE_SECS: u64 = 30;
/// How often the file is rewritten while every loop is live.
const WRITE_INTERVAL: Duration = Duration::from_secs(10);

static START: Lazy<Instant> = Lazy::new(Instant::now);
static LOOPS: Mutex<Vec<(String, Arc<AtomicU64>)>> = Mutex::new(Vec::new());

/// One registered loop's last beat, in seconds since the agent started.
pub struct Beat(Arc<AtomicU64>);

impl Beat {
    pub fn beat(&self) {
        self.0.store(now_secs(), Ordering::Relaxed);
    }
}

/// Register a loop the heartbeat depends on; it must call `beat` at least every `STALE_SECS`.
pub fn register(name: &str) -> Beat {
    let last = Arc::new(AtomicU64::new(now_secs()));
    LOOPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((name.to_string(), last.clone()));
    Beat(last)
}

/// Start rewriting the heartbeat file named by `HEARTBEAT_FILE`, if it is set.
pub fn spawn_writer() {
    let Some(path) = std::env::var_os(HEARTBEAT_FILE_ENV).map(PathBuf::from) else {
        return;
    };
    let shutdown = shutdown_flag();
    std::thread::spawn(move || {
        let mut reported = Vec::new();
        while !shutdown.load(Ordering::Relaxed) {
            let stuck = {
                let loops = LOOPS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                stale_loops(&loops, now_secs())
            };
            if stuck.is_empty() {
                if let Err(e) = std::fs::write(&path, now_secs().to_string()) {
                    warn!("Heartbeat write to {} failed: {e}", path.display());
                }
            } else if stuck != reported {
                warn!("Heartbeat paused: no progress from {}", stuck.join(", "));
            }
            reported = stuck;
            std::thread::sleep(WRITE_INTERVAL);
        }
    });
}

/// Names of the loops whose last beat is older than `STALE_SECS` at `now`.
fn stale_loops(loops: &[(String, Arc<AtomicU64>)], now: u64) -> Vec<String> {
    loops
        .iter()
        .filter(|(_, last)| now.saturating_sub(last.load(Ordering::Relaxed)) > STALE_SECS)
        .map(|(name, _)| name.clone())
        .collect()
}

fn now_secs() -> u64 {
    START.elapsed().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loop_without_a_recent_beat_is_stale() {
        let at = |secs| Arc::new(AtomicU64::new(secs));
        let loops = vec![("ring".to_string(), at(100)), ("main".to_string(), at(60))];
        assert!(stale_loops(&loops, 100 + STALE_SECS).iter().eq(["main"]));
        assert!(stale_loops(&loops, 90).is_empty());
        assert_eq!(stale_loops(&loops, 200).len(), 2);
    }
}
