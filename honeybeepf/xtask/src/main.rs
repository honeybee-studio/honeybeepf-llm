use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
#[command(about = "Build and deploy honeybeepf")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build the complete binary (eBPF + userspace)
    Build {
        /// Build in release mode
        #[arg(long)]
        release: bool,

        /// Target architecture for cross-compilation (e.g., x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu)
        #[arg(long)]
        target: Option<String>,
    },

    /// Deploy the binary to a remote host
    Deploy {
        /// Remote host (e.g., user@host or just host)
        #[arg(long)]
        host: String,

        /// Remote path to deploy to (default: /usr/local/bin/honeybeepf)
        #[arg(long, default_value = "/usr/local/bin/honeybeepf")]
        path: String,

        /// Build in release mode before deploying
        #[arg(long, default_value = "true")]
        release: bool,

        /// Target architecture for cross-compilation
        #[arg(long)]
        target: Option<String>,

        /// Restart the service after deployment (if systemd service exists)
        #[arg(long)]
        restart: bool,
    },

    /// Install systemd service on remote host
    InstallService {
        /// Remote host
        #[arg(long)]
        host: String,

        /// Service configuration options (env vars, etc.)
        #[arg(long)]
        config: Option<String>,
    },

    /// Package binary for distribution
    Package {
        /// Target architecture
        #[arg(long)]
        target: Option<String>,

        /// Output directory
        #[arg(long, default_value = "dist")]
        output: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build { release, target } => {
            build(release, target.as_deref())?;
        }
        Commands::Deploy {
            host,
            path,
            release,
            target,
            restart,
        } => {
            deploy(&host, &path, release, target.as_deref(), restart)?;
        }
        Commands::InstallService { host, config } => {
            install_service(&host, config.as_deref())?;
        }
        Commands::Package { target, output } => {
            package(target.as_deref(), &output)?;
        }
    }

    Ok(())
}

fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn build(release: bool, target: Option<&str>) -> Result<()> {
    let root = project_root();

    println!("🔨 Building honeybeepf...");

    // Determine if we need cross-compilation (Linux target from non-Linux host)
    let is_cross_compile =
        target.map(|t| t.contains("linux")).unwrap_or(false) && !cfg!(target_os = "linux");

    let build_cmd = if is_cross_compile {
        // Check if cross is available
        if which::which("cross").is_ok() {
            println!("   Using 'cross' for cross-compilation");
            "cross"
        } else {
            bail!(
                "Cross-compilation to Linux requires 'cross' tool.\n\
                 Install with: cargo install cross\n\
                 Also requires Docker to be running."
            );
        }
    } else {
        "cargo"
    };

    let mut cmd = Command::new(build_cmd);
    cmd.current_dir(&root);
    cmd.arg("build");

    if release {
        cmd.arg("--release");
    }

    if let Some(t) = target {
        cmd.arg("--target").arg(t);
        println!("   Target: {}", t);
    }

    cmd.arg("-p").arg("honeybeepf");

    let status = cmd.status().context("Failed to run cargo build")?;

    if !status.success() {
        bail!("Build failed");
    }

    let profile = if release { "release" } else { "debug" };
    let binary_path = if let Some(t) = target {
        root.join("target").join(t).join(profile).join("honeybeepf")
    } else {
        root.join("target").join(profile).join("honeybeepf")
    };

    println!("✅ Build complete: {}", binary_path.display());

    Ok(())
}

fn deploy(
    host: &str,
    remote_path: &str,
    release: bool,
    target: Option<&str>,
    restart: bool,
) -> Result<()> {
    // First build
    build(release, target)?;

    let root = project_root();
    let profile = if release { "release" } else { "debug" };

    let binary_path = if let Some(t) = target {
        root.join("target").join(t).join(profile).join("honeybeepf")
    } else {
        root.join("target").join(profile).join("honeybeepf")
    };

    if !binary_path.exists() {
        bail!("Binary not found at: {}", binary_path.display());
    }

    println!("📦 Deploying to {}:{}", host, remote_path);

    // Copy binary using scp
    let status = Command::new("scp")
        .arg(&binary_path)
        .arg(format!("{}:/tmp/honeybeepf.tmp", host))
        .status()
        .context("Failed to run scp")?;

    if !status.success() {
        bail!("scp failed");
    }

    // Move to final location with sudo
    let move_cmd = format!(
        "sudo mv /tmp/honeybeepf.tmp {} && sudo chmod +x {}",
        remote_path, remote_path
    );

    let status = Command::new("ssh")
        .arg(host)
        .arg(&move_cmd)
        .status()
        .context("Failed to run ssh command")?;

    if !status.success() {
        bail!("Failed to move binary to final location");
    }

    println!("✅ Deployed to {}:{}", host, remote_path);

    // Optionally restart service
    if restart {
        println!("🔄 Restarting honeybeepf service...");
        let status = Command::new("ssh")
            .arg(host)
            .arg("sudo systemctl restart honeybeepf || true")
            .status()
            .context("Failed to restart service")?;

        if status.success() {
            println!("✅ Service restarted");
        } else {
            println!("⚠️  Service restart failed (service might not exist)");
        }
    }

    Ok(())
}

