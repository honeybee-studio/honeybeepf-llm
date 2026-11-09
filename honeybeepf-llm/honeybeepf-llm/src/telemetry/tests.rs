use super::prices::reload_prices;
use super::*;
use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{self, ResourceMetrics};
use opentelemetry_sdk::metrics::reader::MetricReader;
use opentelemetry_sdk::metrics::{
    InstrumentKind, ManualReader, MetricResult, Pipeline, Temporality,
};
use serial_test::serial;

#[cfg(feature = "k8s")]
#[test]
fn test_pod_attrs_never_leave_workload_empty() {
    let un = Some(UNATTRIBUTED);
    for (kind, name, want) in [
        (Some("CronJob"), Some("r"), [Some("CronJob"), Some("r")]),
        (None, None, [un, un]),
        (Some(""), Some(""), [un, un]),
    ] {
        let pod = Arc::new(PodInfo {
            pod_name: "p-1".to_string(),
            namespace: "team-a".to_string(),
            workload_kind: kind.map(str::to_string),
            workload_name: name.map(str::to_string),
            pod_labels: vec![("target.pod.label.variant".into(), "b".into())],
        });
        let mut attrs = Vec::new();
        push_pod_attrs(&mut attrs, Some(&pod));
        let labels: Vec<_> = attrs.iter().map(|kv| kv.value.to_string()).collect();
        assert_eq!(labels[..2], ["team-a", "p-1"]);
        assert_eq!(
            labels[2..4]
                .iter()
                .map(|v| Some(v.as_str()))
                .collect::<Vec<_>>(),
            want
        );
        let variant = attrs.last().map(|kv| format!("{}={}", kv.key, kv.value));
        assert_eq!(variant.as_deref(), Some("target.pod.label.variant=b"));
    }

    let mut attrs = Vec::new();
    push_pod_attrs(&mut attrs, None);
    assert!(attrs.is_empty());
}

#[test]
#[serial]
fn test_get_otlp_endpoint_not_set() {
    // Returns None if environment variable is not set
    // SAFETY: #[serial] keeps the tests that read or write this variable from running at the same time.
    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
    assert!(get_otlp_endpoint().is_none());
}

#[test]
#[serial]
fn test_get_otlp_endpoint_empty() {
    // Returns None if environment variable is empty
    // SAFETY: #[serial] keeps the tests that read or write this variable from running at the same time.
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "");
        assert!(get_otlp_endpoint().is_none());
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
    }
}

#[test]
#[serial]
fn test_get_otlp_endpoint_from_env() {
    // SAFETY: #[serial] keeps the tests that read or write this variable from running at the same time.
    unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://custom:4317") };
    let endpoint = get_otlp_endpoint();
    assert_eq!(endpoint, Some("http://custom:4317".to_string()));
    // SAFETY: #[serial] keeps the tests that read or write this variable from running at the same time.
    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
}

#[test]
#[serial]
fn test_get_otlp_endpoint_adds_http_prefix() {
    // SAFETY: #[serial] keeps the tests that read or write this variable from running at the same time.
    unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "collector:4317") };
    let endpoint = get_otlp_endpoint();
    assert_eq!(endpoint, Some("http://collector:4317".to_string()));
    // SAFETY: #[serial] keeps the tests that read or write this variable from running at the same time.
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
fn provider_config_info_is_one_series_with_the_config_labels() -> Result<()> {
    static LABELS: OnceLock<Vec<KeyValue>> = OnceLock::new();
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    register_provider_config_info(&provider.meter("test"), &LABELS);
    let collect = || -> Result<Vec<(u64, Vec<String>)>> {
        let mut rm = ResourceMetrics {
            resource: Resource::empty(),
            scope_metrics: vec![],
        };
        reader.collect(&mut rm)?;
        Ok(rm
            .scope_metrics
            .iter()
            .flat_map(|s| &s.metrics)
            .filter(|m| m.name == "llm_provider_config_info")
            .filter_map(|m| m.data.as_any().downcast_ref::<data::Gauge<u64>>())
            .flat_map(|g| &g.data_points)
            .map(|p| {
                let mut labels: Vec<String> = p
                    .attributes
                    .iter()
                    .map(|kv| format!("{}={}", kv.key, kv.value))
                    .collect();
                labels.sort();
                (p.value, labels)
            })
            .collect())
    };

    // Nothing until the LLM probe has reported its providers.
    assert!(collect()?.is_empty());

    let _ = LABELS.set(provider_config_labels("00ff", "openai,anthropic", 2, ""));
    assert_eq!(
        collect()?,
        [(
            1,
            vec![
                "config_hash=00ff".to_string(),
                "provider_count=2".to_string(),
                "providers=openai,anthropic".to_string(),
                "replaced_builtins=none".to_string(),
            ]
        )]
    );
    Ok(())
}

