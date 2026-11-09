//! HTTP/2 fragment routing: mid-stream join detection, feeding frames to the connection, and the gap
//! bookkeeping a discontinuous capture needs before each direction's bytes are read.

use crate::probes::builtin::llm::background::PendingCall;

use super::background::{defer, record_usage, settle_h2_poll};
use super::*;

impl StreamProcessor {
    /// Classify a connection joined after its preface: no preface was seen, but the client bytes form a run
    /// of frame heads (HTTP/1.1, which starts with an uppercase method, never does). On a match it starts an
    /// HTTP/2 connection with both directions unknown and feeds what was buffered, returning its outcome.
    pub(super) fn detect_h2_midjoin(
        &mut self,
        data: &[u8],
        complete: bool,
        end: u32,
        direction: LlmDirection,
        background: &mut BackgroundCalls,
        ctx: &CallContext,
    ) -> Option<Outcome> {
        if starts_ascii_upper(&self.write_buf, data) {
            return None;
        }
        let client = [&self.write_buf[..], data].concat();
        if !http::looks_like_http2(&client) {
            return None;
        }
        info!("[LLM] Detected HTTP/2 (mid-stream join) (PID: {})", ctx.pid);
        self.write_buf = Vec::new();
        self.state = ProcessorState::Http2(Box::new(http::H2Conn::new_midjoin()));
        self.h2_gap_client = H2Gap::Resync;
        self.h2_gap_server = H2Gap::Resync;
        self.frag_next = if complete { 0 } else { end };
        self.frag_dir = direction;
        let gap = self.take_h2_gap(direction);
        Some(self.feed_http2(direction, &client, gap, background, ctx))
    }

    /// Feed one fragment to the HTTP/2 connection, recording each stream whose response ended. `gap` tells
    /// the connection how this direction's bytes are discontinuous, so it skips a known gap or re-anchors.
    pub(super) fn feed_http2(
        &mut self,
        direction: LlmDirection,
        data: &[u8],
        gap: H2Gap,
        background: &mut BackgroundCalls,
        ctx: &CallContext,
    ) -> Outcome {
        self.last_activity = Instant::now();
        let ProcessorState::Http2(conn) = &mut self.state else {
            return Outcome::Pending;
        };
        let mut outcome = Outcome::Pending;
        conn.feed(direction, data, gap, |event| match event {
            http::H2Event::Detected { path } => {
                #[cfg(feature = "k8s")]
                let pod_str = fmt_pod(ctx.pod_info.as_ref());
                #[cfg(not(feature = "k8s"))]
                let pod_str = "";
                info!(
                    "[LLM] Detected HTTP/2: {} (PID: {}){}",
                    path, ctx.pid, pod_str
                );
            }
            http::H2Event::Recorded {
                usage,
                start,
                midjoin,
            } => {
                record_usage(&usage, start, self.transport, ctx);
                if midjoin {
                    crate::telemetry::record_h2_midjoin(
                        #[cfg(feature = "k8s")]
                        ctx.pod_info.as_ref(),
                    );
                }
                outcome = Outcome::Recorded;
            }
            http::H2Event::Deferred { id, start } => {
                let call = PendingCall::new(
                    start,
                    ctx.pid,
                    #[cfg(feature = "k8s")]
                    ctx.pod_info.clone(),
                );
                outcome = defer(id, call, background);
            }
            http::H2Event::Poll {
                id,
                usage,
                background: state,
            } => {
                outcome = settle_h2_poll(id, usage.map(|u| *u), state, background, self.transport);
            }
            http::H2Event::Unparsed { reason } => {
                outcome = Self::count_unparsed(reason, ctx);
            }
            http::H2Event::Resynced => crate::telemetry::record_h2_resynced(
                #[cfg(feature = "k8s")]
                ctx.pod_info.as_ref(),
            ),
        });
        outcome
    }

    /// The gap this HTTP/2 fragment must act on before its bytes are read. A dropped middle fragment and a
    /// truncated tail both have an exact size, so they are skipped within the current frame when they fit; a
    /// new call starting while the previous was unfinished loses a tail of unknown size, so it re-anchors.
    pub(super) fn h2_loss(
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
}
