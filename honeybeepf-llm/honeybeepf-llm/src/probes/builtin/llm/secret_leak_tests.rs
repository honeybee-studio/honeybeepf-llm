//! Credentials in captured TLS plaintext must never reach the agent's logs or exported metrics.

use std::cell::RefCell;
use std::collections::HashMap;
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
    pub const OPENAI: &str = "sk-proj-HBTEST0openai00000000000000000000000000";
    pub const ANTHROPIC: &str = "sk-ant-api03-HBTEST0anthropic000000000000000000000";
    pub const GEMINI: &str = "AIzaSyHBTEST0gemini0000000000000000000";
    pub const AZURE: &str = "HBTEST0azure00000000000000000000";
    pub const AWS_KEY_ID: &str = "AKIAHBTEST0000000000";
    pub const AWS_SIG: &str = "HBTEST0sigv40000000000000000000000000000000000";
    pub const AWS_TOKEN: &str = "IQoJb3JpZ2luX2VjHBTEST0session0token000000000000";
    pub const ALL: [&str; 8] = [
        OPENAI, ANTHROPIC, GEMINI, AZURE, AWS_KEY_ID, AWS_SIG, AWS_TOKEN, "HBTEST",
    ];
}

thread_local! {
    static LOGS: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Keeps every record at every level, per thread so parallel tests don't mix their output.
struct CaptureLogger;

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, r: &log::Record) {
        let line = format!("{} {} {}\n", r.level(), r.target(), r.args());
        LOGS.with(|logs| logs.borrow_mut().push_str(&line));
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
        let meter = provider.meter("honeybeepf-llm");
        assert!(crate::telemetry::init_test_metrics(&meter));
        (provider, reader)
    });
    reader
}

#[derive(Clone)]
enum Step {
    Write(Vec<u8>),
    Read(Vec<u8>),
    Free,
}

/// Events for one step as the kernel sends them: an SSL call up to its capture limit, in 4 KB fragments.
fn events(step: &Step) -> Vec<LlmEvent> {
    let (dir, call, closed) = match step {
        Step::Write(call) => (LlmDirection::Write, &call[..], 0),
        Step::Read(call) => (LlmDirection::Read, &call[..], 0),
        Step::Free => (LlmDirection::Unknown, &[][..], 1),
    };
    let limit = capture_limit(dir);
    let base = LlmEvent {
        metadata: EventMetadata {
            pid: 4242,
            ..Default::default()
        },
        ssl_ptr: 0x5555_0000_7000,
        rw: dir as u8,
        len: call.len() as u32,
        truncated: u8::from(call.len() > limit),
        closed,
        buf_filled: 1,
        ..Default::default()
    };
    if closed == 1 {
        return vec![base];
    }
    let fragments = call[..call.len().min(limit)].chunks(MAX_SSL_BUF_SIZE);
    let mut out = Vec::new();
    for (i, chunk) in fragments.enumerate() {
        let mut e = base;
        e.offset = (i * MAX_SSL_BUF_SIZE) as u32;
        e.buf[..chunk.len()].copy_from_slice(chunk);
        out.push(e);
    }
    out
}

/// Feeds the steps through the ringbuf handler's routing; returns outcomes, logs and all metrics so far.
fn run(steps: &[Step]) -> (Vec<Outcome>, String, String) {
    let reader = init_capture();
    LOGS.with(|logs| logs.borrow_mut().clear());
    let mut streams = HashMap::new();
    let mut out = Vec::new();
    for e in steps.iter().flat_map(events) {
        #[cfg(feature = "k8s")]
        out.push(route_event(&mut streams, &e, || None));
        #[cfg(not(feature = "k8s"))]
        out.push(route_event(&mut streams, &e));
    }
    let mut rm = ResourceMetrics {
        resource: Resource::empty(),
        scope_metrics: Vec::new(),
    };
    reader.collect(&mut rm).expect("collect metrics");
    (out, LOGS.with(|logs| logs.take()), format!("{rm:?}"))
}

fn resp(status: &str, content_type: &str, body: &str) -> String {
    let len = body.len();
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\n\r\n{body}"
    )
}

/// Request, then the response head and body as separate reads, like a client reading TLS records.
fn exchange(req: &str, resp: &str) -> Vec<Step> {
    let (head, body) = resp.split_at(resp.find("\r\n\r\n").map_or(0, |i| i + 4));
    let read = |s: &str| Step::Read(s.as_bytes().to_vec());
    vec![Step::Write(req.as_bytes().to_vec()), read(head), read(body)]
}

/// One HTTP/2 frame on stream 1.
fn h2_frame(kind: u8, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_be_bytes()[1..].to_vec();
    frame.extend_from_slice(&[kind, flags, 0, 0, 0, 1]);
    frame.extend_from_slice(payload);
    frame
}