#[test]
fn llm_latency_uses_second_scale_buckets() -> Result<()> {
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(None));
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
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(None));
    let usage = UsageInfo {
        prompt_tokens: 5,
        completion_tokens: 4,
        thoughts_tokens: Some(4),
        ..UsageInfo::default()
    };
    metrics.record_llm(&usage, 1.0, false, Transport::Tls, vec![]);
    metrics.record_llm(&usage, 1.0, true, Transport::Tls, vec![]);

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
fn llm_tokens_split_by_modality_with_text_as_the_remainder() -> Result<()> {
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(None));
    let usage = UsageInfo {
        prompt_tokens: 1000,
        completion_tokens: 500,
        input_modalities: ModalityTokens {
            audio: 200,
            image: 100,
            video: 100,
        },
        // All output is image, so the output text remainder is exactly 0.
        output_modalities: ModalityTokens {
            image: 500,
            ..Default::default()
        },
        ..UsageInfo::default()
    };
    metrics.record_llm(&usage, 1.0, false, Transport::Tls, vec![]);

    let mut rm = ResourceMetrics {
        resource: Resource::empty(),
        scope_metrics: vec![],
    };
    reader.collect(&mut rm)?;
    let attr = |a: &[KeyValue], k: &str| {
        a.iter()
            .find(|kv| kv.key.as_str() == k)
            .map(|kv| kv.value.to_string())
    };
    let by = |token_type: &str, modality: &str| -> u64 {
        rm.scope_metrics
            .iter()
            .flat_map(|s| &s.metrics)
            .find(|m| m.name == "llm_tokens_total")
            .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<u64>>())
            .map_or(0, |sum| {
                sum.data_points
                    .iter()
                    .filter(|p| attr(&p.attributes, "token_type").as_deref() == Some(token_type))
                    .filter(|p| attr(&p.attributes, "modality").as_deref() == Some(modality))
                    .map(|p| p.value)
                    .sum()
            })
    };
    // text is the remainder, and the modality series sum back to each total.
    let prompt = (
        by("prompt", "text"),
        by("prompt", "audio"),
        by("prompt", "image"),
        by("prompt", "video"),
    );
    assert_eq!(prompt, (600, 200, 100, 100));
    // An all-image output still emits text=0, and the series sum to the completion total.
    assert_eq!(
        (by("completion", "text"), by("completion", "image")),
        (0, 500)
    );
    // A modality with no tokens in a direction gets no series.
    assert_eq!(
        (by("completion", "audio"), by("completion", "video")),
        (0, 0)
    );
    Ok(())
}

