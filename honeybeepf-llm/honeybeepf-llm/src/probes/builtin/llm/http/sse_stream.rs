//! SSE responses past the response buffer limit: events read as they stream past, the body not kept.

use honeybeepf_llm_core::types::UsageInfo;

use super::h1::{self, Framing};
use super::json_ends::BodyWalk;
use super::sse::SseCursor;
use crate::probes::builtin::llm::processor::MAX_RESPONSE_BUFFER_SIZE;

/// An SSE response too large to buffer, read event by event while its framing is followed.
pub struct SseStream {
    walk: BodyWalk,
    events: SseCursor,
}

impl SseStream {
    /// Takes over a buffered HTTP/1.1 SSE response sent without a content coding.
    pub fn start(buffer: &[u8]) -> Option<Self> {
        let (framing, body) = h1::plain_sse_body(buffer)?;
        let length = match framing {
            Framing::Length(len) => Some(len),
            Framing::Chunked => None,
            // Only the connection's close ends it, so no byte count runs out first.
            Framing::UntilClose => Some(usize::MAX),
        };
        let mut stream = Self {
            walk: BodyWalk::new(length),
            events: SseCursor::default(),
        };
        stream.feed(body).then_some(stream)
    }

    /// Reads more body bytes; false once the framing broke or one line outgrew the buffer limit.
    pub fn feed(&mut self, data: &[u8]) -> bool {
        let Self { walk, events } = self;
        let followed = walk.feed(data, &mut |data| events.feed(data));
        followed && events.partial_len() <= MAX_RESPONSE_BUFFER_SIZE
    }

    /// The merged usage once the events read so far ended the stream.
    pub fn usage(&self) -> Option<UsageInfo> {
        self.events.usage()
    }
}

#[cfg(test)]
mod tests {
    use super::super::sse::parse_sse_body;
    use super::super::test_support::tokens;
    use super::*;

    /// Anthropic events with `\r\n` line ends and data lines that look like stream ends or chunk ends.
    const EVENTS: &str = concat!(
        "event: message_start\r\n",
        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-test\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"data: [DONE]\\n\\n0\\r\\n\\r\\n\"}}\r\n\r\n",
        "data: 0\r\n\r\n",
        "event: message_delta\r\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\r\n\r\n",
        "event: message_stop\r\n",
        "data: {\"type\":\"message_stop\"}\r\n\r\n",
    );

    const HEAD: &str =
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";

    fn chunked(body: &[u8], size: usize) -> Vec<u8> {
        let mut framed = Vec::new();
        for c in body.chunks(size) {
            framed.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
            framed.extend_from_slice(c);
            framed.extend_from_slice(b"\r\n");
        }
        framed.extend_from_slice(b"0\r\n\r\n");
        framed
    }

    #[test]
    fn usage_at_every_split_matches_the_whole_body() {
        let want = tokens(parse_sse_body(EVENTS.as_bytes()));
        assert_eq!(want, Some((25, 15)));
        let body = chunked(EVENTS.as_bytes(), 7);
        for split in 0..=body.len() {
            let first = [HEAD.as_bytes(), &body[..split]].concat();
            let mut stream = SseStream::start(&first).expect("SSE");
            assert!(stream.feed(&body[split..]), "{split}");
            assert_eq!(tokens(stream.usage()), want, "{split}");
        }
        // The last event cut off: never ended, so no usage.
        let cut = chunked(&EVENTS.as_bytes()[..EVENTS.len() - 5], 7);
        let mut stream = SseStream::start(HEAD.as_bytes()).expect("SSE");
        assert!(stream.feed(&cut));
        assert_eq!(tokens(stream.usage()), None);
    }

    #[test]
    fn usage_split_across_the_takeover_is_merged_without_keeping_the_body() {
        let (start, end) = EVENTS.split_at(EVENTS.find("event: message_delta").expect("delta"));
        let delta = "event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\r\n\r\n";
        let body = [start, &delta.repeat((20 << 20) / delta.len()), end].concat();
        let framed = chunked(body.as_bytes(), 8191);
        let (first, rest) = framed.split_at(MAX_RESPONSE_BUFFER_SIZE);
        let mut stream = SseStream::start(&[HEAD.as_bytes(), first].concat()).expect("SSE");
        assert!(rest.chunks(16384).all(|read| stream.feed(read)));
        assert_eq!(tokens(stream.usage()), Some((25, 15)));
        assert!(stream.events.partial_len() < delta.len());
    }

    #[test]
    fn broken_framing_or_an_endless_line_stops_the_stream() {
        let mut stream = SseStream::start(HEAD.as_bytes()).expect("SSE");
        assert!(!stream.feed(b"zz\r\ndata: x\n\n"));

        let head =
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        let mut stream = SseStream::start(head.as_bytes()).expect("SSE");
        let line = vec![b'x'; MAX_RESPONSE_BUFFER_SIZE];
        assert!(stream.feed(&line));
        assert!(!stream.feed(b"x"));
    }

    #[test]
    fn only_unencoded_sse_is_taken_over() {
        for head in [
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n",
        ] {
            assert!(SseStream::start(head.as_bytes()).is_none(), "{head:?}");
        }
    }
}
