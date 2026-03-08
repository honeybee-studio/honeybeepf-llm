use std::time::Instant;

use log::{info, warn};

use crate::probes::builtin::llm::{
    http::{self, ProtocolParser},
    types::LlmDirection,
};

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

pub struct StreamProcessor {
    state: ProcessorState,
    write_buf: Vec<u8>,
    read_buf: Vec<u8>,
    last_activity: Instant,
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

    pub fn handle_event(
        &mut self,
        direction: LlmDirection,
        data: &[u8],
        pid: u32,
        #[cfg(feature = "k8s")] pod_info: Option<std::sync::Arc<crate::k8s::PodInfo>>,
    ) {
        self.last_activity = Instant::now();

        // Early return if finished (except for new Write which triggers reset)
        if matches!(self.state, ProcessorState::Finished) {
            if direction == LlmDirection::Write {
                self.reset();
            } else {
                return; // Ignore Read events after completion
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
                    return;
                }
                self.write_buf.extend_from_slice(data);
            }
            LlmDirection::Read => {
                // Optimization: Don't buffer responses if we haven't detected a valid LLM request yet
                if matches!(self.state, ProcessorState::Detecting) {
                    return;
                }

                if self.read_buf.len() + data.len() > MAX_RESPONSE_BUFFER_SIZE {
                    warn!(
                        "LLM response buffer exceeded {}MB limit (PID: {}), discarding stream",
                        MAX_RESPONSE_BUFFER_SIZE / (1024 * 1024),
                        pid
                    );
                    self.reset();
                    return;
                }
                self.read_buf.extend_from_slice(data);
            }
            _ => return,
        }

        // 2. State Machine Transition
        // Take ownership of current state to enable transitions
        let current_state = std::mem::replace(&mut self.state, ProcessorState::Finished);

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
                // Try parsing response
                if let Some(usage) = parser.parse_response(&self.read_buf) {
                    let latency = start_time.elapsed();
                    let model_str = usage.model.as_deref().unwrap_or("unknown");

                    #[cfg(feature = "k8s")]
                    let pod_str = fmt_pod(&pod_info);
                    #[cfg(not(feature = "k8s"))]
                    let pod_str = "";

                    if usage.prompt_tokens == 0 && usage.completion_tokens == 0 {
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
                        info!(
                            "LLM SUCCESS | PID: {} | Model: {} | Latency: {:.2}s | Tokens: {} (Prompt: {}, Compl: {}{}){}",
                            pid,
                            model_str,
                            latency.as_secs_f64(),
                            usage.prompt_tokens + usage.completion_tokens,
                            usage.prompt_tokens,
                            usage.completion_tokens,
                            thoughts_str,
                            pod_str
                        );
                    }

                    ProcessorState::Finished
                } else {
                    // Incomplete -> keep state
                    ProcessorState::ProcessingResponse { start_time, parser }
                }
            }

            // [State 4] Finished
            ProcessorState::Finished => ProcessorState::Finished,
        };
    }

    fn reset(&mut self) {
        self.state = ProcessorState::Detecting;
        self.write_buf.clear();
        self.read_buf.clear();
    }
}
