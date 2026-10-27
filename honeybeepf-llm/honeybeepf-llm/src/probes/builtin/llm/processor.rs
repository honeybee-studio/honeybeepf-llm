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
    /// What parsing already read of `read_buf`; cleared with it.
    read_progress: http::ResponseProgress,
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
            read_progress: http::ResponseProgress::default(),
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
                if complete
                    && let Some(usage) =
                        parser.parse_response_from(&self.read_buf, &mut self.read_progress)
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
        self.read_progress = http::ResponseProgress::default();
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
mod tests;
