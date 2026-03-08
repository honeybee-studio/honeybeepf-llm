//! OpenTelemetry metrics export module
//!
//! Exports eBPF metrics collected by honeybeepf to OpenTelemetry Collector.
//!
//! ## OTLP Endpoint Priority
//! 1. Helm values (injected via environment variables)
//! 2. Direct environment variable configuration
//! 3. Code default value (FQDN)

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
///
/// Note: Do NOT add _total suffix to Counter names (Prometheus adds it automatically)
pub struct HoneyBeeMetrics {
    pub block_io_events: Counter<u64>,
    pub block_io_bytes: Counter<u64>,
    pub block_io_latency_ns: Histogram<u64>,
    pub network_latency_ns: Histogram<u64>,
    pub network_connection_attempts: Counter<u64>,
    pub gpu_open_events: Counter<u64>,
    // Note: active_probes is registered as ObservableGauge in init_metrics()
}

impl HoneyBeeMetrics {
    fn new(meter: &Meter) -> Self {
        // Note: Do NOT add _total suffix to Counter names!
        // Prometheus automatically adds _total suffix
        Self {
            block_io_events: meter
                .u64_counter("block_io_events")
                .with_description("Number of block I/O events")
                .with_unit("events")
                .build(),
            block_io_bytes: meter
                .u64_counter("block_io_bytes")
                .with_description("Total bytes of block I/O operations")
                .with_unit("bytes")
                .build(),
            block_io_latency_ns: meter
                .u64_histogram("block_io_latency_ns")
                .with_description("Block I/O operation latency in nanoseconds")
                .with_unit("ns")
                .build(),
            network_latency_ns: meter
                .u64_histogram("network_latency_ns")
                .with_description("Network operation latency in nanoseconds")
                .with_unit("ns")
                .build(),
            network_connection_attempts: meter
                .u64_counter("network_connection_attempts")
                .with_description("Number of network connection attempts")
                .with_unit("events")
                .build(),
            gpu_open_events: meter
                .u64_counter("gpu_open_events")
                .with_description("Number of GPU device open events")
                .with_unit("events")
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

/// Build process identity attributes.
///
/// Always includes `process.name`. When the `k8s` feature is enabled and
/// pod info is available, also includes K8s attributes following OTEL
/// semantic conventions.
fn process_attrs(
    process_name: &str,
    #[cfg(feature = "k8s")] pod: Option<&PodInfo>,
) -> Vec<KeyValue> {
    let attrs = vec![KeyValue::new("process.name", process_name.to_string())];

    #[cfg(feature = "k8s")]
    let attrs = {
        let mut attrs = attrs;
        if let Some(info) = pod {
            attrs.push(KeyValue::new("k8s.pod.name", info.pod_name.clone()));
            attrs.push(KeyValue::new("k8s.namespace.name", info.namespace.clone()));
            if let Some(ref name) = info.workload_name {
                attrs.push(KeyValue::new("k8s.workload.name", name.clone()));
            }
            if let Some(ref kind) = info.workload_kind {
                attrs.push(KeyValue::new("k8s.workload.kind", kind.clone()));
            }
        }
        attrs
    };

    attrs
}

pub fn record_block_io_event(
    event_type: &str,
    bytes: u64,
    latency_ns: Option<u64>,
    device: &str,
    process_name: &str,
    #[cfg(feature = "k8s")] pod: Option<&PodInfo>,
) {
    if let Some(m) = metrics() {
        let mut attrs = vec![
            KeyValue::new("event_type", event_type.to_string()),
            KeyValue::new("device", device.to_string()),
        ];
        attrs.extend(process_attrs(
            process_name,
            #[cfg(feature = "k8s")]
            pod,
        ));

        m.block_io_events.add(1, &attrs);
        m.block_io_bytes.add(bytes, &attrs);

        if let Some(lat) = latency_ns {
            m.block_io_latency_ns.record(lat, &attrs);
        }
    }
}

pub fn record_network_latency(
    latency_ns: u64,
    protocol: &str,
    process_name: &str,
    #[cfg(feature = "k8s")] pod: Option<&PodInfo>,
) {
    if let Some(m) = metrics() {
        let mut attrs = vec![KeyValue::new("protocol", protocol.to_string())];
        attrs.extend(process_attrs(
            process_name,
            #[cfg(feature = "k8s")]
            pod,
        ));
        m.network_latency_ns.record(latency_ns, &attrs);
    }
}

pub fn record_gpu_open_event(
    device_path: &str,
    process_name: &str,
    #[cfg(feature = "k8s")] pod: Option<&PodInfo>,
) {
    if let Some(m) = metrics() {
        let mut attrs = vec![KeyValue::new("device", device_path.to_string())];
        attrs.extend(process_attrs(
            process_name,
            #[cfg(feature = "k8s")]
            pod,
        ));
        m.gpu_open_events.add(1, &attrs);
    }
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

pub fn record_network_connection(
    protocol: &str,
    dest: &str,
    process_name: &str,
    #[cfg(feature = "k8s")] pod: Option<&PodInfo>,
) {
    if let Some(m) = metrics() {
        let mut attrs = vec![
            KeyValue::new("protocol", protocol.to_string()),
            KeyValue::new("destination", dest.to_string()),
        ];
        attrs.extend(process_attrs(
            process_name,
            #[cfg(feature = "k8s")]
            pod,
        ));
        m.network_connection_attempts.add(1, &attrs);
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
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://custom:4317");
        }

        let endpoint = get_otlp_endpoint();
        assert_eq!(endpoint, Some("http://custom:4317".to_string()));
        unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
    }

    #[test]
    #[serial]
    fn test_get_otlp_endpoint_adds_http_prefix() {
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "collector:4317");
        }

        let endpoint = get_otlp_endpoint();
        assert_eq!(endpoint, Some("http://collector:4317".to_string()));
        unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
    }
}
