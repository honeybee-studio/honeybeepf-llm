use std::time::Instant;

use honeybeepf_llm_common::capture_limit;
use honeybeepf_llm_core::types::{LlmDirection, UsageInfo};
use log::info;

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
    /// The call was counted as unparsed instead of recorded.
    Unparsed(UnparsedReason),
}

/// Why a call has no usage to record; a fixed set, so the metric label stays small.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnparsedReason {
    CaptureLost,
    NextRequest,
    ConnectionClosed,
    RequestTooLarge,
    ResponseTooLarge,
}

impl UnparsedReason {
    /// Value of the `reason` label on `llm_responses_unparsed_total`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CaptureLost => "capture_lost",
            Self::NextRequest => "next_request",
            Self::ConnectionClosed => "connection_closed",
            Self::RequestTooLarge => "request_too_large",
            Self::ResponseTooLarge => "response_too_large",
        }
    }
}

impl std::fmt::Display for UnparsedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CaptureLost => f.write_str("response bytes lost in capture"),
            Self::NextRequest => f.write_str("next request started before usage was found"),
            Self::ConnectionClosed => f.write_str("connection closed before usage was found"),
            Self::RequestTooLarge => write!(
                f,
                "request exceeded the {}MB buffer limit",
                MAX_REQUEST_BUFFER_SIZE / (1024 * 1024)
            ),
            Self::ResponseTooLarge => write!(
                f,
                "response exceeded the {}MB buffer limit",
                MAX_RESPONSE_BUFFER_SIZE / (1024 * 1024)
            ),
        }
    }
}

pub struct StreamProcessor {
    state: ProcessorState,
    write_buf: Vec<u8>,
    read_buf: Vec<u8>,
    /// What parsing already read of `read_buf`; cleared with it.
    read_progress: http::ResponseProgress,
    /// A response past the buffer limit, followed instead of kept in `read_buf`.
    past_limit: Option<Box<PastLimit>>,
    last_activity: Instant,
    /// Offset the next fragment of the SSL call in progress must have; 0 between calls.
    frag_next: u32,
    frag_dir: LlmDirection,
    /// A request head written before the HTTP/1.1 response ended: where it starts in `write_buf`, and when.
    next_request: Option<(usize, Instant)>,
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
            read_progress: http::ResponseProgress::default(),
            past_limit: None,
            last_activity: Instant::now(),
            frag_next: 0,
            frag_dir: LlmDirection::Unknown,
            next_request: None,
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

        let resync = offset == 0 && !gap && self.lost_http11_tail(direction, data);
        let reason = if gap {
            Some(UnparsedReason::CaptureLost)
        } else if direction == LlmDirection::Write && self.awaits_http11_response() {
            Some(UnparsedReason::NextRequest)
        } else if resync || (offset == 0 && self.lost_http11_head(direction, data)) {
            Some(UnparsedReason::CaptureLost)
        } else {
            None
        };
        let mut dropped = match reason {
            None => Outcome::Pending,
            Some(reason) => {
                #[cfg(feature = "k8s")]
                let dropped = self.drop_response(pid, reason, pod_info.as_ref());
                #[cfg(not(feature = "k8s"))]
                let dropped = self.drop_response(pid, reason);
                dropped
            }
        };
        if resync {
            self.resume_next_request(
                pid,
                #[cfg(feature = "k8s")]
                pod_info.as_ref(),
            );
        } else if offset == 0 && direction == LlmDirection::Write {
            let noted = self.note_next_request(
                data,
                pid,
                #[cfg(feature = "k8s")]
                pod_info.as_ref(),
            );
            if matches!(noted, Outcome::Unparsed(_)) {
                dropped = noted;
            }
        }
        if out_of_order && offset != 0 {
            return dropped;
        }

