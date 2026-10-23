//! Credentials in captured TLS plaintext must never reach the agent's logs or exported metrics.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock, Weak};

use honeybeepf_llm_common::{EventMetadata, LlmEvent, MAX_SSL_BUF_SIZE, capture_limit};
use honeybeepf_llm_core::types::LlmDirection;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::reader::MetricReader;
use opentelemetry_sdk::metrics::{
    InstrumentKind, ManualReader, MetricResult, Pipeline, SdkMeterProvider, Temporality,
};

use super::processor::Outcome;
use super::route_event;

/// Fake credentials shaped like real ones; each carries `HBTEST`, so a partial print is caught too.
mod fake {
    pub const OPENAI_KEY: &str = "sk-proj-HBTEST0openai00000000000000000000000000";
    pub const ANTHROPIC_KEY: &str = "sk-ant-api03-HBTEST0anthropic000000000000000000000";
    pub const GEMINI_KEY: &str = "AIzaSyHBTEST0gemini0000000000000000000";
    pub const AZURE_KEY: &str = "HBTEST0azure00000000000000000000";
    pub const AWS_ACCESS_KEY_ID: &str = "AKIAHBTEST0000000000";
    pub const AWS_SIGNATURE: &str =
        "HBTEST0sigv4000000000000000000000000000000000000000000000000000";
    pub const AWS_SESSION_TOKEN: &str = "IQoJb3JpZ2luX2VjHBTEST0session0token000000000000";
    pub const MARKER: &str = "HBTEST";

    pub const ALL: [&str; 8] = [
        OPENAI_KEY,
        ANTHROPIC_KEY,
        GEMINI_KEY,
        AZURE_KEY,
        AWS_ACCESS_KEY_ID,
        AWS_SIGNATURE,
        AWS_SESSION_TOKEN,
        MARKER,
    ];
}

thread_local! {
    static LOGS: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Keeps every record at every level; per thread, so parallel tests don't mix their output.
struct CaptureLogger;

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        LOGS.with(|logs| {
            let _ = writeln!(
                logs.borrow_mut(),
                "{} {} {}",
                record.level(),
                record.target(),
                record.args()
            );
        });
    }

    fn flush(&self) {}
}

static LOGGER: CaptureLogger = CaptureLogger;

/// Shares one ManualReader between the meter provider and the test, which reads what OTLP would export.
#[derive(Debug)]
struct SharedReader(Arc<ManualReader>);

impl MetricReader for SharedReader {
    fn register_pipeline(&self, pipeline: Weak<Pipeline>) {
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

static METRICS: OnceLock<(SdkMeterProvider, Arc<ManualReader>)> = OnceLock::new();

fn init_capture() -> &'static ManualReader {
    let (_, reader) = METRICS.get_or_init(|| {
        log::set_logger(&LOGGER).expect("no other logger in the test binary");
        log::set_max_level(log::LevelFilter::Trace);
        let reader = Arc::new(ManualReader::builder().build());
        let provider = SdkMeterProvider::builder()
            .with_reader(SharedReader(reader.clone()))
            .build();
        assert!(crate::telemetry::init_test_metrics(
            &provider.meter("honeybeepf-llm")
        ));
        (provider, reader)
    });
    reader
}

/// Every metric point collected so far, attributes included.
fn exported_metrics() -> String {
    let mut rm = ResourceMetrics {
        resource: Resource::empty(),
        scope_metrics: Vec::new(),
    };
    init_capture().collect(&mut rm).expect("collect metrics");
    format!("{rm:?}")
}

fn assert_clean(case: &str, logs: &str, metric: Option<&str>) {
    let metrics = exported_metrics();
    if let Some(metric) = metric {
        assert!(metrics.contains(metric), "{case}: {metric} not exported");
    }
    for secret in fake::ALL {
        assert!(
            !logs.contains(secret),
            "{case}: logs contain {secret}:\n{logs}"
        );
        assert!(
            !metrics.contains(secret),
            "{case}: exported metrics contain {secret}"
        );
    }
}

enum Step<'a> {
    Write(&'a [u8]),
    Read(&'a [u8]),
    Free,
}

const PID: u32 = 4242;
const SSL: u64 = 0x5555_0000_7000;

/// One SSL call as the kernel sends it: up to its capture limit, in MAX_SSL_BUF_SIZE fragments.
fn call_events(dir: LlmDirection, call: &[u8]) -> Vec<LlmEvent> {
    let limit = capture_limit(dir);
    call[..call.len().min(limit)]
        .chunks(MAX_SSL_BUF_SIZE)
        .enumerate()
        .map(|(i, chunk)| {
            let mut buf = [0u8; MAX_SSL_BUF_SIZE];
            buf[..chunk.len()].copy_from_slice(chunk);
            LlmEvent {
                metadata: EventMetadata {
                    pid: PID,
                    ..Default::default()
                },
                rw: dir as u8,
                truncated: u8::from(call.len() > limit),
                len: call.len() as u32,
                buf_filled: 1,
                offset: (i * MAX_SSL_BUF_SIZE) as u32,
                ssl_ptr: SSL,
                buf,
                ..Default::default()
            }
        })
        .collect()
}

/// Feeds the steps through the ringbuf handler's routing and returns the outcomes and the logs they produced.
fn run(steps: &[Step]) -> (Vec<Outcome>, String) {
    init_capture();
    LOGS.with(|logs| logs.borrow_mut().clear());
    let mut streams = HashMap::new();
    let mut out = Vec::new();
    for step in steps {
        let events = match step {
            Step::Write(call) => call_events(LlmDirection::Write, call),
            Step::Read(call) => call_events(LlmDirection::Read, call),
            Step::Free => vec![LlmEvent {
                metadata: EventMetadata {
                    pid: PID,
                    ..Default::default()
                },
                closed: 1,
                ssl_ptr: SSL,
                ..Default::default()
            }],
        };
        for event in &events {
            out.push(route_event(
                &mut streams,
                event,
                #[cfg(feature = "k8s")]
                || None,
            ));
        }
    }
    (out, LOGS.with(|logs| logs.take()))
}

/// A prompt that pastes keys, as users do when asking about an auth error.
fn prompt() -> String {
    format!(
        "why does this fail? {} {} {} {}",
        fake::OPENAI_KEY,
        fake::ANTHROPIC_KEY,
        fake::GEMINI_KEY,
        fake::AWS_ACCESS_KEY_ID
    )
}

fn chat_body(model: &str) -> String {
    format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"{}"}}]}}"#,
        prompt()
    )
}

