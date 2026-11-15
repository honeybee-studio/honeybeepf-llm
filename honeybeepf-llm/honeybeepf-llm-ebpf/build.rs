use std::env;

/// Sets `bpf_target_arch` the way aya-ebpf's build script does: the value passed in, else the build
/// host's architecture. The Go TLS probes read registers by it, so it has to match aya's.
fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_CFG_BPF_TARGET_ARCH");
    let arch = env::var("CARGO_CFG_BPF_TARGET_ARCH").unwrap_or_else(|_| {
        let host = env::var("HOST").unwrap_or_default();
        host.split_once('-')
            .map_or(host.clone(), |(arch, _)| arch.to_owned())
    });
    println!("cargo:rustc-cfg=bpf_target_arch=\"{arch}\"");
    println!("cargo::rustc-check-cfg=cfg(bpf_target_arch, values(\"x86_64\", \"aarch64\"))");
}
