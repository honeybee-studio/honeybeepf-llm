//! OpenTelemetry metrics export module
//!
//! Exports eBPF metrics collected by honeybeepf-llm to OpenTelemetry Collector.
//!
//! ## Metrics Categories
//! - **Filesystem**: File access auditing
//!
//! ## OTLP Endpoint Priority
//! 1. Helm values (injected via environment variables)
//! 2. Direct environment variable configuration

use anyhow::{Context, Result};
use honeybeepf_llm_core::pricing::PriceTable;
use honeybeepf_llm_core::types::UsageInfo;
use log::info;
use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

#[cfg(feature = "k8s")]
use crate::k8s::PodInfo;

/// Metric export interval in seconds
const METRIC_EXPORT_INTERVAL_SECS: u64 = 30;

/// `llm_latency_seconds` bucket bounds; the SDK default is millisecond-scale (0, 5, 10, … 10000).
const LLM_LATENCY_BOUNDARIES_SECS: [f64; 18] = [
    0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 7.5, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0, 90.0, 120.0,
    180.0, 300.0,
];

/// Global metrics handle
static METRICS: OnceLock<HoneyBeeMetrics> = OnceLock::new();

/// Global MeterProvider for graceful shutdown
static METER_PROVIDER: OnceLock<SdkMeterProvider> = OnceLock::new();

/// Global active probes count (for ObservableGauge callback)
static ACTIVE_PROBES: OnceLock<RwLock<HashMap<String, u64>>> = OnceLock::new();

fn active_probes_map() -> &'static RwLock<HashMap<String, u64>> {
    ACTIVE_PROBES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// honeybeepf-llm metrics collection
pub struct HoneyBeeMetrics {
    pub block_io_events: Counter<u64>,
    pub block_io_bytes: Counter<u64>,
    pub block_io_latency_ns: Histogram<u64>,
    pub network_latency_ns: Histogram<u64>,
    pub gpu_open_events: Counter<u64>,
    pub process_exit_total: Counter<u64>,
    pub process_lifetime_seconds: Histogram<f64>,
    pub file_access_events: Counter<u64>,
    pub llm_requests_total: Counter<u64>,
    pub llm_tokens_total: Counter<u64>,
    pub llm_cache_read_tokens_total: Counter<u64>,
    pub llm_cache_write_tokens_total: Counter<u64>,
    pub llm_reasoning_tokens_total: Counter<u64>,
    pub llm_latency_seconds: Histogram<f64>,
    pub llm_responses_unparsed_total: Counter<u64>,
    pub llm_cost_usd_total: Counter<f64>,
    pub llm_unpriced_tokens_total: Counter<u64>,
    /// Built-in price table and its `price_table_version` label, built once.
    prices: Option<(PriceTable, KeyValue)>,
}

