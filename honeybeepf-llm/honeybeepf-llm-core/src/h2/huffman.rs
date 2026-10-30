//! The HPACK Huffman code (RFC 7541 §5.2, Appendix B), decoded from its code lengths alone.

use super::hpack::HpackError;

/// Code length in bits of each symbol, 0 to 255 then EOS, transcribed from RFC 7541 Appendix B.
// The code is canonical (each length's codes are consecutive, in symbol order), so lengths fix every code.
#[rustfmt::skip]
const LENGTHS: [u8; 257] = [
    13, 23, 28, 28, 28, 28, 28, 28, 28, 24, 30, 28, 28, 30, 28, 28, 28, 28, 28, 28, 28, 28, 30, 28, 28, 28, 28, 28, 28, 28, 28, 28,
    6, 10, 10, 12, 13, 6, 8, 11, 10, 10, 8, 11, 8, 6, 6, 6, 5, 5, 5, 6, 6, 6, 6, 6, 6, 6, 7, 8, 15, 6, 12, 10,
    13, 6, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 8, 7, 8, 13, 19, 13, 14, 6,
    15, 5, 6, 5, 6, 5, 6, 6, 6, 5, 7, 7, 6, 6, 6, 5, 6, 7, 6, 5, 5, 6, 7, 7, 7, 7, 7, 15, 11, 14, 13, 28,
    20, 22, 20, 20, 22, 22, 22, 23, 22, 23, 23, 23, 23, 23, 24, 23, 24, 24, 22, 23, 24, 23, 23, 23, 23, 21, 22, 23, 22, 23, 23, 24,
    22, 21, 20, 22, 22, 23, 23, 21, 23, 22, 22, 24, 21, 22, 23, 23, 21, 21, 22, 21, 23, 22, 23, 23, 20, 22, 22, 22, 23, 22, 22, 23,
    26, 26, 20, 19, 22, 23, 22, 25, 26, 26, 26, 27, 27, 26, 24, 25, 19, 21, 26, 27, 27, 26, 27, 24, 21, 21, 26, 26, 28, 27, 27, 27,
    20, 24, 20, 21, 22, 21, 21, 23, 22, 22, 25, 25, 24, 24, 26, 23, 26, 27, 26, 26, 27, 27, 27, 27, 27, 28, 27, 27, 27, 27, 27, 26,
    30,
];

const EOS: u16 = 256;
const MAX_LEN: usize = 30;

/// The canonical code laid out for decoding.
struct Canonical {
    /// Symbols sorted by code length, then by symbol.
    symbols: [u16; 257],
    /// Per length: the first code, its place in `symbols`, and how many codes have that length.
    first_code: [u32; MAX_LEN + 1],
    first_index: [u16; MAX_LEN + 1],
    count: [u16; MAX_LEN + 1],
}

static CODE: Canonical = canonical();

const fn canonical() -> Canonical {
    let mut c = Canonical {
        symbols: [0; 257],
        first_code: [0; MAX_LEN + 1],
        first_index: [0; MAX_LEN + 1],
        count: [0; MAX_LEN + 1],
    };
    let mut s = 0;
    while s < 257 {
        c.count[LENGTHS[s] as usize] += 1;
        s += 1;
    }
    let (mut code, mut index, mut len) = (0u32, 0u16, 1);
    while len <= MAX_LEN {
        c.first_code[len] = code;
        c.first_index[len] = index;
        code = (code + c.count[len] as u32) << 1;
        index += c.count[len];
        len += 1;
    }
    let mut next = c.first_index;
    s = 0;
    while s < 257 {
        let len = LENGTHS[s] as usize;
        c.symbols[next[len] as usize] = s as u16;
        next[len] += 1;
        s += 1;
    }
    c
}

/// Appends the Huffman-decoded `input` to `out`.
pub(crate) fn decode(input: &[u8], out: &mut Vec<u8>) -> Result<(), HpackError> {
    let (mut code, mut len) = (0u32, 0usize);
    for &byte in input {
        for shift in (0..8).rev() {
            code = (code << 1) | u32::from((byte >> shift) & 1);
            len += 1;
            // Below the length's first code the subtraction wraps past `count`.
            let offset = code.wrapping_sub(CODE.first_code[len]);
            if offset < u32::from(CODE.count[len]) {
                let symbol = CODE.symbols[usize::from(CODE.first_index[len]) + offset as usize];
                if symbol == EOS {
                    return Err(HpackError::Huffman);
                }
                out.push(symbol as u8);
                (code, len) = (0, 0);
            } else if len == MAX_LEN {
                // Unreachable for this complete code; it bounds the table index.
                return Err(HpackError::Huffman);
            }
        }
    }
    // Padding is the most significant bits of EOS: up to 7 bits, all ones (RFC 7541 §5.2).
    if len > 7 || code != (1 << len) - 1 {
        return Err(HpackError::Huffman);
    }
    Ok(())
}
