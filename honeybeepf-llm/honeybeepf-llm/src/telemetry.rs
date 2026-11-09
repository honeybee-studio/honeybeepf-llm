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
use arc_swap::ArcSwapOption;
use honeybeepf_llm_core::pricing::PriceTable;
use honeybeepf_llm_core::types::{ModalityTokens, Operation, UsageInfo};
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
use crate::k8s::{PodInfo, UNATTRIBUTED};

mod prices;
use prices::{PriceFile, inline_prices, load_prices, start_price_reload};

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

/// Labels of `llm_provider_config_info`, set once when the LLM probe starts.
static PROVIDER_CONFIG: OnceLock<Vec<KeyValue>> = OnceLock::new();

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
    pub llm_h2_resynced_total: Counter<u64>,
    pub llm_h2_midjoin_recorded_total: Counter<u64>,
    pub llm_cost_usd_total: Counter<f64>,
    pub llm_cache_savings_ceiling_usd_total: Counter<f64>,
    pub llm_unpriced_tokens_total: Counter<u64>,
    /// Swapped whole when the user price file changes, so a call never waits on a reload.
    prices: ArcSwapOption<Prices>,
}

/// A price table and its `price_table_version` label.
struct Prices {
    table: PriceTable,
    version: KeyValue,
}

impl Prices {
    fn new(table: PriceTable) -> Self {
        let version = KeyValue::new("price_table_version", Arc::<str>::from(table.version()));
        Self { table, version }
    }
}

/// Which attempt of a server-side fallback a metric row is; a closed two-value `attempt` label
/// so a declined attempt's billed tokens and cost are not read as a served call to that model.
#[derive(Clone, Copy)]
enum Attempt {
    Served,
    Declined,
}

impl Attempt {
    fn as_str(self) -> &'static str {
        match self {
            Attempt::Served => "served",
            Attempt::Declined => "declined",
        }
    }
}

/// How a call was captured: `tls` from the OpenSSL uprobes, `plain` from the socket-level plaintext
/// path. A label on request, token and latency metrics.
#[derive(Clone, Copy)]
pub enum Transport {
    Tls,
    Plain,
}

impl Transport {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Transport::Tls => "tls",
            Transport::Plain => "plain",
        }
    }
}

