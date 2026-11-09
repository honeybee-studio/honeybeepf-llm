//! HTTP `Content-Encoding` decoding of response bodies.

use std::borrow::Cow;
use std::io::{self, Read, Write};

use brotli_decompressor::{BrotliDecoderParameter, Decompressor, DecompressorWriter};
use flate2::read::{DeflateDecoder, MultiGzDecoder, ZlibDecoder};
use flate2::write::{MultiGzDecoder as GzWriter, ZlibDecoder as ZlibWriter};
use ruzstd::decoding::{FrameDecoder, StreamingDecoder};

use crate::probes::builtin::llm::processor::MAX_RESPONSE_BUFFER_SIZE;

/// RFC 9659 limits the window of the zstd content coding to 8 MB.
const ZSTD_MAX_WINDOW: u64 = 8 * 1024 * 1024;

/// Compressed bytes held for a zstd block not fully arrived (one block is ≤ 128 KB plus its header).
const ZSTD_HOLDBACK_MAX: usize = 256 * 1024;

/// Decoded bytes handed to the SSE cursor per step, so a decompression bomb is caught near the cap.
const STREAM_CHUNK: usize = 16 * 1024;

/// Every stage may expand up to the response cap, so a long chain is refused rather than decoded.
const MAX_CODINGS: usize = 4;

/// Content codings other than identity, in the order the server applied them.
fn codings<'a>(
    headers: &'a [httparse::Header<'a>],
) -> impl DoubleEndedIterator<Item = &'a [u8]> + 'a {
    headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("Content-Encoding"))
        .flat_map(|h| h.value.split(|&b| b == b','))
        .map(<[u8]>::trim_ascii)
        .filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case(b"identity"))
}

