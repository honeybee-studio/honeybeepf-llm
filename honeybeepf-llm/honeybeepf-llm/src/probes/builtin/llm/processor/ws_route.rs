//! WebSocket fragment routing: the server's answer to the upgrade, then its frames. Client frames are dropped
//! unread. Anything that leaves the server's bytes unreadable ends the reading, and the connection is skipped.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::probes::builtin::llm::http::websocket::{
    Handshake, ServerFrames, Upgrade, WsError, WsEvent, read_handshake,
};

use super::background::record_usage;
use super::*;

/// WebSocket connections read at once; a later one is left alone, so many connections cost at most this
/// many inflate windows.
const MAX_SOCKETS: usize = 256;
/// Responses started and not yet ended per connection; the oldest goes first.
const MAX_STARTED: usize = 16;

static SOCKETS: AtomicUsize = AtomicUsize::new(0);

/// One of the `MAX_SOCKETS` reading slots, given back on drop.
struct Slot;

impl Slot {
    fn take() -> Option<Self> {
        if SOCKETS.fetch_add(1, Ordering::Relaxed) < MAX_SOCKETS {
            Some(Self)
        } else {
            SOCKETS.fetch_sub(1, Ordering::Relaxed);
            None
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        SOCKETS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// An upgraded connection's server side.
pub(super) struct WsConn {
    stage: Stage,
    /// From the upgrade's query, then from the session.
    model: Option<String>,
    matched: MatchedBy,
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

impl WsConn {
    /// None when `MAX_SOCKETS` connections are already read.
    pub(super) fn new(upgrade: Upgrade) -> Option<Self> {
        Some(Self {
            stage: Stage::Handshake(Vec::new()),
            model: upgrade.model,
            matched: upgrade.matched,
            started: VecDeque::new(),
            read_next: 0,
            _slot: Slot::take()?,
        })
    }

    /// Read the next server bytes, or why they cannot be read; `Ok(false)` when the server refused the upgrade.
    fn read(&mut self, data: &[u8], events: &mut Vec<WsEvent>) -> Result<bool, WsError> {
        match &mut self.stage {
            Stage::Open(frames) => frames.feed(data, events).map(|()| true),
            Stage::Handshake(head) => {
                head.extend_from_slice(data);
                match read_handshake(head) {
                    Handshake::Partial => Ok(true),
                    Handshake::Refused => Ok(false),
                    Handshake::Switched { len, deflate } => {
                        let rest = head.split_off(len);
                        let mut frames = ServerFrames::new(deflate);
                        let read = frames.feed(&rest, events);
                        self.stage = Stage::Open(frames);
                        read.map(|()| true)
                    }
                }
            }
        }
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
            Ok(true) => outcome,
            // Not a call: the server answered the upgrade with an error or an unread extension.
            Ok(false) => self.end_websocket(None, true, ctx),
            Err(e) => {
                debug!("WebSocket of PID {} left unread: {e:?}", ctx.pid);
                let reason = match e {
                    WsError::TooLarge => UnparsedReason::ResponseTooLarge,
                    WsError::Protocol | WsError::Inflate => UnparsedReason::CaptureLost,
                };
                self.end_websocket(Some(reason), true, ctx)
            }
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
                WsEvent::Model(model) => ws.model = Some(model),
                WsEvent::Ended(id, mut usage) => {
                    let at = ws
                        .started
                        .iter()
                        .position(|(started, _)| Some(started) == id.as_ref());
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
            }
        }
        outcome
    }

    /// Stop reading the connection: each response started and not ended counts as unparsed for `reason`
    /// (none when the upgrade was refused), and with `skip` the kernel drops the connection's later calls.
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
