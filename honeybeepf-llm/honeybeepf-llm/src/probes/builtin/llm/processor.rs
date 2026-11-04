use std::time::Instant;

use honeybeepf_llm_common::capture_limit;
use honeybeepf_llm_core::types::{LlmDirection, UsageInfo};
use log::info;

use crate::probes::builtin::llm::background::{BackgroundCalls, PendingCall};
use crate::probes::builtin::llm::http::{self, BackgroundState, ProtocolParser};

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
        background: &mut BackgroundCalls,
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<std::sync::Arc<crate::k8s::PodInfo>>,
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
            return self.feed_http2(
                direction,
                data,
                gap,
                pid,
                #[cfg(feature = "k8s")]
                pod_info,
            );
        }

        // A connection joined after its preface: no preface was seen, but the client bytes form a run of
        // frame heads (HTTP/1.1, which starts with an uppercase method, never does). Classify it HTTP/2,
        // start both directions unknown, and feed what was buffered as the first (lost) client bytes.
        if matches!(self.state, ProcessorState::Detecting)
            && direction == LlmDirection::Write
            && !starts_ascii_upper(&self.write_buf, data)
        {
            let client = [&self.write_buf[..], data].concat();
            if http::looks_like_http2(&client) {
                info!("[LLM] Detected HTTP/2 (mid-stream join) (PID: {})", pid);
                self.write_buf = Vec::new();
                self.state = ProcessorState::Http2(Box::new(http::H2Conn::new_midjoin()));
                self.h2_gap_client = H2Gap::Resync;
                self.h2_gap_server = H2Gap::Resync;
                self.frag_next = if complete { 0 } else { end };
                self.frag_dir = direction;
                let gap = self.take_h2_gap(direction);
                return self.feed_http2(
                    direction,
                    &client,
                    gap,
                    pid,
                    #[cfg(feature = "k8s")]
                    pod_info,
                );
            }
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
                background,
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
        let fed = self.handle_event(direction, data, complete, background, pid, pod_info);
        #[cfg(not(feature = "k8s"))]
        let fed = self.handle_event(direction, data, complete, background, pid);
        if matches!(dropped, Outcome::Unparsed(_)) {
            dropped
        } else {
            fed
        }
    }

    /// Feed one fragment to the HTTP/2 connection, recording each stream whose response ended. `gap` tells
    /// the connection how this direction's bytes are discontinuous, so it skips a known gap or re-anchors.
    fn feed_http2(
        &mut self,
        direction: LlmDirection,
        data: &[u8],
        gap: H2Gap,
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> Outcome {
        self.last_activity = Instant::now();
        let ProcessorState::Http2(conn) = &mut self.state else {
            return Outcome::Pending;
        };
        let mut outcome = Outcome::Pending;
        conn.feed(direction, data, gap, |event| match event {
            http::H2Event::Detected { path } => {
                #[cfg(feature = "k8s")]
                let pod_str = fmt_pod(&pod_info);
                #[cfg(not(feature = "k8s"))]
                let pod_str = "";
                info!("[LLM] Detected HTTP/2: {} (PID: {}){}", path, pid, pod_str);
            }
            http::H2Event::Recorded {
                usage,
                start,
                midjoin,
            } => {
                record_usage(
                    &usage,
                    start,
                    pid,
                    #[cfg(feature = "k8s")]
                    pod_info.clone(),
                );
                if midjoin {
                    crate::telemetry::record_h2_midjoin(
                        #[cfg(feature = "k8s")]
                        pod_info.as_ref(),
                    );
                }
                outcome = Outcome::Recorded;
            }
            http::H2Event::Unparsed { reason } => {
                outcome = Self::count_unparsed(
                    pid,
                    reason,
                    #[cfg(feature = "k8s")]
                    pod_info.as_ref(),
                );
            }
            http::H2Event::Resynced => crate::telemetry::record_h2_resynced(
                #[cfg(feature = "k8s")]
                pod_info.as_ref(),
            ),
        });
        outcome
    }

    /// The gap this HTTP/2 fragment must act on before its bytes are read. A dropped middle fragment and a
    /// truncated tail both have an exact size, so they are skipped within the current frame when they fit; a
    /// new call starting while the previous was unfinished loses a tail of unknown size, so it re-anchors.
    fn h2_loss(
        &mut self,
        direction: LlmDirection,
        offset: u32,
        len: u32,
        data_len: usize,
        truncated: bool,
    ) -> H2Gap {
        if offset == 0 && self.frag_next != 0 {
            self.set_h2_gap(self.frag_dir, H2Gap::Resync);
        }
        // A dropped middle fragment: exactly `offset - frag_next` bytes are missing, here and now.
        let now = if offset != 0 && offset > self.frag_next {
            H2Gap::Skip((offset - self.frag_next) as usize)
        } else {
            self.take_h2_gap(direction)
        };
        if truncated {
            let lost = (len as usize).saturating_sub(offset as usize + data_len);
            if lost > 0 {
                self.set_h2_gap(direction, H2Gap::Skip(lost));
            }
        }
        now
    }

    fn set_h2_gap(&mut self, direction: LlmDirection, gap: H2Gap) {
        match direction {
            LlmDirection::Write => self.h2_gap_client = gap,
            LlmDirection::Read => self.h2_gap_server = gap,
            _ => {}
        }
    }

    fn take_h2_gap(&mut self, direction: LlmDirection) -> H2Gap {
        match direction {
            LlmDirection::Write => std::mem::replace(&mut self.h2_gap_client, H2Gap::None),
            LlmDirection::Read => std::mem::replace(&mut self.h2_gap_server, H2Gap::None),
            _ => H2Gap::None,
        }
    }

    /// End of the connection: an LLM call still without usage (request or response) is counted as unparsed.
    pub fn close(
        &mut self,
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> Outcome {
        // Each HTTP/2 stream still open at close was a detected call that never reported usage.
        if let ProcessorState::Http2(conn) = &self.state {
            let open = conn.open_streams();
            self.state = ProcessorState::Finished;
            let mut outcome = Outcome::Pending;
            for _ in 0..open {
                outcome = Self::count_unparsed(
                    pid,
                    UnparsedReason::ConnectionClosed,
                    #[cfg(feature = "k8s")]
                    pod_info,
                );
            }
            return outcome;
        }
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
        background: &BackgroundCalls,
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
            background,
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

    /// Count the response in progress as unparsed rather than record it as a 0-token failure; a poll's is not a call.
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
        // A poll cut short settles nothing: its call stays pending until a result or the TTL.
        if self.continuation.take().is_some() {
            return Outcome::Pending;
        }
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
        background: &mut BackgroundCalls,
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
                    background,
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
                if complete {
                    let usage = match &mut self.past_limit {
                        Some(followed) => followed.take_usage(),
                        None => parser.parse_response_from(&self.read_buf, &mut self.read_progress),
                    };
                    let state = self.read_progress.take_background();
                    if let Some(outcome) = self.settle(
                        usage,
                        state,
                        start_time,
                        background,
                        pid,
                        #[cfg(feature = "k8s")]
                        pod_info,
                    ) {
                        return outcome;
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
        &mut self,
        background: &BackgroundCalls,
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<&std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> ProcessorState {
        let (version, path, parser): (&str, String, Box<dyn ProtocolParser>) =
            if let Some(path) = http::Http11Parser.detect_request(&self.write_buf) {
                ("HTTP/1.1", path, Box::new(http::Http11Parser))
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
        self.continuation = None;
        self.h2_gap_client = H2Gap::None;
        self.h2_gap_server = H2Gap::None;
    }

    /// Record, defer or drop the call once its response parsed to `usage` and `state`; None keeps waiting.
    fn settle(
        &mut self,
        usage: Option<UsageInfo>,
        state: Option<BackgroundState>,
        start_time: Instant,
        background: &mut BackgroundCalls,
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<std::sync::Arc<crate::k8s::PodInfo>>,
    ) -> Option<Outcome> {
        match self.continuation.take() {
            Some(id) => self.settle_poll(id, usage, state, background),
            None => settle_create(
                usage,
                state,
                start_time,
                background,
                pid,
                #[cfg(feature = "k8s")]
                pod_info,
            ),
        }
    }

    /// A poll of background call `id`: its result is recorded once, against the create request.
    fn settle_poll(
        &mut self,
        id: Box<str>,
        usage: Option<UsageInfo>,
        state: Option<BackgroundState>,
        background: &mut BackgroundCalls,
    ) -> Option<Outcome> {
        // A body past the buffer limit keeps no state; usage with tokens is still the call's result.
        let state = state.or_else(|| {
            usage
                .as_ref()
                .filter(|u| u.prompt_tokens > 0 || u.completion_tokens > 0)
                .map(|_| BackgroundState::Ended)
        });
        let end = match state {
            Some(BackgroundState::Running(_)) => {
                background.touch(&id);
                return Some(Outcome::Pending);
            }
            Some(end) => end,
            // An error body (unknown id, 5xx) settles nothing; the call waits for a result or its TTL.
            None if usage.is_some() => return Some(Outcome::Pending),
            None => {
                // A resumed stream can outlast the TTL.
                background.touch(&id);
                self.continuation = Some(id);
                return None;
            }
        };
        let Some(call) = background.take(&id) else {
            // Another poll recorded it already.
            return Some(Outcome::Pending);
        };
        let usage = match (usage, end) {
            (Some(usage), _) => usage,
            // A cancel can come before any token was counted: one failed call.
            (None, BackgroundState::Cancelled) => UsageInfo::default(),
            (None, _) => return Some(give_up(call)),
        };
        record_usage(
            &usage,
            call.start_time,
            call.pid,
            #[cfg(feature = "k8s")]
            call.pod_info,
        );
        Some(Outcome::Recorded)
    }
}

/// Any call but a poll: recorded with usage, deferred when it was queued in background mode.
fn settle_create(
    usage: Option<UsageInfo>,
    state: Option<BackgroundState>,
    start_time: Instant,
    background: &mut BackgroundCalls,
    pid: u32,
    #[cfg(feature = "k8s")] pod_info: Option<std::sync::Arc<crate::k8s::PodInfo>>,
) -> Option<Outcome> {
    match (usage, state) {
        (Some(usage), _) => {
            record_usage(
                &usage,
                start_time,
                pid,
                #[cfg(feature = "k8s")]
                pod_info,
            );
            Some(Outcome::Recorded)
        }
        (None, Some(BackgroundState::Running(id))) => {
            let call = PendingCall::new(
                start_time,
                pid,
                #[cfg(feature = "k8s")]
                pod_info,
            );
            Some(defer(id, call, background))
        }
        (None, _) => None,
    }
}

/// Keep a queued background call until a poll brings its result.
fn defer(id: Box<str>, call: PendingCall, background: &mut BackgroundCalls) -> Outcome {
    #[cfg(feature = "k8s")]
    let pod_str = fmt_pod(&call.pod_info);
    #[cfg(not(feature = "k8s"))]
    let pod_str = "";
    info!(
        "LLM BACKGROUND | PID: {} | {} queued, waiting for its result{}",
        call.pid, id, pod_str
    );
    if let Some(evicted) = background.insert(id, call) {
        give_up(evicted);
    }
    Outcome::Deferred
}

/// Count a background call whose usage will not be seen: evicted, idle past its TTL, or ended without it.
pub(super) fn give_up(call: PendingCall) -> Outcome {
    StreamProcessor::count_unparsed(
        call.pid,
        UnparsedReason::BackgroundUnfinished,
        #[cfg(feature = "k8s")]
        call.pod_info.as_ref(),
    )
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
