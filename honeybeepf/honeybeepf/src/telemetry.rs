//! OpenTelemetry metrics export module
//!
//! Exports eBPF metrics collected by honeybeepf to OpenTelemetry Collector.
//!
//! ## Metrics Categories
//! - **Filesystem**: File access auditing
//!
//! ## OTLP Endpoint Priority
//! 1. Helm values (injected via environment variables)
//! 2. Direct environment variable configuration

use anyhow::{Context, Result};
use log::info;
use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

#[cfg(feature = "k8s")]
use crate::k8s::PodInfo;

/// Metric export interval in seconds
const METRIC_EXPORT_INTERVAL_SECS: u64 = 30;

/// Global metrics handle
static METRICS: OnceLock<HoneyBeeMetrics> = OnceLock::new();

/// Global MeterProvider for graceful shutdown
static METER_PROVIDER: OnceLock<SdkMeterProvider> = OnceLock::new();

/// Global active probes count (for ObservableGauge callback)
static ACTIVE_PROBES: OnceLock<RwLock<HashMap<String, u64>>> = OnceLock::new();

fn active_probes_map() -> &'static RwLock<HashMap<String, u64>> {
    ACTIVE_PROBES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// honeybeepf metrics collection
pub struct HoneyBeeMetrics {
    // === Filesystem metrics ===
    pub file_access_events: Counter<u64>,
    // === LLM metrics ===
    pub llm_requests_total: Counter<u64>,
    pub llm_tokens_total: Counter<u64>,
    pub llm_latency_seconds: Histogram<f64>,
}

impl HoneyBeeMetrics {
    fn new(meter: &Meter) -> Self {
        Self {
            // === Filesystem ===
            file_access_events: meter
                .u64_counter("file_access_events")
                .with_description("Number of monitored file access events")
                .with_unit("events")
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
            llm_latency_seconds: meter
                .f64_histogram("llm_latency_seconds")
                .with_description("Latency of LLM requests")
                .with_unit("s")
                .build(),
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
        KeyValue::new("service.name", "honeybeepf"),
        KeyValue::new("telemetry.sdk.language", "rust"),
    ]));

    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();

    global::set_meter_provider(provider.clone());
    let _ = METER_PROVIDER.set(provider);

    // Meter name is used as prefix only
    let meter = global::meter("honeybeepf");

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

pub fn record_file_access_event(filename: &str, flags: &str, comm: &str, cgroup_id: u64) {
    if let Some(m) = metrics() {
        let attrs = [
            KeyValue::new("filename", filename.to_string()),
            KeyValue::new("flags", flags.to_string()),
            KeyValue::new("process", comm.to_string()),
            KeyValue::new("cgroup_id", cgroup_id as i64),
        ];
        m.file_access_events.add(1, &attrs);
    }
}

// Record LLM request metrics
pub fn record_llm_request(
    model: &str,
    latency_secs: f64,
    prompt_tokens: u64,
    completion_tokens: u64,
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

        // 1. total requests
        m.llm_requests_total.add(1, &attrs);

        // 2. latency
        m.llm_latency_seconds.record(latency_secs, &attrs);

        // 3. tokens
        if !is_error {
            let mut prompt_attrs = attrs.clone();
            prompt_attrs.push(KeyValue::new("token_type", "prompt"));
            m.llm_tokens_total.add(prompt_tokens, &prompt_attrs);

            let mut comp_attrs = attrs; // move ownership
            comp_attrs.push(KeyValue::new("token_type", "completion"));
            m.llm_tokens_total.add(completion_tokens, &comp_attrs);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