        #[cfg(feature = "k8s")]
        let fed = self.handle_event(direction, data, complete, pid, pod_info);
        #[cfg(not(feature = "k8s"))]
        let fed = self.handle_event(direction, data, complete, pid);
        if matches!(dropped, Outcome::Unparsed(_)) {
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
            UnparsedReason::ConnectionClosed,
            #[cfg(feature = "k8s")]
            pod_info,
        )
    }

    /// Whether a whole HTTP/1.1 response without usage is buffered, so the next Write starts a new request.
    fn awaits_http11_response(&self) -> bool {
        match &self.state {
            // A Write before that is a request body sent after `100 Continue`; a body past 16 MB is
            // long past it, and a lost read would otherwise let its walk run into the next response.
            ProcessorState::ProcessingResponse { parser, .. } => {
                self.past_limit.is_some()
                    || !parser.writes_during_response()
                        && http::Http11Parser::is_complete_response(&self.read_buf)
            }
            _ => false,
        }
    }

    /// Whether an HTTP/1.1 response is in progress, so a Write now is pipelined or follows a lost read.
    fn mid_http11_response(&self) -> bool {
        matches!(&self.state, ProcessorState::ProcessingResponse { parser, .. }
            if !parser.writes_during_response() && self.past_limit.is_none())
    }

    /// Whether a new status line answers the request written mid-response: the response's last reads were lost.
    fn lost_http11_tail(&self, direction: LlmDirection, data: &[u8]) -> bool {
        direction == LlmDirection::Read
            && self.next_request.is_some()
            && self.mid_http11_response()
            && http::Http11Parser::starts_with_response_head(skip_empty_lines(data))
    }

    /// Remember a request head written mid-response; one already remembered never got its response.
    fn note_next_request(
        &mut self,
        data: &[u8],
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> Outcome {
        if !self.mid_http11_response() || !http::Http11Parser::is_request_head(data) {
            return Outcome::Pending;
        }
        // The next status line answers the newest request, since clients do not pipeline.
        let skipped = self
            .next_request
            .replace((self.write_buf.len(), Instant::now()));
        if skipped.is_none() {
            return Outcome::Pending;
        }
        Self::count_unparsed(
            pid,
            UnparsedReason::CaptureLost,
            #[cfg(feature = "k8s")]
            pod_info,
        )
    }

    /// Start over from the request written mid-response, so this Read is parsed as its response.
    fn resume_next_request(
        &mut self,
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<crate::k8s::PodInfo>>,
    ) {
        let Some((at, start_time)) = self.next_request.take() else {
            return;
        };
        self.write_buf.drain(..at.min(self.write_buf.len()));
        self.read_buf.clear();
        self.read_progress = http::ResponseProgress::default();
        // Detected again, so a non-LLM request after the loss is skipped like any other.
        self.state = match self.detect(
            pid,
            #[cfg(feature = "k8s")]
            pod_info,
        ) {
            ProcessorState::ProcessingRequest { parser, .. } => {
                ProcessorState::ProcessingRequest { start_time, parser }
            }
            other => other,
        };
    }

    /// Whether the HTTP/1.1 response lost its status line before the ringbuf, so it can never be framed.
    fn lost_http11_head(&self, direction: LlmDirection, data: &[u8]) -> bool {
        const STATUS: &[u8] = b"HTTP/1.";
        match (&self.state, direction) {
            // A short first read may carry only part of the status line.
            (ProcessorState::ProcessingRequest { parser, .. }, LlmDirection::Read) => {
                let head = skip_empty_lines(data);
                !parser.writes_during_response()
                    && !STATUS.starts_with(&head[..head.len().min(STATUS.len())])
            }
            // The next request before any response byte: the whole response was lost.
            (ProcessorState::ProcessingRequest { parser, .. }, LlmDirection::Write) => {
                !parser.writes_during_response() && http::Http11Parser::is_request_head(data)
            }
            (ProcessorState::ProcessingResponse { parser, .. }, LlmDirection::Write) => {
                !parser.writes_during_response()
                    && !skip_empty_lines(&self.read_buf).starts_with(STATUS)
                    && http::Http11Parser::is_request_head(data)
            }
            _ => false,
        }
    }

    /// Count the response in progress as unparsed rather than record it as a 0-token failure.
    fn drop_response(
        &mut self,
        pid: u32,
        reason: UnparsedReason,
        #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> Outcome {
        if !self.is_llm() {
            return Outcome::Pending;
        }
        self.state = ProcessorState::Finished;
        Self::count_unparsed(
            pid,
            reason,
            #[cfg(feature = "k8s")]
            pod_info,
        )
    }

    /// Count one LLM call as unparsed, in the metric and the log.
    fn count_unparsed(
        pid: u32,
        reason: UnparsedReason,
        #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> Outcome {
        crate::telemetry::record_llm_unparsed(
            reason.as_str(),
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
        Outcome::Unparsed(reason)
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
            LlmDirection::Write => self.buffer_write(data),
            LlmDirection::Read => self.buffer_read(data),
            _ => Ok(false),
        };
        match buffered {
            Ok(true) => {}
            Ok(false) => return Outcome::Pending,
            Err(reason) => {
                let dropped = self.drop_response(
                    pid,
                    reason,
                    #[cfg(feature = "k8s")]
                    pod_info.as_ref(),
                );
                self.reset();
                return dropped;
            }
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
                if complete
                    && let Some(usage) = match &mut self.past_limit {
                        Some(followed) => followed.take_usage(),
                        None => parser.parse_response_from(&self.read_buf, &mut self.read_progress),
                    }
                {
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

    /// Append a request fragment; an error on overflow.
    fn buffer_write(&mut self, data: &[u8]) -> Result<bool, UnparsedReason> {
        if matches!(self.state, ProcessorState::Finished) {
            self.reset();
        }
        if self.write_buf.len() + data.len() > MAX_REQUEST_BUFFER_SIZE {
            return Err(UnparsedReason::RequestTooLarge);
        }
        self.write_buf.extend_from_slice(data);
        Ok(true)
    }

    /// Append a response fragment; false when no LLM request is waiting for it, an error on overflow.
    fn buffer_read(&mut self, data: &[u8]) -> Result<bool, UnparsedReason> {
        match self.state {
            // Ignore Read events after completion
            ProcessorState::Finished => return Ok(false),
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
                return Ok(false);
            }
            _ => {}
        }
        if self.past_limit.is_none() && self.read_buf.len() + data.len() > MAX_RESPONSE_BUFFER_SIZE
        {
            let followed =
                PastLimit::start(&self.read_buf).ok_or(UnparsedReason::ResponseTooLarge)?;
            self.past_limit = Some(Box::new(followed));
            self.read_buf = Vec::new();
            self.read_progress = http::ResponseProgress::default();
        }
        match &mut self.past_limit {
            Some(followed) => followed
                .feed(data)
                .then_some(true)
                .ok_or(UnparsedReason::ResponseTooLarge),
            None => {
                self.read_buf.extend_from_slice(data);
                Ok(true)
            }
        }
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
        self.read_progress = http::ResponseProgress::default();
        self.past_limit = None;
        self.next_request = None;
    }
}

/// How a response past the buffer limit is followed without keeping its body.
enum PastLimit {
    /// A plain JSON body has model near its start and usage at its end, so the middle can go.
    Json(http::JsonEnds),
    /// SSE usage is in its last events, read as they pass.
    Sse(http::SseStream),
}

impl PastLimit {
    fn start(buffer: &[u8]) -> Option<Self> {
        http::JsonEnds::start(buffer)
            .map(Self::Json)
            .or_else(|| http::SseStream::start(buffer).map(Self::Sse))
    }

    fn feed(&mut self, data: &[u8]) -> bool {
        match self {
            Self::Json(ends) => ends.feed(data),
            Self::Sse(stream) => stream.feed(data),
        }
    }

    fn take_usage(&mut self) -> Option<UsageInfo> {
        match self {
            Self::Json(ends) => ends.usage.take(),
            Self::Sse(stream) => stream.usage(),
        }
    }
}

/// The bytes after any empty lines, which httparse skips before a status line.
fn skip_empty_lines(buf: &[u8]) -> &[u8] {
    let start = buf
        .iter()
        .position(|b| !matches!(b, b'\r' | b'\n'))
        .unwrap_or(buf.len());
    &buf[start..]
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
mod tests;
