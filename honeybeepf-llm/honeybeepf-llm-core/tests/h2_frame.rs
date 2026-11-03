use std::collections::BTreeMap;

use honeybeepf_llm_core::h2::frame::{Broken, Frame, FrameReader, PREFACE};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ev {
    Headers(u32, Vec<u8>),
    BlockTooLarge(u32),
    Data(u32, Vec<u8>),
    End(u32),
}

type Read = (Vec<Ev>, Option<Broken>);
type Row = (&'static str, Vec<u8>, Vec<Ev>, Option<Broken>);
type BodyCheck = dyn Fn(&[u8]) -> bool;

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() as u32).to_be_bytes();
    [&len[1..], &[kind, flags], &stream.to_be_bytes(), payload].concat()
}

/// Events from the pieces fed in order; consecutive DATA of one stream merged.
fn read_pieces<'a>(mut r: FrameReader, pieces: impl IntoIterator<Item = &'a [u8]>) -> Read {
    let mut evs = Vec::new();
    for piece in pieces {
        r.read(piece, |f| match (f, evs.last_mut()) {
            (Frame::Data { stream, data }, Some(Ev::Data(s, d))) if *s == stream => {
                d.extend_from_slice(data)
            }
            (Frame::Data { stream, data }, _) => evs.push(Ev::Data(stream, data.to_vec())),
            (Frame::Headers { stream, block }, _) => evs.push(Ev::Headers(stream, block.to_vec())),
            (Frame::HeadersTooLarge { stream }, _) => evs.push(Ev::BlockTooLarge(stream)),
            (Frame::StreamEnd { stream }, _) => evs.push(Ev::End(stream)),
            (other, _) => panic!("unexpected {other:?}"),
        });
    }
    (evs, r.broken())
}

/// Reads `bytes` whole, one byte at a time, and cut once at every point; all must agree.
fn read_any_split(new: fn() -> FrameReader, bytes: &[u8]) -> Read {
    let whole = read_pieces(new(), [bytes]);
    assert_eq!(read_pieces(new(), bytes.chunks(1)), whole, "byte by byte");
    for cut in 1..bytes.len() {
        let (a, b) = bytes.split_at(cut);
        assert_eq!(read_pieces(new(), [a, b]), whole, "cut at {cut}");
    }
    whole
}

#[test]
fn test_frames_read_the_same_at_any_split() {
    use Broken::*;
    use Ev::*;
    let d = |s: &[u8]| s.to_vec();
    let late = frame(0, 0, 1, b"late");
    let lookalike = b"PRI * HTTP/2.0\r\n\0\0\x09\x01\x04\0\0\0\x01";
    #[rustfmt::skip]
    let rows: Vec<Row> = vec![
        ("padded DATA with END_STREAM", frame(0, 0x9, 1, b"\x03hi\0\0\0"), vec![Data(1, d(b"hi")), End(1)], None),
        ("HEADERS padded and with priority, then a plain one", [frame(1, 0x2c, 1, b"\x02\x80\0\0\x03\x10blk\0\0"), frame(1, 0x4, 3, b"two")].concat(),
            vec![Headers(1, d(b"blk")), Headers(3, d(b"two"))], None),
        ("HEADERS and two CONTINUATION, END_STREAM after the block",
            [frame(1, 0x1, 3, b"ab"), frame(9, 0, 3, b"cd"), frame(9, 0x4, 3, b"e")].concat(), vec![Headers(3, d(b"abcde")), End(3)], None),
        ("empty DATA with END_STREAM, padding only", [frame(0, 0x1, 1, b""), frame(0, 0x8, 3, b"\x02\0\0")].concat(), vec![End(1)], None),
        ("DATA that looks like a preface and a frame head", frame(0, 0, 1, lookalike), vec![Data(1, d(lookalike))], None),
        ("SETTINGS, PING, WINDOW_UPDATE, PRIORITY, RST_STREAM, GOAWAY, unknown type skipped", [
            frame(4, 0, 0, b"\0\x01\0\0\x10\0"), frame(6, 0, 0, &[0; 8]), frame(8, 0, 1, b"\0\0\x10\0"), frame(2, 0, 1, &[0; 5]),
            frame(3, 0, 1, b"\0\0\0\x08"), frame(7, 0, 0, b"\0\0\0\x05\0\0\0\0debug"), frame(0x42, 0x8, 1, b"zz"), late.clone()].concat(),
            vec![Data(1, d(b"late"))], None),
        ("CONTINUATION without HEADERS", [frame(9, 0x4, 1, b"x"), late.clone()].concat(), vec![], Some(Continuation)),
        ("DATA inside a header block", [frame(1, 0, 1, b"a"), late.clone()].concat(), vec![], Some(Continuation)),
        ("CONTINUATION on another stream", [frame(1, 0, 1, b"a"), frame(9, 0x4, 3, b"b")].concat(), vec![], Some(Continuation)),
        ("frame over the max frame size", [frame(0, 0, 1, &[7; 16_385]), late.clone()].concat(), vec![], Some(TooLarge)),
        ("DATA on stream 0", [frame(0, 0, 0, b"x"), late.clone()].concat(), vec![], Some(NoStream)),
        ("DATA padding past the payload", [frame(0, 0x8, 1, b"\x05ab"), late].concat(), vec![], Some(BadPadding)),
        ("HEADERS priority past the payload", frame(1, 0x24, 1, b"\0\0"), vec![], Some(BadPadding)),
        ("PADDED DATA without a pad length", frame(0, 0x8, 1, b""), vec![], Some(BadPadding)),
        ("PUSH_PROMISE", frame(5, 0x4, 1, b"\0\0\0\x02x"), vec![], Some(PushPromise)),
    ];
    for (name, bytes, want, broken) in rows {
        let got = if bytes.len() > 1000 {
            read_pieces(FrameReader::server(), bytes.chunks(4096))
        } else {
            read_any_split(FrameReader::server, &bytes)
        };
        assert_eq!(got, (want, broken), "{name}");
    }
}