impl HoneyBeeMetrics {
    fn new(meter: &Meter, prices: Option<Prices>) -> Self {
        let (llm_cost_usd_total, llm_cache_savings_ceiling_usd_total, llm_unpriced_tokens_total) =
            Self::cost_counters(meter);
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
                .with_description("Total number of LLM requests, by transport (tls|plain)")
                .with_unit("requests")
                .build(),
            llm_tokens_total: meter
                .u64_counter("llm_tokens_total")
                .with_description("Total number of LLM tokens processed, by transport (tls|plain)")
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
                .with_description("Latency of LLM requests, by transport (tls|plain)")
                .with_unit("s")
                .with_boundaries(LLM_LATENCY_BOUNDARIES_SECS.to_vec())
                .build(),
            llm_responses_unparsed_total: meter
                .u64_counter("llm_responses_unparsed_total")
                .with_description(
                    "LLM calls not recorded because no usage could be read, by reason",
                )
                .with_unit("responses")
                .build(),
            llm_h2_resynced_total: meter
                .u64_counter("llm_h2_resynced_total")
                .with_description("HTTP/2 directions re-anchored after a capture gap")
                .with_unit("events")
                .build(),
            llm_h2_midjoin_recorded_total: meter
                .u64_counter("llm_h2_midjoin_recorded_total")
                .with_description(
                    "LLM calls recorded on an HTTP/2 connection joined mid-stream (host unknown)",
                )
                .with_unit("responses")
                .build(),
            llm_cost_usd_total,
            llm_cache_savings_ceiling_usd_total,
            llm_unpriced_tokens_total,
            prices: ArcSwapOption::from_pointee(prices),
        }
    }

    /// The cost-related LLM counters: realized cost, the cache-savings ceiling, unpriced tokens.
    fn cost_counters(meter: &Meter) -> (Counter<f64>, Counter<f64>, Counter<u64>) {
        // No unit on the USD counters: the Prometheus exporter would append it to the name.
        let cost = meter
            .f64_counter("llm_cost_usd_total")
            .with_description("Cost of LLM calls in USD from the price table, by cost_type")
            .build();
        let savings = meter
            .f64_counter("llm_cache_savings_ceiling_usd_total")
            .with_description(
                "Upper bound on USD caching could save: uncached input rebilled at the \
                 model's cache-read price; cache_price=assumed_free when the model has none",
            )
            .build();
        let unpriced = meter
            .u64_counter("llm_unpriced_tokens_total")
            .with_description(
                "Tokens of LLM calls the price table could not price, by reason; never counted as $0",
            )
            .with_unit("tokens")
            .build();
        (cost, savings, unpriced)
    }

    /// Record a call's cost by category, or its tokens as unpriced with the reason.
    fn record_cost(&self, usage: &UsageInfo, labels: &mut Vec<KeyValue>) {
        let prices = self.prices.load();
        let Some(Prices { table, version }) = prices.as_deref() else {
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
                // Savings ceiling: most the uncached input could save if that prefix were cached.
                // Only priced calls contribute; embeddings have no prompt cache, so they are left out.
                if usage.operation != Operation::Embeddings
                    && let Ok((usd, exact)) = table.cache_savings_ceiling(usage)
                    && usd > 0.0
                {
                    labels.push(KeyValue::new(
                        "cache_price",
                        if exact { "known" } else { "assumed_free" },
                    ));
                    self.llm_cache_savings_ceiling_usd_total.add(usd, labels);
                    labels.pop();
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

    /// Record a served call, then the billed tokens and cost of each earlier server-side fallback
    /// attempt under its own model. The served call owns the one request count and its latency;
    /// `attrs_for(model, status, attempt)` builds the label set, pod labels included.
    fn record_llm_call(
        &self,
        usage: &UsageInfo,
        model: &str,
        latency_secs: f64,
        is_error: bool,
        transport: Transport,
        attrs_for: impl Fn(&str, &str, Attempt) -> Vec<KeyValue>,
    ) {
        let status = if is_error { "error" } else { "success" };
        self.record_llm(
            usage,
            latency_secs,
            is_error,
            transport,
            attrs_for(model, status, Attempt::Served),
        );
        for attempt in &usage.iterations {
            if attempt.prompt_tokens == 0 && attempt.completion_tokens == 0 {
                continue;
            }
            let model = attempt.model.as_deref().unwrap_or("unknown");
            self.record_tokens(
                attempt,
                transport,
                attrs_for(model, "success", Attempt::Declined),
            );
        }
    }

    fn record_llm(
        &self,
        usage: &UsageInfo,
        latency_secs: f64,
        is_error: bool,
        transport: Transport,
        attrs: Vec<KeyValue>,
    ) {
        // Requests and latency carry `operation` and `transport`; cost and cache rows do not.
        let mut call_attrs = attrs.clone();
        call_attrs.push(KeyValue::new("operation", usage.operation.as_str()));
        call_attrs.push(KeyValue::new("transport", transport.as_str()));
        self.llm_requests_total.add(1, &call_attrs);
        self.llm_latency_seconds.record(latency_secs, &call_attrs);
        if !is_error {
            self.record_tokens(usage, transport, attrs);
        }
    }

    /// The tokens, cache, reasoning and cost of one billed attempt; only the token counts carry the
    /// transport label.
    fn record_tokens(&self, usage: &UsageInfo, transport: Transport, mut attrs: Vec<KeyValue>) {
        if let Some(n) = usage.cache_read_tokens {
            self.llm_cache_read_tokens_total.add(n, &attrs);
        }
        if let Some(n) = usage.cache_write_tokens {
            self.llm_cache_write_tokens_total.add(n, &attrs);
        }
        if let Some(n) = usage.thoughts_tokens {
            self.llm_reasoning_tokens_total.add(n, &attrs);
        }
        // A fallback attempt declined before output in a non-billed category records tokens but no cost.
        if !usage.cost_suppressed {
            self.record_cost(usage, &mut attrs);
        }
        attrs.push(KeyValue::new("transport", transport.as_str()));
        attrs.push(KeyValue::new("operation", usage.operation.as_str()));
        self.record_token_modalities(
            usage.prompt_tokens,
            usage.input_modalities,
            "prompt",
            &mut attrs,
        );
        self.record_token_modalities(
            usage.completion_tokens,
            usage.output_modalities,
            "completion",
            &mut attrs,
        );
    }

    /// Split one direction's tokens across `llm_tokens_total` by modality; text is the remainder, so
    /// the modality series still sum to the total. Only the non-text modalities present are emitted.
    fn record_token_modalities(
        &self,
        total: u64,
        modalities: ModalityTokens,
        token_type: &'static str,
        attrs: &mut Vec<KeyValue>,
    ) {
        attrs.push(KeyValue::new("token_type", token_type));
        self.add_modality(attrs, "text", total.saturating_sub(modalities.non_text()));
        if modalities.audio > 0 {
            self.add_modality(attrs, "audio", modalities.audio);
        }
        if modalities.image > 0 {
            self.add_modality(attrs, "image", modalities.image);
        }
        if modalities.video > 0 {
            self.add_modality(attrs, "video", modalities.video);
        }
        attrs.pop();
    }

    /// Add one `modality` label, record, and restore `attrs`, so the label vector is reused, not cloned.
    fn add_modality(&self, attrs: &mut Vec<KeyValue>, modality: &'static str, n: u64) {
        attrs.push(KeyValue::new("modality", modality));
        self.llm_tokens_total.add(n, attrs);
        attrs.pop();
    }

    fn record_unparsed(&self, reason: &'static str, mut attrs: Vec<KeyValue>) {
        attrs.push(KeyValue::new("reason", reason));
        self.llm_responses_unparsed_total.add(1, &attrs);
    }

    fn record_resynced(&self, attrs: Vec<KeyValue>) {
        self.llm_h2_resynced_total.add(1, &attrs);
    }

    fn record_midjoin(&self, attrs: Vec<KeyValue>) {
        self.llm_h2_midjoin_recorded_total.add(1, &attrs);
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
        Some(format!("http://{endpoint}"))
    } else {
        Some(endpoint)
    }
}

/// Initialize OpenTelemetry metrics provider
///
/// Configures metrics export to OTLP Collector via gRPC.
/// Skips initialization if OTEL_EXPORTER_OTLP_ENDPOINT is not set.
pub fn init_metrics() -> Result<()> {
    let Some(endpoint) = get_otlp_endpoint() else {
        info!("OTEL_EXPORTER_OTLP_ENDPOINT not set. Metrics export disabled.");
        return Ok(());
    };

    info!("Initializing OpenTelemetry metrics exporter");
    info!("OTLP endpoint: {endpoint}");

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
                for (stat, v) in honeybeepf_llm_common::LlmStat::ALL.iter().zip(totals) {
                    observer.observe(v, &[KeyValue::new("stage", stat.name())]);
                }
            }
        })
        .build();

    register_provider_config_info(&meter, &PROVIDER_CONFIG);

    let mut price_file = std::env::var("LLM_PRICES_CONFIG_FILE")
        .ok()
        .map(PriceFile::new);
    let custom = price_file
        .as_mut()
        .and_then(PriceFile::changed)
        .map(str::to_owned)
        .or_else(inline_prices);
    let _ = METRICS.set(HoneyBeeMetrics::new(&meter, load_prices(custom.as_deref())));
    if let (Some(m), Some(file)) = (METRICS.get(), price_file)
        && m.prices.load().is_some()
    {
        start_price_reload(m, file);
    }

    info!("OpenTelemetry metrics initialized successfully");
    Ok(())
}

