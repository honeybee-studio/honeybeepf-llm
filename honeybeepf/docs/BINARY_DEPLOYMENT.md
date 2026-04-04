# HoneybeePF Binary Deployment Guide

HoneybeePF is an eBPF-based monitoring tool. This document explains how to deploy the binary directly.

## 📦 Build

### Build on Linux (Recommended)

```bash
# Release build
cargo xtask build --release

# Binary location
ls -la target/release/honeybeepf
```

### Cross-compile for Linux on macOS

To build Linux binaries on macOS, you need the `cross` tool:

```bash
# Install cross (requires Docker)
cargo install cross

# Verify Docker is running
docker info

# Build for Linux
cargo xtask build --release --target x86_64-unknown-linux-gnu

# Build for ARM64 Linux (AWS Graviton, Apple Silicon Linux VM, etc.)
cargo xtask build --release --target aarch64-unknown-linux-gnu
```

## 🚀 Deployment

### Method 1: Automated Deployment with xtask

```bash
# Deploy to remote server (build + scp + install)
cargo xtask deploy --host <user>@<your-server> --release

# Install as systemd service and restart
cargo xtask deploy --host <user>@<your-server> --release --restart

# Deploy to a specific path
cargo xtask deploy --host root@<your-server> --path /opt/honeybeepf/bin/honeybeepf --release
```

### Method 2: Manual Deployment

```bash
# 1. Build
cargo xtask build --release

# 2. Copy binary
scp target/release/honeybeepf <user>@<your-server>:/tmp/

# 3. Install on server
ssh <user>@<your-server>
sudo mv /tmp/honeybeepf /usr/local/bin/
sudo chmod +x /usr/local/bin/honeybeepf
```

### Method 3: Package and Deploy

```bash
# Create distribution package
cargo xtask package --output dist

# Check generated tarball
ls dist/*.tar.gz

# Transfer and install on server
scp dist/honeybeepf-*.tar.gz <user>@<your-server>:/tmp/
ssh <user>@<your-server> "cd /tmp && tar xzf honeybeepf-*.tar.gz && cd honeybeepf-* && sudo ./install.sh"
```

## 🔐 Permission Requirements

HoneybeePF needs to load eBPF programs into the kernel, which **requires special privileges**.

### Option 1: Run as root (Simple)

```bash
sudo /usr/local/bin/honeybeepf
```

### Option 2: Set Capabilities (Recommended, more secure)

```bash
# Grant required capabilities
sudo setcap 'cap_sys_admin,cap_bpf,cap_perfmon,cap_net_admin+ep' /usr/local/bin/honeybeepf

# Run as regular user
/usr/local/bin/honeybeepf
```

**Required Capabilities:**
| Capability | Purpose |
|------------|---------|
| `CAP_SYS_ADMIN` | Load eBPF programs (kernel < 5.8) |
| `CAP_BPF` | Load eBPF programs (kernel 5.8+) |
| `CAP_PERFMON` | Access perf events |
| `CAP_NET_ADMIN` | Network-related eBPF programs |

### Check Kernel Version

```bash
uname -r
# 5.8+ recommended (CAP_BPF support)
```

## 🔧 Systemd Service Setup

### Automated Installation

```bash
cargo xtask install-service --host <user>@<your-server>
```

### Manual Installation

1. Create service file:

```bash
sudo tee /etc/systemd/system/honeybeepf.service << 'EOF'
[Unit]
Description=HoneybeePF eBPF Monitoring
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/honeybeepf
Restart=on-failure
RestartSec=5
EnvironmentFile=-/etc/honeybeepf/honeybeepf.env

# Security hardening (if not running as root)
# User=honeybeepf
# Group=honeybeepf

# Capabilities required for eBPF
NoNewPrivileges=no
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_BPF CAP_PERFMON CAP_NET_ADMIN
AmbientCapabilities=CAP_SYS_ADMIN CAP_BPF CAP_PERFMON CAP_NET_ADMIN

[Install]
WantedBy=multi-user.target
EOF
```

2. Environment configuration file (optional):

```bash
sudo mkdir -p /etc/honeybeepf
sudo tee /etc/honeybeepf/honeybeepf.env << 'EOF'
# HoneybeePF Configuration
# RUST_LOG=info
# HONEYBEEPF_METRICS_PORT=9090
EOF
```

3. Enable and start service:

```bash
sudo systemctl daemon-reload
sudo systemctl enable honeybeepf
sudo systemctl start honeybeepf

# Check status
sudo systemctl status honeybeepf

# View logs
sudo journalctl -u honeybeepf -f
```

## 📊 Status Check

### Check Process

```bash
ps aux | grep honeybeepf
```

### Check eBPF Programs

```bash
# List loaded eBPF programs
sudo bpftool prog list

# List loaded eBPF maps
sudo bpftool map list
```

### View Logs

```bash
# systemd logs
sudo journalctl -u honeybeepf -f

# Run in verbose mode
sudo /usr/local/bin/honeybeepf --verbose
```

## 🗑 Uninstall

```bash
# Stop and disable service
sudo systemctl stop honeybeepf
sudo systemctl disable honeybeepf

# Delete files
sudo rm /etc/systemd/system/honeybeepf.service
sudo rm /usr/local/bin/honeybeepf
sudo rm -rf /etc/honeybeepf

# Reload systemd
sudo systemctl daemon-reload
```