#[test]
fn llm_fallback_bills_each_attempt_under_its_model_without_a_second_request() -> Result<()> {
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(None));
    let usage = UsageInfo {
        prompt_tokens: 412,
        completion_tokens: 264,
        model: Some("claude-opus-4-8".to_string()),
        iterations: vec![UsageInfo {
            prompt_tokens: 535,
            completion_tokens: 0,
            model: Some("claude-fable-5".to_string()),
            ..UsageInfo::default()
        }],
        ..UsageInfo::default()
    };
    metrics.record_llm_call(
        &usage,
        "claude-opus-4-8",
        1.0,
        false,
        Transport::Tls,
        |model, status, attempt| {
            vec![
                KeyValue::new("model", model.to_string()),
                KeyValue::new("status", status.to_string()),
                KeyValue::new("attempt", attempt.as_str()),
            ]
        },
    );

    let mut rm = ResourceMetrics {
        resource: Resource::empty(),
        scope_metrics: vec![],
    };
    reader.collect(&mut rm)?;
    let points = |name: &str| -> Vec<(Vec<(String, String)>, u64)> {
        rm.scope_metrics
            .iter()
            .flat_map(|s| &s.metrics)
            .find(|m| m.name == name)
            .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<u64>>())
            .map(|sum| {
                sum.data_points
                    .iter()
                    .map(|p| {
                        let attrs = p
                            .attributes
                            .iter()
                            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                            .collect();
                        (attrs, p.value)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let total = |name: &str| -> u64 { points(name).iter().map(|(_, v)| v).sum() };
    let tokens_for = |model: &str| -> u64 {
        points("llm_tokens_total")
            .iter()
            .filter(|(a, _)| a.iter().any(|(k, v)| k == "model" && v == model))
            .map(|(_, v)| v)
            .sum()
    };

    // The served call owns the one request and the latency observation.
    assert_eq!(total("llm_requests_total"), 1);
    assert_eq!(tokens_for("claude-opus-4-8"), 676);
    assert_eq!(tokens_for("claude-fable-5"), 535);
    Ok(())
}

#[test]
fn llm_fallback_attempt_label_separates_declined_tokens_from_served() -> Result<()> {
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(None));
    // Same model is declined once and serves once; only attempt tells the two rows apart.
    let usage = UsageInfo {
        prompt_tokens: 100,
        completion_tokens: 40,
        model: Some("claude-opus-4-8".to_string()),
        iterations: vec![UsageInfo {
            prompt_tokens: 70,
            completion_tokens: 0,
            model: Some("claude-opus-4-8".to_string()),
            ..UsageInfo::default()
        }],
        ..UsageInfo::default()
    };
    metrics.record_llm_call(
        &usage,
        "claude-opus-4-8",
        1.0,
        false,
        Transport::Tls,
        |model, status, attempt| {
            llm_attrs(
                model,
                status,
                attempt,
                #[cfg(feature = "k8s")]
                None,
            )
        },
    );

    let mut rm = ResourceMetrics {
        resource: Resource::empty(),
        scope_metrics: vec![],
    };
    reader.collect(&mut rm)?;
    let get = |key: &str, attrs: &[KeyValue]| {
        attrs
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    };
    // Prompt tokens by attempt: served carries the served call, declined the fallback attempt.
    let prompt_by_attempt = |want: &str| -> u64 {
        rm.scope_metrics
            .iter()
            .flat_map(|s| &s.metrics)
            .find(|m| m.name == "llm_tokens_total")
            .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<u64>>())
            .map_or(0, |sum| {
                sum.data_points
                    .iter()
                    .filter(|p| get("token_type", &p.attributes).as_deref() == Some("prompt"))
                    .filter(|p| get("attempt", &p.attributes).as_deref() == Some(want))
                    .map(|p| p.value)
                    .sum()
            })
    };
    // Distinct series, so a dashboard can read a declined attempt's tokens on their own
    // rather than summed into the served call to the same model.
    assert_eq!(prompt_by_attempt("served"), 100);
    assert_eq!(prompt_by_attempt("declined"), 70);
    // The declined attempt's cost lands on its own attempt="declined" row, not inside
    // the served model's cost.
    let cost_attempts: std::collections::BTreeSet<String> = rm
        .scope_metrics
        .iter()
        .flat_map(|s| &s.metrics)
        .find(|m| m.name == "llm_cost_usd_total")
        .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<f64>>())
        .map(|sum| {
            sum.data_points
                .iter()
                .filter(|p| p.value > 0.0)
                .filter_map(|p| get("attempt", &p.attributes))
                .collect()
        })
        .unwrap_or_default();
    assert!(cost_attempts.contains("served"));
    assert!(cost_attempts.contains("declined"));
    Ok(())
}

