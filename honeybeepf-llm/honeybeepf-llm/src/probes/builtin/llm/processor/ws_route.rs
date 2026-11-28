//! WebSocket fragment routing: the server's answer to the upgrade, then its frames. Client frames are dropped
//! unread, but for a Gemini Live setup's model. Anything that leaves the server's bytes unreadable ends the
//! reading, and the connection is skipped.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::probes::builtin::llm::http::websocket::{
    Dialect, Handshake, ServerFrames, Upgrade, WsError, WsEvent, read_handshake, setup_model,
};

use super::background::record_usage;
use super::*;

/// WebSocket connections read at once, about 45 KB each with compression: at most about 46 MB. A later one is
/// left alone and counted under `too_many_websockets`.
const MAX_SOCKETS: usize = 1024;
/// Responses started and not yet ended per connection; the oldest goes first.
const MAX_STARTED: usize = 16;

static SOCKETS: AtomicUsize = AtomicUsize::new(0);
static WARNED: AtomicBool = AtomicBool::new(false);

/// One of `cap` reading slots counted in `open`, given back on drop.
struct Slot(&'static AtomicUsize);

impl Slot {
    fn take(open: &'static AtomicUsize, cap: usize) -> Option<Self> {
        if open.fetch_add(1, Ordering::Relaxed) < cap {
            Some(Self(open))
        } else {
            open.fetch_sub(1, Ordering::Relaxed);
            None
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Count a WebSocket left alone because `MAX_SOCKETS` are already read; the first one is also a warning.
pub(super) fn count_too_many(ctx: &CallContext) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "More than {MAX_SOCKETS} WebSockets open at once; later ones are not read (reason too_many_websockets)"
        );
    }
    crate::telemetry::record_llm_unparsed(
        UnparsedReason::TooManyWebsockets.as_str(),
        #[cfg(feature = "k8s")]
        ctx.pod_info.as_ref(),
    );
    debug!("WebSocket of PID {} not read: too many open", ctx.pid);
}

/// An upgraded connection's server side.
pub(super) struct WsConn {
    stage: Stage,
    /// From the upgrade's query, or a Gemini Live setup; a Realtime or Live response names no model.
    model: Option<String>,
    matched: MatchedBy,
    dialect: Dialect,
    /// A Gemini Live client's first message is still to come.
    awaits_setup: bool,
    started: VecDeque<(Box<str>, Instant)>,
    /// Offset the next fragment of the server call in progress must have; 0 between calls.
    read_next: u32,
    _slot: Slot,
}

enum Stage {
    /// The server's answer to the upgrade, until its head ends.
    Handshake(Vec<u8>),
    Open(ServerFrames),
}

/// What the server bytes of one fragment left the reading at.
enum Read {
    Going,
    /// The upgrade was refused: no call to read.
    Skipped,
    Broken(WsError),
}

impl WsConn {
    /// None when `MAX_SOCKETS` connections are already read.
    pub(super) fn new(upgrade: Upgrade) -> Option<Self> {
        Some(Self {
            stage: Stage::Handshake(Vec::new()),
            model: upgrade.model,
            matched: upgrade.matched,
            dialect: upgrade.dialect,
            awaits_setup: upgrade.dialect == Dialect::Gemini,
            started: VecDeque::new(),
            read_next: 0,
            _slot: Slot::take(&SOCKETS, MAX_SOCKETS)?,
        })
    }

    fn read(&mut self, data: &[u8], events: &mut Vec<WsEvent>) -> Read {
        let (rest, deflate) = match &mut self.stage {
            Stage::Open(frames) => {
                return frames
                    .feed(data, events)
                    .map_or_else(Read::Broken, |()| Read::Going);
            }
            Stage::Handshake(head) => {
                head.extend_from_slice(data);
                match read_handshake(head) {
                    Handshake::Partial => return Read::Going,
                    Handshake::Refused => return Read::Skipped,
                    Handshake::Switched { len, deflate } => (head.split_off(len), deflate),
                }
            }
        };
        let mut frames = ServerFrames::new(deflate, self.dialect);
        let read = frames.feed(&rest, events);
        self.stage = Stage::Open(frames);
        read.map_or_else(Read::Broken, |()| Read::Going)
    }
}

impl StreamProcessor {
    /// Feed one fragment of an upgraded connection and record each response that ended in it.
    pub(super) fn feed_websocket(
        &mut self,
        direction: LlmDirection,
        data: &[u8],
        offset: u32,
        len: u32,
        truncated: bool,
        ctx: &CallContext,
    ) -> Outcome {
        self.last_activity = Instant::now();
        let limit = match self.transport {
            Transport::Tls => capture_limit(direction),
            Transport::Plain => plain_capture_limit(direction),
        };
        let ProcessorState::WebSocket(ws) = &mut self.state else {
            return Outcome::Pending;
        };
        if direction != LlmDirection::Read {
            if ws.awaits_setup
                && offset == 0
                && let Some(model) = setup_model(data)
            {
                ws.awaits_setup = false;
                ws.model = ws.model.take().or(model);
            }
            return Outcome::Pending;
        }
        let end = offset.saturating_add(data.len() as u32);
        // Each lost server byte breaks the framing, and with compression every later message.
        let lost = truncated || offset != ws.read_next || len as usize > limit;
        ws.read_next = if end as usize >= len as usize { 0 } else { end };
        if lost {
            return self.end_websocket(Some(UnparsedReason::CaptureLost), true, ctx);
        }
        let mut events = Vec::new();
        let read = ws.read(data, &mut events);
        let outcome = self.record_ws_events(events, ctx);
        match read {
            Read::Going => outcome,
            Read::Skipped => {
                debug!("WebSocket of PID {} not read: upgrade refused", ctx.pid);
                self.end_websocket(None, true, ctx)
            }
            Read::Broken(e) => {
                debug!("WebSocket of PID {} left unread: {e:?}", ctx.pid);
                let reason = match e {
                    WsError::TooLarge => UnparsedReason::ResponseTooLarge,
                    WsError::Protocol | WsError::Inflate => UnparsedReason::CaptureLost,
                };
                self.end_websocket(Some(reason), true, ctx)
            }
        }
    }

    #[cfg(test)]
    pub(super) fn websocket_model(&self) -> Option<&str> {
        match &self.state {
            ProcessorState::WebSocket(ws) => ws.model.as_deref(),
            _ => None,
        }
    }

    fn record_ws_events(&mut self, events: Vec<WsEvent>, ctx: &CallContext) -> Outcome {
        let ProcessorState::WebSocket(ws) = &mut self.state else {
            return Outcome::Pending;
        };
        let mut outcome = Outcome::Pending;
        for event in events {
            match event {
                WsEvent::Started(id) => {
                    if ws.started.len() == MAX_STARTED {
                        ws.started.pop_front();
                    }
                    ws.started.push_back((id, Instant::now()));
                }
                WsEvent::Ended(id, mut usage) => {
                    // A Gemini turn has no id: it ends the oldest one started.
                    let at = match &id {
                        Some(id) => ws.started.iter().position(|(s, _)| s == id),
                        None => (!ws.started.is_empty()).then_some(0),
                    };
                    let start = at
                        .and_then(|at| ws.started.remove(at))
                        .map_or_else(Instant::now, |(_, start)| start);
                    if usage.model.is_none() {
                        usage.model.clone_from(&ws.model);
                    }
                    record_usage(
                        &usage,
                        start,
                        None,
                        self.transport,
                        &ctx.with_match(ws.matched),
                    );
                    outcome = Outcome::Recorded;
                }
                WsEvent::Unread => {
                    ws.started.pop_front();
                    outcome = Self::count_unparsed(
                        UnparsedReason::ResponseTooLarge,
                        &ctx.with_match(ws.matched),
                    );
                }
            }
        }
        outcome
    }

    /// Stop reading the connection: each response started and not ended counts as unparsed for `reason`
    /// (none when there was no call), and with `skip` the kernel drops the connection's later calls.
    pub(super) fn end_websocket(
        &mut self,
        reason: Option<UnparsedReason>,
        skip: bool,
        ctx: &CallContext,
    ) -> Outcome {
        let ProcessorState::WebSocket(ws) =
            std::mem::replace(&mut self.state, ProcessorState::Finished)
        else {
            return Outcome::Pending;
        };
        self.skip |= skip;
        let mut outcome = Outcome::Pending;
        if let Some(reason) = reason {
            for _ in &ws.started {
                outcome = Self::count_unparsed(reason, &ctx.with_match(ws.matched));
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_past_the_cap_are_refused_until_one_is_given_back() {
        static OPEN: AtomicUsize = AtomicUsize::new(0);
        let first = Slot::take(&OPEN, 2);
        let second = Slot::take(&OPEN, 2);
        assert!(first.is_some() && second.is_some());
        assert!(Slot::take(&OPEN, 2).is_none());
        drop(first);
        assert!(Slot::take(&OPEN, 2).is_some());
        assert_eq!(OPEN.load(Ordering::Relaxed), 1);
    }
}