fn post(path: &str, host: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut req = format!("POST {path} HTTP/1.1\r\nHost: {host}\r\n");
    for (name, value) in headers {
        let _ = write!(req, "{name}: {value}\r\n");
    }
    let _ = write!(
        req,
        "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    req.into_bytes()
}

fn json_response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";

/// Request, then the response head and body as separate reads, like a client reading TLS records.
fn exchange<'a>(req: &'a [u8], resp: &'a [u8]) -> Vec<Step<'a>> {
    let split = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(resp.len(), |i| i + 4);
    let (head, body) = resp.split_at(split);
    vec![Step::Write(req), Step::Read(head), Step::Read(body)]
}

fn bearer(key: &str) -> String {
    format!("Bearer {key}")
}

/// The completion repeats the pasted key, so response content is covered too.
fn openai_completion(model: &str) -> String {
    format!(
        r#"{{"id":"c1","object":"chat.completion","model":"{model}","choices":[{{"index":0,"message":{{"role":"assistant","content":"rotate {}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":11,"completion_tokens":4}}}}"#,
        fake::OPENAI_KEY
    )
}

#[test]
fn openai_bearer_key_stays_out_of_logs_and_metrics() {
    let req = post(
        "/v1/chat/completions",
        "api.openai.com",
        &[("Authorization", &bearer(fake::OPENAI_KEY))],
        &chat_body("gpt-4o"),
    );
    let resp = json_response("200 OK", &openai_completion("leakcheck-openai"));
    let (out, logs) = run(&exchange(&req, &resp));
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    assert!(logs.contains("LLM SUCCESS"), "{logs}");
    assert_clean("openai", &logs, Some("leakcheck-openai"));
}

#[test]
fn anthropic_x_api_key_stays_out_of_logs_and_metrics() {
    let req = post(
        "/v1/messages",
        "api.anthropic.com",
        &[
            ("x-api-key", fake::ANTHROPIC_KEY),
            ("anthropic-version", "2023-06-01"),
        ],
        &chat_body("claude-opus-5-5"),
    );
    let resp = format!(
        "{SSE_HEAD}event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m1\",\"model\":\"leakcheck-anthropic\",\"usage\":{{\"input_tokens\":25,\"output_tokens\":1}}}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"rotate {}\"}}}}\n\n\
         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":15}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        fake::ANTHROPIC_KEY
    );
    let (out, logs) = run(&exchange(&req, resp.as_bytes()));
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    assert!(logs.contains("LLM SUCCESS"), "{logs}");
    assert_clean("anthropic", &logs, Some("leakcheck-anthropic"));
}