#[test]
fn llm_suppressed_fallback_attempt_records_tokens_but_no_cost() -> Result<()> {
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    // A custom table prices both models, so a missing cost proves suppression, not a missing price.
    let json = r#"{"version": "t", "providers": [{"name": "p", "source": "s",
        "checked": "2026-10-01", "models": {"served-m": {"input": 2}, "declined-m": {"input": 3}}}]}"#;
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(Some(json)));
    let usage = UsageInfo {
        prompt_tokens: 1_000_000,
        model: Some("served-m".to_string()),
        iterations: vec![UsageInfo {
            prompt_tokens: 1_000_000,
            model: Some("declined-m".to_string()),
            cost_suppressed: true,
            ..UsageInfo::default()
        }],
        ..UsageInfo::default()
    };
    metrics.record_llm_call(
        &usage,
        "served-m",
        1.0,
        false,
        Transport::Tls,
        |model, status, attempt| {
            vec![
                KeyValue::new("model", model.to_string()),
                KeyValue::new("status", status.to_string()),
                KeyValue::new("attempt", attempt.as_str()),
            ]
        },
    );

    let mut rm = ResourceMetrics {
        resource: Resource::empty(),
        scope_metrics: vec![],
    };
    reader.collect(&mut rm)?;
    let models = |name: &str| -> Vec<String> {
        rm.scope_metrics
            .iter()
            .flat_map(|s| &s.metrics)
            .find(|m| m.name == name)
            .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<f64>>())
            .map(|sum| {
                sum.data_points
                    .iter()
                    .filter(|p| p.value > 0.0)
                    .filter_map(|p| {
                        p.attributes
                            .iter()
                            .find(|kv| kv.key.as_str() == "model")
                            .map(|kv| kv.value.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    // Both attempts' tokens are recorded, but only the served model is billed.
    let cost_models = models("llm_cost_usd_total");
    assert!(cost_models.contains(&"served-m".to_string()));
    assert!(!cost_models.contains(&"declined-m".to_string()));
    Ok(())
}

#[test]
fn llm_unparsed_calls_are_counted_by_reason() -> Result<()> {
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(None));
    let pod = || vec![KeyValue::new("target.namespace", "team-a")];
    metrics.record_unparsed("response_too_large", pod());
    metrics.record_unparsed("next_request", pod());
    metrics.record_unparsed("next_request", pod());

    let mut rm = ResourceMetrics {
        resource: Resource::empty(),
        scope_metrics: vec![],
    };
    reader.collect(&mut rm)?;
    let sum = rm
        .scope_metrics
        .iter()
        .flat_map(|s| &s.metrics)
        .find(|m| m.name == "llm_responses_unparsed_total")
        .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<u64>>())
        .context("llm_responses_unparsed_total counter not collected")?;
    let mut points: Vec<(Vec<String>, u64)> = sum
        .data_points
        .iter()
        .map(|p| {
            let mut kv: Vec<String> = p
                .attributes
                .iter()
                .map(|kv| format!("{}={}", kv.key, kv.value))
                .collect();
            kv.sort();
            (kv, p.value)
        })
        .collect();
    points.sort();
    let labels = |reason: &str| {
        vec![
            format!("reason={reason}"),
            "target.namespace=team-a".to_string(),
        ]
    };
    assert_eq!(
        points,
        [
            (labels("next_request"), 2),
            (labels("response_too_large"), 1)
        ]
    );
    Ok(())
}

// #79: operation is on requests, tokens and latency only; cache, reasoning and cost stay off it.
#[test]
fn llm_operation_label_is_on_requests_tokens_latency_only() -> Result<()> {
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(None));

    // gpt-5 has no cache-write price, so this chat call is unpriced (the unpriced-tokens row) while
    // still emitting cache and reasoning rows; the embedding call is priced and emits a cost row.
    let chat = UsageInfo {
        prompt_tokens: 1200,
        completion_tokens: 800,
        cache_read_tokens: Some(1000),
        cache_write_tokens: Some(200),
        thoughts_tokens: Some(50),
        model: Some("gpt-5".to_string()),
        ..UsageInfo::default()
    };
    let embeddings = UsageInfo {
        prompt_tokens: 410_000,
        completion_tokens: 0,
        model: Some("text-embedding-3-small".to_string()),
        operation: honeybeepf_llm_core::types::Operation::Embeddings,
        ..UsageInfo::default()
    };
    metrics.record_llm(&chat, 1.0, false, Transport::Tls, vec![]);
    metrics.record_llm(&embeddings, 1.0, false, Transport::Tls, vec![]);

    let mut rm = ResourceMetrics {
        resource: Resource::empty(),
        scope_metrics: vec![],
    };
    reader.collect(&mut rm)?;
    let label = |attrs: &[KeyValue], key: &str| {
        attrs
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    };
    let u64_ops = |name: &str| -> std::collections::BTreeSet<Option<String>> {
        rm.scope_metrics
            .iter()
            .flat_map(|s| &s.metrics)
            .find(|m| m.name == name)
            .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<u64>>())
            .map(|sum| {
                sum.data_points
                    .iter()
                    .map(|p| label(&p.attributes, "operation"))
                    .collect()
            })
            .unwrap_or_default()
    };

    // Requests and tokens carry operation, both values present.
    let want: std::collections::BTreeSet<Option<String>> =
        [Some("chat".to_string()), Some("embeddings".to_string())].into();
    assert_eq!(u64_ops("llm_requests_total"), want);
    assert_eq!(u64_ops("llm_tokens_total"), want);

    // Latency is a histogram and also carries operation.
    let latency_ops: std::collections::BTreeSet<Option<String>> = rm
        .scope_metrics
        .iter()
        .flat_map(|s| &s.metrics)
        .find(|m| m.name == "llm_latency_seconds")
        .and_then(|m| m.data.as_any().downcast_ref::<data::Histogram<f64>>())
        .map(|h| {
            h.data_points
                .iter()
                .map(|p| label(&p.attributes, "operation"))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(latency_ops, want);

    // Cache, reasoning and unpriced rows (u64 sums) must not carry operation (kept off to limit
    // their cardinality). A metric with rows but no operation attribute has the value set {None}.
    let only_none: std::collections::BTreeSet<Option<String>> = [None].into();
    for name in [
        "llm_cache_read_tokens_total",
        "llm_cache_write_tokens_total",
        "llm_reasoning_tokens_total",
        "llm_unpriced_tokens_total",
    ] {
        assert_eq!(u64_ops(name), only_none, "{name} must not carry operation");
    }
    // Cost (f64 sum) must not carry operation either.
    let cost_has_op = rm
        .scope_metrics
        .iter()
        .flat_map(|s| &s.metrics)
        .find(|m| m.name == "llm_cost_usd_total")
        .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<f64>>())
        .is_some_and(|sum| {
            sum.data_points
                .iter()
                .any(|p| label(&p.attributes, "operation").is_some())
        });
    assert!(!cost_has_op, "cost must not carry operation");
    Ok(())
}

#[test]
fn llm_cost_is_priced_per_call_and_unknown_models_stay_unpriced() -> Result<()> {
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(None));
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
    metrics.record_llm(&priced, 1.0, false, Transport::Tls, vec![]);
    metrics.record_llm(&priced, 1.0, true, Transport::Tls, vec![]);
    metrics.record_llm(&unknown, 1.0, false, Transport::Tls, vec![]);

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
    assert!(
        cost.data_points.iter().all(|p| {
            label(&p.attributes, "price_table_version").as_deref() == Some("2026-11-09")
        })
    );
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

#[test]
fn llm_cache_savings_ceiling_is_emitted_with_the_cache_price_basis() -> Result<()> {
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(None));
    // claude-opus-4-8: input $5, cache read $0.5 per million → 1000 uncached could save $0.0045.
    let usage = UsageInfo {
        prompt_tokens: 1000,
        model: Some("claude-opus-4-8".to_string()),
        ..UsageInfo::default()
    };
    metrics.record_llm(&usage, 1.0, false, Transport::Tls, vec![]);

    let mut rm = ResourceMetrics {
        resource: Resource::empty(),
        scope_metrics: vec![],
    };
    reader.collect(&mut rm)?;
    let ceiling = rm
        .scope_metrics
        .iter()
        .flat_map(|s| &s.metrics)
        .find(|m| m.name == "llm_cache_savings_ceiling_usd_total")
        .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<f64>>())
        .context("ceiling counter not collected")?;
    let point = ceiling.data_points.first().context("one ceiling point")?;
    assert_eq!((point.value * 1e6).round(), 4500.0);
    let basis = point
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == "cache_price")
        .map(|kv| kv.value.to_string());
    assert_eq!(basis.as_deref(), Some("known"));
    Ok(())
}

