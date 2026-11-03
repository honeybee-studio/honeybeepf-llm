use honeybeepf_llm_core::h2::hpack::{HpackError, STATIC_TABLE, decode_int, decode_string};

type IntRow = (
    &'static str,
    &'static [u8],
    u8,
    Result<(u32, usize), HpackError>,
);
type StringRow = (&'static str, Vec<u8>, Result<Vec<u8>, HpackError>);

/// Bytes of a hex dump written in groups, as in RFC 7541 Appendix C.
fn hex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn string(input: &[u8]) -> Result<(Vec<u8>, usize), HpackError> {
    let mut out = b"kept:".to_vec();
    let used = decode_string(input, &mut out)?;
    Ok((out.split_off(5), used))
}

/// The bytes of `(code, bits)` pairs in order, padded with ones to a whole byte.
fn huffman_bytes(codes: &[(u32, u32)]) -> Vec<u8> {
    let (mut acc, mut bits, mut out) = (0u64, 0u32, Vec::new());
    for &(code, len) in codes {
        (acc, bits) = ((acc << len) | u64::from(code), bits + len);
        while bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if bits > 0 {
        out.push(((acc << (8 - bits)) as u8) | ((1u8 << (8 - bits)) - 1));
    }
    let mut lit = vec![0x80 | out.len() as u8];
    lit.extend(out);
    lit
}

#[test]
fn test_integers_from_rfc_7541_c1_and_their_limits() {
    use HpackError::*;
    #[rustfmt::skip]
    let rows: &[IntRow] = &[
        ("C.1.1 10, 5-bit prefix, flag bits set", &[0xea], 5, Ok((10, 1))),
        ("C.1.2 1337, 5-bit prefix", &[0x1f, 0x9a, 0x0a, 0xff], 5, Ok((1337, 3))),
        ("C.1.3 42 at an octet boundary", &[0x2a], 8, Ok((42, 1))),
        ("prefix full, value exactly the prefix max", &[0x1f, 0x00], 5, Ok((31, 2))),
        ("u32::MAX", &[0x1f, 0xe0, 0xff, 0xff, 0xff, 0x0f], 5, Ok((u32::MAX, 6))),
        ("u32::MAX + 1", &[0x1f, 0xe1, 0xff, 0xff, 0xff, 0x0f], 5, Err(IntegerOverflow)),
        ("six continuation bytes, zeros", &[0x1f, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00], 5, Err(IntegerOverflow)),
        ("empty", &[], 5, Err(Truncated)),
        ("prefix full, nothing after", &[0x1f], 5, Err(Truncated)),
        ("continuation bit on the last byte", &[0x1f, 0x9a], 5, Err(Truncated)),
        ("1-bit prefix", &[0xfe], 1, Ok((0, 1))),
        ("7-bit prefix full, as a string length", &[0x7f, 0x00], 7, Ok((127, 2))),
    ];
    for (name, input, prefix, want) in rows {
        assert_eq!(decode_int(input, *prefix), *want, "{name}");
    }
}

#[test]
fn test_huffman_strings_from_rfc_7541_c4_and_c6() {
    #[rustfmt::skip]
    let rows: &[(&str, &str)] = &[
        ("8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff", "www.example.com"),
        ("86a8 eb10 649c bf", "no-cache"),
        ("8825 a849 e95b a97d 7f", "custom-key"),
        ("8925 a849 e95b b8e8 b4bf", "custom-value"),
        ("8264 02", "302"),
        ("85ae c377 1a4b", "private"),
        ("96d0 7abe 9410 54d4 44a8 2005 9504 0b81 66e0 82a6 2d1b ff", "Mon, 21 Oct 2013 20:13:21 GMT"),
        ("919d 29ad 1718 63c7 8f0b 97c8 e9ae 82ae 43d3", "https://www.example.com"),
        ("8364 0eff", "307"),
        ("96d0 7abe 9410 54d4 44a8 2005 9504 0b81 66e0 84a6 2d1b ff", "Mon, 21 Oct 2013 20:13:22 GMT"),
        ("839b d9ab", "gzip"),
        ("ad94 e782 1dd7 f2e6 c7b3 35df dfcd 5b39 60d5 af27 087f 3672 c1ab 270f b529 1f95 8731 6065 c003 ed4e e5b1 063d 5007",
            "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1"),
    ];
    for (input, want) in rows {
        let bytes = [hex(input), b"next".to_vec()].concat();
        let got = string(&bytes);
        assert_eq!(
            got,
            Ok((want.as_bytes().to_vec(), bytes.len() - 4)),
            "{want}"
        );
    }
}

#[test]
fn test_huffman_codes_from_rfc_7541_appendix_b() {
    // (symbol, code, bits) rows of Appendix B: every length class and both ends of the table.
    #[rustfmt::skip]
    let rows: &[(u8, u32, u32)] = &[
        (0, 0x1ff8, 13), (10, 0x3fff_fffc, 30), (13, 0x3fff_fffd, 30), (22, 0x3fff_fffe, 30), (b' ', 0x14, 6),
        (b'0', 0x0, 5), (b'a', 0x3, 5), (127, 0xfff_fffc, 28), (128, 0xf_ffe6, 20), (200, 0x3ff_ffe2, 26),
        (254, 0x7ff_fff0, 27), (255, 0x3ff_ffee, 26),
    ];
    for &(symbol, code, bits) in rows {
        assert_eq!(
            string(&huffman_bytes(&[(code, bits)])).map(|r| r.0),
            Ok(vec![symbol]),
            "{symbol}"
        );
    }
    let all: Vec<_> = rows.iter().map(|&(_, c, b)| (c, b)).collect();
    let want: Vec<_> = rows.iter().map(|r| r.0).collect();
    assert_eq!(string(&huffman_bytes(&all)).map(|r| r.0), Ok(want));
}

#[test]
fn test_bad_strings_are_errors() {
    use HpackError::*;
    let eos = (0x3fff_ffff, 30);
    #[rustfmt::skip]
    let rows: Vec<StringRow> = vec![
        ("empty Huffman string", vec![0x80], Ok(vec![])),
        ("raw string", [&[0x05][..], b"hello"].concat(), Ok(b"hello".to_vec())),
        ("EOS inside the data", huffman_bytes(&[(0x3, 5), eos, (0x3, 5)]), Err(Huffman)),
        ("EOS alone", huffman_bytes(&[eos]), Err(Huffman)),
        ("padding of 8 ones", [huffman_bytes(&[(0x3, 5)]), vec![0xff]].concat().iter().enumerate()
            .map(|(i, &b)| if i == 0 { 0x82 } else { b }).collect(), Err(Huffman)),
        ("padding with a zero bit", vec![0x81, 0b0001_1110], Err(Huffman)),
        ("length past the input", vec![0x8c, 0xf1, 0xe3], Err(Truncated)),
        ("length integer cut", vec![0xff], Err(Truncated)),
        ("length near u32::MAX", vec![0x7f, 0xff, 0xff, 0xff, 0x0f, b'x'], Err(Truncated)),
    ];
    for (name, input, want) in rows {
        assert_eq!(string(&input).map(|r| r.0), want, "{name}");
    }
    // '0' decodes before the bad padding is seen; the caller's bytes must come back as they were.
    let mut out = b"keep".to_vec();
    assert_eq!(decode_string(&[0x81, 0x00], &mut out), Err(Huffman));
    assert_eq!(out, b"keep");
}

#[test]
fn test_static_table_matches_rfc_7541_appendix_a_at_its_ends_and_pseudo_headers() {
    #[rustfmt::skip]
    let rows = [(1, ":authority", ""), (2, ":method", "GET"), (3, ":method", "POST"), (4, ":path", "/"),
        (8, ":status", "200"), (16, "accept-encoding", "gzip, deflate"), (26, "content-encoding", ""),
        (31, "content-type", ""), (61, "www-authenticate", "")];
    for (index, name, value) in rows {
        assert_eq!(STATIC_TABLE[index - 1], (name, value), "{index}");
    }
}
