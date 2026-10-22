# Compression test fixtures

Binary inputs for the `encoding.rs` tests. gzip and deflate inputs are built inside the tests with flate2.
Brotli fixtures are fixed files because the `brotli` encoder crate pins `brotli-decompressor ~6.0`, which
cannot be used next to the 6.1 decoder. The zstd fixtures come from the reference libzstd, so the tests do
not only round-trip ruzstd's own encoder.

## Files

Run the commands from a directory holding the inputs below. Every row was regenerated and compared byte
for byte with the committed file.

| file | sha256 | command |
|---|---|---|
| json.br | 89e31c851cf1d15a5abd51b547ca7dd910ebe3e8bfa95e41e966afb615b4001f | `python3 -c 'import brotli,sys; sys.stdout.buffer.write(brotli.compress(sys.stdin.buffer.read(), quality=5, lgwin=22))' < json.txt > json.br` |
| sse.br | 051397aa9fed96e7bee5e7fb4525029172d70dbdc362801fe295a029e86aa2d5 | `python3 -c 'import brotli,sys; sys.stdout.buffer.write(brotli.compress(sys.stdin.buffer.read(), quality=5, lgwin=22))' < sse.txt > sse.br` |
| at-cap.br | 0addf49764312af345cb0bcf9a50911c02e28a04751b9033205df5ab894a73df | `python3 -c 'import brotli,sys; sys.stdout.buffer.write(brotli.compress(sys.stdin.buffer.read(), quality=5, lgwin=22))' < a-16777216.txt > at-cap.br` |
| over-cap.br | 708aaff13f0d942c70fd0f1071c7bd4dbc1158ce19399836cedbaeb80d8b9c49 | `python3 -c 'import brotli,sys; sys.stdout.buffer.write(brotli.compress(sys.stdin.buffer.read(), quality=5, lgwin=22))' < a-16777217.txt > over-cap.br` |
| json-large-window.br | c78b82631fc6bac24f8b8f2aeafdd1b3713d796a2850453843961bccdd3c8ecd | `cargo run -q --release --manifest-path brotli-lw/Cargo.toml < json.txt > json-large-window.br` |
| json.zst | 048750a5c8122a7086bb7ed232f028ac3be4def4260c5417247d1a5cb1fa386a | `zstd -3 --check -q -c < json.txt > json.zst` |
| sse-2frames.zst | b3bbf21021b3dd6ee59964e3966987a17437adc900fb0d51899cb13d91df5bdc | `{ zstd -3 --check -q -c < sse-a.txt; zstd -3 --check -q -c < sse-b.txt; } > sse-2frames.zst` |

## Contents

| file | plaintext | used by |
|---|---|---|
| json.br | `json.txt`: the test constant `JSON` (110 bytes) | `br()` helper: every-coding, framed-end tests |
| sse.br | `sse.txt`: the test constant `SSE` (161 bytes, three `data:` events ending in `[DONE]`) | `br()` helper: every-coding, framed-end, truncation tests |
| at-cap.br | 16,777,216 × `a` (`MAX_RESPONSE_BUFFER_SIZE`) | `output_over_the_response_cap_is_none` |
| over-cap.br | 16,777,217 × `a` (cap + 1) | `output_over_the_response_cap_is_none` |
| json-large-window.br | `json.txt` in a large-window stream (window 2^26) | `brotli_large_window_stream_is_rejected` |
| json.zst | `json.txt`, one frame with content checksum, no content size | `multi_member_gzip_and_multi_frame_zstd_decode_whole` |
| sse-2frames.zst | `sse.txt` split at byte 80 (`SSE.len() / 2`), one checksummed frame per half | `multi_member_gzip_and_multi_frame_zstd_decode_whole` |

Inputs (sha256 of each: json.txt `ea982ff5…27c1`, sse.txt `fd69e268…2c8b`, sse-a.txt `f8fcb345…1fd0`,
sse-b.txt `b7063a10…35a8`, a-16777216.txt `5b6ff2e1…783a`, a-16777217.txt `422a9e5e…1503`):

```sh
printf '%s' '{"id":"m1","type":"message","model":"claude-test","content":[],"usage":{"input_tokens":25,"output_tokens":15}}' > json.txt
printf 'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\ndata: {"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":4}}\n\ndata: [DONE]\n\n' > sse.txt
head -c 80 sse.txt > sse-a.txt
tail -c +81 sse.txt > sse-b.txt
head -c 16777216 /dev/zero | tr '\0' a > a-16777216.txt
head -c 16777217 /dev/zero | tr '\0' a > a-16777217.txt
```

`brotli-lw/` is a two-file crate (`Cargo.toml` depends on `brotli = "=9.0.0"`), with this `src/main.rs`:

```rust
use std::io::Write;

fn main() {
    let params = brotli::enc::BrotliEncoderParams {
        quality: 5,
        lgwin: 26,
        large_window: true,
        ..Default::default()
    };
    let mut out = Vec::new();
    brotli::BrotliCompress(&mut std::io::stdin().lock(), &mut out, &params).unwrap();
    std::io::stdout().write_all(&out).unwrap();
}
```

## Tools and versions

- Brotli: Debian/Ubuntu `python3-brotli` 1.1.0 (libbrotli 1.1.0). The Rust `brotli` crate 9.0.0 gives the
  same bytes for the four standard-window files at quality 5, window 22.
- `json-large-window.br`: Rust `brotli` crate 9.0.0 (with `brotli-decompressor` 6.0.1), rustc 1.93.1 stable.
  python3-brotli 1.1.0 has no large-window option, so this file can't be made with it. The C `brotli` CLI
  (`brotli -q 5 --large_window=26 -c < json.txt`) was not installed here, so it was not compared.
- zstd: Zstandard CLI v1.5.5, reading stdin, so the frame header carries no content size. The files were
  first written with libzstd 1.5.7 through the Rust `zstd` crate 0.13.3 at level 3 with checksums. The
  CLI 1.5.5 output is identical. Other zstd releases may pick different matches at level 3.