impl HoneyBeeMetrics {
    fn new(meter: &Meter) -> Self {
        Self {
            block_io_events: meter
                .u64_counter("block_io_events")
                .with_description("Number of block I/O events")
                .build(),
            block_io_bytes: meter
                .u64_counter("block_io_bytes")
                .with_description("Total block I/O bytes")
                .build(),
            block_io_latency_ns: meter
                .u64_histogram("block_io_latency_ns")
                .with_description("Block I/O latency in nanoseconds")
                .build(),
            network_latency_ns: meter
                .u64_histogram("network_latency_ns")
                .with_description("Network latency in nanoseconds")
                .build(),
            gpu_open_events: meter
                .u64_counter("gpu_open`_events")
                .with_description("Number of GPU open events")
                .build(),
            file_access_events: meter
                .u64_counter("file_access_events")
                .with_description("Number of monitored file access events")
                .with_unit("events")
                .build(),
            process_exit_total: meter
                .u64_counter("process_exit")
                .with_description("Total number of process exits")
                .build(),
            process_lifetime_seconds: meter
                .f64_histogram("process_lifetime_seconds")
                .with_description("Process lifetime in seconds")
                .with_unit("s")
                .build(),

            // === LLM ===
            llm_requests_total: meter
                .u64_counter("llm_requests_total")
                .with_description("Total number of LLM requests")
                .with_unit("requests")
                .build(),
            llm_tokens_total: meter
                .u64_counter("llm_tokens_total")
                .with_description("Total number of LLM tokens processed")
                .with_unit("tokens")
                .build(),
            // Separate counters, not token_type values: these are subsets of the prompt count,
            // so summing llm_tokens_total stays the true total (OTel cache_read/cache_write counters).
            llm_cache_read_tokens_total: meter
                .u64_counter("llm_cache_read_tokens_total")
                .with_description(
                    "Prompt tokens served from the provider cache, already in llm_tokens_total",
                )
                .with_unit("tokens")
                .build(),
            llm_cache_write_tokens_total: meter
                .u64_counter("llm_cache_write_tokens_total")
                .with_description(
                    "Prompt tokens written to the provider cache, already in llm_tokens_total",
                )
                .with_unit("tokens")
                .build(),
            // Subset of the completion count, for the same reason.
            llm_reasoning_tokens_total: meter
                .u64_counter("llm_reasoning_tokens_total")
                .with_description(
                    "Completion tokens spent on reasoning, already in llm_tokens_total",
                )
                .with_unit("tokens")
                .build(),
            llm_latency_seconds: meter
                .f64_histogram("llm_latency_seconds")
                .with_description("Latency of LLM requests")
                .with_unit("s")
                .with_boundaries(LLM_LATENCY_BOUNDARIES_SECS.to_vec())
                .build(),
            llm_responses_unparsed_total: meter
                .u64_counter("llm_responses_unparsed_total")
                .with_description(
                    "LLM responses not recorded because bytes were lost in capture or a complete response had no usage",
                )
                .with_unit("responses")
                .build(),
            // No unit: the Prometheus exporter would append it to the name.
            llm_cost_usd_total: meter
                .f64_counter("llm_cost_usd_total")
                .with_description("Cost of LLM calls in USD from the price table, by cost_type")
                .build(),
            llm_unpriced_tokens_total: meter
                .u64_counter("llm_unpriced_tokens_total")
                .with_description(
                    "Tokens of LLM calls the price table could not price, by reason; never counted as $0",
                )
                .with_unit("tokens")
                .build(),
            prices: load_prices(),
        }
    }

    /// Record a call's cost by category, or its tokens as unpriced with the reason.
    fn record_cost(&self, usage: &UsageInfo, labels: &mut Vec<KeyValue>) {
        let Some((table, version)) = &self.prices else {
            return;
        };
        let base = labels.len();
        labels.push(version.clone());
        match table.cost(usage) {
            Ok(cost) => {
                let parts = [
                    ("input", cost.input),
                    ("cache_read", cost.cache_read),
                    ("cache_write", cost.cache_write),
                    ("output", cost.output),
                ];
                for (cost_type, usd) in parts {
                    if usd > 0.0 {
                        labels.push(KeyValue::new("cost_type", cost_type));
                        self.llm_cost_usd_total.add(usd, labels);
                        labels.pop();
                    }
                }
            }
            Err(why) => {
                labels.push(KeyValue::new("reason", why.as_str()));
                let tokens = usage.prompt_tokens.saturating_add(usage.completion_tokens);
                self.llm_unpriced_tokens_total.add(tokens, labels);
            }
        }
        labels.truncate(base);
    }

