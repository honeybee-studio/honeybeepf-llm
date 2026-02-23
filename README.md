# honeybeepf-llm

An eBPF-based LLM observability agent written in Rust. It intercepts TLS traffic at the kernel level — without requiring any code changes or proxies — and extracts LLM API usage metrics (model name, token counts, latency) for any process using OpenSSL on the host.

## How It Works

`honeybeepf-llm` uses Linux uprobes to attach to `SSL_read` and `SSL_write` in OpenSSL, capturing plaintext LLM API traffic after TLS decryption. The captured data is processed in user space to identify LLM API calls and extract usage metrics.

```
┌─────────────────────────────────────────────────────────────┐
│ Kernel Space                                                │
│                                                             │
│   uprobes (SSL_read)          uprobes (SSL_write)           │
│       │  Capture (buf, len)       │  Capture (buf, len)     │
│       └──────────────────┬────────┘                        │
│                          ▼                                  │
│                  SSL_EVENTS RingBuf                         │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│ User Space                                                  │
│                                                             │
│              RingBufHandler (Consumer Thread)               │
│                     │ Consume Events                        │
│                     ▼                                       │
│              StreamMap (Lookup PID+FD)                      │
│                     │ Get/Create                            │
│                     ▼                                       │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                  StreamProcessor                     │   │
│  │                                                      │   │
│  │  WriteBuf ──► Protocol Detection ──► ProcessingReq   │   │
│  │  (User       HTTP/1.1 or HTTP/2     (Extract Prompt) │   │
│  │  Request)                               │            │   │
│  │                                   Count Tokens       │   │
│  │  ReadBuf ◄─────────────────────── ProcessingResp     │   │
│  │  (LLM Resp,  Accumulate until              │         │   │
│  │  streaming)  response complete             ▼         │   │
│  │                                    Dechunk & Gunzip  │   │
│  │                                         │            │   │
│  │                                    Parse Usage JSON  │   │
│  │                                    (Extract Metrics) │   │
│  │                                         │            │   │
│  │                                      UsageInfo       │   │
│  └─────────────────────────────────────────┼────────────┘   │
│                                            ▼                │
│                                          OTEL               │
└─────────────────────────────────────────────────────────────┘
```

### Key Components

| Component | Description |
|---|---|
| **uprobes** | eBPF uprobes on `SSL_read` / `SSL_write` / `SSL_do_handshake` to capture TLS plaintext buffers |
| **SSL_EVENTS RingBuf** | Kernel ring buffer for efficient, low-overhead transfer of captured events to user space |
| **RingBufHandler** | Consumer thread that processes incoming `LlmEvent`s from the ring buffer |
| **StreamMap** | `HashMap<(PID, FD), StreamProcessor>` — maintains per-connection state |
| **StreamProcessor** | State machine per connection: `Detecting → ProcessingRequest → ProcessingResponse → Finished` |
| **Protocol Detection** | Detects HTTP/1.1 and HTTP/2 framing from the write buffer |
| **Dechunk & Gunzip** | Handles chunked transfer encoding (SSE streaming) and gzip-compressed responses |
| **Parse Usage JSON** | Extracts `prompt_tokens`, `completion_tokens`, `model` from provider-specific JSON payloads |
| **OTEL** | Exports metrics via OpenTelemetry OTLP to any compatible collector |

### Dynamic SSL Discovery

At startup, `honeybeepf-llm` scans all running processes for loaded OpenSSL / libssl libraries and attaches uprobes automatically. A `sched_process_exec` tracepoint watches for new processes and triggers re-discovery, so probes are attached to newly launched processes without restarting the agent.

### Supported LLM Providers

Providers are configured via `providers.yaml`. Any HTTP-based LLM API that returns a standard `usage` JSON field is supported. Built-in support includes OpenAI-compatible APIs (OpenAI, Anthropic, Google, Fireworks, etc.).

## Quick Start

### Pre-built Binary

Download from [GitHub Releases](https://github.com/honeybee-studio/honeybeepf-llm/releases):

```bash
curl -LO https://github.com/honeybee-studio/honeybeepf-llm/releases/latest/download/honeybeepf-llm-linux-x86_64.tar.gz
tar xzf honeybeepf-llm-linux-x86_64.tar.gz
cd honeybeepf-llm-x86_64
sudo ./install.sh
sudo honeybeepf-llm --verbose
```

### Build from Source

```bash
# Build release
cargo xtask build --release

# Deploy to remote server
cargo xtask deploy --host user@server --release --restart

# Create distribution package
cargo xtask package --output dist
```

See [Binary Deployment Guide](docs/BINARY_DEPLOYMENT.md) for detailed instructions.

## Prerequisites

- Rust toolchains:
  - Stable: `rustup toolchain install stable`
  - Nightly (for eBPF builds): `rustup toolchain install nightly --component rust-src`
- `bpf-linker`: `cargo install bpf-linker` (use `--no-default-features` on macOS)
- Linux kernel with eBPF and uprobe support (kernel ≥ 5.8 recommended)

## Build & Run (macOS via Lima VM)

eBPF programs require a Linux kernel. On macOS, use a lightweight VM via Lima.

### 1) Create a VM and install packages

```bash
brew install lima
limactl start --name ebpf-dev --vm-type=vz --mount-writable --cpus=5 --memory=8 --disk=20
lima

echo 'export CARGO_TARGET_DIR=~/cargo-target' >> ~/.bashrc
source ~/.bashrc
sudo apt-get update && sudo apt-get install -y \
    clang llvm pkg-config build-essential libelf-dev \
    linux-tools-common linux-tools-generic \
    linux-headers-$(uname -r) bpftool

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
rustup default nightly
cargo install bpf-linker
```

### 2) Build & run

```bash
cd /<your-repo>/honeybeepf-llm/honeybeepf-llm
cargo build --release
sudo $(which cargo) run -- --verbose
```

Manage the VM from macOS:

```bash
limactl stop ebpf-dev      # stop
limactl start ebpf-dev     # restart
limactl delete ebpf-dev    # delete
```

## Build & Run (native Linux)

```bash
cargo build
sudo cargo run --release -- --verbose
```

The build script compiles eBPF artifacts and bundles them into the binary automatically.

## Configuration

Configure via environment variables or a `.env` file:

```bash
# Enable LLM probing
BUILTIN_PROBES__LLM=true

# OpenTelemetry OTLP endpoint for metrics export
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
```

See `example.env` for all available options.

## Troubleshooting

- **Permission errors**: use `sudo` or grant the binary `CAP_SYS_ADMIN`, `CAP_BPF`, `CAP_PERFMON`.
- **Missing kernel headers**: install `linux-headers-$(uname -r)` on the host/VM.
- **No targets found**: ensure the target process loads OpenSSL (check with `ldd <binary> | grep ssl`).
- **macOS path mounts**: verify the project path is mounted in Lima (`limactl list`).

## License

With the exception of eBPF code, honeybeepf-llm is distributed under either the [MIT license] or the [Apache License] (version 2.0), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

### eBPF licensing

All eBPF code is distributed under either the terms of the [GNU General Public License, Version 2] or the [MIT license], at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the GPL-2 license, shall be dual licensed as above, without any additional terms or conditions.

[Apache license]: LICENSE-APACHE
[MIT license]: LICENSE-MIT
[GNU General Public License, Version 2]: LICENSE-GPL2
