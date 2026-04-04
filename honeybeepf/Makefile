.PHONY: build build-release build-linux test clean package deploy help fmt fmt-check lint lint-fix lint-all lint-fix-all

# Default target
help:
	@echo "HoneybeePF Build Commands"
	@echo ""
	@echo "Usage: make <target>"
	@echo ""
	@echo "Targets:"
	@echo "  build          Build debug version (local platform)"
	@echo "  build-release  Build release version (local platform)"
	@echo "  build-linux    Build for Linux (requires Docker on macOS)"
	@echo "  test           Run tests"
	@echo "  clean          Clean build artifacts"
	@echo "  package        Create distribution package"
	@echo "  deploy         Deploy to remote host (set HOST=user@server)"
	@echo "  fmt            Format code"
	@echo "  fmt-check      Check code formatting (CI)"
	@echo "  lint           Run clippy (common crate only, works on macOS)"
	@echo "  lint-fix       Run clippy auto-fix (common crate only)"
	@echo "  lint-all       Run clippy on all crates (Linux only)"
	@echo "  lint-fix-all   Run clippy auto-fix on all (Linux only)"
	@echo ""
	@echo "Examples:"
	@echo "  make build-release"
	@echo "  make build-linux"
	@echo "  make deploy HOST=root@192.168.1.100"
	@echo "  make package"

# Build commands
build:
	cargo build -p honeybeepf

build-release:
	cargo build --release -p honeybeepf

# Linux build (uses Docker/cross on non-Linux)
build-linux:
ifeq ($(shell uname -s),Linux)
	cargo build --release -p honeybeepf
else
	@echo "Building for Linux using Docker..."
	@docker run --rm -v $(PWD):/workspace -w /workspace \
		rust:latest \
		bash -c "rustup install nightly && \
			rustup component add rust-src --toolchain nightly && \
			cargo install bpf-linker && \
			cargo build --release -p honeybeepf"
endif

# Test
test:
	cargo test

# Clean
clean:
	cargo clean
	rm -rf dist/

# Package
package: build-release
	cargo xtask package

# Deploy (requires HOST variable)
deploy:
ifndef HOST
	$(error HOST is required. Usage: make deploy HOST=user@server)
endif
	cargo xtask deploy --host $(HOST) --release

# Install dependencies (for CI/local dev)
setup:
	rustup install nightly
	rustup component add rust-src --toolchain nightly
	cargo install bpf-linker || true

# Format code
fmt:
	cargo fmt --all

# Check formatting (for CI)
fmt-check:
	cargo fmt --all -- --check

# Lint with clippy (excludes eBPF packages that only compile on Linux)
lint:
	cargo clippy -p honeybeepf-common

# Lint with auto-fix
lint-fix:
	cargo clippy --fix -p honeybeepf-common --allow-dirty --allow-staged

# Lint all (Linux only - includes eBPF and userspace)
lint-all:
	cargo clippy --all-targets --all-features

# Lint all with auto-fix (Linux only)
lint-fix-all:
	cargo clippy --fix --all-targets --all-features --allow-dirty --allow-staged
