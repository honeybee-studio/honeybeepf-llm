use std::time::Instant;

use honeybeepf_llm_common::capture_limit;
use honeybeepf_llm_core::types::{LlmDirection, UsageInfo};
use log::{info, warn};

use crate::probes::builtin::llm::http::{self, ProtocolParser};

// Buffer size constants
const MAX_REQUEST_BUFFER_SIZE: usize = 8 * 1024 * 1024; // 8MB max for request (images, large context)
pub(crate) const MAX_RESPONSE_BUFFER_SIZE: usize = 16 * 1024 * 1024; // 16MB max for response (streaming)
const DETECTION_BUFFER_THRESHOLD: usize = 4096; // Give up detection after 4KB

/// State definition: lifecycle of an LLM connection
enum ProcessorState {
    /// Initial state: detecting protocol and request
    Detecting,
    /// Buffering request body
    ProcessingRequest {
        start_time: Instant,
        parser: Box<dyn ProtocolParser>,
    },
    /// Request finished, buffering response
    ProcessingResponse {
        start_time: Instant,
        parser: Box<dyn ProtocolParser>,
    },
    /// Finished or Invalid
    Finished,
}

#[cfg(feature = "k8s")]
fn fmt_pod(pod_info: &Option<std::sync::Arc<crate::k8s::PodInfo>>) -> String {
    pod_info
        .as_ref()
        .map(|p| format!(" | Pod: {}/{}", p.namespace, p.pod_name))
        .unwrap_or_default()
}

/// What one fragment led to.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Recorded,
    /// The response lost bytes in capture, so it was counted as unparsed instead of recorded.
    Unparsed,
}

pub struct StreamProcessor {
    state: ProcessorState,
    write_buf: Vec<u8>,
    read_buf: Vec<u8>,
    last_activity: Instant,
    /// Offset the next fragment of the SSL call in progress must have; 0 between calls.
    frag_next: u32,
    frag_dir: LlmDirection,
}

impl Default for StreamProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamProcessor {
    pub fn new() -> Self {
        Self {
            state: ProcessorState::Detecting,
            // One per connection: most never carry an LLM call, so allocate on first use.
            write_buf: Vec::new(),
            read_buf: Vec::new(),
            last_activity: Instant::now(),
            frag_next: 0,
            frag_dir: LlmDirection::Unknown,
        }
    }

    pub fn last_activity(&self) -> Instant {
        self.last_activity
    }

    pub fn is_llm(&self) -> bool {
        !matches!(
            self.state,
            ProcessorState::Detecting | ProcessorState::Finished
        )
    }

    /// Feed one ringbuf fragment: `data` starts at `offset` within the SSL call's `len` bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_fragment(
        &mut self,
        direction: LlmDirection,
        data: &[u8],
        offset: u32,
        len: u32,
        truncated: bool,
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> Outcome {
        let end = offset.saturating_add(data.len() as u32);
        let complete = end as usize >= (len as usize).min(capture_limit(direction));
        // The kernel sends a call's fragments in order and stops at the first with no ringbuf room.
        let out_of_order = offset != self.frag_next;
        let lost_tail = out_of_order && offset == 0 && self.frag_dir == LlmDirection::Read;
        let lost_here =
            (out_of_order && offset != 0 || truncated) && direction == LlmDirection::Read;
        let gap = lost_tail || lost_here;
        self.frag_next = if complete { 0 } else { end };
        self.frag_dir = direction;

        let reason = if gap {
            Some("response bytes lost in capture")
        } else if direction == LlmDirection::Write && self.awaits_http11_response() {
            Some("next request started before usage was found")
        } else {
            None
        };
        let dropped = match reason {
            None => Outcome::Pending,
            Some(reason) => {
                #[cfg(feature = "k8s")]
                let dropped = self.drop_response(pid, reason, pod_info.as_ref());
                #[cfg(not(feature = "k8s"))]
                let dropped = self.drop_response(pid, reason);
                dropped
            }
        };
        if out_of_order && offset != 0 {
            return dropped;
        }

        #[cfg(feature = "k8s")]
        let fed = self.handle_event(direction, data, complete, pid, pod_info);
        #[cfg(not(feature = "k8s"))]
        let fed = self.handle_event(direction, data, complete, pid);
        if dropped == Outcome::Unparsed {
            dropped
        } else {
            fed
        }
    }