    fn record_llm(
        &self,
        usage: &UsageInfo,
        latency_secs: f64,
        is_error: bool,
        mut attrs: Vec<KeyValue>,
    ) {
        // 1. total requests
        self.llm_requests_total.add(1, &attrs);

        // 2. latency
        self.llm_latency_seconds.record(latency_secs, &attrs);

        // 3. tokens
        if !is_error {
            if let Some(n) = usage.cache_read_tokens {
                self.llm_cache_read_tokens_total.add(n, &attrs);
            }
            if let Some(n) = usage.cache_write_tokens {
                self.llm_cache_write_tokens_total.add(n, &attrs);
            }
            if let Some(n) = usage.thoughts_tokens {
                self.llm_reasoning_tokens_total.add(n, &attrs);
            }
            self.record_cost(usage, &mut attrs);

            let mut prompt_attrs = attrs.clone();
            prompt_attrs.push(KeyValue::new("token_type", "prompt"));
            self.llm_tokens_total
                .add(usage.prompt_tokens, &prompt_attrs);

            let mut comp_attrs = attrs; // move ownership
            comp_attrs.push(KeyValue::new("token_type", "completion"));
            self.llm_tokens_total
                .add(usage.completion_tokens, &comp_attrs);
        }
    }
}

/// Load the built-in price table; if it is rejected the agent runs on without cost metrics.
fn load_prices() -> Option<(PriceTable, KeyValue)> {
    match PriceTable::builtin() {
        Ok(table) => {
            for s in table.sources() {
                info!(
                    "Price table {}: {} prices from {} (checked {})",
                    table.version(),
                    s.provider,
                    s.url,
                    s.checked
                );
            }
            let version = KeyValue::new("price_table_version", Arc::<str>::from(table.version()));
            Some((table, version))
        }
        Err(e) => {
            log::warn!("Built-in price table rejected, LLM cost not recorded: {e}");
            None
        }
    }
}

/// Priority:
/// 1. OTEL_EXPORTER_OTLP_ENDPOINT environment variable (injected from Helm values)
/// 2. If not set, metrics are disabled (no default fallback)
fn get_otlp_endpoint() -> Option<String> {
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()?;
    if endpoint.is_empty() {
        return None;
    }

    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        Some(format!("http://{}", endpoint))
    } else {
        Some(endpoint)
    }
}

/// Initialize OpenTelemetry metrics provider
///
/// Configures metrics export to OTLP Collector via gRPC.
/// Skips initialization if OTEL_EXPORTER_OTLP_ENDPOINT is not set.
pub fn init_metrics() -> Result<()> {
    let endpoint = match get_otlp_endpoint() {
        Some(ep) => ep,
        None => {
            info!("OTEL_EXPORTER_OTLP_ENDPOINT not set. Metrics export disabled.");
            return Ok(());
        }
    };

    info!("Initializing OpenTelemetry metrics exporter");
    info!("OTLP endpoint: {}", endpoint);

    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .with_timeout(Duration::from_secs(10))
        .build()
        .context("Failed to create OTLP metric exporter")?;

    let reader = PeriodicReader::builder(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_interval(Duration::from_secs(METRIC_EXPORT_INTERVAL_SECS))
        .build();

    let resource = Resource::default().merge(&Resource::new(vec![
        KeyValue::new("service.name", "honeybeepf-llm"),
        KeyValue::new("telemetry.sdk.language", "rust"),
    ]));

    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();

    global::set_meter_provider(provider.clone());
    let _ = METER_PROVIDER.set(provider);

    // Meter name is used as prefix only
    let meter = global::meter("honeybeepf-llm");

    // This is the correct way to export gauge metrics via OTLP
    let _active_probes_gauge = meter
        .u64_observable_gauge("active_probes")
        .with_description("Number of currently active eBPF probes")
        .with_unit("probes")
        .with_callback(|observer| {
            if let Ok(probes) = active_probes_map().read() {
                for (probe_name, count) in probes.iter() {
                    observer.observe(*count, &[KeyValue::new("probe", probe_name.clone())]);
                }
            }
        })
        .build();

    // Kernel LLM event pipeline counters (cumulative, read from the LLM_STATS map)
    let _llm_events_counter = meter
        .u64_observable_counter("llm_events_total")
        .with_description("SSL events seen by the LLM eBPF probes, by pipeline stage")
        .with_unit("events")
        .with_callback(|observer| {
            if let Some(totals) = crate::probes::builtin::llm::event_stats() {
                for (name, v) in honeybeepf_llm_common::LLM_STAT_NAMES.iter().zip(totals) {
                    observer.observe(v, &[KeyValue::new("stage", *name)]);
                }
            }
        })
        .build();

    let _ = METRICS.set(HoneyBeeMetrics::new(&meter));

    info!("OpenTelemetry metrics initialized successfully");
    Ok(())
}

pub fn metrics() -> Option<&'static HoneyBeeMetrics> {
    METRICS.get()
}