#[test]
fn gemini_query_and_header_keys_stay_out_of_logs_and_metrics() {
    let body = format!(
        r#"{{"contents":[{{"role":"user","parts":[{{"text":"{}"}}]}}]}}"#,
        prompt()
    );
    let usage = |model: &str| {
        format!(
            r#"{{"candidates":[{{"content":{{"parts":[{{"text":"ok"}}],"role":"model"}},"finishReason":"STOP"}}],"usageMetadata":{{"promptTokenCount":7,"candidatesTokenCount":3}},"modelVersion":"{model}"}}"#
        )
    };
    let json = json_response("200 OK", &usage("leakcheck-gemini-json"));
    let sse = format!("{SSE_HEAD}data: {}\r\n\r\n", usage("leakcheck-gemini-sse"));
    let cases: [(&str, &[u8], &str); 2] = [
        (":generateContent?", &json, "leakcheck-gemini-json"),
        (
            ":streamGenerateContent?alt=sse&",
            sse.as_bytes(),
            "leakcheck-gemini-sse",
        ),
    ];
    for (method, resp, model) in cases {
        let path = format!(
            "/v1beta/models/gemini-2.5-flash{method}key={}",
            fake::GEMINI_KEY
        );
        let req = post(
            &path,
            "generativelanguage.googleapis.com",
            &[("x-goog-api-key", fake::GEMINI_KEY)],
            &body,
        );
        let (out, logs) = run(&exchange(&req, resp));
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{method}: {out:?}");
        assert!(logs.contains("Detected HTTP/1.1"), "{logs}");
        assert_clean(method, &logs, Some(model));
    }
}

#[test]
fn azure_api_key_stays_out_of_logs_and_metrics() {
    let req = post(
        "/openai/deployments/gpt-4o/chat/completions?api-version=2024-10-21",
        "myres.openai.azure.com",
        &[("api-key", fake::AZURE_KEY)],
        &chat_body("gpt-4o"),
    );
    let resp = json_response("200 OK", &openai_completion("leakcheck-azure"));
    let (out, logs) = run(&exchange(&req, &resp));
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    assert!(logs.contains("LLM SUCCESS"), "{logs}");
    assert_clean("azure", &logs, Some("leakcheck-azure"));
}

#[test]
fn aws_sigv4_credentials_stay_out_of_logs_and_metrics() {
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/20261023/us-east-1/bedrock/aws4_request, \
         SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, Signature={}",
        fake::AWS_ACCESS_KEY_ID,
        fake::AWS_SIGNATURE
    );
    let body = format!(
        r#"{{"messages":[{{"role":"user","content":[{{"text":"{}"}}]}}]}}"#,
        prompt()
    );
    let path = "/model/anthropic.claude-sonnet-5-5-v1:0/converse";
    let presigned = format!(
        "{path}?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential={}%2F20261023%2Fus-east-1%2Fbedrock%2Faws4_request\
         &X-Amz-Security-Token={}&X-Amz-Signature={}",
        fake::AWS_ACCESS_KEY_ID,
        fake::AWS_SESSION_TOKEN,
        fake::AWS_SIGNATURE
    );
    let signed_headers = [
        ("Authorization", authorization.as_str()),
        ("X-Amz-Date", "20261023T000000Z"),
        ("X-Amz-Security-Token", fake::AWS_SESSION_TOKEN),
    ];
    let resp = json_response(
        "200 OK",
        r#"{"output":{"message":{"role":"assistant","content":[{"text":"ok"}]}},"stopReason":"end_turn","usage":{"inputTokens":30,"outputTokens":9,"totalTokens":39}}"#,
    );
    let cases: [(&str, &[(&str, &str)]); 2] = [(path, &signed_headers), (presigned.as_str(), &[])];
    for (target, headers) in cases {
        let req = post(
            target,
            "bedrock-runtime.us-east-1.amazonaws.com",
            headers,
            &body,
        );
        let (out, logs) = run(&exchange(&req, &resp));
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
        assert!(logs.contains("LLM SUCCESS"), "{logs}");
        assert_clean("bedrock", &logs, Some("llm_tokens_total"));
    }
}

#[test]
fn provider_error_echoing_the_key_stays_out_of_logs_and_metrics() {
    let req = post(
        "/v1/chat/completions",
        "api.openai.com",
        &[("Authorization", &bearer(fake::OPENAI_KEY))],
        &chat_body("gpt-4o"),
    );
    let resp = json_response(
        "401 Unauthorized",
        &format!(
            r#"{{"error":{{"message":"Incorrect API key provided: {}.","type":"invalid_request_error","code":"invalid_api_key"}}}}"#,
            fake::OPENAI_KEY
        ),
    );
    let (out, logs) = run(&exchange(&req, &resp));
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    assert!(logs.contains("LLM FAILED"), "{logs}");
    assert_clean("provider error", &logs, Some("llm_requests_total"));
}

