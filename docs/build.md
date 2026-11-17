# Build from source

The agent is a Rust workspace with eBPF programs written in Rust ([Aya](https://aya-rs.dev/)).

## Prerequisites

- The Rust **nightly** toolchain with the `rust-src` component: the eBPF programs are built with `-Z build-std=core`.
- `bpf-linker` 0.9.15, the version the `Dockerfile` and the image workflow pin.
- A C toolchain (`build-essential` on Debian and Ubuntu) for native dependencies.

```bash
rustup install nightly
rustup component add rust-src --toolchain nightly
cargo install bpf-linker --version 0.9.15 --locked
```

Run every `cargo` command from the `honeybeepf-llm/` workspace directory, where the `xtask` alias is defined, and keep
the `+nightly`: `build.rs` builds the eBPF programs with the same `cargo`, so a stable toolchain fails with
``the `-Z` flag is only accepted on the nightly channel of Cargo``.

## Build

=== "For Kubernetes"

    The image is built with the `k8s` feature, which adds the namespace, pod and workload labels:

    ```bash
    git clone https://github.com/honeybee-studio/honeybeepf-llm.git
    cd honeybeepf-llm/honeybeepf-llm
    cargo +nightly build --release --package honeybeepf-llm --features k8s
    ```

    The chart in `charts/` on `main` can expect a newer agent than the latest release image, so install it only
    together with an image built from the same commit.

=== "For a Linux host"

    Without Kubernetes, build without the `k8s` feature:

    ```bash
    git clone https://github.com/honeybee-studio/honeybeepf-llm.git
    cd honeybeepf-llm/honeybeepf-llm
    cargo +nightly xtask build --release
    ls -la target/release/honeybeepf-llm
    ```

    Then [run it on the host](run-binary.md).

## Cross-compile on macOS

Building Linux binaries on macOS needs [`cross`](https://github.com/cross-rs/cross), which runs the build in Docker:

```bash
cargo install cross
docker info                     # Docker must be running
cargo +nightly xtask build --release --target x86_64-unknown-linux-gnu
cargo +nightly xtask build --release --target aarch64-unknown-linux-gnu   # arm64, such as AWS Graviton
```
