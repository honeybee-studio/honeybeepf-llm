# honeybeepf

High-performance eBPF-based tooling built in Rust.

## Quick Start

### Pre-built Binary (Recommended)

Download from [GitHub Releases](https://github.com/jundorok/honeybeePF/releases):

```bash
# Download (x86_64)
curl -LO https://github.com/jundorok/honeybeePF/releases/latest/download/honeybeepf-linux-x86_64.tar.gz
tar xzf honeybeepf-linux-x86_64.tar.gz
cd honeybeepf-x86_64

# Install
sudo ./install.sh

# Run
sudo honeybeepf --verbose
```

### Using xtask (Build from source)

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
- bpf-linker: `cargo install bpf-linker` (use `--no-default-features` on macOS)
- For Mac (for cross-compiling from macOS):
  - follow the steps below

## Build & Run (macOS via Lima VM)

Running eBPF programs typically requires a Linux kernel. On macOS, use a lightweight Linux VM via Lima.

### 1) Create and enter the VM and install packages

```bash
# Install Lima (macOS)
brew install lima

# limactl start --name ebpf-dev /tmp/ebpf-dev.yaml
limactl start --name ebpf-dev --vm-type=vz --mount-writable --cpus=5 --memory=8 --disk=20

# Enter the VM
lima

echo 'export CARGO_TARGET_DIR=~/cargo-target' >> ~/.bashrc
source ~/.bashrc
sudo apt-get update
sudo apt-get install -y \
    clang \
    llvm \
    pkg-config \
    build-essential \
    libelf-dev \
    linux-tools-common \
    linux-tools-generic \
    linux-headers-$(uname -r) \
    bpftool

# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Install nightly toolchain for eBPF
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
rustup default nightly
cargo install bpf-linker

# Verify
which llvm-objcopy
which clang
llvm-objcopy --version
rustc --version
cargo --version
```

### 2) Build & run the project

```bash
# Inside VM: navigate to the project (mounted from macOS via virtiofs)
cd /<your-repo>/honeybeepf/honeybeepf

# Build
cargo build --release

# Run (requires elevated privileges for eBPF)
sudo $(which cargo) run -- --verbose

```

Exit the VM with `exit`. Manage the VM from macOS:

```bash
# Stop VM when done
limactl stop ebpf-dev

# Restart later
limactl start ebpf-dev

# Delete 
limactl stop ebpf-dev --force 2>/dev/null || true                                        
limactl delete ebpf-dev 2>/dev/null || true
rm -rf ~/.lima/ebpf-dev
```

## Build & Run (native Linux)

If you are on Linux with a recent kernel and headers installed:

```bash
cargo build
sudo cargo run --release -- --verbose
```

Cargo build scripts will compile the eBPF artifacts and bundle them into the binary automatically.

## Troubleshooting

- Permission errors on run: use `sudo` or ensure your user has the appropriate capabilities to load eBPF programs.
- Missing kernel headers: install `linux-headers-$(uname -r)` inside the VM/host.
- macOS path mounts: verify the project path is mounted in Lima (`limactl list` and instance config).

## License

With the exception of eBPF code, honeybeepf is distributed under either the [MIT license] or the [Apache License] (version 2.0), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

### eBPF licensing

All eBPF code is distributed under either the terms of the [GNU General Public License, Version 2] or the [MIT license], at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the GPL-2 license, shall be dual licensed as above, without any additional terms or conditions.

[Apache license]: LICENSE-APACHE
[MIT license]: LICENSE-MIT
[GNU General Public License, Version 2]: LICENSE-GPL2