    /// End of the connection: an LLM call still without usage (request or response) is counted as unparsed.
    pub fn close(
        &mut self,
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> Outcome {
        self.drop_response(
            pid,
            "connection closed before usage was found",
            #[cfg(feature = "k8s")]
            pod_info,
        )
    }

    /// Whether a whole HTTP/1.1 response without usage is buffered, so the next Write starts a new request.
    fn awaits_http11_response(&self) -> bool {
        match &self.state {
            // A Write before that is a request body sent after `100 Continue`.
            ProcessorState::ProcessingResponse { parser, .. } => {
                !parser.writes_during_response()
                    && http::Http11Parser::is_complete_response(&self.read_buf)
            }
            _ => false,
        }
    }

    /// Count the response in progress as unparsed rather than record it as a 0-token failure.
    fn drop_response(
        &mut self,
        pid: u32,
        reason: &str,
        #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> Outcome {
        if !self.is_llm() {
            return Outcome::Pending;
        }
        crate::telemetry::record_llm_unparsed(
            #[cfg(feature = "k8s")]
            pod_info,
        );

        #[cfg(feature = "k8s")]
        let pod_str = fmt_pod(&pod_info.cloned());
        #[cfg(not(feature = "k8s"))]
        let pod_str = "";

        info!(
            "LLM UNPARSED | PID: {} | {}, not recorded{}",
            pid, reason, pod_str
        );
        self.state = ProcessorState::Finished;
        Outcome::Unparsed
    }

    fn handle_event(
        &mut self,
        direction: LlmDirection,
        data: &[u8],
        complete: bool,
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> Outcome {
        self.last_activity = Instant::now();

        let buffered = match direction {
            LlmDirection::Write => self.buffer_write(data, pid),
            LlmDirection::Read => self.buffer_read(data, pid),
            _ => false,
        };
        if !buffered {
            return Outcome::Pending;
        }

        // Take ownership of current state to enable transitions
        match std::mem::replace(&mut self.state, ProcessorState::Finished) {
            ProcessorState::Detecting => {
                self.state = self.detect(
                    pid,
                    #[cfg(feature = "k8s")]
                    pod_info.as_ref(),
                );
            }
            ProcessorState::ProcessingRequest { start_time, parser }
                if direction != LlmDirection::Read =>
            {
                self.state = ProcessorState::ProcessingRequest { start_time, parser };
            }
            // The first Read ends the request and may already hold the whole response.
            ProcessorState::ProcessingRequest { start_time, parser }
            | ProcessorState::ProcessingResponse { start_time, parser } => {
                // Parse once the SSL call's last fragment is in, not on a partial record
                if complete && let Some(usage) = parser.parse_response(&self.read_buf) {
                    record_usage(
                        &usage,
                        start_time,
                        pid,
                        #[cfg(feature = "k8s")]
                        pod_info,
                    );
                    return Outcome::Recorded;
                }
                self.state = ProcessorState::ProcessingResponse { start_time, parser };
            }
            ProcessorState::Finished => {}
        }
        Outcome::Pending
    }

    /// Append a request fragment; false when the stream was discarded instead.
    fn buffer_write(&mut self, data: &[u8], pid: u32) -> bool {
        if matches!(self.state, ProcessorState::Finished) {
            self.reset();
        }
        if self.write_buf.len() + data.len() > MAX_REQUEST_BUFFER_SIZE {
            warn!(
                "LLM request buffer exceeded {}MB limit (PID: {}), discarding stream",
                MAX_REQUEST_BUFFER_SIZE / (1024 * 1024),
                pid
            );
            self.reset();
            return false;
        }
        self.write_buf.extend_from_slice(data);
        true
    }

    /// Append a response fragment; false when no LLM request is waiting for it.
    fn buffer_read(&mut self, data: &[u8], pid: u32) -> bool {
        match self.state {
            // Ignore Read events after completion
            ProcessorState::Finished => return false,
            // Don't buffer responses if we haven't detected a valid LLM request yet
            ProcessorState::Detecting => {
                // The server is answering a non-LLM request; the next request must not sit behind it.
                // An HTTP/1.x status line also covers upload body writes left over after detection gave up.
                if !self.write_buf.is_empty()
                    && (data.starts_with(b"HTTP/1.")
                        || http::Http11Parser::is_request_head(&self.write_buf))
                {
                    self.write_buf.clear();
                }
                return false;
            }
            _ => {}
        }
        if self.read_buf.len() + data.len() > MAX_RESPONSE_BUFFER_SIZE {
            warn!(
                "LLM response buffer exceeded {}MB limit (PID: {}), discarding stream",
                MAX_RESPONSE_BUFFER_SIZE / (1024 * 1024),
                pid
            );
            self.reset();
            return false;
        }
        self.read_buf.extend_from_slice(data);
        true
    }

    /// Next state for the request bytes buffered so far.
    fn detect(
        &self,
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> ProcessorState {
        let (version, path, parser): (&str, String, Box<dyn ProtocolParser>) =
            if let Some(path) = http::Http11Parser.detect_request(&self.write_buf) {
                ("HTTP/1.1", path, Box::new(http::Http11Parser))
            } else if let Some(path) = http::Http2Parser.detect_request(&self.write_buf) {
                ("HTTP/2", path, Box::new(http::Http2Parser))
            } else if self.write_buf.len() > DETECTION_BUFFER_THRESHOLD {
                // Buffer too large and still not detected -> likely not LLM
                return ProcessorState::Finished;
            } else {
                return ProcessorState::Detecting;
            };

        #[cfg(feature = "k8s")]
        let pod_str = fmt_pod(&pod_info.cloned());
        #[cfg(not(feature = "k8s"))]
        let pod_str = "";

        info!(
            "[LLM] Detected {}: {} (PID: {}){}",
            version, path, pid, pod_str
        );
        ProcessorState::ProcessingRequest {
            start_time: Instant::now(),
            parser,
        }
    }

    fn reset(&mut self) {
        self.state = ProcessorState::Detecting;
        self.write_buf.clear();
        self.read_buf.clear();
    }
}

/// Export one finished call's usage and log it.
fn record_usage(
    usage: &UsageInfo,
    start_time: Instant,
    pid: u32,
    #[cfg(feature = "k8s")] pod_info: Option<std::sync::Arc<crate::k8s::PodInfo>>,
) {
    let latency = start_time.elapsed();
    let latency_secs = latency.as_secs_f64();
    let model_str = usage.model.as_deref().unwrap_or("unknown");
    let is_error = usage.prompt_tokens == 0 && usage.completion_tokens == 0;

    crate::telemetry::record_llm_request(
        usage,
        model_str,
        latency_secs,
        is_error,
        #[cfg(feature = "k8s")]
        pod_info.as_ref(),
    );

    #[cfg(feature = "k8s")]
    let pod_str = fmt_pod(&pod_info);
    #[cfg(not(feature = "k8s"))]
    let pod_str = "";

    if is_error {
        info!(
            "LLM FAILED/ERROR | PID: {} | Model: {} | Latency: {:.2}s{}",
            pid,
            model_str,
            latency.as_secs_f64(),
            pod_str
        );
    } else {
        let thoughts_str = usage
            .thoughts_tokens
            .map(|t| format!(", Thoughts: {}", t))
            .unwrap_or_default();
        let cache_str = match (usage.cache_read_tokens, usage.cache_write_tokens) {
            (None, None) => String::new(),
            (read, write) => format!(
                ", Cache read: {}, Cache write: {}",
                read.unwrap_or(0),
                write.unwrap_or(0)
            ),
        };
        info!(
            "LLM SUCCESS | PID: {} | Model: {} | Latency: {:.2}s | Tokens: {} (Prompt: {}, Compl: {}{}{}){}",
            pid,
            model_str,
            latency.as_secs_f64(),
            usage.prompt_tokens.saturating_add(usage.completion_tokens),
            usage.prompt_tokens,
            usage.completion_tokens,
            thoughts_str,
            cache_str,
            pod_str
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use honeybeepf_llm_common::{MAX_SSL_BUF_SIZE, MAX_SSL_FRAGMENTS};

    const REQUEST: &[u8] = b"POST /v1/chat/completions HTTP/1.1\r\nHost: api.openai.com\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n{\"model\":\"t\",\"messages\":[]}";

    /// Sends one SSL call the way the kernel does, leaving out the fragment at `skip`.
    fn send(
        p: &mut StreamProcessor,
        dir: LlmDirection,
        call: &[u8],
        skip: Option<usize>,
    ) -> Vec<Outcome> {
        let limit = capture_limit(dir);
        let truncated = call.len() > limit;
        let mut out = Vec::new();
        for i in 0..MAX_SSL_FRAGMENTS {
            let offset = i * MAX_SSL_BUF_SIZE;
            if i > 0 && (offset >= call.len() || offset >= limit) {
                break;
            }
            if skip == Some(i) {
                continue;
            }
            let data = &call[offset..call.len().min(offset + MAX_SSL_BUF_SIZE)];
            out.push(p.handle_fragment(
                dir,
                data,
                offset as u32,
                call.len() as u32,
                truncated,
                1,
                #[cfg(feature = "k8s")]
                None,
            ));
        }
        out
    }

    /// Response head plus body, split into the 16 KB TLS records a client reads one at a time.
    fn h1_response(body_len: usize) -> Vec<Vec<u8>> {
        let pad = "x".repeat(body_len.saturating_sub(200));
        let body = format!(
            r#"{{"id": "c1", "object": "chat.completion", "model": "gpt-4o", "choices": [{{"index": 0, "message": {{"role": "assistant", "content": "{pad}"}}, "finish_reason": "stop"}}], "usage": {{"prompt_tokens": 11, "completion_tokens": 4, "total_tokens": 15}}}}"#
        );
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut reads = vec![head.into_bytes()];
        reads.extend(body.as_bytes().chunks(16384).map(<[u8]>::to_vec));
        reads
    }

    fn outcomes(
        p: &mut StreamProcessor,
        reads: &[Vec<u8>],
        skip: Option<(usize, usize)>,
    ) -> Vec<Outcome> {
        let mut out = send(p, LlmDirection::Write, REQUEST, None);
        for (n, r) in reads.iter().enumerate() {
            let skip = skip.filter(|(read, _)| *read == n).map(|(_, frag)| frag);
            out.extend(send(p, LlmDirection::Read, r, skip));
        }
        out
    }

    #[test]
    fn test_response_with_last_record_over_4kb_is_recorded_once() {
        // 40 KB body: records of 16384, 16384 and ~7.5 KB; usage sits in the last one.
        for body_len in [6000, 16000, 40237] {
            let mut p = StreamProcessor::new();
            let out = outcomes(&mut p, &h1_response(body_len), None);
            let recorded = out.iter().filter(|o| **o == Outcome::Recorded).count();
            assert_eq!(
                (recorded, out.last()),
                (1, Some(&Outcome::Recorded)),
                "{out:?}"
            );
        }
    }

    #[test]
    fn test_lost_fragment_counts_response_as_unparsed() {
        // 20 KB body: a lost middle fragment falls inside the content string, so parsing could
        // still "succeed"; a lost tail fragment only shows when the next call starts at offset 0.
        for lost in [1, 3] {
            let mut p = StreamProcessor::new();
            let out = outcomes(&mut p, &h1_response(20200), Some((1, lost)));
            let unparsed = out.iter().filter(|o| **o == Outcome::Unparsed).count();
            assert_eq!(unparsed, 1, "fragment {lost}: {out:?}");
            assert!(
                !out.contains(&Outcome::Recorded),
                "fragment {lost}: {out:?}"
            );

            let next = outcomes(&mut p, &h1_response(3000), None);
            assert_eq!(next.last(), Some(&Outcome::Recorded), "{next:?}");
        }
    }

    #[test]
    fn test_read_over_capture_limit_is_unparsed() {
        let mut p = StreamProcessor::new();
        let mut reads = h1_response(40237);
        // One SSL_read returning more than one TLS record.
        let joined = reads.drain(1..).flatten().collect::<Vec<u8>>();
        reads.push(joined);
        let out = outcomes(&mut p, &reads, None);
        assert!(out.contains(&Outcome::Unparsed), "{out:?}");
        assert!(!out.contains(&Outcome::Recorded), "{out:?}");
    }

    #[test]
    fn test_request_over_capture_limit_still_detected() {
        // Large prompts (images) arrive as one SSL_write; only the head is needed for detection.
        let mut p = StreamProcessor::new();
        let mut big = REQUEST.to_vec();
        big.extend(std::iter::repeat_n(b' ', 30000));
        let mut out = send(&mut p, LlmDirection::Write, &big, None);
        for r in &h1_response(3000) {
            out.extend(send(&mut p, LlmDirection::Read, r, None));
        }
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    const MESSAGES_REQUEST: &[u8] = b"POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"claude-opus-5-5\",\"max_tokens\":64,\"messages\":[{\"role\":\"user\",\"content\":\"Hello, Claude\"}]}";
    const MESSAGES_RESPONSE: &str = r#"{"id":"msg_1","type":"message","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":14,"output_tokens":36}}"#;

    fn h1(content_type: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// One request and its response on the same stream; the response head and body are read separately.
    fn exchange(p: &mut StreamProcessor, request: &[u8], response: &[u8]) -> Vec<Outcome> {
        let mut out = send(p, LlmDirection::Write, request, None);
        let split = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map_or(response.len(), |i| i + 4);
        let (head, body) = response.split_at(split);
        out.extend(send(p, LlmDirection::Read, head, None));
        if !body.is_empty() {
            out.extend(send(p, LlmDirection::Read, body, None));
        }
        out
    }

    fn count(out: &[Outcome], want: Outcome) -> usize {
        out.iter().filter(|o| **o == want).count()
    }

    #[test]
    fn test_messages_after_count_tokens_is_recorded() {
        let mut p = StreamProcessor::new();
        let count_tokens = b"POST /v1/messages/count_tokens HTTP/1.1\r\nHost: api.anthropic.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"claude-opus-5-5\",\"messages\":[{\"role\":\"user\",\"content\":\"Hello, Claude\"}]}";
        let mut out = exchange(
            &mut p,
            count_tokens,
            &h1("application/json", r#"{"input_tokens":14}"#),
        );
        out.extend(exchange(
            &mut p,
            MESSAGES_REQUEST,
            &h1("application/json", MESSAGES_RESPONSE),
        ));
        assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
        assert_eq!(count(&out, Outcome::Unparsed), 0, "{out:?}");
    }

    #[test]
    fn test_chat_after_non_llm_request_is_recorded() {
        let mut p = StreamProcessor::new();
        let embeddings = b"POST /v1/embeddings HTTP/1.1\r\nHost: api.openai.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"text-embedding-3-small\",\"input\":\"hi\"}";
        let mut out = exchange(
            &mut p,
            embeddings,
            &h1(
                "application/json",
                r#"{"object":"list","data":[],"usage":{"prompt_tokens":1,"total_tokens":1}}"#,
            ),
        );
        let reads = h1_response(3000);
        out.extend(outcomes(&mut p, &reads, None));
        assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    /// A multipart upload whose last body writes land after detection gave up at 4 KB, as httpx sends it.
    fn upload(p: &mut StreamProcessor, expect_continue: bool) -> Vec<Outcome> {
        let head = format!(
            "POST /v1/audio/transcriptions HTTP/1.1\r\nHost: api.openai.com\r\n\
             Content-Type: multipart/form-data; boundary=b0\r\nContent-Length: 66614\r\n{}\r\n",
            if expect_continue {
                "Expect: 100-continue\r\n"
            } else {
                ""
            }
        );
        let mut out = send(p, LlmDirection::Write, head.as_bytes(), None);
        if expect_continue {
            out.extend(send(
                p,
                LlmDirection::Read,
                b"HTTP/1.1 100 Continue\r\n\r\n",
                None,
            ));
        }
        let part =
            b"--b0\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\r\n";
        for body in [&part[..], &[0xAB; 65536], &[0xCD; 1000], b"\r\n--b0--\r\n"] {
            out.extend(send(p, LlmDirection::Write, body, None));
        }
        out.extend(send(
            p,
            LlmDirection::Read,
            &h1("application/json", r#"{"text":"hello"}"#),
            None,
        ));
        out
    }

    #[test]
    fn test_calls_after_upload_are_recorded() {
        use std::io::Write as _;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(MESSAGES_RESPONSE.as_bytes()).expect("gzip");
        let body = gz.finish().expect("gzip");
        let gzip = [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\n\
                 Content-Length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
            &body,
        ]
        .concat();
        for expect_continue in [false, true] {
            let mut p = StreamProcessor::new();
            let mut out = upload(&mut p, expect_continue);
            for response in [&gzip, &h1("application/json", MESSAGES_RESPONSE)] {
                out.extend(exchange(&mut p, MESSAGES_REQUEST, response));
            }
            assert_eq!(
                count(&out, Outcome::Recorded),
                2,
                "{expect_continue}: {out:?}"
            );
            assert_eq!(
                count(&out, Outcome::Unparsed),
                0,
                "{expect_continue}: {out:?}"
            );
        }
    }

    #[test]
    fn test_stored_completion_retrieval_is_not_recorded() {
        let mut p = StreamProcessor::new();
        let mut out = outcomes(&mut p, &h1_response(3000), None);
        let retrieve =
            b"GET /v1/chat/completions/chatcmpl-abc123 HTTP/1.1\r\nHost: api.openai.com\r\n\r\n";
        let stored = r#"{"object":"chat.completion","id":"chatcmpl-abc123","model":"gpt-6-astra","choices":[],"usage":{"total_tokens":31,"completion_tokens":18,"prompt_tokens":13}}"#;
        out.extend(exchange(&mut p, retrieve, &h1("application/json", stored)));
        assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
    }

    #[test]
    fn test_response_without_usage_is_unparsed_when_next_request_starts() {
        let mut p = StreamProcessor::new();
        let bad_gateway = b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/html\r\nContent-Length: 34\r\n\r\n<html><body>502 Bad Gateway</body>";
        let mut out = exchange(&mut p, MESSAGES_REQUEST, bad_gateway);
        out.extend(exchange(
            &mut p,
            MESSAGES_REQUEST,
            &h1("application/json", MESSAGES_RESPONSE),
        ));
        assert_eq!(count(&out, Outcome::Unparsed), 1, "{out:?}");
        assert_eq!(count(&out, Outcome::Recorded), 1, "{out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    #[test]
    fn test_body_after_100_continue_is_recorded() {
        let mut p = StreamProcessor::new();
        let (head, body) = REQUEST.split_at(REQUEST.len() - 27);
        let mut out = send(&mut p, LlmDirection::Write, head, None);
        out.extend(send(
            &mut p,
            LlmDirection::Read,
            b"HTTP/1.1 100 Continue\r\n\r\n",
            None,
        ));
        out.extend(send(&mut p, LlmDirection::Write, body, None));
        for r in &h1_response(3000) {
            out.extend(send(&mut p, LlmDirection::Read, r, None));
        }
        assert_eq!(count(&out, Outcome::Unparsed), 0, "{out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    #[test]
    fn test_write_before_whole_response_keeps_streaming_response() {
        let mut p = StreamProcessor::new();
        let mut out = send(&mut p, LlmDirection::Write, REQUEST, None);
        for r in [
            &b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n"[..],
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        ] {
            out.extend(send(&mut p, LlmDirection::Read, r, None));
        }
        out.extend(send(&mut p, LlmDirection::Write, MESSAGES_REQUEST, None));
        let tail = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                     data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n\
                     data: [DONE]\n\n";
        out.extend(send(&mut p, LlmDirection::Read, tail, None));
        assert_eq!(count(&out, Outcome::Unparsed), 0, "{out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    #[test]
    fn test_unparsed_response_after_100_continue_frees_next_request() {
        let mut p = StreamProcessor::new();
        let (head, body) = REQUEST.split_at(REQUEST.len() - 27);
        let mut out = send(&mut p, LlmDirection::Write, head, None);
        out.extend(send(
            &mut p,
            LlmDirection::Read,
            b"HTTP/1.1 100 Continue\r\n\r\n",
            None,
        ));
        out.extend(send(&mut p, LlmDirection::Write, body, None));
        let event =
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let chunked = format!("{:x}\r\n{event}\r\n0\r\n\r\n", event.len());
        for r in [
            &b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
            chunked.as_bytes(),
        ] {
            out.extend(send(&mut p, LlmDirection::Read, r, None));
        }
        out.extend(exchange(
            &mut p,
            MESSAGES_REQUEST,
            &h1("application/json", MESSAGES_RESPONSE),
        ));
        assert_eq!(count(&out, Outcome::Unparsed), 1, "{out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    #[test]
    fn test_streamed_response_after_100_continue_is_recorded() {
        let mut p = StreamProcessor::new();
        let (head, body) = REQUEST.split_at(REQUEST.len() - 27);
        let mut out = send(&mut p, LlmDirection::Write, head, None);
        out.extend(send(
            &mut p,
            LlmDirection::Read,
            b"HTTP/1.1 100 Continue\r\n\r\n",
            None,
        ));
        out.extend(send(&mut p, LlmDirection::Write, body, None));
        let event = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n";
        let chunked = format!("{:x}\r\n{event}\r\n0\r\n\r\n", event.len());
        for r in [
            &b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
            chunked.as_bytes(),
        ] {
            out.extend(send(&mut p, LlmDirection::Read, r, None));
        }
        assert_eq!(count(&out, Outcome::Unparsed), 0, "{out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    #[test]
    fn test_whole_response_in_one_read_is_recorded() {
        let sse = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n\
                    data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n\
                    data: [DONE]\n\n";
        for response in [h1("application/json", MESSAGES_RESPONSE), sse.to_vec()] {
            let mut p = StreamProcessor::new();
            let mut out = Vec::new();
            for _ in 0..2 {
                out.extend(send(&mut p, LlmDirection::Write, MESSAGES_REQUEST, None));
                out.extend(send(&mut p, LlmDirection::Read, &response, None));
            }
            assert_eq!(
                out,
                [
                    Outcome::Pending,
                    Outcome::Recorded,
                    Outcome::Pending,
                    Outcome::Recorded
                ]
            );
        }
    }

    #[test]
    fn test_head_with_part_of_body_in_one_read_is_recorded_once() {
        let response = h1("application/json", MESSAGES_RESPONSE);
        let (first, rest) = response.split_at(response.len() - 20);
        let mut p = StreamProcessor::new();
        let mut out = send(&mut p, LlmDirection::Write, MESSAGES_REQUEST, None);
        out.extend(send(&mut p, LlmDirection::Read, first, None));
        out.extend(send(&mut p, LlmDirection::Read, rest, None));
        assert_eq!(out, [Outcome::Pending, Outcome::Pending, Outcome::Recorded]);
    }

    #[test]
    fn test_chunked_response_with_trailers_frees_next_request() {
        let mut p = StreamProcessor::new();
        let error = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\n\
                      Transfer-Encoding: chunked\r\nTrailer: X-Request-Id\r\n\r\n\
                      5\r\noops!\r\n0\r\nX-Request-Id: r1\r\n\r\n";
        let mut out = exchange(&mut p, MESSAGES_REQUEST, error);
        out.extend(exchange(
            &mut p,
            MESSAGES_REQUEST,
            &h1("application/json", MESSAGES_RESPONSE),
        ));
        assert_eq!(count(&out, Outcome::Unparsed), 1, "{out:?}");
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    #[test]
    fn test_chunk_larger_than_response_buffer_frees_next_request() {
        for size in ["2000000", "ffffffffffffffff"] {
            let mut p = StreamProcessor::new();
            let garbled = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n\
                 {size}\r\n{{}}\r\n0\r\n\r\n"
            );
            let mut out = exchange(&mut p, MESSAGES_REQUEST, garbled.as_bytes());
            out.extend(exchange(
                &mut p,
                MESSAGES_REQUEST,
                &h1("application/json", MESSAGES_RESPONSE),
            ));
            assert_eq!(count(&out, Outcome::Unparsed), 1, "{size}: {out:?}");
            assert_eq!(out.last(), Some(&Outcome::Recorded), "{size}: {out:?}");
        }
    }

    thread_local! {
        static LOGS: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }

    /// Keeps every record at every level, per thread so parallel tests don't mix their output.
    struct CaptureLogger;

    impl log::Log for CaptureLogger {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }

        fn log(&self, r: &log::Record) {
            let line = format!("{} {}\n", r.level(), r.args());
            LOGS.with(|logs| logs.borrow_mut().push_str(&line));
        }

        fn flush(&self) {}
    }

    static LOGGER: CaptureLogger = CaptureLogger;

    #[test]
    fn test_credentials_stay_out_of_logs() {
        log::set_logger(&LOGGER).expect("no other logger in the test binary");
        log::set_max_level(log::LevelFilter::Trace);
        // Fake keys carry HBTEST; the query and header forms are how Gemini and SigV4 send them.
        let gemini = "AIzaSyHBTEST0gemini0000000000000000000";
        let presigned = "X-Amz-Credential=AKIAHBTEST0000000000%2F20261023%2Fus-east-1%2Fbedrock%2Faws4_request\
                         &X-Amz-Security-Token=HBTEST0token&X-Amz-Signature=HBTEST0signature";
        let openai = "sk-proj-HBTEST0openai00000000000000000000000000";
        let requests = [
            format!(
                "POST /v1beta/models/gemini-2.5-flash:generateContent?key={gemini} HTTP/1.1\r\n\
                     Host: generativelanguage.googleapis.com\r\nx-goog-api-key: {gemini}\r\n\r\n{{}}"
            ),
            format!(
                "POST /model/anthropic.claude-sonnet-5-5-v1:0/converse?{presigned} HTTP/1.1\r\n\
                     Host: bedrock-runtime.us-east-1.amazonaws.com\r\n\r\n{{}}"
            ),
            format!(
                "POST /v1/chat/completions HTTP/1.1\r\nHost: api.openai.com\r\nAuthorization: Bearer {openai}\r\n\r\n\
                     {{\"messages\":[{{\"role\":\"user\",\"content\":\"{openai}\"}}]}}"
            ),
        ];
        let response = h1(
            "application/json",
            r#"{"model":"m","usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
        );
        for request in &requests {
            LOGS.with(|logs| logs.borrow_mut().clear());
            let out = exchange(&mut StreamProcessor::new(), request.as_bytes(), &response);
            let logs = LOGS.with(|logs| logs.take());
            assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
            assert!(logs.contains("[LLM] Detected"), "{logs}");
            assert!(!logs.contains("HBTEST"), "{logs}");
        }
    }
}
