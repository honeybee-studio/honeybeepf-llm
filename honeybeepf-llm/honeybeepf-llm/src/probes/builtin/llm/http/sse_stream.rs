//! SSE responses past the response buffer limit: events read as they stream past, the body not kept.

use honeybeepf_llm_core::types::UsageInfo;

use super::encoding::SseDecoder;
use super::h1::{self, Framing};
use super::json_ends::BodyWalk;
use super::sse::SseCursor;
use crate::probes::builtin::llm::processor::MAX_RESPONSE_BUFFER_SIZE;

/// An SSE response too large to buffer, read event by event while its framing is followed.
pub struct SseStream {
    walk: BodyWalk,
    events: SseCursor,
    /// Set when the body carries a content coding, undone incrementally before the events are read.
    decoder: Option<SseDecoder>,
}

impl SseStream {
    /// Takes over a buffered HTTP/1.1 SSE response sent without a content coding.
    pub fn start(buffer: &[u8]) -> Option<Self> {
        let (framing, body) = h1::plain_sse_body(buffer)?;
        Self::take_over(framing, None, body)
    }

    /// Takes over a buffered HTTP/1.1 SSE response under a single content coding.
    pub fn start_encoded(buffer: &[u8]) -> Option<Self> {
        let (framing, body, coding) = h1::encoded_sse_body(buffer)?;
        Self::take_over(framing, Some(SseDecoder::new_for(&coding)?), body)
    }

    fn take_over(framing: Framing, decoder: Option<SseDecoder>, body: &[u8]) -> Option<Self> {
        let length = match framing {
            Framing::Length(len) => Some(len),
            Framing::Chunked => None,
            // Only the connection's close ends it, so no byte count runs out first.
            Framing::UntilClose => Some(usize::MAX),
        };
        let mut stream = Self {
            walk: BodyWalk::new(length),
            events: SseCursor::default(),
            decoder,
        };
        stream.feed(body).then_some(stream)
    }

    /// Reads more body bytes; false once the framing broke or one line outgrew the buffer limit.
    pub fn feed(&mut self, data: &[u8]) -> bool {
        let Self {
            walk,
            events,
            decoder,
        } = self;
        let followed = walk.feed(data, &mut |body| match decoder {
            Some(dec) => dec.push(body, &mut |decoded| {
                events.feed(decoded);
                events.partial_len() <= MAX_RESPONSE_BUFFER_SIZE
            }),
            None => events.feed(body),
        });
        followed
            && decoder.as_ref().is_none_or(SseDecoder::healthy)
            && events.partial_len() <= MAX_RESPONSE_BUFFER_SIZE
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

    use flate2::Compression;
    use flate2::write::{GzEncoder, ZlibEncoder};
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::fast());
        e.write_all(data).expect("gzip");
        e.finish().expect("gzip")
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::fast());
        e.write_all(data).expect("zlib");
        e.finish().expect("zlib")
    }

    fn zstd(data: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
    }

    /// Many filler events, one usage event, then `[DONE]`: a body past the response cap.
    fn big_sse() -> Vec<u8> {
        let filler = b"data: {\"choices\":[{\"delta\":{\"content\":\"pad\"}}]}\n\n";
        let reps = MAX_RESPONSE_BUFFER_SIZE / filler.len() + 64;
        let mut body = filler.repeat(reps);
        body.extend_from_slice(
            b"data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n",
        );
        body.extend_from_slice(b"data: [DONE]\n\n");
        body
    }

    fn encoded_head(coding: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Encoding: {coding}\r\n\
             Transfer-Encoding: chunked\r\n\r\n"
        )
        .into_bytes()
    }

    /// Stream a chunked, content-coded SSE body in small reads; None if a read broke the stream.
    fn stream_tokens(coding: &str, compressed: &[u8]) -> Option<(u64, u64)> {
        let mut stream = SseStream::start_encoded(&encoded_head(coding))?;
        let body = chunked(compressed, 4096);
        for read in body.chunks(1500) {
            if !stream.feed(read) {
                return None;
            }
        }
        tokens(stream.usage())
    }

    #[test]
    fn every_codec_streams_a_compressed_sse_past_16mb() {
        let sse = big_sse();
        assert!(
            sse.len() > MAX_RESPONSE_BUFFER_SIZE,
            "decoded must exceed the cap"
        );
        assert_eq!(stream_tokens("gzip", &gzip(&sse)), Some((11, 4)));
        assert_eq!(stream_tokens("deflate", &zlib(&sse)), Some((11, 4)));
        assert_eq!(stream_tokens("zstd", &zstd(&sse)), Some((11, 4)));
        // No in-crate brotli encoder here; the fixture decodes to a varied >16 MB SSE (testdata/README.md).
        let br = include_bytes!("testdata/sse-over-16mb.br");
        assert_eq!(stream_tokens("br", br), Some((11, 4)));
    }

    #[test]
    fn a_decompression_bomb_is_refused_at_the_cap() {
        // 17 MB with no event boundary: the cursor's partial line passes the cap, so the stream stops.
        let bomb = vec![b'a'; 17 * 1024 * 1024];
        assert_eq!(stream_tokens("gzip", &gzip(&bomb)), None);
        assert_eq!(stream_tokens("zstd", &zstd(&bomb)), None);
    }

    #[test]
    fn encoded_start_rejects_multiple_or_unknown_codings() {
        // Only a single, known coding is streamed past the limit; others stay response_too_large.
        assert!(SseStream::start_encoded(&encoded_head("gzip, zstd")).is_none());
        assert!(SseStream::start_encoded(&encoded_head("compress")).is_none());
        assert!(SseStream::start_encoded(HEAD.as_bytes()).is_none());
    }
}
