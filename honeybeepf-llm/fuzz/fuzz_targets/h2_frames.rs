//! HTTP/2 frames as the agent reads them from either side of a connection, split at an arbitrary point.
#![no_main]

use honeybeepf_llm_core::h2::{
    frame::{Frame, FrameReader},
    hpack::Decoder,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (first, input) = data.split_first().unwrap_or((&0, &[]));
    let mut reader = if first & 1 == 0 { FrameReader::client() } else { FrameReader::server() };
    let mut hpack = Decoder::new();
    let cut = usize::from(*first).min(input.len());
    for part in [&input[..cut], &input[cut..]] {
        reader.read(part, |frame| {
            if let Frame::Headers { block, .. } = frame {
                let _ = hpack.decode(block, |_| {});
            }
        });
    }
});
