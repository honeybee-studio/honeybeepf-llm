# Build from source

The agent is a Rust workspace with eBPF programs written in Rust ([Aya](https://aya-rs.dev/)). Building it needs the
nightly toolchain with `rust-src` and `bpf-linker`; see the
[prerequisites](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/honeybeepf-llm/docs/BINARY_DEPLOYMENT.md#-prerequisites).

```bash
git clone https://github.com/honeybee-studio/honeybeepf-llm.git
cd honeybeepf-llm/honeybeepf-llm
cargo +nightly build --release --package honeybeepf-llm --features k8s
```

The `k8s` feature adds the pod, namespace and workload labels and is not enabled by default; the Docker image is built
with it.

The chart in `charts/` on `main` can expect a newer agent than the latest release image, so install it only together
with an image built from the same commit. Without Kubernetes, see
[running the binary](https://github.com/honeybee-studio/honeybeepf-llm/blob/main/honeybeepf-llm/docs/BINARY_DEPLOYMENT.md).
