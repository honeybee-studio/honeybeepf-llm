//! Background-poll settlement: a create request's usage is recorded once a later poll brings its result.

use crate::probes::builtin::llm::background::PendingCall;
use crate::probes::builtin::llm::http::BackgroundState;

use super::*;

impl StreamProcessor {
    /// Record, defer or drop the call once its response parsed to `usage` and `state`; None keeps waiting.
    pub(super) fn settle(
        &mut self,
        usage: Option<UsageInfo>,
        state: Option<BackgroundState>,
        start_time: Instant,
        background: &mut BackgroundCalls,
        ctx: &CallContext,
    ) -> Option<Outcome> {
        match self.continuation.take() {
            Some(id) => self.settle_poll(id, usage, state, background),
            None => settle_create(usage, state, start_time, background, self.transport, ctx),
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
        let end = match poll_end_state(&usage, state) {
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
        Some(record_poll_result(
            &id,
            usage,
            end,
            self.transport,
            background,
        ))
    }
}

/// A poll result's end state: the body's own, or Ended when usage carried tokens past the buffer limit.
fn poll_end_state(
    usage: &Option<UsageInfo>,
    state: Option<BackgroundState>,
) -> Option<BackgroundState> {
    state.or_else(|| {
        usage
            .as_ref()
            .filter(|u| u.prompt_tokens > 0 || u.completion_tokens > 0)
            .map(|_| BackgroundState::Ended)
    })
}

/// Record a settled background call once: its create request's pid/pod, this poll's usage. A poll that no
/// longer holds the call (another already recorded it, or this node never queued it) records nothing.
fn record_poll_result(
    id: &str,
    usage: Option<UsageInfo>,
    end: BackgroundState,
    transport: Transport,
    background: &mut BackgroundCalls,
) -> Outcome {
    let Some(call) = background.take(id) else {
        return Outcome::Pending;
    };
    let usage = match (usage, end) {
        (Some(usage), _) => usage,
        // A cancel can come before any token was counted: one failed call.
        (None, BackgroundState::Cancelled) => UsageInfo::default(),
        (None, _) => return give_up(call),
    };
    record_usage(
        &usage,
        call.start_time,
        transport,
        &CallContext {
            pid: call.pid,
            #[cfg(feature = "k8s")]
            pod_info: call.pod_info,
        },
    );
    Outcome::Recorded
}

/// Any call but a poll: recorded with usage, deferred when it was queued in background mode.
fn settle_create(
    usage: Option<UsageInfo>,
    state: Option<BackgroundState>,
    start_time: Instant,
    background: &mut BackgroundCalls,
    transport: Transport,
    ctx: &CallContext,
) -> Option<Outcome> {
    match (usage, state) {
        (Some(usage), _) => {
            record_usage(&usage, start_time, transport, ctx);
            Some(Outcome::Recorded)
        }
        (None, Some(BackgroundState::Running(id))) => {
            let call = PendingCall::new(
                start_time,
                ctx.pid,
                #[cfg(feature = "k8s")]
                ctx.pod_info.clone(),
            );
            Some(defer(id, call, background))
        }
        (None, _) => None,
    }
}

/// An HTTP/2 poll of background call `id`: its result is recorded once, against the create request. Each poll
/// is its own stream, so a result still pending just keeps the call waiting (no continuation to carry on).
pub(super) fn settle_h2_poll(
    id: Box<str>,
    usage: Option<UsageInfo>,
    state: Option<BackgroundState>,
    background: &mut BackgroundCalls,
    transport: Transport,
) -> Outcome {
    let end = match poll_end_state(&usage, state) {
        // Still running, or a result still absent (error body, 0-token poll): wait for a later poll or the TTL.
        Some(BackgroundState::Running(_)) | None => {
            background.touch(&id);
            return Outcome::Pending;
        }
        Some(end) => end,
    };
    record_poll_result(&id, usage, end, transport, background)
}

/// Keep a queued background call until a poll brings its result.
pub(super) fn defer(id: Box<str>, call: PendingCall, background: &mut BackgroundCalls) -> Outcome {
    #[cfg(feature = "k8s")]
    let pod_str = fmt_pod(call.pod_info.as_ref());
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
pub(crate) fn give_up(call: PendingCall) -> Outcome {
    StreamProcessor::count_unparsed(
        UnparsedReason::BackgroundUnfinished,
        &CallContext {
            pid: call.pid,
            #[cfg(feature = "k8s")]
            pod_info: call.pod_info,
        },
    )
}

/// Export one finished call's usage and log it.
pub(super) fn record_usage(
    usage: &UsageInfo,
    start_time: Instant,
    transport: Transport,
    ctx: &CallContext,
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
        transport,
        #[cfg(feature = "k8s")]
        ctx.pod_info.as_ref(),
    );

    #[cfg(feature = "k8s")]
    let pod_str = fmt_pod(ctx.pod_info.as_ref());
    #[cfg(not(feature = "k8s"))]
    let pod_str = "";

    if is_error {
        info!(
            "LLM FAILED/ERROR | PID: {} | Model: {} | Transport: {} | Latency: {:.2}s{}",
            ctx.pid,
            model_str,
            transport.as_str(),
            latency.as_secs_f64(),
            pod_str
        );
    } else {
        let thoughts_str = usage
            .thoughts_tokens
            .map(|t| format!(", Thoughts: {t}"))
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
            "LLM SUCCESS | PID: {} | Model: {} | Transport: {} | Latency: {:.2}s | Tokens: {} (Prompt: {}, Compl: {}{}{}){}",
            ctx.pid,
            model_str,
            transport.as_str(),
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
