//! HPACK (RFC 7541) primitives: integers, string literals and the static table.

use super::huffman;

/// Why a header block could not be decoded.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HpackError {
    /// The block ended inside an integer or a string.
    Truncated,
    /// An integer longer than five continuation bytes or past `u32`.
    IntegerOverflow,
    /// Huffman data with EOS, or padding that is longer than 7 bits or not all ones.
    Huffman,
}

/// The static table (RFC 7541 Appendix A); index 1 is `STATIC_TABLE[0]`.
#[rustfmt::skip]
pub static STATIC_TABLE: [(&str, &str); 61] = [
    (":authority", ""), (":method", "GET"), (":method", "POST"), (":path", "/"), (":path", "/index.html"),
    (":scheme", "http"), (":scheme", "https"), (":status", "200"), (":status", "204"), (":status", "206"),
    (":status", "304"), (":status", "400"), (":status", "404"), (":status", "500"), ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"), ("accept-language", ""), ("accept-ranges", ""), ("accept", ""),
    ("access-control-allow-origin", ""), ("age", ""), ("allow", ""), ("authorization", ""), ("cache-control", ""),
    ("content-disposition", ""), ("content-encoding", ""), ("content-language", ""), ("content-length", ""),
    ("content-location", ""), ("content-range", ""), ("content-type", ""), ("cookie", ""), ("date", ""),
    ("etag", ""), ("expect", ""), ("expires", ""), ("from", ""), ("host", ""), ("if-match", ""),
    ("if-modified-since", ""), ("if-none-match", ""), ("if-range", ""), ("if-unmodified-since", ""),
    ("last-modified", ""), ("link", ""), ("location", ""), ("max-forwards", ""), ("proxy-authenticate", ""),
    ("proxy-authorization", ""), ("range", ""), ("referer", ""), ("refresh", ""), ("retry-after", ""),
    ("server", ""), ("set-cookie", ""), ("strict-transport-security", ""), ("transfer-encoding", ""),
    ("user-agent", ""), ("vary", ""), ("via", ""), ("www-authenticate", ""),
];

/// Decodes an integer with a `prefix`-bit prefix, 1 to 8 (RFC 7541 §5.1); returns it and the bytes it took.
pub fn decode_int(input: &[u8], prefix: u8) -> Result<(u32, usize), HpackError> {
    debug_assert!((1..=8).contains(&prefix), "HPACK prefixes are 1 to 8 bits");
    let first = *input.first().ok_or(HpackError::Truncated)?;
    let max = (1u64 << prefix) - 1;
    let mut value = u64::from(first) & max;
    if value < max {
        return Ok((value as u32, 1));
    }
    // Five continuation bytes add at most 35 bits, so the sum cannot overflow u64.
    for (i, &byte) in input.iter().enumerate().skip(1).take(5) {
        value += u64::from(byte & 0x7f) << (7 * (i - 1));
        if byte & 0x80 == 0 {
            let value = u32::try_from(value).map_err(|_| HpackError::IntegerOverflow)?;
            return Ok((value, i + 1));
        }
    }
    if input.len() > 5 {
        Err(HpackError::IntegerOverflow)
    } else {
        Err(HpackError::Truncated)
    }
}

/// Appends the string literal at the start of `input` (RFC 7541 §5.2) to `out`, which an error leaves as it was; returns the bytes it took.
pub fn decode_string(input: &[u8], out: &mut Vec<u8>) -> Result<usize, HpackError> {
    let huffman = input.first().is_some_and(|b| b & 0x80 != 0);
    let (len, used) = decode_int(input, 7)?;
    let end = used
        .checked_add(len as usize)
        .filter(|&end| end <= input.len())
        .ok_or(HpackError::Truncated)?;
    let bytes = &input[used..end];
    if huffman {
        let start = out.len();
        if let Err(e) = huffman::decode(bytes, out) {
            out.truncate(start);
            return Err(e);
        }
    } else {
        out.extend_from_slice(bytes);
    }
    Ok(end)
}