#[test]
fn llm_cache_savings_ceiling_skips_unpriced_calls_and_flags_assumed_free() -> Result<()> {
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(None));
    // gpt-5.5-pro: input $30, no cache-read price → 1000 uncached bounds at the full input cost.
    let priced = UsageInfo {
        prompt_tokens: 1000,
        model: Some("gpt-5.5-pro".to_string()),
        ..UsageInfo::default()
    };
    metrics.record_llm(&priced, 1.0, false, Transport::Tls, vec![]);
    // Same model reporting a cache read has no cache-read price, so cost() cannot price it;
    // the ceiling must not be emitted for a call that never reaches the cost panels.
    let unpriced = UsageInfo {
        prompt_tokens: 1000,
        cache_read_tokens: Some(300),
        model: Some("gpt-5.5-pro".to_string()),
        ..UsageInfo::default()
    };
    metrics.record_llm(&unpriced, 1.0, false, Transport::Tls, vec![]);

    let mut rm = ResourceMetrics {
        resource: Resource::empty(),
        scope_metrics: vec![],
    };
    reader.collect(&mut rm)?;
    let ceiling = rm
        .scope_metrics
        .iter()
        .flat_map(|s| &s.metrics)
        .find(|m| m.name == "llm_cache_savings_ceiling_usd_total")
        .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<f64>>())
        .context("ceiling counter not collected")?;
    assert_eq!(
        ceiling.data_points.len(),
        1,
        "only the priced call contributes"
    );
    let point = &ceiling.data_points[0];
    assert_eq!((point.value * 1e6).round(), 30000.0);
    let attr = |key| {
        point
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    };
    assert_eq!(attr("cache_price").as_deref(), Some("assumed_free"));
    assert_eq!(
        attr("reason"),
        None,
        "the unpriced reason label never leaks here"
    );
    Ok(())
}

