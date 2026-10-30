use honeybeepf_llm_core::h2::frame::{Frame, FrameReader};
use honeybeepf_llm_core::h2::hpack::{Decoder, HpackError, Text};

/// A field as text; `None` is `Text::Unknown`.
type Pair = (Option<String>, Option<String>);
/// An RFC example block: hex dump, decoded fields, table size after it.
type Block = (&'static str, &'static [&'static str], usize);
/// Named blocks fed in order (an empty one is lost), and the fields of the last.
type Steps = (&'static str, Vec<Vec<u8>>, Vec<Pair>);

/// Bytes of a hex dump written in groups, as in RFC 7541 Appendix C.
fn hex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn text(t: Text<'_>) -> Option<String> {
    match t {
        Text::Known(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Text::Unknown => None,
    }
}

fn decode(d: &mut Decoder, block: &[u8]) -> Result<Vec<Pair>, HpackError> {
    let mut fields = Vec::new();
    d.decode(block, |f| fields.push((text(f.name), text(f.value))))?;
    Ok(fields)
}

/// `"name: value"` lines as known pairs; `"?"` stands for an unknown name and value.
fn pairs(list: &[&str]) -> Vec<Pair> {
    let pair = |l: &&str| match l.split_once(": ") {
        Some((n, v)) => (Some(n.to_string()), Some(v.to_string())),
        None => (None, None),
    };
    list.iter().map(pair).collect()
}

const REQ1: &[&str] = &[
    ":method: GET",
    ":scheme: http",
    ":path: /",
    ":authority: www.example.com",
];
const REQ2: &[&str] = &[
    ":method: GET",
    ":scheme: http",
    ":path: /",
    ":authority: www.example.com",
    "cache-control: no-cache",
];
const REQ3: &[&str] = &[
    ":method: GET",
    ":scheme: https",
    ":path: /index.html",
    ":authority: www.example.com",
    "custom-key: custom-value",
];
const RES1: &[&str] = &[
    ":status: 302",
    "cache-control: private",
    "date: Mon, 21 Oct 2013 20:13:21 GMT",
    "location: https://www.example.com",
];
const RES2: &[&str] = &[
    ":status: 307",
    "cache-control: private",
    "date: Mon, 21 Oct 2013 20:13:21 GMT",
    "location: https://www.example.com",
];
const RES3: &[&str] = &[
    ":status: 200",
    "cache-control: private",
    "date: Mon, 21 Oct 2013 20:13:22 GMT",
    "location: https://www.example.com",
    "content-encoding: gzip",
    "set-cookie: foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1",
];

#[test]
fn test_rfc_7541_appendix_c_examples_decode_and_size_the_table() {
    #[rustfmt::skip]
    let sequences: &[(&str, Option<u32>, &[Block])] = &[
        ("C.2.1", None, &[("400a 6375 7374 6f6d 2d6b 6579 0d63 7573 746f 6d2d 6865 6164 6572", &["custom-key: custom-header"], 55)]),
        ("C.2.2", None, &[("040c 2f73 616d 706c 652f 7061 7468", &[":path: /sample/path"], 0)]),
        ("C.2.3", None, &[("1008 7061 7373 776f 7264 0673 6563 7265 74", &["password: secret"], 0)]),
        ("C.2.4", None, &[("82", &[":method: GET"], 0)]),
        ("C.3", None, &[
            ("8286 8441 0f77 7777 2e65 7861 6d70 6c65 2e63 6f6d", REQ1, 57),
            ("8286 84be 5808 6e6f 2d63 6163 6865", REQ2, 110),
            ("8287 85bf 400a 6375 7374 6f6d 2d6b 6579 0c63 7573 746f 6d2d 7661 6c75 65", REQ3, 164)]),
        ("C.4", None, &[
            ("8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff", REQ1, 57),
            ("8286 84be 5886 a8eb 1064 9cbf", REQ2, 110),
            ("8287 85bf 4088 25a8 49e9 5b a97d 7f89 25a8 49e9 5bb8 e8b4 bf", REQ3, 164)]),
        ("C.5", Some(256), &[
            ("4803 3330 3258 0770 7269 7661 7465 611d 4d6f 6e2c 2032 3120 4f63 7420 3230 3133 2032 303a 3133 3a32 3120 474d
              546e 1768 7474 7073 3a2f 2f77 7777 2e65 7861 6d70 6c65 2e63 6f6d", RES1, 222),
            ("4803 3330 37c1 c0bf", RES2, 222),
            ("88c1 611d 4d6f 6e2c 2032 3120 4f63 7420 3230 3133 2032 303a 3133 3a32 3220 474d 54c0 5a04 677a 6970 7738 666f
              6f3d 4153 444a 4b48 514b 425a 584f 5157 454f 5049 5541 5851 5745 4f49 553b 206d 6178 2d61 6765 3d33 3630 303b
              2076 6572 7369 6f6e 3d31", RES3, 215)]),
        ("C.6", Some(256), &[
            ("4882 6402 5885 aec3 771a 4b61 96d0 7abe 9410 54d4 44a8 2005 9504 0b81 66e0 82a6 2d1b ff6e 919d 29ad 1718 63c7
              8f0b 97c8 e9ae 82ae 43d3", RES1, 222),
            ("4883 640e ffc1 c0bf", RES2, 222),
            ("88c1 6196 d07a be94 1054 d444 a820 0595 040b 8166 e084 a62d 1bff c05a 839b d9ab 77ad 94e7 821d d7f2 e6c7 b335
              dfdf cd5b 3960 d5af 2708 7f36 72c1 ab27 0fb5 291f 9587 3160 65c0 03ed 4ee5 b106 3d50 07", RES3, 215)]),
    ];
    for (name, limit, blocks) in sequences {
        let mut d = Decoder::new();
        if let Some(limit) = limit {
            d.set_limit(*limit);
        }
        for (i, (dump, want, size)) in blocks.iter().enumerate() {
            assert_eq!(
                decode(&mut d, &hex(dump)),
                Ok(pairs(want)),
                "{name} block {}",
                i + 1
            );
            assert_eq!(
                (d.table_size(), d.forgotten()),
                (*size, false),
                "{name} block {}",
                i + 1
            );
        }
    }
}

#[test]
fn test_forgotten_table_gives_unknown_never_a_wrong_field() {
    let (c31, c32, c33) = (
        hex("8286 8441 0f77 7777 2e65 7861 6d70 6c65 2e63 6f6d"),
        hex("8286 84be 5808 6e6f 2d63 6163 6865"),
        hex("8287 85bf 400a 6375 7374 6f6d 2d6b 6579 0c63 7573 746f 6d2d 7661 6c75 65"),
    );
    let mut d = Decoder::new();
    decode(&mut d, &c31).expect("C.3.1");
    d.forget();
    // C.3.2's index 62 was added by C.3.1; C.3.3's 63 is that same entry, behind one added after the loss.
    let want2 = pairs(&[
        ":method: GET",
        ":scheme: http",
        ":path: /",
        "?",
        "cache-control: no-cache",
    ]);
    let want3 = pairs(&[
        ":method: GET",
        ":scheme: https",
        ":path: /index.html",
        "?",
        "custom-key: custom-value",
    ]);
    assert_eq!(decode(&mut d, &c32), Ok(want2));
    assert_eq!(decode(&mut d, &c33), Ok(want3));
    assert!(d.forgotten());

    // A literal named by an unknown entry: its value is known, and so is the entry it adds.
    let mut d = Decoder::new();
    d.forget();
    let want = vec![(None, Some("abc".to_string())); 2];
    assert_eq!(
        decode(&mut d, &[0x7e, 0x03, b'a', b'b', b'c', 0xbe]),
        Ok(want)
    );

    // A size update to 0 empties both tables, so every index is known again.
    assert_eq!(decode(&mut d, &[0x20, 0x82]), Ok(pairs(&[":method: GET"])));
    assert!(!d.forgotten());
    assert_eq!(decode(&mut d, &[0xbe]), Err(HpackError::BadIndex));
}

#[test]
fn test_bad_blocks_are_errors_and_leave_the_table_forgotten() {
    use HpackError::*;
    #[rustfmt::skip]
    let rows: &[(&str, &[u8], HpackError)] = &[
        ("index 0", &[0x80], BadIndex),
        ("index past a table seen whole (63)", &[0xbf], BadIndex),
        ("literal name index past the table (63)", &[0x7f, 0x00, 0x01, b'x'], BadIndex),
        ("size update over the limit (4097)", &[0x3f, 0xe2, 0x1f], TableSize),
        ("size update after a field", &[0x82, 0x20], TableSize),
        ("value cut", &[0x40, 0x01, b'a', 0x05, b'b'], Truncated),
        ("bad Huffman value", &[0x40, 0x01, b'a', 0x81, 0x00], Huffman),
    ];
    for (name, block, err) in rows {
        let mut d = Decoder::new();
        decode(
            &mut d,
            &hex("400a 6375 7374 6f6d 2d6b 6579 0d63 7573 746f 6d2d 6865 6164 6572"),
        )
        .expect("C.2.1");
        assert_eq!(decode(&mut d, block), Err(*err), "{name}");
        assert!(d.forgotten(), "{name}");
        assert_eq!(
            decode(&mut d, &[0xbe]),
            Ok(vec![(None, None)]),
            "{name}: the C.2.1 entry is gone"
        );
    }
}

#[test]
fn test_entry_larger_than_the_table_empties_it_and_memory_cap_forgets() {
    // Size update to 40, then a 49-byte entry: the table is emptied, so nothing is unknown any more.
    let mut d = Decoder::new();
    d.forget();
    let block = [&[0x3f, 0x09, 0x40, 0x01, b'a', 0x10][..], &[b'b'; 16]].concat();
    assert_eq!(decode(&mut d, &block).map(|f| f.len()), Ok(1));
    assert_eq!((d.table_size(), d.forgotten()), (0, false));

    // A receiver allowing 1 MiB: past 64 KiB the oldest entries are dropped and their indexes become unknown.
    let mut d = Decoder::new();
    d.set_limit(1 << 20);
    let mut block = vec![0x3f, 0xe1, 0xff, 0x3f]; // size update to 1 MiB
    for c in [b'x', b'y', b'z'] {
        block.extend([0x40, 0x01, c, 0x7f, 0xb1, 0xe9, 0x01]); // value length 30,000
        block.extend(vec![c; 30_000]);
    }
    assert_eq!(decode(&mut d, &block).map(|f| f.len()), Ok(3));
    assert!(d.forgotten() && d.table_size() <= 64 * 1024);
    let newest = decode(&mut d, &[0xbe]).expect("index 62");
    assert_eq!(newest, vec![(Some("z".into()), Some("z".repeat(30_000)))]);
    assert_eq!(
        decode(&mut d, &[0xc0]),
        Ok(vec![(None, None)]),
        "index 64, dropped"
    );
}

#[test]
fn test_table_size_rules_and_resync_after_a_lost_size_update() {
    let x80 = vec![b'x'; 80];
    let field = || vec![(Some("n".to_string()), Some("x".repeat(80)))];
    #[rustfmt::skip]
    let rows: Vec<Steps> = vec![
        // Size update to 100, then the update back to 4096 is lost: a 113-byte entry must still be kept.
        ("lost size update", vec![vec![0x3f, 0x45], vec![], [&[0x40, 0x01, b'n', 0x50][..], &x80, &[0xbe]].concat()],
            [field(), field()].concat()),
        // A bad block that set the size to 0 must not keep later entries out after the resync.
        ("bad block shrank the table", vec![vec![0x20, 0x80], vec![0x40, 0x01, b'k', 0x01, b'v', 0xbe]],
            pairs(&["k: v", "k: v"])),
        ("two size updates at the block start", vec![vec![0x20, 0x3f, 0xe1, 0x1f, 0x82]], pairs(&[":method: GET"])),
        ("entry exactly the table size is kept", vec![vec![0x3f, 0x03, 0x40, 0x01, b'a', 0x01, b'b', 0xbe]],
            pairs(&["a: b", "a: b"])),
        ("unknown name, not indexed", vec![vec![], vec![0x0f, 0x2f, 0x01, b'v', 0xbe]], vec![(None, Some("v".into())), (None, None)]),
    ];
    for (name, blocks, want) in rows {
        let mut d = Decoder::new();
        let mut last = Vec::new();
        for block in &blocks {
            // An empty block stands for a lost one.
            if block.is_empty() {
                d.forget();
                continue;
            }
            last = decode(&mut d, block).unwrap_or_default();
        }
        assert_eq!(last, want, "{name}");
    }
}

/// Header blocks of one direction of a recorded connection, from `tests/testdata/h2`.
fn blocks(name: &str) -> Vec<Vec<u8>> {
    let path = format!(
        "{}/tests/testdata/h2/{name}.bin",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(&path).expect(&path);
    let mut r = if name.ends_with("c2s") {
        FrameReader::client()
    } else {
        FrameReader::server()
    };
    let mut out = Vec::new();
    r.read(&bytes, |f| {
        if let Frame::Headers { block, .. } = f {
            out.push(block.to_vec());
        }
    });
    out
}

#[test]
fn test_recorded_connections_decode_and_a_lost_block_never_gives_a_wrong_field() {
    #[rustfmt::skip]
    let expect: &[(&str, &[&str])] = &[
        ("curl-json.c2s", &[":method: POST", ":authority: api.openai.com", ":path: /v1/chat/completions"]),
        ("curl-json.s2c", &[":status: 200", "content-type: application/json"]),
        ("curl-sse.s2c", &["content-type: text/event-stream"]),
        ("curl-gzip.s2c", &["content-encoding: gzip"]),
        ("httpx-reuse.c2s", &[":authority: api.openai.com", ":path: /v1/chat/completions"]),
        ("node26-openai-sse.c2s", &[":authority: api.openai.com", ":path: /v1/chat/completions"]),
        ("node26-anthropic-json.c2s", &[":authority: api.anthropic.com", ":path: /v1/messages"]),
    ];
    let mut unknown = 0;
    for (name, want) in expect {
        let blocks = blocks(name);
        let mut d = Decoder::new();
        let all: Vec<_> = blocks
            .iter()
            .map(|b| decode(&mut d, b).expect(name))
            .collect();
        for (i, fields) in all.iter().enumerate() {
            for w in pairs(want) {
                assert!(
                    fields.contains(&w),
                    "{name} block {i}: no {w:?} in {fields:?}"
                );
            }
        }
        // Lose block k: the decoder forgets, and every later field is right or unknown.
        for k in 0..blocks.len() {
            let mut d = Decoder::new();
            for (j, block) in blocks.iter().enumerate() {
                if j == k {
                    d.forget();
                    continue;
                }
                let got = decode(&mut d, block).expect(name);
                assert_eq!(got.len(), all[j].len(), "{name} lose {k}, block {j}");
                for (g, a) in got.iter().zip(&all[j]) {
                    let ok = |g: &Option<String>, a: &Option<String>| g.is_none() || g == a;
                    assert!(
                        ok(&g.0, &a.0) && ok(&g.1, &a.1),
                        "{name} lose {k}, block {j}: {g:?} vs {a:?}"
                    );
                    unknown += usize::from(g.0.is_none() || g.1.is_none());
                }
            }
        }
    }
    // httpx's second request refers to entries its first one added, so losing that block must show.
    assert!(unknown > 0, "no loss made a field unknown");
}
