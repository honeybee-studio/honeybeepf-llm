//! A chunked HTTP/1.1 body under every combination of the walk's rules, fed in two parts.
#![no_main]

use honeybeepf_llm_core::chunked::{ChunkRules, ChunkWalk};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&flags, input)) = data.split_first() else {
        return;
    };
    let rules = ChunkRules {
        extensions: flags & 1 != 0,
        utf8_line: flags & 2 != 0,
        data_crlf: flags & 4 != 0,
        bare_lf_breaks: flags & 8 != 0,
        max_size: if flags & 16 != 0 { 1 << 20 } else { usize::MAX },
        max_line: if flags & 32 != 0 { 64 } else { usize::MAX },
    };
    let mut walk = ChunkWalk::new(rules);
    let cut = input.len() / 2;
    let mut consumed = walk.walk(&input[..cut], |_| {});
    consumed += walk.walk(&input[cut..], |_| {});
    assert!(consumed <= input.len());
    let _ = walk.state();
});