#[test]
fn test_client_direction_opens_with_the_preface() {
    let ok = [&PREFACE[..], &frame(0, 0x1, 1, b"{}")].concat();
    let want = (vec![Ev::Data(1, b"{}".to_vec()), Ev::End(1)], None);
    assert_eq!(read_any_split(FrameReader::client, &ok), want);
    let h1 = b"POST /v1/chat/completions HTTP/1.1\r\n\r\n";
    assert_eq!(
        read_any_split(FrameReader::client, h1),
        (vec![], Some(Broken::Preface))
    );
}

#[test]
fn test_header_block_cap_counts_block_bytes_and_the_next_block_is_read() {
    // HEADERS with END_STREAM, then CONTINUATION frames of at most 16 KB until `size` bytes.
    let block = |size: usize| -> Vec<u8> {
        let chunks: Vec<_> = vec![b'a'; size]
            .chunks(16_384)
            .map(<[u8]>::to_vec)
            .collect();
        let last = chunks.len() - 1;
        let frames = chunks.iter().enumerate().map(|(i, c)| {
            let end_headers = if i == last { 0x4 } else { 0 };
            if i == 0 {
                frame(1, 0x1 | end_headers, 1, c)
            } else {
                frame(9, end_headers, 1, c)
            }
        });
        [
            frames.collect::<Vec<_>>().concat(),
            frame(1, 0x4, 3, b"next"),
        ]
        .concat()
    };
    let next = Ev::Headers(3, b"next".to_vec());
    let at_cap = vec![Ev::Headers(1, vec![b'a'; 65_536]), Ev::End(1), next.clone()];
    let over = vec![Ev::BlockTooLarge(1), Ev::End(1), next];
    for (size, want) in [(65_536, at_cap), (65_537, over)] {
        let bytes = block(size);
        for step in [1, 9, 4096, 16_393, bytes.len()] {
            let got = read_pieces(FrameReader::server(), bytes.chunks(step));
            assert_eq!(got, (want.clone(), None), "{size} {step}");
        }
    }
    // One large frame: its pad length, priority and padding are not block bytes.
    let mut r = FrameReader::server();
    r.set_max_frame_size(1 << 20);
    let payload = [&[200][..], &[0x80, 0, 0, 3, 16], &[b'a'; 65_536], &[0; 200]].concat();
    let got = read_pieces(r, [&frame(1, 0x2c, 1, &payload)[..]]);
    assert_eq!(got, (vec![Ev::Headers(1, vec![b'a'; 65_536])], None));
}

fn json_with(key: &'static str) -> impl Fn(&[u8]) -> bool {
    move |b| serde_json::from_slice::<serde_json::Value>(b).is_ok_and(|v| !v[key].is_null())
}

#[test]
fn test_recorded_connections_read_into_their_streams_at_any_split() {
    let sse = |b: &[u8]| b.starts_with(b"data: ") && b.ends_with(b"data: [DONE]\n\n");
    let gzip = |b: &[u8]| b.starts_with(&[0x1f, 0x8b]);
    let json = json_with("usage");
    #[rustfmt::skip]
    let rows: &[(&str, &[u32], &BodyCheck, Option<usize>)] = &[
        // recording, streams, response body check, response bytes per stream
        ("curl-json", &[1], &json, Some(239)),
        ("curl-sse", &[1], &sse, Some(306)),
        ("curl-large", &[1], &json, Some(40_237)),
        ("curl-gzip", &[1], &gzip, Some(172)),
        ("httpx-reuse", &[1, 3], &json, Some(239)),
        ("node26-openai-sse", &[1], &sse, Some(306)),
        ("node26-anthropic-json", &[1], &json, None),
    ];
    let request = json_with("model");
    for &(name, ids, body, size) in rows {
        for (dir, new) in [
            ("c2s", FrameReader::client as fn() -> FrameReader),
            ("s2c", FrameReader::server),
        ] {
            let path = format!(
                "{}/tests/testdata/h2/{name}.{dir}.bin",
                env!("CARGO_MANIFEST_DIR")
            );
            let bytes = std::fs::read(&path).expect(&path);
            let (evs, broken) = if bytes.len() > 2000 {
                let whole = read_pieces(new(), [&bytes[..]]);
                for step in [1, 7, 9, 4096, 16_393] {
                    assert_eq!(
                        read_pieces(new(), bytes.chunks(step)),
                        whole,
                        "{name}.{dir} {step}"
                    );
                }
                whole
            } else {
                read_any_split(new, &bytes)
            };
            let (mut heads, mut ends, mut data) = (vec![], vec![], BTreeMap::<u32, Vec<u8>>::new());
            for ev in evs {
                match ev {
                    Ev::Headers(s, _) => heads.push(s),
                    Ev::End(s) => ends.push(s),
                    Ev::Data(s, d) => data.entry(s).or_default().extend(d),
                    Ev::BlockTooLarge(s) => panic!("{name}.{dir}: block of stream {s} too large"),
                }
            }
            assert_eq!(
                (broken, &heads[..], &ends[..]),
                (None, ids, ids),
                "{name}.{dir}"
            );
            assert_eq!(
                data.keys().copied().collect::<Vec<_>>(),
                ids,
                "{name}.{dir}"
            );
            for (s, d) in &data {
                let ok = if dir == "c2s" {
                    request(d)
                } else {
                    body(d) && size.is_none_or(|n| d.len() == n)
                };
                assert!(ok, "{name}.{dir} stream {s}: {} bytes", d.len());
            }
        }
    }
}
