use std::time::Instant;

use honeybeepf_llm_common::capture_limit;
use honeybeepf_llm_core::types::{LlmDirection, UsageInfo};
use log::info;

use crate::probes::builtin::llm::background::BackgroundCalls;
use crate::probes::builtin::llm::http::{self, ProtocolParser};
use crate::telemetry::Transport;

mod background;
mod h2_route;
pub(crate) use self::background::give_up;

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
    /// An HTTP/2 connection: frames demultiplexed into streams, recorded per stream.
    Http2(Box<http::H2Conn>),
    /// Finished or Invalid
    Finished,
}

#[cfg(feature = "k8s")]
fn fmt_pod(pod_info: Option<&std::sync::Arc<crate::k8s::PodInfo>>) -> String {
    pod_info
        .map(|p| format!(" | Pod: {}/{}", p.namespace, p.pod_name))
        .unwrap_or_default()
}

/// The identity a recorded call is attributed to: the process, and with k8s its pod. Passed by
/// reference so the one place that owns it (the ringbuf handler) reads `/proc` at most once per event.
pub struct CallContext {
    pub(crate) pid: u32,
    #[cfg(feature = "k8s")]
    pub(crate) pod_info: Option<std::sync::Arc<crate::k8s::PodInfo>>,
}

/// What one fragment led to.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Recorded,
    /// A background call was queued; its result is recorded from a later poll.
    Deferred,
    /// The call was counted as unparsed instead of recorded.
    Unparsed(UnparsedReason),
}

/// What a captured gap means for an HTTP/2 direction's next feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum H2Gap {
    /// No gap; read the bytes as a continuation.
    None,
    /// A known count of lost bytes to skip inside the current frame before reading; if they do not fall
    /// within it, the reader re-anchors instead.
    Skip(usize),
    /// A gap of unknown size or structure; re-anchor the reader.
    Resync,
}