// Embeddings are priced but offer no prompt cache, so they must not inflate the savings ceiling.
// record_llm adds no model label, so each call is checked on its own reader by point count.
#[test]
fn llm_cache_savings_ceiling_excludes_priced_embeddings() -> Result<()> {
    let ceiling_points = |usage: &UsageInfo| -> Result<(usize, usize)> {
        let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
        let provider = SdkMeterProvider::builder()
            .with_reader(reader.clone())
            .build();
        let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(None));
        metrics.record_llm(usage, 1.0, false, Transport::Tls, vec![]);
        let mut rm = ResourceMetrics {
            resource: Resource::empty(),
            scope_metrics: vec![],
        };
        reader.collect(&mut rm)?;
        let points = |name: &str| {
            rm.scope_metrics
                .iter()
                .flat_map(|s| &s.metrics)
                .find(|m| m.name == name)
                .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<f64>>())
                .map_or(0, |s| s.data_points.len())
        };
        Ok((
            points("llm_cost_usd_total"),
            points("llm_cache_savings_ceiling_usd_total"),
        ))
    };
    // Priced embedding call: input cost is recorded, but no savings-ceiling point.
    let embeddings = UsageInfo {
        prompt_tokens: 1000,
        model: Some("text-embedding-3-small".to_string()),
        operation: Operation::Embeddings,
        ..UsageInfo::default()
    };
    let (cost, ceiling) = ceiling_points(&embeddings)?;
    assert!(cost > 0, "the embedding call is still priced");
    assert_eq!(ceiling, 0, "embeddings raise no savings ceiling");
    // Control: the same shape as a chat call does raise one, so the guard is not suppressing all.
    let chat = UsageInfo {
        prompt_tokens: 1000,
        model: Some("gpt-5.5-pro".to_string()),
        ..UsageInfo::default()
    };
    assert_eq!(ceiling_points(&chat)?.1, 1, "chat still raises a ceiling");
    Ok(())
}

