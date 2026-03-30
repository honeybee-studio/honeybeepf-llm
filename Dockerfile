FROM --platform=$BUILDPLATFORM rustlang/rust:nightly-trixie AS chef

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    RUSTUP_TOOLCHAIN=nightly-x86_64-unknown-linux-gnu

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends \
    clang llvm libelf-dev pkg-config build-essential \
    libclang-dev llvm-dev zlib1g-dev \
    gcc-aarch64-linux-gnu \
    libc6-dev-arm64-cross

RUN rustup component add rust-src

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    # NOTE: bpf-linker is pinned to 0.9.15 for compatibility with the current Rust nightly
    # and eBPF toolchain. If updating Rust or related dependencies, verify newer bpf-linker
    # versions work before changing this pin.
    cargo install bpf-linker --version 0.9.15 --locked && \
    cargo install cargo-chef --locked

WORKDIR /app/honeybeepf

FROM chef AS planner
COPY . /app
WORKDIR /app/honeybeepf
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG TARGETARCH
WORKDIR /app/honeybeepf

COPY --from=planner /app/honeybeepf/recipe.json recipe.json

RUN case ${TARGETARCH} in \
    "amd64") rustup target add x86_64-unknown-linux-gnu ;; \
    "arm64") rustup target add aarch64-unknown-linux-gnu ;; \
    esac

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/app/honeybeepf/target,sharing=locked \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc && \
    TARGET_TRIPLE=$(case ${TARGETARCH} in "amd64") echo "x86_64-unknown-linux-gnu" ;; "arm64") echo "aarch64-unknown-linux-gnu" ;; esac) && \
    cargo chef cook --release --features k8s --recipe-path recipe.json --package honeybeepf --target $TARGET_TRIPLE

COPY . /app

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/app/honeybeepf/target,sharing=locked \
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc && \
    TARGET_TRIPLE=$(case ${TARGETARCH} in "amd64") echo "x86_64-unknown-linux-gnu" ;; "arm64") echo "aarch64-unknown-linux-gnu" ;; esac) && \
    # eBPF build, using rust-src to build the target from source
    cargo build --release --package honeybeepf-ebpf --target=bpfel-unknown-none -Z build-std=core && \
    cargo build --release --features k8s --package honeybeepf --target $TARGET_TRIPLE && \
    cp target/$TARGET_TRIPLE/release/honeybeepf /app/honeybeepf-bin

FROM debian:trixie-slim AS runtime
RUN --mount=type=cache,target=/var/cache/apt \
    apt-get update && apt-get install -y libelf1 ca-certificates
WORKDIR /app
COPY --from=builder /app/honeybeepf-bin /usr/local/bin/honeybeepf

ENTRYPOINT ["/usr/local/bin/honeybeepf"]