/// A constant 1 whose labels name the provider set, so a config change shows as a new series.
fn register_provider_config_info(meter: &Meter, labels: &'static OnceLock<Vec<KeyValue>>) {
    let _gauge = meter
        .u64_observable_gauge("llm_provider_config_info")
        .with_description("LLM provider set the agent matches calls with; always 1, see the labels")
        .with_callback(move |observer| {
            if let Some(labels) = labels.get() {
                observer.observe(1, labels);
            }
        })
        .build();
}

/// Record the provider set the LLM probe runs with, for `llm_provider_config_info`.
pub fn record_provider_config(config_hash: &str, providers: &str, count: usize, replaced: &str) {
    let _ = PROVIDER_CONFIG.set(provider_config_labels(
        config_hash,
        providers,
        count,
        replaced,
    ));
}

fn provider_config_labels(
    config_hash: &str,
    providers: &str,
    count: usize,
    replaced: &str,
) -> Vec<KeyValue> {
    // Prometheus drops an empty label, which would read as "label missing" rather than "nothing replaced".
    let replaced = if replaced.is_empty() {
        "none"
    } else {
        replaced
    };
    vec![
        KeyValue::new("config_hash", config_hash.to_string()),
        KeyValue::new("providers", providers.to_string()),
        KeyValue::new("provider_count", count.to_string()),
        KeyValue::new("replaced_builtins", replaced.to_string()),
    ]
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
        info!("Active probe registered: {probe_name} = {count}");
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
    transport: Transport,
    #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<PodInfo>>,
) {
    if let Some(m) = metrics() {
        m.record_llm_call(
            usage,
            model,
            latency_secs,
            is_error,
            transport,
            |model, status, attempt| {
                llm_attrs(
                    model,
                    status,
                    attempt,
                    #[cfg(feature = "k8s")]
                    pod_info,
                )
            },
        );
    }
}