/// HPACK literals without Huffman; real clients Huffman-code headers, so there only a body key shows as text.
fn hpack(headers: &str) -> Vec<u8> {
    let mut block = Vec::new();
    for (name, value) in headers.lines().filter_map(|h| h.split_once(": ")) {
        let (n, v) = ([0x00, name.len() as u8], [value.len() as u8]);
        block.extend([&n[..], name.as_bytes(), &v, value.as_bytes()].concat());
    }
    block
}

/// An HTTP/2 request on a new connection, then the response HEADERS and DATA frames as separate reads.
fn http2(headers: &str, body: &str, response: &str) -> Vec<Step> {
    let mut req = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    req.extend(h2_frame(0x4, 0, &[]));
    req.extend(h2_frame(0x1, 0x4, &hpack(headers)));
    req.extend(h2_frame(0x0, 0x1, body.as_bytes()));
    let status = h2_frame(0x1, 0x4, &hpack(":status: 200"));
    let data = h2_frame(0x0, 0x1, response.as_bytes());
    vec![Step::Write(req), Step::Read(status), Step::Read(data)]
}

/// What a case must reach (recorded with its name as model, failed, unparsed, not an LLM call),
/// so a clean result can't come from code that never ran.
enum Kind {
    Rec,
    Fail,
    Lost,
    Skip,
}

