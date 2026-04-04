use std::{env, path::PathBuf, process::Command};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed=../honeybeepf-ebpf/src");

    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();

    let bpf_target = match arch.as_str() {
        "x86_64" | "x86" | "aarch64" | "arm" | "riscv64" => "bpfel-unknown-none",
        "mips" | "mips64" | "powerpc" | "powerpc64" | "s390x" => "bpfeb-unknown-none",
        _ => {
            eprintln!(
                "Warning: Unknown architecture '{}', defaulting to bpfel-unknown-none",
                arch
            );
            "bpfel-unknown-none"
        }
    };

    println!(
        "cargo:warning=Building eBPF for target: {} (host arch: {})",
        bpf_target, arch
    );

    let ebpf_dir = PathBuf::from("../honeybeepf-ebpf");

    // Use a separate target directory for eBPF to avoid file lock conflicts
    let ebpf_target_dir = PathBuf::from(env::var("HOME").unwrap()).join("cargo-target-ebpf");

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

    println!("cargo:warning=Running eBPF build...");

    let status = Command::new(cargo)
        .current_dir(&ebpf_dir)
        .env("RUSTUP_TOOLCHAIN", "nightly")
        .env("CARGO_TARGET_DIR", &ebpf_target_dir) // Separate target dir
        .args([
            "build",
            "--release",
            &format!("--target={}", bpf_target),
            "-Z",
            "build-std=core",
        ])
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .expect("Failed to execute cargo command");

    if !status.success() {
        panic!("Failed to build eBPF program");
    }

    println!("cargo:warning=eBPF build completed, copying binary...");

    let ebpf_binary = ebpf_target_dir.join(format!("{}/release/honeybeepf", bpf_target));

    let out_file = out_dir.join("honeybeepf");
    std::fs::copy(&ebpf_binary, &out_file).expect("Failed to copy eBPF object file");

    println!("cargo:warning=Build script completed successfully");
}