/// Whether the body carries a content coding that has to be undone before parsing.
pub(super) fn is_encoded(headers: &[httparse::Header<'_>]) -> bool {
    codings(headers).next().is_some()
}

/// The one content coding to undo, or None for none or more than one: only a single coding is
/// streamed incrementally past the buffer limit.
pub(super) fn single_coding(headers: &[httparse::Header<'_>]) -> Option<Box<[u8]>> {
    let mut it = codings(headers);
    let first = it.next()?;
    it.next().is_none().then(|| Box::from(first))
}

/// Undo the content codings last-applied first; None on an unknown coding, corrupt data or output over the response cap.
pub(super) fn decode<'a>(
    body: Cow<'a, [u8]>,
    headers: &[httparse::Header<'_>],
) -> Option<Cow<'a, [u8]>> {
    if codings(headers).nth(MAX_CODINGS).is_some() {
        return None;
    }
    let mut data = body;
    for coding in codings(headers).rev() {
        data = Cow::Owned(decode_one(&data, coding)?);
    }
    Some(data)
}

fn decode_one(data: &[u8], coding: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    if coding.eq_ignore_ascii_case(b"gzip") || coding.eq_ignore_ascii_case(b"x-gzip") {
        // A body may be several gzip members, e.g. one per flushed event.
        read_capped(MultiGzDecoder::new(data), &mut out)?;
    } else if coding.eq_ignore_ascii_case(b"deflate") {
        // RFC 9110 deflate is zlib-wrapped, but some servers send raw deflate under the same name.
        if read_capped(ZlibDecoder::new(data), &mut out).is_none() {
            out.clear();
            read_capped(DeflateDecoder::new(data), &mut out)?;
        }
    } else if coding.eq_ignore_ascii_case(b"br") {
        let mut decoder = Decompressor::new(data, 4096);
        // On by default; a large-window header alone can demand a 1 GiB window (RUSTSEC-2026-0348).
        decoder.set_parameter(BrotliDecoderParameter::BROTLI_DECODER_PARAM_LARGE_WINDOW, 0);
        read_capped(decoder, &mut out)?;
    } else if coding.eq_ignore_ascii_case(b"zstd") {
        let mut rest = data;
        while !rest.is_empty() {
            let decoder =
                StreamingDecoder::new_with_max_window_size(&mut rest, ZSTD_MAX_WINDOW).ok()?;
            read_capped(decoder, &mut out)?;
        }
    } else {
        return None;
    }
    Some(out)
}

/// Append the decoder's output; None on a decode error or once the output passes the response cap.
fn read_capped(decoder: impl Read, out: &mut Vec<u8>) -> Option<()> {
    let room = MAX_RESPONSE_BUFFER_SIZE.saturating_sub(out.len()) as u64;
    decoder.take(room + 1).read_to_end(out).ok()?;
    (out.len() <= MAX_RESPONSE_BUFFER_SIZE).then_some(())
}

/// A single content coding undone incrementally for an SSE body past the buffer limit, keeping the
/// decoder state across reads. The window is capped (zstd 8 MB, brotli 16 MB); a decode past the
/// response cap or a stuck block fails the stream.
pub(super) struct SseDecoder {
    inner: Inner,
    failed: bool,
}

enum Inner {
    Gz(Box<GzWriter<CappedSink>>),
    Zlib(Box<ZlibWriter<CappedSink>>),
    Br(Box<DecompressorWriter<CappedSink>>),
    Zstd {
        decoder: Box<FrameDecoder>,
        holdback: Vec<u8>,
    },
}

impl SseDecoder {
    /// A streaming decoder for one coding (`deflate` is read as zlib, RFC 9110), or None if unknown.
    pub(super) fn new_for(coding: &[u8]) -> Option<Self> {
        let inner =
            if coding.eq_ignore_ascii_case(b"gzip") || coding.eq_ignore_ascii_case(b"x-gzip") {
                Inner::Gz(Box::new(GzWriter::new(CappedSink::default())))
            } else if coding.eq_ignore_ascii_case(b"deflate") {
                Inner::Zlib(Box::new(ZlibWriter::new(CappedSink::default())))
            } else if coding.eq_ignore_ascii_case(b"br") {
                let mut w = DecompressorWriter::new(CappedSink::default(), 4096);
                // A large-window header alone can demand a 1 GiB window (RUSTSEC-2026-0348).
                w.set_parameter(BrotliDecoderParameter::BROTLI_DECODER_PARAM_LARGE_WINDOW, 0);
                Inner::Br(Box::new(w))
            } else if coding.eq_ignore_ascii_case(b"zstd") {
                let mut decoder = Box::new(FrameDecoder::new());
                decoder.set_max_window_size(ZSTD_MAX_WINDOW);
                Inner::Zstd {
                    decoder,
                    holdback: Vec::new(),
                }
            } else {
                return None;
            };
        Some(Self {
            inner,
            failed: false,
        })
    }

    /// Decode `input`, handing decoded bytes to `out` in order; `out` returns false past the cap.
    pub(super) fn push(&mut self, input: &[u8], out: &mut impl FnMut(&[u8]) -> bool) {
        if self.failed {
            return;
        }
        let ok = match &mut self.inner {
            Inner::Gz(d) => pump_writer(&mut **d, input, out),
            Inner::Zlib(d) => pump_writer(&mut **d, input, out),
            Inner::Br(d) => pump_writer(&mut **d, input, out),
            Inner::Zstd { decoder, holdback } => pump_zstd(decoder, holdback, input, out),
        };
        self.failed = !ok;
    }

    pub(super) fn healthy(&self) -> bool {
        !self.failed
    }
}

/// Collects a write-adapter decoder's output but refuses past the response cap, so a decompression
/// bomb fails the write instead of growing memory.
#[derive(Default)]
struct CappedSink(Vec<u8>);

impl Write for CappedSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.0.len() + buf.len() > MAX_RESPONSE_BUFFER_SIZE {
            return Err(io::ErrorKind::WriteZero.into());
        }
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Get a write-adapter decoder's capped output buffer, to drain after each write.
trait InnerSink: Write {
    fn sink(&mut self) -> &mut CappedSink;
}
impl InnerSink for GzWriter<CappedSink> {
    fn sink(&mut self) -> &mut CappedSink {
        self.get_mut()
    }
}
impl InnerSink for ZlibWriter<CappedSink> {
    fn sink(&mut self) -> &mut CappedSink {
        self.get_mut()
    }
}
impl InnerSink for DecompressorWriter<CappedSink> {
    fn sink(&mut self) -> &mut CappedSink {
        self.get_mut()
    }
}

/// Feed compressed input to a write-adapter decoder and forward its decoded output in capped chunks.
fn pump_writer(
    dec: &mut impl InnerSink,
    input: &[u8],
    out: &mut impl FnMut(&[u8]) -> bool,
) -> bool {
    // flush drains this write's decoded output into the sink; it sits in the decoder's buffer otherwise.
    if dec.write_all(input).is_err() || dec.flush().is_err() {
        return false;
    }
    let produced = std::mem::take(&mut dec.sink().0);
    produced.chunks(STREAM_CHUNK).all(out)
}

/// Decode zstd blocks from the holdback, forwarding output; keeps a partial block for the next read.
fn pump_zstd(
    decoder: &mut FrameDecoder,
    holdback: &mut Vec<u8>,
    input: &[u8],
    out: &mut impl FnMut(&[u8]) -> bool,
) -> bool {
    holdback.extend_from_slice(input);
    let mut target = [0u8; STREAM_CHUNK];
    let mut consumed = 0;
    loop {
        // A split header or block: keep the bytes unless the holdback already exceeds a block.
        let Ok((read, written)) = decoder.decode_from_to(&holdback[consumed..], &mut target) else {
            holdback.drain(..consumed);
            return holdback.len() <= ZSTD_HOLDBACK_MAX;
        };
        consumed += read;
        if written > 0 && !out(&target[..written]) {
            holdback.drain(..consumed);
            return false;
        }
        if read == 0 && written == 0 {
            break;
        }
    }
    holdback.drain(..consumed);
    holdback.len() <= ZSTD_HOLDBACK_MAX
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::{DeflateEncoder, GzEncoder, ZlibEncoder};

    use super::super::{Http11Parser, ProtocolParser};
    use super::*;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(data).expect("gzip");
        enc.finish().expect("gzip")
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(data).expect("zlib");
        enc.finish().expect("zlib")
    }

    fn raw_deflate(data: &[u8]) -> Vec<u8> {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(data).expect("deflate");
        enc.finish().expect("deflate")
    }

    /// Fixed streams: the `brotli` encoder crate pins brotli-decompressor ~6.0, which has no large-window switch.
    fn br(data: &[u8]) -> Vec<u8> {
        let fixture: &[u8] = if data == JSON.as_bytes() {
            include_bytes!("testdata/json.br")
        } else if data == SSE.as_bytes() {
            include_bytes!("testdata/sse.br")
        } else if data == vec![b'a'; MAX_RESPONSE_BUFFER_SIZE] {
            include_bytes!("testdata/at-cap.br")
        } else if data == vec![b'a'; MAX_RESPONSE_BUFFER_SIZE + 1] {
            include_bytes!("testdata/over-cap.br")
        } else {
            panic!("no brotli fixture for {} bytes", data.len())
        };
        fixture.to_vec()
    }

    fn zstd(data: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
    }

    type Encode = fn(&[u8]) -> Vec<u8>;

    const CODINGS: [(&str, Encode); 6] = [
        ("gzip", gzip),
        ("x-gzip", gzip),
        ("deflate", zlib),
        ("deflate", raw_deflate),
        ("br", br),
        ("zstd", zstd),
    ];

    const JSON: &str = "{\"id\":\"m1\",\"type\":\"message\",\"model\":\"claude-test\",\"content\":[],\
                        \"usage\":{\"input_tokens\":25,\"output_tokens\":15}}";

    const SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n",
        "data: [DONE]\n\n",
    );

    fn json_response(coding: &str, body: &[u8]) -> Vec<u8> {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: {coding}\r\n\
             Content-Length: {}\r\n\r\n",
            body.len()
        );
        [head.as_bytes(), body].concat()
    }

    /// A chunked SSE response split mid-body, as a server flushing its compressor would send it.
    fn sse_response(coding: &str, body: &[u8], last_chunk: bool) -> Vec<u8> {
        let (a, b) = body.split_at(body.len() / 2);
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Encoding: {coding}\r\n\
             Transfer-Encoding: chunked\r\n\r\n"
        )
        .into_bytes();
        for chunk in [a, b] {
            resp.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            resp.extend_from_slice(chunk);
            resp.extend_from_slice(b"\r\n");
        }
        if last_chunk {
            resp.extend_from_slice(b"0\r\n\r\n");
        }
        resp
    }

    fn tokens(resp: &[u8]) -> Option<(u64, u64)> {
        Http11Parser
            .parse_response(resp)
            .map(|u| (u.prompt_tokens, u.completion_tokens))
    }

    #[test]
    fn every_coding_decodes_json_and_chunked_sse() {
        let got: Vec<_> = CODINGS
            .iter()
            .map(|(coding, encode)| {
                let json = json_response(coding, &encode(JSON.as_bytes()));
                let sse = sse_response(coding, &encode(SSE.as_bytes()), true);
                (*coding, tokens(&json), tokens(&sse))
            })
            .collect();
        let want: Vec<_> = CODINGS
            .iter()
            .map(|(coding, _)| (*coding, Some((25, 15)), Some((11, 4))))
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn encoded_chunked_body_with_trailers_is_decoded_at_the_last_chunk() {
        let mut resp = sse_response("gzip", &gzip(SSE.as_bytes()), false);
        resp.extend_from_slice(b"0\r\nX-Request-Id: r1\r\n\r\n");
        assert_eq!(tokens(&resp), Some((11, 4)));
        assert_eq!(tokens(&resp[..resp.len() - 2]), None);
    }

    #[test]
    fn encoded_body_after_100_continue_is_decoded() {
        let interim = b"HTTP/1.1 100 Continue\r\n\r\n";
        let json = [interim, &json_response("gzip", &gzip(JSON.as_bytes()))[..]].concat();
        assert_eq!(tokens(&json), Some((25, 15)));
        let sse = [
            interim,
            &sse_response("gzip", &gzip(SSE.as_bytes()), true)[..],
        ]
        .concat();
        assert_eq!(tokens(&sse), Some((11, 4)));
    }

    #[test]
    fn stacked_codings_are_undone_last_first() {
        let body = zstd(&gzip(JSON.as_bytes()));
        assert_eq!(tokens(&json_response("gzip, zstd", &body)), Some((25, 15)));
        assert_eq!(tokens(&json_response("zstd, gzip", &body)), None);

        let body = zstd(&zlib(SSE.as_bytes()));
        let resp = sse_response("deflate\r\nContent-Encoding: identity, zstd", &body, true);
        assert_eq!(tokens(&resp), Some((11, 4)));

        let four = gzip(&gzip(&gzip(&gzip(JSON.as_bytes()))));
        let header = "gzip, gzip, gzip, gzip";
        assert_eq!(tokens(&json_response(header, &four)), Some((25, 15)));
        let five = gzip(&four);
        let header = "gzip, gzip, gzip, gzip, gzip";
        assert_eq!(tokens(&json_response(header, &five)), None);
    }

    #[test]
    fn multi_member_gzip_and_multi_frame_zstd_decode_whole() {
        let (a, b) = SSE.as_bytes().split_at(SSE.len() / 2);
        let members = [gzip(a), gzip(b)].concat();
        assert_eq!(tokens(&sse_response("gzip", &members, true)), Some((11, 4)));
        // libzstd frames with content checksums, not ruzstd's own encoder output.
        let frames = include_bytes!("testdata/sse-2frames.zst");
        assert_eq!(tokens(&sse_response("zstd", frames, true)), Some((11, 4)));
        let json = include_bytes!("testdata/json.zst");
        assert_eq!(tokens(&json_response("zstd", json)), Some((25, 15)));
    }

    #[test]
    fn identity_passes_through_and_unknown_coding_is_none() {
        assert_eq!(
            tokens(&json_response("identity", JSON.as_bytes())),
            Some((25, 15))
        );
        assert_eq!(tokens(&json_response("compress", JSON.as_bytes())), None);
    }

    #[test]
    fn encoded_body_waits_for_its_framed_end() {
        let sse = sse_response("br", &br(SSE.as_bytes()), false);
        assert_eq!(tokens(&sse), None);
        let mut json = json_response("zstd", &zstd(JSON.as_bytes()));
        json.pop();
        assert_eq!(tokens(&json), None);
    }

    #[test]
    fn output_over_the_response_cap_is_none() {
        let at_cap = vec![b'a'; MAX_RESPONSE_BUFFER_SIZE];
        let over_cap = vec![b'a'; MAX_RESPONSE_BUFFER_SIZE + 1];
        for (coding, encode) in CODINGS {
            let decoded = decode_one(&encode(&at_cap), coding.as_bytes());
            assert_eq!(decoded.map(|d| d.len()), Some(at_cap.len()), "{coding}");
            assert!(
                decode_one(&encode(&over_cap), coding.as_bytes()).is_none(),
                "{coding}"
            );
        }
    }

    #[test]
    fn corrupt_or_truncated_data_is_none() {
        let checked: [(&str, Encode); 3] = [("gzip", gzip), ("br", br), ("zstd", zstd)];
        for (coding, encode) in checked {
            let data = encode(SSE.as_bytes());
            let half = &data[..data.len() / 2];
            assert!(decode_one(half, coding.as_bytes()).is_none(), "{coding}");
        }
        for coding in ["gzip", "deflate", "zstd"] {
            assert!(
                decode_one(b"\xff\xff\xff\xff", coding.as_bytes()).is_none(),
                "{coding}"
            );
        }
    }

    #[test]
    fn brotli_large_window_stream_is_rejected() {
        let data = include_bytes!("testdata/json-large-window.br");
        let mut default = Vec::new();
        Decompressor::new(&data[..], 4096)
            .read_to_end(&mut default)
            .expect("default decoder accepts large windows");
        assert_eq!(default, JSON.as_bytes());
        assert_eq!(decode_one(data, b"br"), None);
    }
}