#[test]
fn unparsed_responses_stay_out_of_logs_and_metrics() {
    let req = post(
        "/v1/messages",
        "api.anthropic.com",
        &[("x-api-key", fake::ANTHROPIC_KEY)],
        &chat_body("claude-opus-5-5"),
    );
    let echo = format!(
        "<html><body>502 Bad Gateway: upstream rejected x-api-key {}</body></html>",
        fake::ANTHROPIC_KEY
    );
    let bad_gateway = format!(
        "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{echo}",
        echo.len()
    );
    let broken_json = json_response("200 OK", &format!(r#"{{"usage": {}"#, fake::ANTHROPIC_KEY));
    let partial = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"text\":\"{}\"}}}}\n\n",
        fake::ANTHROPIC_KEY
    );
    let stream = format!("{SSE_HEAD}{partial}");
    // One SSL_read over the 16 KB capture limit: the tail is lost, so the response is unparsed.
    let mut oversized = SSE_HEAD.as_bytes().to_vec();
    while oversized.len() <= capture_limit(LlmDirection::Read) + SSE_HEAD.len() {
        oversized.extend_from_slice(partial.as_bytes());
    }

    let cases: [(&str, &[u8], Option<Step>); 4] = [
        (
            "no usage, next request",
            bad_gateway.as_bytes(),
            Some(Step::Write(&req)),
        ),
        (
            "parse failure, next request",
            &broken_json,
            Some(Step::Write(&req)),
        ),
        ("closed mid-stream", stream.as_bytes(), Some(Step::Free)),
        ("truncated read", &oversized, None),
    ];
    for (case, resp, last) in cases {
        let mut steps = exchange(&req, resp);
        steps.extend(last);
        let (out, logs) = run(&steps);
        assert!(out.contains(&Outcome::Unparsed), "{case}: {out:?}");
        assert!(logs.contains("LLM UNPARSED"), "{case}: {logs}");
        assert_clean(case, &logs, Some("llm_responses_unparsed_total"));
    }
}

fn h2_frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_be_bytes()[1..].to_vec();
    frame.extend_from_slice(&[kind, flags]);
    frame.extend_from_slice(&stream.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// HPACK literal header field without indexing, no Huffman (lengths under 127).
fn hpack_literal(block: &mut Vec<u8>, name: &str, value: &str) {
    block.extend_from_slice(&[0x00, name.len() as u8]);
    block.extend_from_slice(name.as_bytes());
    block.push(value.len() as u8);
    block.extend_from_slice(value.as_bytes());
}

// Real clients Huffman-code HPACK headers, so a header key would not show as text; the body key still would.
#[test]
fn http2_authorization_stays_out_of_logs_and_metrics() {
    let mut headers = Vec::new();
    for (name, value) in [
        (":method", "POST"),
        (":path", "/v1/chat/completions"),
        (":authority", "api.openai.com"),
        ("authorization", &bearer(fake::OPENAI_KEY)),
    ] {
        hpack_literal(&mut headers, name, value);
    }
    let mut req = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    req.extend(h2_frame(0x4, 0, 0, &[]));
    req.extend(h2_frame(0x1, 0x4, 1, &headers));
    req.extend(h2_frame(0x0, 0x1, 1, chat_body("gpt-4o").as_bytes()));

    let mut status = Vec::new();
    hpack_literal(&mut status, ":status", "200");
    let head = h2_frame(0x1, 0x4, 1, &status);
    let data = h2_frame(0x0, 0x1, 1, openai_completion("leakcheck-h2").as_bytes());

    let (out, logs) = run(&[Step::Write(&req), Step::Read(&head), Step::Read(&data)]);
    assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    assert!(logs.contains("Detected HTTP/2"), "{logs}");
    assert_clean("http2", &logs, Some("leakcheck-h2"));
}

#[test]
fn non_generation_requests_with_keys_stay_out_of_logs_and_metrics() {
    let auth = bearer(fake::OPENAI_KEY);
    let models =
        format!("GET /v1/models HTTP/1.1\r\nHost: api.openai.com\r\nAuthorization: {auth}\r\n\r\n");
    let embeddings = post(
        "/v1/embeddings",
        "api.openai.com",
        &[("Authorization", &auth)],
        &format!(
            r#"{{"model":"text-embedding-3-small","input":"{}"}}"#,
            prompt()
        ),
    );
    let other_host = post(
        "/v1/chat/completions",
        "llm.internal.example",
        &[("Authorization", &auth)],
        &chat_body("gpt-4o"),
    );
    let ok = json_response("200 OK", r#"{"object":"list","data":[]}"#);
    let (out, logs) = run(&[
        Step::Write(models.as_bytes()),
        Step::Read(&ok),
        Step::Write(&embeddings),
        Step::Read(&ok),
        Step::Write(&other_host),
        Step::Read(&ok),
    ]);
    assert!(out.iter().all(|o| *o == Outcome::Pending), "{out:?}");
    assert_clean("non-generation", &logs, None);
}