#[test]
fn custom_prices_merge_over_the_builtin_table_or_are_ignored() {
    let json = r#"{"version": "acme", "providers": [{"name": "vllm", "source": "s",
        "checked": "2026-10-01", "models": {"my-vllm-llama": {"input": 1}}}]}"#;
    let version = |custom| load_prices(custom).map(|p| p.version.value.to_string());
    let builtin = version(None).expect("built-in table loads");
    assert_eq!(version(Some("{")), Some(builtin.clone()));
    assert!(version(Some(json)).is_some_and(|v| v.starts_with(&format!("{builtin}+custom-"))));
}

/// A ConfigMap volume: `prices.json -> ..data/prices.json`, `..data -> ..<n>`.
struct ConfigMapDir {
    root: std::path::PathBuf,
    n: u32,
}

impl ConfigMapDir {
    fn new(name: &str) -> Result<Self> {
        let dir = format!("hb-prices-{name}-{}", std::process::id());
        let root = std::env::temp_dir().join(dir);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;
        std::os::unix::fs::symlink("..data/prices.json", root.join("prices.json"))?;
        Ok(Self { root, n: 0 })
    }

    fn path(&self) -> String {
        self.root.join("prices.json").to_string_lossy().into_owned()
    }

    /// Publish new contents the way kubelet does: a new directory, then a rename over `..data`.
    fn publish(&mut self, contents: impl AsRef<[u8]>) -> Result<()> {
        self.n += 1;
        let dir = format!("..{}", self.n);
        std::fs::create_dir(self.root.join(&dir))?;
        std::fs::write(self.root.join(&dir).join("prices.json"), contents)?;
        std::os::unix::fs::symlink(&dir, self.root.join("..data_tmp"))?;
        std::fs::rename(self.root.join("..data_tmp"), self.root.join("..data"))?;
        let old = self.root.join(format!("..{}", self.n - 1));
        let _ = std::fs::remove_dir_all(old);
        Ok(())
    }
}

impl Drop for ConfigMapDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn vllm_prices(input: &str) -> String {
    format!(
        r#"{{"version": "acme", "providers": [{{"name": "vllm", "source": "s",
        "checked": "2026-10-01", "models": {{"my-vllm-llama": {{"input": {input}}}}}}}]}}"#
    )
}

fn vllm_call() -> UsageInfo {
    UsageInfo {
        prompt_tokens: 1_000_000,
        model: Some("my-vllm-llama".to_string()),
        ..UsageInfo::default()
    }
}