fn install_service(host: &str, config: Option<&str>) -> Result<()> {
    let service_content = generate_systemd_service(config);

    println!("📝 Installing systemd service on {}...", host);

    // Write service file via ssh
    let escaped_content = service_content.replace("'", "'\\''");
    let cmd = format!(
        "echo '{}' | sudo tee /etc/systemd/system/honeybeepf.service > /dev/null && \
         sudo systemctl daemon-reload && \
         sudo systemctl enable honeybeepf",
        escaped_content
    );

    let status = Command::new("ssh")
        .arg(host)
        .arg(&cmd)
        .status()
        .context("Failed to install service")?;

    if !status.success() {
        bail!("Failed to install systemd service");
    }

    println!("✅ Systemd service installed and enabled");
    println!(
        "   Start with: ssh {} sudo systemctl start honeybeepf",
        host
    );

    Ok(())
}

fn generate_systemd_service(config: Option<&str>) -> String {
    let env_line = config
        .map(|c| format!("EnvironmentFile={}", c))
        .unwrap_or_default();

    format!(
        r#"[Unit]
Description=HoneybeePF eBPF Monitoring
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/honeybeepf
Restart=on-failure
RestartSec=5
{}

# Security hardening
NoNewPrivileges=no
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_BPF CAP_PERFMON CAP_NET_ADMIN
AmbientCapabilities=CAP_SYS_ADMIN CAP_BPF CAP_PERFMON CAP_NET_ADMIN

[Install]
WantedBy=multi-user.target
"#,
        env_line
    )
}

fn package(target: Option<&str>, output_dir: &str) -> Result<()> {
    // Build release
    build(true, target)?;

    let root = project_root();
    let output_path = root.join(output_dir);

    fs::create_dir_all(&output_path).context("Failed to create output directory")?;

    let profile = "release";
    let binary_path = if let Some(t) = target {
        root.join("target").join(t).join(profile).join("honeybeepf")
    } else {
        root.join("target").join(profile).join("honeybeepf")
    };

    if !binary_path.exists() {
        bail!("Binary not found at: {}", binary_path.display());
    }

    // Determine package name
    let arch = target.unwrap_or(std::env::consts::ARCH);
    let version = env!("CARGO_PKG_VERSION");
    let package_name = format!("honeybeepf-{}-{}", version, arch);

    let package_dir = output_path.join(&package_name);
    fs::create_dir_all(&package_dir)?;

    // Copy binary
    fs::copy(&binary_path, package_dir.join("honeybeepf"))?;

    // Copy example.env
    let env_example = root.join("example.env");
    if env_example.exists() {
        fs::copy(&env_example, package_dir.join("honeybeepf.env.example"))?;
    }

    // Generate install script
    let install_script = r#"#!/bin/bash
set -e

INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${CONFIG_DIR:-/etc/honeybeepf}"

echo "Installing honeybeepf..."

# Install binary
sudo install -m 755 honeybeepf "$INSTALL_DIR/honeybeepf"

# Install config
sudo mkdir -p "$CONFIG_DIR"
if [ -f honeybeepf.env.example ]; then
    sudo cp honeybeepf.env.example "$CONFIG_DIR/honeybeepf.env.example"
    if [ ! -f "$CONFIG_DIR/honeybeepf.env" ]; then
        sudo cp honeybeepf.env.example "$CONFIG_DIR/honeybeepf.env"
    fi
fi

echo "✅ Installed to $INSTALL_DIR/honeybeepf"
echo ""
echo "To install as a systemd service, run:"
echo "  sudo ./install-service.sh"
"#;

    fs::write(package_dir.join("install.sh"), install_script)?;

    // Generate service install script
    let service_script = format!(
        r#"#!/bin/bash
set -e

cat > /tmp/honeybeepf.service << 'EOF'
{}
EOF

sudo mv /tmp/honeybeepf.service /etc/systemd/system/honeybeepf.service
sudo systemctl daemon-reload
sudo systemctl enable honeybeepf

echo "✅ Systemd service installed"
echo "   Start with: sudo systemctl start honeybeepf"
"#,
        generate_systemd_service(Some("/etc/honeybeepf/honeybeepf.env"))
    );

    fs::write(package_dir.join("install-service.sh"), service_script)?;

    // Create tarball
    let tarball = output_path.join(format!("{}.tar.gz", package_name));

    let status = Command::new("tar")
        .current_dir(&output_path)
        .args(&["-czf", &tarball.to_string_lossy(), &package_name])
        .status()
        .context("Failed to create tarball")?;

    if !status.success() {
        bail!("Failed to create tarball");
    }

    // Cleanup directory
    fs::remove_dir_all(&package_dir)?;

    println!("✅ Package created: {}", tarball.display());

    Ok(())
}
