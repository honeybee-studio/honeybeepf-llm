//! Rebuilding the meter provider before an instrument reaches the SDK's 2,000-series limit, past which
//! new series fold into `otel.metric.overflow`. Active counter series carry their totals into the new
//! provider, so a backend sees them keep rising; series of pods that are gone are dropped.

use log::{info, warn};
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::HoneyBeeMetrics;

/// Series an instrument may reach before the provider is rebuilt; the SDK folds everything past 2,000.
pub(super) const ROTATE_AT: usize = 1_500;
/// How often the rotation task checks whether a rebuild is due.
const CHECK_INTERVAL_SECS: u64 = 10;
/// Fewest seconds between rebuilds, so a flood of new label values cannot keep the agent rebuilding.
const MIN_INTERVAL_SECS: u64 = 600;
/// How long the old provider keeps taking records after the swap, before its totals are carried over.
/// Well under the export interval, so the new provider has the totals before its first export.
const DRAIN_SECS: u64 = 2;
/// A series written this recently carries its total into the new provider. Longer than the bundled
/// collector's 5 minute expiry, so a series it still exports never goes backwards.
pub(super) const ACTIVE_WINDOW_SECS: u64 = 900;

/// What one record adds to its series.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Total {
    U64(u64),
    F64(f64),
    /// A histogram sample: counted toward the budget, never carried over.
    Hist,
}

impl From<u64> for Total {
    fn from(n: u64) -> Self {
        Total::U64(n)
    }
}

impl From<f64> for Total {
    fn from(v: f64) -> Self {
        Total::F64(v)
    }
}

struct Series {
    attrs: Vec<KeyValue>,
    total: Total,
    last: Instant,
}

/// The series each instrument of one provider has written, with the running total of each counter.
#[derive(Default)]
pub(super) struct SeriesBudget {
    seen: Mutex<HashMap<&'static str, HashMap<u64, Series>>>,
    due: AtomicBool,
}

impl SeriesBudget {
    /// Note one record of `instrument`; a new series that brings it to `ROTATE_AT` marks a rebuild due.
    pub(super) fn note(&self, instrument: &'static str, attrs: &[KeyValue], add: Total) {
        let mut h = DefaultHasher::new();
        for kv in attrs {
            kv.key.as_str().hash(&mut h);
            kv.value.as_str().hash(&mut h);
        }
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        let series = seen.entry(instrument).or_default();
        let len = series.len();
        let s = series.entry(h.finish()).or_insert_with(|| Series {
            attrs: attrs.to_vec(),
            total: match add {
                Total::U64(_) => Total::U64(0),
                Total::F64(_) => Total::F64(0.0),
                Total::Hist => Total::Hist,
            },
            last: Instant::now(),
        });
        s.last = Instant::now();
        s.total = match (s.total, add) {
            (Total::U64(a), Total::U64(b)) => Total::U64(a.saturating_add(b)),
            (Total::F64(a), Total::F64(b)) => Total::F64(a + b),
            (t, _) => t,
        };
        if series.len() > len && series.len() >= ROTATE_AT {
            self.due.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn due(&self) -> bool {
        self.due.load(Ordering::Relaxed)
    }

    /// Add the totals of counter series written within `window` to `into`; returns how many were carried.
    pub(super) fn carry_over(&self, into: &HoneyBeeMetrics, window: Duration) -> usize {
        let Ok(seen) = self.seen.lock() else {
            return 0;
        };
        let mut carried = 0;
        for (name, series) in seen.iter() {
            for s in series.values().filter(|s| s.last.elapsed() <= window) {
                if into.carry(name, s.total, &s.attrs) {
                    carried += 1;
                }
            }
        }
        carried
    }
}

/// Check every few seconds and, once a budget is spent, rebuild the provider through `rebuild`, which
/// returns the provider and metrics it replaced. After a short drain the active totals are carried into
/// the new metrics, then the old provider is shut down, exporting its last values.
pub(super) fn spawn(
    rebuild: impl Fn() -> anyhow::Result<Option<(SdkMeterProvider, Arc<HoneyBeeMetrics>)>>
    + Send
    + 'static,
) {
    let shutdown = crate::probes::shutdown_flag();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(CHECK_INTERVAL_SECS));
        let mut last = Instant::now();
        while !shutdown.load(Ordering::Relaxed) {
            tick.tick().await;
            if !super::metrics().is_some_and(|m| m.budget.due())
                || last.elapsed() < Duration::from_secs(MIN_INTERVAL_SECS)
            {
                continue;
            }
            last = Instant::now();
            let (old_provider, old_metrics) = match rebuild() {
                Ok(Some(old)) => old,
                Ok(None) => continue,
                Err(e) => {
                    warn!("Metrics provider rebuild failed, keeping the current one: {e:#}");
                    continue;
                }
            };
            tokio::time::sleep(Duration::from_secs(DRAIN_SECS)).await;
            let carried = super::metrics().map_or(0, |new| {
                old_metrics
                    .budget
                    .carry_over(&new, Duration::from_secs(ACTIVE_WINDOW_SECS))
            });
            info!(
                "Metrics provider rebuilt: an instrument reached {ROTATE_AT} series; {carried} active \
                 counter series carried over, the rest dropped"
            );
            if super::metrics().is_some_and(|m| m.budget.due()) {
                warn!(
                    "More than {ROTATE_AT} series of one instrument were active in the last \
                     {ACTIVE_WINDOW_SECS} s; series past the SDK limit of 2000 go to otel.metric.overflow"
                );
            }
            // Shutdown blocks until the last export ends, so keep it off the runtime's workers.
            let shut = tokio::task::spawn_blocking(move || old_provider.shutdown()).await;
            if let Ok(Err(e)) = shut {
                warn!("Failed to shut down the replaced MeterProvider: {e}");
            }
        }
    });
}