/// The model, status and attempt labels of an LLM call, plus the pod target labels when k8s is on.
fn llm_attrs(
    model: &str,
    status: &str,
    attempt: Attempt,
    #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<PodInfo>>,
) -> Vec<KeyValue> {
    #[allow(unused_mut)]
    let mut attrs = vec![
        KeyValue::new("model", model.to_string()),
        KeyValue::new("status", status.to_string()),
        KeyValue::new("attempt", attempt.as_str()),
    ];
    #[cfg(feature = "k8s")]
    push_pod_attrs(&mut attrs, pod_info);
    attrs
}

/// Count an LLM call that has no usage to record, by `reason`.
pub fn record_llm_unparsed(
    reason: &'static str,
    #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<PodInfo>>,
) {
    if let Some(m) = metrics() {
        #[allow(unused_mut)]
        let mut attrs = Vec::new();

        #[cfg(feature = "k8s")]
        push_pod_attrs(&mut attrs, pod_info);

        m.record_unparsed(reason, attrs);
    }
}

/// Count an HTTP/2 direction re-anchoring its frame reader after a capture gap.
pub fn record_h2_resynced(#[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<PodInfo>>) {
    if let Some(m) = metrics() {
        #[allow(unused_mut)]
        let mut attrs = Vec::new();

        #[cfg(feature = "k8s")]
        push_pod_attrs(&mut attrs, pod_info);

        m.record_resynced(attrs);
    }
}

/// Count an LLM call recorded on an HTTP/2 connection joined mid-stream, where the host is unknown.
pub fn record_h2_midjoin(#[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<PodInfo>>) {
    if let Some(m) = metrics() {
        #[allow(unused_mut)]
        let mut attrs = Vec::new();

        #[cfg(feature = "k8s")]
        push_pod_attrs(&mut attrs, pod_info);

        m.record_midjoin(attrs);
    }
}

/// Target labels of an LLM call; the workload is a function of the pod, so it adds no series.
#[cfg(feature = "k8s")]
fn push_pod_attrs(attrs: &mut Vec<KeyValue>, pod_info: Option<&std::sync::Arc<PodInfo>>) {
    let Some(pod) = pod_info else { return };
    let or_unattributed = |v: &Option<String>| {
        let v = v.as_deref().filter(|s| !s.is_empty());
        v.unwrap_or(UNATTRIBUTED).to_string()
    };
    attrs.push(KeyValue::new("target.namespace", pod.namespace.clone()));
    attrs.push(KeyValue::new("target.pod.name", pod.pod_name.clone()));
    attrs.push(KeyValue::new(
        "target.workload.kind",
        or_unattributed(&pod.workload_kind),
    ));
    attrs.push(KeyValue::new(
        "target.workload.name",
        or_unattributed(&pod.workload_name),
    ));
    // Only allowlisted labels: each one multiplies the series by its distinct values.
    for (attr, value) in &pod.pod_labels {
        attrs.push(KeyValue::new(attr.clone(), value.clone()));
    }
}

/// Shutdown OpenTelemetry (graceful shutdown)
/// Flushes pending metrics and shuts down the MeterProvider
pub fn shutdown_metrics() {
    info!("Shutting down OpenTelemetry metrics...");
    if let Some(provider) = METER_PROVIDER.get() {
        if let Err(e) = provider.shutdown() {
            log::warn!("Failed to shutdown MeterProvider: {e}");
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
mod tests;