/// Record active probe count
/// Updates the global active probes map for ObservableGauge callback
pub fn record_active_probe(probe_name: &str, count: u64) {
    // Update the global map (ObservableGauge callback reads from this)
    if let Ok(mut probes) = active_probes_map().write() {
        probes.insert(probe_name.to_string(), count);
        info!("Active probe registered: {} = {}", probe_name, count);
    }
}

pub fn record_file_access_event(
    filename: &str,
    flags: &str,
    comm: &str,
    cgroup_id: u64,
    #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<PodInfo>>,
) {
    if let Some(m) = metrics() {
        #[allow(unused_mut)]
        let mut attrs = vec![
            KeyValue::new("filename", filename.to_string()),
            KeyValue::new("flags", flags.to_string()),
            KeyValue::new("process", comm.to_string()),
            KeyValue::new("cgroup_id", cgroup_id as i64),
        ];

        #[cfg(feature = "k8s")]
        if let Some(pod) = pod_info {
            attrs.push(KeyValue::new("target.namespace", pod.namespace.clone()));
            attrs.push(KeyValue::new("target.pod.name", pod.pod_name.clone()));
        }

        m.file_access_events.add(1, &attrs);
    }
}

// Record LLM request metrics
pub fn record_llm_request(
    usage: &UsageInfo,
    model: &str,
    latency_secs: f64,
    is_error: bool,
    #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<PodInfo>>,
) {
    if let Some(m) = metrics() {
        let status = if is_error { "error" } else { "success" };

        #[allow(unused_mut)]
        let mut attrs = vec![
            KeyValue::new("model", model.to_string()),
            KeyValue::new("status", status.to_string()),
        ];

        // Only add pod info if k8s feature is enabled
        #[cfg(feature = "k8s")]
        if let Some(pod) = pod_info {
            attrs.push(KeyValue::new("target.namespace", pod.namespace.clone()));
            attrs.push(KeyValue::new("target.pod.name", pod.pod_name.clone()));
        }

        m.record_llm(usage, latency_secs, is_error, attrs);
    }
}

/// Count an LLM response that lost bytes in capture and so has no usage to record.
pub fn record_llm_unparsed(#[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<PodInfo>>) {
    if let Some(m) = metrics() {
        #[allow(unused_mut)]
        let mut attrs = Vec::new();

        #[cfg(feature = "k8s")]
        if let Some(pod) = pod_info {
            attrs.push(KeyValue::new("target.namespace", pod.namespace.clone()));
            attrs.push(KeyValue::new("target.pod.name", pod.pod_name.clone()));
        }

        m.llm_responses_unparsed_total.add(1, &attrs);
    }
}

/// Shutdown OpenTelemetry (graceful shutdown)
/// Flushes pending metrics and shuts down the MeterProvider
pub fn shutdown_metrics() {
    info!("Shutting down OpenTelemetry metrics...");
    if let Some(provider) = METER_PROVIDER.get() {
        if let Err(e) = provider.shutdown() {
            log::warn!("Failed to shutdown MeterProvider: {}", e);
        } else {
            info!("OpenTelemetry metrics shutdown complete");
        }
    }
}

