use std::time::Instant;

use honeybeepf_llm_common::capture_limit;
use honeybeepf_llm_core::types::LlmDirection;
use log::{info, warn};

use crate::probes::builtin::llm::http::{self, ProtocolParser};

// Buffer size constants
const INITIAL_BUFFER_CAPACITY: usize = 8 * 1024; // 8KB initial allocation
const MAX_REQUEST_BUFFER_SIZE: usize = 8 * 1024 * 1024; // 8MB max for request (images, large context)
const MAX_RESPONSE_BUFFER_SIZE: usize = 16 * 1024 * 1024; // 16MB max for response (streaming)
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
            write_buf: Vec::with_capacity(INITIAL_BUFFER_CAPACITY),
            read_buf: Vec::with_capacity(INITIAL_BUFFER_CAPACITY),
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

        let dropped = if !gap {
            Outcome::Pending
        } else {
            #[cfg(feature = "k8s")]
            let dropped = self.drop_response(pid, pod_info.as_ref());
            #[cfg(not(feature = "k8s"))]
            let dropped = self.drop_response(pid);
            dropped
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

    /// Count the response in progress as unparsed rather than record it as a 0-token failure.
    fn drop_response(
        &mut self,
        pid: u32,
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
            "LLM UNPARSED | PID: {} | response bytes lost in capture, not recorded{}",
            pid, pod_str
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

        // Early return if finished (except for new Write which triggers reset)
        if matches!(self.state, ProcessorState::Finished) {
            if direction == LlmDirection::Write {
                self.reset();
            } else {
                return Outcome::Pending; // Ignore Read events after completion
            }
        }

        // 1. Buffer Management
        match direction {
            LlmDirection::Write => {
                if self.write_buf.len() + data.len() > MAX_REQUEST_BUFFER_SIZE {
                    warn!(
                        "LLM request buffer exceeded {}MB limit (PID: {}), discarding stream",
                        MAX_REQUEST_BUFFER_SIZE / (1024 * 1024),
                        pid
                    );
                    self.reset();
                    return Outcome::Pending;
                }
                self.write_buf.extend_from_slice(data);
            }
            LlmDirection::Read => {
                // Optimization: Don't buffer responses if we haven't detected a valid LLM request yet
                if matches!(self.state, ProcessorState::Detecting) {
                    return Outcome::Pending;
                }

                if self.read_buf.len() + data.len() > MAX_RESPONSE_BUFFER_SIZE {
                    warn!(
                        "LLM response buffer exceeded {}MB limit (PID: {}), discarding stream",
                        MAX_RESPONSE_BUFFER_SIZE / (1024 * 1024),
                        pid
                    );
                    self.reset();
                    return Outcome::Pending;
                }
                self.read_buf.extend_from_slice(data);
            }
            _ => return Outcome::Pending,
        }

        // 2. State Machine Transition
        // Take ownership of current state to enable transitions
        let current_state = std::mem::replace(&mut self.state, ProcessorState::Finished);
        let mut outcome = Outcome::Pending;

        self.state = match current_state {
            // [State 1] Detecting
            ProcessorState::Detecting => {
                // Try H1
                if let Some(path) = http::Http11Parser.detect_request(&self.write_buf) {
                    #[cfg(feature = "k8s")]
                    let pod_str = fmt_pod(&pod_info);
                    #[cfg(not(feature = "k8s"))]
                    let pod_str = "";

                    info!(
                        "[LLM] Detected HTTP/1.1: {} (PID: {}){}",
                        path, pid, pod_str
                    );
                    ProcessorState::ProcessingRequest {
                        start_time: Instant::now(),
                        parser: Box::new(http::Http11Parser),
                    }
                }
                // Try H2
                else if let Some(path) = http::Http2Parser.detect_request(&self.write_buf) {
                    #[cfg(feature = "k8s")]
                    let pod_str = fmt_pod(&pod_info);
                    #[cfg(not(feature = "k8s"))]
                    let pod_str = "";

                    info!("[LLM] Detected HTTP/2: {} (PID: {}){}", path, pid, pod_str);
                    ProcessorState::ProcessingRequest {
                        start_time: Instant::now(),
                        parser: Box::new(http::Http2Parser),
                    }
                } else {
                    // Not detected yet
                    if self.write_buf.len() > DETECTION_BUFFER_THRESHOLD {
                        // Buffer too large and still not detected -> likely not LLM
                        ProcessorState::Finished
                    } else {
                        ProcessorState::Detecting // Keep detecting
                    }
                }
            }

            // [State 2] Processing Request
            ProcessorState::ProcessingRequest { start_time, parser } => {
                if direction == LlmDirection::Read {
                    // Write finished (implied by Read starting), transition to Response phase
                    ProcessorState::ProcessingResponse { start_time, parser }
                } else {
                    // Still writing -> keep state
                    ProcessorState::ProcessingRequest { start_time, parser }
                }
            }

            // [State 3] Processing Response
            ProcessorState::ProcessingResponse { start_time, parser } => {
                // Parse once the SSL call's last fragment is in, not on a partial record
                if complete && let Some(usage) = parser.parse_response(&self.read_buf) {
                    let latency = start_time.elapsed();
                    let latency_secs = latency.as_secs_f64();
                    let model_str = usage.model.as_deref().unwrap_or("unknown");
                    let is_error = usage.prompt_tokens == 0 && usage.completion_tokens == 0;

                    crate::telemetry::record_llm_request(
                        model_str,
                        latency_secs,
                        usage.prompt_tokens,
                        usage.completion_tokens,
                        (usage.cache_read_tokens, usage.cache_write_tokens),
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

                    outcome = Outcome::Recorded;
                    ProcessorState::Finished
                } else {
                    // Incomplete -> keep state
                    ProcessorState::ProcessingResponse { start_time, parser }
                }
            }

            // [State 4] Finished
            ProcessorState::Finished => ProcessorState::Finished,
        };
        outcome
    }

    fn reset(&mut self) {
        self.state = ProcessorState::Detecting;
        self.write_buf.clear();
        self.read_buf.clear();
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
}