/// Why a call has no usage to record; a fixed set, so the metric label stays small.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnparsedReason {
    CaptureLost,
    NextRequest,
    ConnectionClosed,
    RequestTooLarge,
    ResponseTooLarge,
    BackgroundUnfinished,
    HeadersLost,
    UsageAbsent,
    StreamError,
    AgentSession,
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
            Self::BackgroundUnfinished => "background_unfinished",
            Self::HeadersLost => "headers_lost",
            Self::UsageAbsent => "usage_absent",
            Self::StreamError => "stream_error",
            Self::AgentSession => "agent_session",
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
            Self::BackgroundUnfinished => f.write_str("background call's usage was never seen"),
            Self::HeadersLost => {
                f.write_str("HTTP/2 request headers were lost, so the stream could not be matched")
            }
            Self::UsageAbsent => f.write_str("response completed with no usage to record"),
            Self::StreamError => f.write_str("response stream ended on a provider error event"),
            Self::AgentSession => {
                f.write_str("Managed Agents session call; its usage is a running total, not read")
            }
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
    /// Response id of the background call this request polls.
    continuation: Option<Box<str>>,
    /// A captured gap pending for an HTTP/2 direction (client=Write, server=Read), applied on its next feed.
    h2_gap_client: H2Gap,
    h2_gap_server: H2Gap,
    /// The detected request is a record-only provider (Managed Agents session), so its response is
    /// counted under `agent_session` without reading the running session total.
    record_only: bool,
    /// Capture transport of this connection; a metric label on the calls it records.
    transport: Transport,
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
            continuation: None,
            h2_gap_client: H2Gap::None,
            h2_gap_server: H2Gap::None,
            record_only: false,
            transport: Transport::Tls,
        }
    }

    /// Set the capture transport; the plaintext reader calls this, the SSL path keeps the Tls default.
    pub(super) fn set_transport(&mut self, transport: Transport) {
        self.transport = transport;
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
        background: &mut BackgroundCalls,
        ctx: &CallContext,
    ) -> Outcome {
        let end = offset.saturating_add(data.len() as u32);
        let complete = end as usize >= (len as usize).min(capture_limit(direction));
        // The kernel sends a call's fragments in order and stops at the first with no ringbuf room.
        let out_of_order = offset != self.frag_next;

        // HTTP/2 is a connection-level path: once the client preface is seen, every fragment feeds the
        // frame readers and the HTTP/1.1 state machine below is left untouched. A gap in a direction makes
        // the connection re-anchor that direction's frame reader before the bytes are fed.
        if matches!(self.state, ProcessorState::Http2(_))
            || matches!(self.state, ProcessorState::Detecting)
                && direction == LlmDirection::Write
                && offset == 0
                && self.write_buf.is_empty()
                && http::starts_preface(data)
        {
            if matches!(self.state, ProcessorState::Detecting) {
                self.state = ProcessorState::Http2(Box::new(http::H2Conn::new()));
            }
            let gap = self.h2_loss(direction, offset, len, data.len(), truncated);
            self.frag_next = if complete { 0 } else { end };
            self.frag_dir = direction;
            return self.feed_http2(direction, data, gap, background, ctx);
        }

        // A connection joined after its preface is recognized by a run of client frame heads.
        if matches!(self.state, ProcessorState::Detecting)
            && direction == LlmDirection::Write
            && let Some(outcome) =
                self.detect_h2_midjoin(data, complete, end, direction, background, ctx)
        {
            return outcome;
        }

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
            Some(reason) => self.drop_response(reason, ctx),
        };
        if resync {
            self.resume_next_request(background, ctx);
        } else if offset == 0 && direction == LlmDirection::Write {
            let noted = self.note_next_request(data, ctx);
            if matches!(noted, Outcome::Unparsed(_)) {
                dropped = noted;
            }
        }
        if out_of_order && offset != 0 {
            return dropped;
        }

        let fed = self.handle_event(direction, data, complete, background, ctx);
        if matches!(dropped, Outcome::Unparsed(_)) {
            dropped
        } else {
            fed
        }
    }

    /// End of the connection: an LLM call still without usage (request or response) is counted as unparsed.
    pub fn close(&mut self, ctx: &CallContext) -> Outcome {
        // Each HTTP/2 stream still open at close was a detected call that never reported usage.
        if let ProcessorState::Http2(conn) = &self.state {
            let open = conn.open_streams();
            self.state = ProcessorState::Finished;
            let mut outcome = Outcome::Pending;
            for _ in 0..open {
                outcome = Self::count_unparsed(UnparsedReason::ConnectionClosed, ctx);
            }
            return outcome;
        }
        self.drop_response(UnparsedReason::ConnectionClosed, ctx)
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
    fn note_next_request(&mut self, data: &[u8], ctx: &CallContext) -> Outcome {
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
        Self::count_unparsed(UnparsedReason::CaptureLost, ctx)
    }

    /// Start over from the request written mid-response, so this Read is parsed as its response.
    fn resume_next_request(&mut self, background: &BackgroundCalls, ctx: &CallContext) {
        let Some((at, start_time)) = self.next_request.take() else {
            return;
        };
        self.write_buf.drain(..at.min(self.write_buf.len()));
        self.read_buf.clear();
        self.read_progress = http::ResponseProgress::default();
        // Detected again, so a non-LLM request after the loss is skipped like any other.
        self.state = match self.detect(background, ctx) {
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

    /// Count the response in progress as unparsed rather than record it as a 0-token failure; a poll's is not a call.
    fn drop_response(&mut self, reason: UnparsedReason, ctx: &CallContext) -> Outcome {
        if !self.is_llm() {
            return Outcome::Pending;
        }
        self.state = ProcessorState::Finished;
        // A poll cut short settles nothing: its call stays pending until a result or the TTL.
        if self.continuation.take().is_some() {
            return Outcome::Pending;
        }
        // A stream that ended on a provider error event is told apart from capture loss and the
        // next-request close reasons it would otherwise fall under.
        let reason = if self.read_progress.stream_errored() {
            UnparsedReason::StreamError
        } else {
            reason
        };
        Self::count_unparsed(reason, ctx)
    }

    /// Count one LLM call as unparsed, in the metric and the log.
    fn count_unparsed(reason: UnparsedReason, ctx: &CallContext) -> Outcome {
        crate::telemetry::record_llm_unparsed(
            reason.as_str(),
            #[cfg(feature = "k8s")]
            ctx.pod_info.as_ref(),
        );

        #[cfg(feature = "k8s")]
        let pod_str = fmt_pod(ctx.pod_info.as_ref());
        #[cfg(not(feature = "k8s"))]
        let pod_str = "";

        info!(
            "LLM UNPARSED | PID: {} | {}, not recorded{}",
            ctx.pid, reason, pod_str
        );
        Outcome::Unparsed(reason)
    }

    fn handle_event(
        &mut self,
        direction: LlmDirection,
        data: &[u8],
        complete: bool,
        background: &mut BackgroundCalls,
        ctx: &CallContext,
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
                let dropped = self.drop_response(reason, ctx);
                self.reset();
                return dropped;
            }
        }

        // Take ownership of current state to enable transitions
        match std::mem::replace(&mut self.state, ProcessorState::Finished) {
            ProcessorState::Detecting => {
                self.state = self.detect(background, ctx);
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
                if complete {
                    // A Managed Agents session call: its usage is a running session total, so it is not
                    // read at all; the call is counted when its response framing ends, matching the
                    // HTTP/2 path, which counts every stream end regardless of the body.
                    if self.record_only {
                        if http::Http11Parser::is_complete_response(&self.read_buf) {
                            self.state = ProcessorState::Finished;
                            return Self::count_unparsed(UnparsedReason::AgentSession, ctx);
                        }
                    } else {
                        let usage = match &mut self.past_limit {
                            Some(followed) => followed.take_usage(),
                            None => {
                                parser.parse_response_from(&self.read_buf, &mut self.read_progress)
                            }
                        };
                        let state = self.read_progress.take_background();
                        // A framed 2xx response that ended with no usage is the provider omitting it, not
                        // capture loss, so count it apart from the next-request and close reasons. An
                        // error body, SSE and background polls are left to their existing paths.
                        let framed_no_usage = usage.is_none()
                            && state.is_none()
                            && self.continuation.is_none()
                            && self.past_limit.is_none()
                            && !parser.writes_during_response()
                            && http::Http11Parser::is_complete_success_response(&self.read_buf);
                        if framed_no_usage {
                            self.state = ProcessorState::ProcessingResponse { start_time, parser };
                            return self.drop_response(UnparsedReason::UsageAbsent, ctx);
                        }
                        if let Some(outcome) =
                            self.settle(usage, state, start_time, background, ctx)
                        {
                            return outcome;
                        }
                    }
                }
                self.state = ProcessorState::ProcessingResponse { start_time, parser };
            }
            // handle_fragment routes HTTP/2 before this point, so it is never reached here.
            ProcessorState::Http2(conn) => self.state = ProcessorState::Http2(conn),
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
        if let Some(followed) = &mut self.past_limit {
            followed
                .feed(data)
                .then_some(true)
                .ok_or(UnparsedReason::ResponseTooLarge)
        } else {
            self.read_buf.extend_from_slice(data);
            Ok(true)
        }
    }

    /// Next state for the request bytes buffered so far.
    fn detect(&mut self, background: &BackgroundCalls, ctx: &CallContext) -> ProcessorState {
        self.record_only = false;
        let (version, path, parser): (&str, String, Box<dyn ProtocolParser>) =
            if let Some(d) = http::Http11Parser.detect_request(&self.write_buf) {
                self.record_only = d.record_only;
                ("HTTP/1.1", d.path, Box::new(http::Http11Parser))
            } else if let Some(id) = http::Http11Parser::retrieved_response_id(&self.write_buf)
                .filter(|id| background.contains(id))
            {
                // Only a call this node saw queued: other GETs read stored objects already recorded.
                self.continuation = Some(id.into());
                (
                    "HTTP/1.1",
                    format!("poll of background call {id}"),
                    Box::new(http::Http11Parser),
                )
            } else if let Some(d) = http::Http2Parser.detect_request(&self.write_buf) {
                ("HTTP/2", d.path, Box::new(http::Http2Parser))
            } else if self.write_buf.len() > DETECTION_BUFFER_THRESHOLD {
                // Buffer too large and still not detected -> likely not LLM
                return ProcessorState::Finished;
            } else {
                return ProcessorState::Detecting;
            };

        #[cfg(feature = "k8s")]
        let pod_str = fmt_pod(ctx.pod_info.as_ref());
        #[cfg(not(feature = "k8s"))]
        let pod_str = "";

        info!(
            "[LLM] Detected {}: {} (PID: {}){}",
            version, path, ctx.pid, pod_str
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
        self.continuation = None;
        self.h2_gap_client = H2Gap::None;
        self.h2_gap_server = H2Gap::None;
        self.record_only = false;
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
            .or_else(|| http::SseStream::start_encoded(buffer).map(Self::Sse))
    }

    fn feed(&mut self, data: &[u8]) -> bool {
        match self {
            Self::Json(ends) => ends.feed(data),
            Self::Sse(stream) => stream.feed(data),
        }
    }

    fn take_usage(&mut self) -> Option<UsageInfo> {
        match self {
            Self::Json(ends) => ends.usage.take().map(|u| *u),
            Self::Sse(stream) => stream.usage(),
        }
    }
}

/// Whether the client bytes start like HTTP/1.1 or the preface (an ASCII uppercase method), so the HTTP/2
/// mid-join frame probe can be skipped. A real HTTP/2 frame starts with a length byte, 0 for any frame.
fn starts_ascii_upper(buffered: &[u8], data: &[u8]) -> bool {
    buffered
        .first()
        .or(data.first())
        .is_some_and(u8::is_ascii_uppercase)
}

/// The bytes after any empty lines, which httparse skips before a status line.
fn skip_empty_lines(buf: &[u8]) -> &[u8] {
    let start = buf
        .iter()
        .position(|b| !matches!(b, b'\r' | b'\n'))
        .unwrap_or(buf.len());
    &buf[start..]
}

#[cfg(test)]
mod tests;