pub fn record_process_exit(cause: &str, exit_code: u32, lifetime_secs: f64, cgroup_id: u64) {
    if let Some(m) = metrics() {
        let attrs = [
            KeyValue::new("cause", cause.to_string()),
            KeyValue::new("exit_code", exit_code.to_string()),
            KeyValue::new("cgroup_id", cgroup_id.to_string()),
        ];
        m.process_exit_total.add(1, &attrs);
        m.process_lifetime_seconds.record(lifetime_secs, &attrs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{self, ResourceMetrics};
    use opentelemetry_sdk::metrics::reader::MetricReader;
    use opentelemetry_sdk::metrics::{
        InstrumentKind, ManualReader, MetricResult, Pipeline, Temporality,
    };
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_get_otlp_endpoint_not_set() {
        // Returns None if environment variable is not set
        unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
        assert!(get_otlp_endpoint().is_none());
    }

    #[test]
    #[serial]
    fn test_get_otlp_endpoint_empty() {
        // Returns None if environment variable is empty
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "");
            assert!(get_otlp_endpoint().is_none());
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
    }

    #[test]
    #[serial]
    fn test_get_otlp_endpoint_from_env() {
        unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://custom:4317") };
        let endpoint = get_otlp_endpoint();
        assert_eq!(endpoint, Some("http://custom:4317".to_string()));
        unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
    }

    #[test]
    #[serial]
    fn test_get_otlp_endpoint_adds_http_prefix() {
        unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "collector:4317") };
        let endpoint = get_otlp_endpoint();
        assert_eq!(endpoint, Some("http://collector:4317".to_string()));
        unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
    }

    /// Lets the test keep a handle to the reader the provider owns.
    #[derive(Debug, Clone)]
    struct SharedReader(std::sync::Arc<ManualReader>);

    impl MetricReader for SharedReader {
        fn register_pipeline(&self, pipeline: std::sync::Weak<Pipeline>) {
            self.0.register_pipeline(pipeline);
        }
        fn collect(&self, rm: &mut ResourceMetrics) -> MetricResult<()> {
            self.0.collect(rm)
        }
        fn force_flush(&self) -> MetricResult<()> {
            self.0.force_flush()
        }
        fn shutdown(&self) -> MetricResult<()> {
            self.0.shutdown()
        }
        fn temporality(&self, kind: InstrumentKind) -> Temporality {
            self.0.temporality(kind)
        }
    }

    #[test]
    fn llm_latency_uses_second_scale_buckets() -> Result<()> {
        let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
        let provider = SdkMeterProvider::builder()
            .with_reader(reader.clone())
            .build();
        let metrics = HoneyBeeMetrics::new(&provider.meter("test"));
        for secs in [0.3, 1.2, 3.0, 45.0] {
            metrics.llm_latency_seconds.record(secs, &[]);
        }

        let mut rm = ResourceMetrics {
            resource: Resource::empty(),
            scope_metrics: vec![],
        };
        reader.collect(&mut rm)?;
        let hist = rm
            .scope_metrics
            .iter()
            .flat_map(|s| &s.metrics)
            .find(|m| m.name == "llm_latency_seconds")
            .and_then(|m| m.data.as_any().downcast_ref::<data::Histogram<f64>>())
            .context("llm_latency_seconds histogram not collected")?;
        let point = &hist.data_points[0];

        assert_eq!(
            point.bounds,
            [
                0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 7.5, 10.0, 15.0, 20.0, 30.0, 45.0, 60.0, 90.0,
                120.0, 180.0, 300.0
            ]
        );
        // Typical 0.3-45 s calls must spread over buckets, not pile into one.
        let filled: Vec<u64> = point
            .bucket_counts
            .iter()
            .copied()
            .filter(|&c| c > 0)
            .collect();
        assert_eq!(filled, [1, 1, 1, 1]);
        Ok(())
    }

    #[test]
    fn llm_reasoning_tokens_are_a_subset_and_skip_errors() -> Result<()> {
        let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
        let provider = SdkMeterProvider::builder()
            .with_reader(reader.clone())
            .build();
        let metrics = HoneyBeeMetrics::new(&provider.meter("test"));
        let usage = UsageInfo {
            prompt_tokens: 5,
            completion_tokens: 4,
            thoughts_tokens: Some(4),
            ..UsageInfo::default()
        };
        metrics.record_llm(&usage, 1.0, false, vec![]);
        metrics.record_llm(&usage, 1.0, true, vec![]);

        let mut rm = ResourceMetrics {
            resource: Resource::empty(),
            scope_metrics: vec![],
        };
        reader.collect(&mut rm)?;
        let total = |name: &str| -> Result<u64> {
            let sum = rm
                .scope_metrics
                .iter()
                .flat_map(|s| &s.metrics)
                .find(|m| m.name == name)
                .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<u64>>())
                .with_context(|| format!("{name} counter not collected"))?;
            Ok(sum.data_points.iter().map(|p| p.value).sum())
        };

        assert_eq!(total("llm_reasoning_tokens_total")?, 4);
        assert_eq!(total("llm_tokens_total")?, 9);
        Ok(())
    }

    #[test]
    fn llm_cost_is_priced_per_call_and_unknown_models_stay_unpriced() -> Result<()> {
        let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
        let provider = SdkMeterProvider::builder()
            .with_reader(reader.clone())
            .build();
        let metrics = HoneyBeeMetrics::new(&provider.meter("test"));
        let priced = UsageInfo {
            prompt_tokens: 1200,
            completion_tokens: 800,
            cache_read_tokens: Some(1000),
            thoughts_tokens: Some(600),
            model: Some("gpt-5".to_string()),
            ..UsageInfo::default()
        };
        let unknown = UsageInfo {
            prompt_tokens: 30,
            completion_tokens: 12,
            model: Some("my-vllm-llama".to_string()),
            ..UsageInfo::default()
        };
        metrics.record_llm(&priced, 1.0, false, vec![]);
        metrics.record_llm(&priced, 1.0, true, vec![]);
        metrics.record_llm(&unknown, 1.0, false, vec![]);

        let mut rm = ResourceMetrics {
            resource: Resource::empty(),
            scope_metrics: vec![],
        };
        reader.collect(&mut rm)?;
        let find = |name: &str| {
            rm.scope_metrics
                .iter()
                .flat_map(|s| &s.metrics)
                .find(|m| m.name == name)
                .with_context(|| format!("{name} not collected"))
        };
        let label = |attrs: &[KeyValue], key: &str| {
            attrs
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.to_string())
        };
        let cost = find("llm_cost_usd_total")?
            .data
            .as_any()
            .downcast_ref::<data::Sum<f64>>()
            .context("llm_cost_usd_total is not an f64 sum")?;
        // gpt-5, error call not billed: 200×$1.25 + 1000×$0.125 + 800×$10 per million.
        let mut parts: Vec<(Option<String>, f64)> = cost
            .data_points
            .iter()
            .map(|p| (label(&p.attributes, "cost_type"), (p.value * 1e9).round()))
            .collect();
        parts.sort_by(|a, b| a.0.cmp(&b.0));
        let kind = |s: &str| Some(s.to_string());
        assert_eq!(
            parts,
            [
                (kind("cache_read"), 125_000.0),
                (kind("input"), 250_000.0),
                (kind("output"), 8_000_000.0)
            ]
        );
        assert!(cost.data_points.iter().all(|p| {
            label(&p.attributes, "price_table_version").as_deref() == Some("2026-10-26")
        }));
        let unpriced = find("llm_unpriced_tokens_total")?
            .data
            .as_any()
            .downcast_ref::<data::Sum<u64>>()
            .context("llm_unpriced_tokens_total is not a u64 sum")?;
        let points: Vec<_> = unpriced
            .data_points
            .iter()
            .map(|p| (label(&p.attributes, "reason"), p.value))
            .collect();
        assert_eq!(points, [(kind("unknown_model"), 42)]);
        Ok(())
    }
}