fn cases() -> Vec<(&'static str, Kind, Vec<Step>)> {
    use Kind::*;
    use fake::*;
    let json = |body: &str| resp("200 OK", "application/json", body);
    let sse = |body: &str| resp("200 OK", "text/event-stream", body);
    let prompt = format!("why does this fail? {OPENAI} {ANTHROPIC} {GEMINI} {AWS_KEY_ID}");
    let chat = format!(r#"{{"messages":[{{"role":"user","content":"{prompt}"}}]}}"#);
    let usage = r#""usage":{"prompt_tokens":11,"completion_tokens":4}"#;
    let completion = |model: &str| format!(r#"{{"model":"{model}","content":"{OPENAI}",{usage}}}"#);
    let ok = |model: &str| json(&completion(model));
    let auth = format!("Bearer {OPENAI}");
    let openai = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: api.openai.com\r\nAuthorization: {auth}\r\n\r\n{chat}"
    );
    let azure = format!(
        "POST /openai/deployments/gpt-4o/chat/completions?api-version=2024-10-21 HTTP/1.1\r\n\
         Host: myres.openai.azure.com\r\napi-key: {AZURE}\r\n\r\n{chat}"
    );
    let anthropic = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\nx-api-key: {ANTHROPIC}\r\n\r\n{chat}"
    );
    let anthropic_events = sse(&format!(
        "data: {{\"type\":\"message_start\",\"message\":{{\"model\":\"lc-anthropic\",\"usage\":{{\"input_tokens\":25}}}}}}\n\n\
         data: {{\"type\":\"content_block_delta\",\"delta\":{{\"text\":\"rotate {ANTHROPIC}\"}}}}\n\n\
         data: {{\"type\":\"message_delta\",\"usage\":{{\"output_tokens\":15}}}}\n\ndata: {{\"type\":\"message_stop\"}}\n\n"
    ));
    let gemini = |model: &str, stream: bool| {
        let method = ["generateContent?", "streamGenerateContent?alt=sse&"][usize::from(stream)];
        let req = format!(
            "POST /v1beta/models/gemini-2.5-flash:{method}key={GEMINI} HTTP/1.1\r\n\
             Host: generativelanguage.googleapis.com\r\nx-goog-api-key: {GEMINI}\r\n\r\n\
             {{\"contents\":[{{\"parts\":[{{\"text\":\"{prompt}\"}}]}}]}}"
        );
        let usage = format!(
            r#"{{"candidates":[{{"finishReason":"STOP"}}],"usageMetadata":{{"promptTokenCount":7,"candidatesTokenCount":3}},"modelVersion":"{model}"}}"#
        );
        let data = format!("data: {usage}\r\n\r\n");
        exchange(&req, &[json(&usage), sse(&data)][usize::from(stream)])
    };
    let converse = "POST /model/anthropic.claude-sonnet-5-5-v1:0/converse";
    let bedrock = format!(
        "Host: bedrock-runtime.us-east-1.amazonaws.com\r\n\r\n{{\"messages\":[{{\"content\":[{{\"text\":\"{prompt}\"}}]}}]}}"
    );
    let signed = format!(
        "{converse} HTTP/1.1\r\nAuthorization: AWS4-HMAC-SHA256 Credential={AWS_KEY_ID}/20261023/us-east-1/bedrock/aws4_request, \
         SignedHeaders=host;x-amz-date;x-amz-security-token, Signature={AWS_SIG}\r\nX-Amz-Security-Token: {AWS_TOKEN}\r\n{bedrock}"
    );
    let presig = format!(
        "{converse}?X-Amz-Credential={AWS_KEY_ID}%2F20261023%2Fus-east-1%2Fbedrock%2Faws4_request\
         &X-Amz-Security-Token={AWS_TOKEN}&X-Amz-Signature={AWS_SIG} HTTP/1.1\r\n{bedrock}"
    );
    let converse_usage = r#""usage":{"inputTokens":30,"outputTokens":9}"#;
    let bedrock_ok = |model: &str| json(&format!(r#"{{"model":"{model}",{converse_usage}}}"#));

    let h2_headers = format!(
        ":method: POST\n:path: /v1/chat/completions\n:authority: api.openai.com\nauthorization: {auth}"
    );
    let h2 = http2(&h2_headers, &chat, &completion("lc-h2"));

    let invalid = format!(r#"{{"error":{{"message":"Incorrect API key provided: {OPENAI}."}}}}"#);
    let invalid = resp("401 Unauthorized", "application/json", &invalid);
    let rejected = format!("rejected {ANTHROPIC}");
    let bad_gateway = resp("502 Bad Gateway", "text/html", &rejected);
    let broken_json = json(&format!(r#"{{"usage": {ANTHROPIC}"#));
    let partial =
        format!("data: {{\"type\":\"content_block_delta\",\"text\":\"{ANTHROPIC}\"}}\n\n");
    let oversized = sse(&partial.repeat(capture_limit(LlmDirection::Read) / partial.len() + 1));
    let then = |resp: &str, last: Step| [exchange(&anthropic, resp), vec![last]].concat();
    let next = Step::Write(anthropic.as_bytes().to_vec());

    let listed = json(r#"{"object":"list","data":[]}"#);
    let mut not_llm = Vec::new();
    for (line, host) in [
        ("GET /v1/models", "api.openai.com"),
        ("POST /v1/embeddings", "api.openai.com"),
        ("POST /v1/chat/completions", "llm.internal.example"),
    ] {
        let req = format!("{line} HTTP/1.1\r\nHost: {host}\r\nAuthorization: {auth}\r\n\r\n{chat}");
        not_llm.extend(exchange(&req, &listed));
    }

    vec![
        ("lc-openai", Rec, exchange(&openai, &ok("lc-openai"))),
        ("lc-anthropic", Rec, exchange(&anthropic, &anthropic_events)),
        ("lc-gemini-json", Rec, gemini("lc-gemini-json", false)),
        ("lc-gemini-sse", Rec, gemini("lc-gemini-sse", true)),
        ("lc-azure", Rec, exchange(&azure, &ok("lc-azure"))),
        ("lc-sigv4", Rec, exchange(&signed, &bedrock_ok("lc-sigv4"))),
        (
            "lc-presig",
            Rec,
            exchange(&presig, &bedrock_ok("lc-presig")),
        ),
        ("lc-h2", Rec, h2),
        ("401 echoing the key", Fail, exchange(&openai, &invalid)),
        ("502 without usage", Lost, then(&bad_gateway, next.clone())),
        ("broken json", Lost, then(&broken_json, next)),
        ("closed mid-stream", Lost, then(&sse(&partial), Step::Free)),
        ("read over 16 KB", Lost, exchange(&anthropic, &oversized)),
        ("not generation calls", Skip, not_llm),
    ]
}

#[test]
fn credentials_stay_out_of_logs_and_metrics() {
    let mut failures = Vec::new();
    for (name, kind, steps) in cases() {
        let (out, logs, metrics) = run(&steps);
        let (want, line, metric) = match kind {
            Kind::Rec => (Outcome::Recorded, "LLM SUCCESS", name),
            Kind::Fail => (Outcome::Recorded, "LLM FAILED", "llm_requests_total"),
            Kind::Lost => (Outcome::Unparsed, "LLM UNPARSED", "unparsed_total"),
            Kind::Skip => (Outcome::Pending, "", "llm_requests_total"),
        };
        let reached = match want {
            Outcome::Pending => out.iter().all(|o| *o == want),
            _ => out.contains(&want),
        };
        if !reached || !logs.contains(line) || !metrics.contains(metric) {
            failures.push(format!(
                "{name}: want {want:?}, {line:?}, {metric:?}; got {out:?}\n{logs}"
            ));
        }
        if let Some(secret) = fake::ALL.iter().find(|s| logs.contains(*s)) {
            failures.push(format!("{name}: logs contain {secret}:\n{logs}"));
        }
        if let Some(secret) = fake::ALL.iter().find(|s| metrics.contains(*s)) {
            failures.push(format!("{name}: exported metrics contain {secret}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
