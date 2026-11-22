//! WebSocket fragment routing: the server's answer to the upgrade. Client frames are dropped unread, and a
//! connection whose server bytes are not read is skipped in the kernel.

use crate::probes::builtin::llm::http::websocket::{Handshake, Upgrade, read_handshake};

use super::*;

/// An upgraded connection's server side.
pub(super) struct WsConn {
    /// The server's answer to the upgrade, until its head ends.
    head: Vec<u8>,
    /// From the upgrade's query.
    model: Option<String>,
    /// Offset the next fragment of the server call in progress must have; 0 between calls.
    read_next: u32,
}

impl WsConn {
    pub(super) fn new(upgrade: Upgrade) -> Self {
        Self {
            head: Vec::new(),
            model: upgrade.model,
            read_next: 0,
        }
    }
}

impl StreamProcessor {
    /// Feed one fragment of an upgraded connection.
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
        let lost = truncated || offset != ws.read_next || len as usize > limit;
        ws.read_next = if end as usize >= len as usize { 0 } else { end };
        if lost {
            return self.end_websocket(true);
        }
        ws.head.extend_from_slice(data);
        match read_handshake(&ws.head) {
            Handshake::Partial => Outcome::Pending,
            // Not a call: the server answered the upgrade with an error or an unread extension.
            Handshake::Refused => self.end_websocket(true),
            Handshake::Switched { deflate, .. } => {
                debug!(
                    "WebSocket of PID {} (model {:?}, compressed: {}) not read: frames are not parsed",
                    ctx.pid,
                    ws.model,
                    deflate.is_some()
                );
                self.end_websocket(true)
            }
        }
    }

    /// Stop reading the connection; with `skip` the kernel drops its later calls.
    pub(super) fn end_websocket(&mut self, skip: bool) -> Outcome {
        if self.is_websocket() {
            self.state = ProcessorState::Finished;
            self.skip |= skip;
        }
        Outcome::Pending
    }
}
