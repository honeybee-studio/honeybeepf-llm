use honeybeepf_llm_core::chunked::{ChunkRules, ChunkState, ChunkWalk};

#[test]
fn test_walk_uses_whole_lines_and_any_data_and_resumes() {
    let body = b"5\r\nhello\r\n3;x=1\r\nabc\r\n0\r\nT: 1\r\n\r\n";
    let rules = ChunkRules {
        extensions: true,
        data_crlf: true,
        ..ChunkRules::BASIC
    };
    for split in 0..=body.len() {
        let (mut walk, mut data) = (ChunkWalk::new(rules), Vec::new());
        let used = walk.walk(&body[..split], |d| data.extend_from_slice(d));
        // Only a cut line is left for the next call; cut data is passed on as it comes.
        assert!(!body[used..split].contains(&b'\n'), "{split}");
        walk.walk(&body[used..], |d| data.extend_from_slice(d));
        assert_eq!(
            (data.as_slice(), walk.state()),
            (&b"helloabc"[..], ChunkState::Ended)
        );
    }
}

#[test]
fn test_each_rule_changes_what_a_walk_makes_of_a_body() {
    use ChunkState::{Broken, Ended};
    let basic = ChunkRules::BASIC;
    let ext = ChunkRules {
        extensions: true,
        ..basic
    };
    type Row = (ChunkRules, &'static [u8], &'static [u8], ChunkState);
    #[rustfmt::skip]
    let rows: &[Row] = &[
        (basic,                                     b"5;x\r\nhello\r\n0\r\n\r\n",    b"",       Broken),
        (ext,                                       b"5;x\r\nhello\r\n0\r\n\r\n",    b"hello",  Ended),
        (ext,                                       b"5;\xff\r\nhello\r\n0\r\n\r\n", b"hello",  Ended),
        (ChunkRules { utf8_line: true, ..ext },     b"5;\xff\r\nhello\r\n0\r\n\r\n", b"",       Broken),
        (basic,                                     b"5\r\nhelloXY0\r\n\r\n",        b"hello",  Ended),
        (ChunkRules { data_crlf: true, ..basic },   b"5\r\nhelloXY0\r\n\r\n",        b"hello",  Broken),
        (basic,                                     b"6\r\nhello!\r\n0\r\n\r\n",     b"hello!", Ended),
        (ChunkRules { max_size: 5, ..basic },       b"6\r\nhello!\r\n0\r\n\r\n",     b"",       Broken),
        (basic,                                     b"5\n\r\nhello\r\n0\r\n\r\n",    b"hello",  Ended),
        (ChunkRules { bare_lf_breaks: true, ..basic }, b"5\n\r\nhello\r\n0\r\n\r\n", b"",       Broken),
        (ChunkRules { max_line: 3, ..basic },       b"05\r\nhello\r\n0\r\n\r\n",     b"hello",  Ended),
        (ChunkRules { max_line: 2, ..basic },       b"05\r\nhello\r\n0\r\n\r\n",     b"",       Broken),
    ];
    for &(rules, body, want, state) in rows {
        let (mut walk, mut data) = (ChunkWalk::new(rules), Vec::new());
        walk.walk(body, |d| data.extend_from_slice(d));
        assert_eq!(
            (data.as_slice(), walk.state()),
            (want, state),
            "{rules:?} {body:?}"
        );
    }
}
