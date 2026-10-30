//! Chunked bodies and what each of the chunk walkers makes of them, so a change to one shows up here.

/// Where the json_ends walk stands after the whole body.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum Walked {
    Pending,
    Ended,
    Broken,
}

/// Marks a body passed on unchanged.
pub(super) const RAW: &[u8] = b"<unchanged>";

pub(super) struct ChunkCase {
    pub(super) body: &'static [u8],
    /// `byte_utils::decode_chunked_body`.
    pub(super) decoded: &'static [u8],
    /// h1 `Dechunker`: the bytes it passes on and whether it stopped.
    pub(super) dechunked: (&'static [u8], bool),
    /// h1 `chunked_ended`.
    pub(super) ended: bool,
    /// json_ends `BodyWalk`: the data bytes it kept and where it stands.
    pub(super) walked: (&'static [u8], Walked),
}

impl ChunkCase {
    /// `want`, or the whole body for `RAW`.
    pub(super) fn or_body(&self, want: &'static [u8]) -> &'static [u8] {
        if want == RAW { self.body } else { want }
    }
}

/// Boundaries, desyncs, extensions and trailers, with the walkers' answers as they are today.
#[rustfmt::skip]
pub(super) fn chunk_cases() -> Vec<ChunkCase> {
    use Walked::{Broken, Ended, Pending};
    // A size line longer than the json_ends line limit; leading zeros keep it a valid size.
    let long_size: &'static [u8] =
        Box::leak([&b"0".repeat(5000)[..], b"5\r\nhello\r\n0\r\n\r\n"].concat().into_boxed_slice());
    type Bytes = &'static [u8];
    type Row = (Bytes, Bytes, (Bytes, bool), bool, (Bytes, Walked));
    let rows: &[Row] = &[
        // body                                    decode_chunked_body   Dechunker                   chunked_ended  BodyWalk
        (b"5\r\nhello\r\n0\r\n\r\n",               b"hello",             (b"hello", true),            true,  (b"hello", Ended)),
        (b"5;x=1\r\nhello\r\n0\r\n\r\n",           RAW,                  (RAW, true),                 true,  (b"hello", Ended)),
        (b"5 ;x\r\nhello\r\n0\r\n\r\n",            RAW,                  (RAW, true),                 true,  (b"hello", Ended)),
        (b"5\r\nhello\r\n0;x=1\r\n\r\n",           b"hello",             (b"hello", true),            true,  (b"hello", Ended)),
        (b" 5 \r\nhello\r\n0\r\n\r\n",             b"hello",             (b"hello", true),            true,  (b"hello", Ended)),
        (b"+5\r\nhello\r\n0\r\n\r\n",              b"hello",             (b"hello", true),            true,  (b"hello", Ended)),
        (b"5;\xff\r\nhello\r\n0\r\n\r\n",          RAW,                  (RAW, true),                 true,  (b"", Broken)),
        (b"5\r\nhello\r\n0\r\nT: 1\r\n\r\n",       b"hello",             (b"hello", true),            true,  (b"hello", Ended)),
        (b"5\r\nhello\r\n0\r\nT: 1\r\n",           b"hello",             (b"hello", true),            false, (b"hello", Pending)),
        (b"5\r\nhello\r\n0\r\nT\n\r\n",            b"hello",             (b"hello", true),            false, (b"hello", Broken)),
        (b"0\r\nT\xff\r\n\r\n",                    RAW,                  (RAW, true),                 true,  (b"", Ended)),
        (b"0\r\n\r\n",                             RAW,                  (RAW, true),                 true,  (b"", Ended)),
        (b"5\r\n0\r\n\r\n\r\n0\r\n\r\n",           b"0\r\n\r\n",         (b"0\r\n\r\n", true),        true,  (b"0\r\n\r\n", Ended)),
        // Bytes after the data that are not CRLF.
        (b"5\r\nhelloXY3\r\nabc\r\n0\r\n\r\n",     b"helloabc",          (b"helloabc", true),         true,  (b"hello", Broken)),
        (b"5\r\nhelloX",                           b"hello",             (b"hello", false),           false, (b"hello", Pending)),
        (b"5\r\nhello\r\r\n0\r\n\r\n",             b"hello",             (b"hello", true),            true,  (b"hello", Broken)),
        // Size lines that do not parse.
        (b"5\r\nhello\r\nzz\r\n0\r\n\r\n",         b"hello",             (b"hello", true),            true,  (b"hello", Broken)),
        (b"zz\r\nhello",                           RAW,                  (RAW, true),                 false, (b"", Broken)),
        (b"\xff5\r\nhello\r\n0\r\n\r\n",           RAW,                  (RAW, true),                 true,  (b"", Broken)),
        (b"5\nhello\r\n0\r\n\r\n",                 RAW,                  (RAW, true),                 true,  (b"", Broken)),
        (b"5\n\r\nhello\r\n0\r\n\r\n",             b"hello",             (b"hello", true),            true,  (b"", Broken)),
        (long_size,                                b"hello",             (b"hello", true),            true,  (b"", Broken)),
        // Sizes at and past the response buffer and usize.
        (b"1000000\r\n{}0\r\n\r\n",                b"{}0\r\n\r\n",       (b"{}0\r\n\r\n", false),     false, (b"{}0\r\n\r\n", Pending)),
        (b"1000001\r\n{}0\r\n\r\n",                b"{}0\r\n\r\n",       (b"{}0\r\n\r\n", false),     true,  (b"{}0\r\n\r\n", Pending)),
        (b"ffffffffffffffff\r\n{}\r\n0\r\n\r\n",   b"{}\r\n0\r\n\r\n",   (b"{}\r\n0\r\n\r\n", false), true,  (b"{}\r\n0\r\n\r\n", Pending)),
        (b"10000000000000000\r\n{}",               RAW,                  (RAW, true),                 false, (b"", Broken)),
        // Cut short.
        (b"",                                      RAW,                  (b"", false),                false, (b"", Pending)),
        (b"hello",                                 RAW,                  (b"", false),                false, (b"", Pending)),
        (b"5\r\nabc",                              b"abc",               (b"abc", false),             false, (b"abc", Pending)),
        (b"5\r\nhello\r",                          b"hello",             (b"hello", false),           false, (b"hello", Pending)),
        (b"5\r\nhello\r\n",                        b"hello",             (b"hello", false),           false, (b"hello", Pending)),
    ];
    rows.iter()
        .map(|&(body, decoded, dechunked, ended, walked)| ChunkCase { body, decoded, dechunked, ended, walked })
        .collect()
}
