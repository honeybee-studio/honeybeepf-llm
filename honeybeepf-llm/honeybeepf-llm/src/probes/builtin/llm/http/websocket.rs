//! WebSocket calls (OpenAI Realtime, Responses over a WebSocket): the upgrade request to a listed provider
//! and the server's 101 answer.

use super::{MatchedBy, matched_by, registry::PROVIDER_REGISTRY};

/// Longest 101 answer read; real ones are under 1 KB.
const MAX_HANDSHAKE: usize = 16 * 1024;

/// A WebSocket upgrade request to a listed provider's Realtime or Responses path.
#[derive(Debug, PartialEq, Eq)]
pub struct Upgrade {
    /// The path without its query, which can carry a credential (Azure `api-key=`).
    pub path: String,
    /// `model` or `deployment` from the query, the only query parameters read.
    pub model: Option<String>,
    pub matched: MatchedBy,
}

/// The WebSocket upgrade a complete GET head asks for; only a listed host counts, not a shape match.
pub fn detect_upgrade(buffer: &[u8]) -> Option<Upgrade> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    if !req.parse(buffer).ok()?.is_complete() || req.method != Some("GET") {
        return None;
    }
    let header = |name: &str| {
        req.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| String::from_utf8_lossy(h.value))
    };
    if !header("Upgrade")?.trim().eq_ignore_ascii_case("websocket") {
        return None;
    }
    let host = header("Host").unwrap_or_default();
    let path = req.path?;
    let provider = PROVIDER_REGISTRY.find_provider(&host, path)?;
    if provider.record_only {
        return None;
    }
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let model = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| matches!(*key, "model" | "deployment"))
        .and_then(|(_, value)| super::request_model::from_query(value));
    Some(Upgrade {
        path: path.to_string(),
        model,
        matched: matched_by(Some(provider)),
    })
}

/// How far the server's answer to the upgrade has come.
#[derive(Debug, PartialEq, Eq)]
pub enum Handshake {
    /// The head has not ended yet.
    Partial,
    /// `101 Switching Protocols`: frames follow the head's `len` bytes, compressed when `deflate` is set.
    Switched {
        len: usize,
        deflate: Option<Deflate>,
    },
    /// Any other answer, or an extension that changes the framing in a way not read.
    Refused,
}

/// The permessage-deflate parameters the server agreed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deflate {
    /// The server starts each message's compression afresh.
    pub no_context_takeover: bool,
}

/// Read the server's answer to the upgrade from the start of `buf`.
pub fn read_handshake(buf: &[u8]) -> Handshake {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut resp = httparse::Response::new(&mut headers);
    let len = match resp.parse(buf) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) if buf.len() < MAX_HANDSHAKE => return Handshake::Partial,
        _ => return Handshake::Refused,
    };
    if resp.code != Some(101) {
        return Handshake::Refused;
    }
    let mut deflate = None;
    for h in resp.headers.iter() {
        if !h.name.eq_ignore_ascii_case("Sec-WebSocket-Extensions") {
            continue;
        }
        for extension in String::from_utf8_lossy(h.value).split(',') {
            let mut params = extension.split(';').map(str::trim);
            if params.next() != Some("permessage-deflate") || deflate.is_some() {
                return Handshake::Refused;
            }
            deflate = Some(Deflate {
                no_context_takeover: params.any(|p| p == "server_no_context_takeover"),
            });
        }
    }
    Handshake::Switched { len, deflate }
}

#[cfg(test)]
mod tests;
