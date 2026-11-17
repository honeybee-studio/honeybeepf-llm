//! One HPACK decoder fed several header blocks in a row, so dynamic-table state carries between them.
#![no_main]

use honeybeepf_llm_core::h2::hpack::Decoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut decoder = Decoder::new();
    for block in data.split(|&b| b == 0xff) {
        let _ = decoder.decode(block, |_| {});
    }
});
