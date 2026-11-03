//! build.rs must build the eBPF object for the crate's target arch, not the build host's.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Remove the temp tree on every exit, panics included.
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_build_script(
    script: &Path,
    manifest: &Path,
    tmp: &Path,
    ebpf_dir: &Path,
    arch: &str,
) -> Vec<u8> {
    let out = tmp.join(format!("out-{arch}"));
    fs::create_dir_all(&out).expect("create OUT_DIR");
    let host = format!("{}-unknown-linux-gnu", env::consts::ARCH);
    let status = Command::new(script)
        .env("OUT_DIR", &out)
        .env("CARGO_CFG_TARGET_ARCH", arch)
        .env("CARGO_MANIFEST_DIR", manifest)
        .env(
            "CARGO",
            env::var("CARGO").unwrap_or_else(|_| "cargo".into()),
        )
        // arch aya falls back to when no bpf_target_arch cfg is passed
        .env("HOST", &host)
        // absolute, so build.rs's join() ignores the manifest dir and both arches share it
        .env("HONEYBEEPF_EBPF_TARGET_DIR", ebpf_dir)
        .stdout(Stdio::null())
        .status()
        .expect("spawn build script");
    assert!(status.success(), "build.rs failed for target arch {arch}");
    fs::read(out.join("honeybeepf-llm")).expect("build.rs produced no eBPF object")
}

#[test]
fn ebpf_object_follows_target_arch() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tmp = env::temp_dir().join(format!("hb-ebpf-arch-{}", std::process::id()));
    fs::create_dir_all(&tmp).expect("create temp dir");
    let _cleanup = Cleanup(tmp.clone());

    // Compile the tree's build.rs standalone so we can run it with a chosen target arch
    // without cross-compiling the whole agent (the eBPF object itself is always bpfel).
    let script = tmp.join("build_script");
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let compiled = Command::new(&rustc)
        .args(["--edition", "2024"])
        .arg(manifest.join("build.rs"))
        .arg("-o")
        .arg(&script)
        .status()
        .expect("spawn rustc");
    assert!(compiled.success(), "could not compile build.rs standalone");

    let ebpf_dir = tmp.join("ebpf");
    let x86 = run_build_script(&script, &manifest, &tmp, &ebpf_dir, "x86_64");
    let arm = run_build_script(&script, &manifest, &tmp, &ebpf_dir, "aarch64");

    assert!(!x86.is_empty() && !arm.is_empty(), "empty eBPF object");
    assert_ne!(
        x86, arm,
        "eBPF object is identical for x86_64 and aarch64: build.rs is not passing \
         bpf_target_arch to the nested build, so a cross-built arm64 image ships the \
         x86 pt_regs offsets and drops every captured call"
    );
}