#[test]
fn price_file_swaps_the_table_only_for_new_contents_that_pass_the_checks() -> Result<()> {
    let mut dir = ConfigMapDir::new("swap")?;
    let mut file = PriceFile::new(dir.path());
    assert_eq!(file.changed(), None, "no file yet");
    dir.publish(vllm_prices("1"))?;
    let prices = ArcSwapOption::from_pointee(load_prices(file.changed()));
    let input_usd = |p: &ArcSwapOption<Prices>| {
        let cost = p.load().as_deref().map(|p| p.table.cost(&vllm_call()));
        cost.and_then(Result::ok).map(|c| c.input)
    };
    assert_eq!(input_usd(&prices), Some(1.0));
    let loaded = prices.load_full().context("prices loaded")?;
    let kept = |p: &ArcSwapOption<Prices>| p.load_full().is_some_and(|t| Arc::ptr_eq(&t, &loaded));

    for (contents, why) in [
        (vllm_prices("1"), "same contents rewritten"),
        ("{".to_string(), "not JSON"),
        (vllm_prices("-1"), "fails the table checks"),
        (vllm_prices("1"), "back to the loaded contents"),
    ] {
        dir.publish(contents)?;
        reload_prices(&prices, &mut file);
        assert!(kept(&prices), "{why}");
    }
    dir.publish(b"{\"version\": \"\xff\"}")?;
    reload_prices(&prices, &mut file);
    assert!(kept(&prices), "not UTF-8");
    std::fs::remove_file(dir.root.join("..data"))?;
    reload_prices(&prices, &mut file);
    assert!(kept(&prices), "file removed");

    dir.publish(vllm_prices("2"))?;
    reload_prices(&prices, &mut file);
    assert_eq!(input_usd(&prices), Some(2.0));
    let version =
        |p: &ArcSwapOption<Prices>| p.load().as_deref().map(|p| p.table.version().to_string());
    assert!(version(&prices).is_some_and(|v| v != loaded.table.version()));
    Ok(())
}

#[test]
fn price_file_rejected_at_startup_is_loaded_once_fixed() -> Result<()> {
    let mut dir = ConfigMapDir::new("fixed")?;
    dir.publish(vllm_prices("-1"))?;
    let mut file = PriceFile::new(dir.path());
    let prices = ArcSwapOption::from_pointee(load_prices(file.changed()));
    let builtin = load_prices(None).context("built-in table loads")?;
    let builtin = builtin.table.version();
    let version =
        |p: &ArcSwapOption<Prices>| p.load().as_deref().map(|p| p.table.version().to_string());
    assert_eq!(version(&prices).as_deref(), Some(builtin));
    reload_prices(&prices, &mut file);
    assert_eq!(
        version(&prices).as_deref(),
        Some(builtin),
        "same rejected file"
    );

    dir.publish(vllm_prices("1"))?;
    reload_prices(&prices, &mut file);
    let custom = format!("{builtin}+custom-");
    assert!(version(&prices).is_some_and(|v| v.starts_with(&custom)));
    Ok(())
}

#[test]
fn llm_cost_after_a_reload_has_the_new_price_and_version() -> Result<()> {
    let reader = SharedReader(std::sync::Arc::new(ManualReader::builder().build()));
    let provider = SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    let mut dir = ConfigMapDir::new("label")?;
    dir.publish(vllm_prices("1"))?;
    let mut file = PriceFile::new(dir.path());
    let metrics = HoneyBeeMetrics::new(&provider.meter("test"), load_prices(file.changed()));
    metrics.record_llm(&vllm_call(), 1.0, false, Transport::Tls, vec![]);
    dir.publish(vllm_prices("3"))?;
    reload_prices(&metrics.prices, &mut file);
    metrics.record_llm(&vllm_call(), 1.0, false, Transport::Tls, vec![]);

    let mut rm = ResourceMetrics {
        resource: Resource::empty(),
        scope_metrics: vec![],
    };
    reader.collect(&mut rm)?;
    let cost = rm
        .scope_metrics
        .iter()
        .flat_map(|s| &s.metrics)
        .find(|m| m.name == "llm_cost_usd_total")
        .and_then(|m| m.data.as_any().downcast_ref::<data::Sum<f64>>())
        .context("llm_cost_usd_total counter not collected")?;
    let mut points: Vec<(f64, String)> = cost
        .data_points
        .iter()
        .flat_map(|p| {
            let version = p
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == "price_table_version");
            version.map(|kv| (p.value, kv.value.to_string()))
        })
        .collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    let usd: Vec<f64> = points.iter().map(|p| p.0).collect();
    assert_eq!(usd, [1.0, 3.0]);
    assert!(points[0].1.contains("+custom-") && points[0].1 != points[1].1);
    Ok(())
}
