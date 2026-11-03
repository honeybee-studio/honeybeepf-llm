//! PR 0 spike: print every field and enum value plaintext capture reads from a kernel's BTF,
//! exiting non-zero when support is missing (as the agent decides at startup).
//! Run on a target kernel: `btf_spike [path-to-vmlinux-btf]` (default /sys/kernel/btf/vmlinux).

use honeybeepf_llm::btf_offsets::Btf;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/sys/kernel/btf/vmlinux".into());
    let btf = match Btf::from_path(&path) {
        Ok(b) => b,
        Err(e) => {
            println!("BTF  status=unavailable path={path} reason={e}");
            println!("RESULT plaintext_capture=DISABLED (no usable kernel BTF)");
            std::process::exit(2);
        }
    };
    println!("BTF  status=ok path={path}");

    let s = btf.plaintext_support();
    for (name, off) in &s.offsets {
        println!("FIELD ok    {name} = {off}");
    }
    match &s.payload {
        Some((name, off)) => println!("FIELD ok    iov_iter payload pointer -> {name} = {off}"),
        None => println!("FIELD MISS  iov_iter payload pointer"),
    }
    println!(
        "ENUM  ITER_IOVEC = {}",
        s.iter_iovec.map_or("missing".into(), |v| v.to_string())
    );
    println!(
        "ENUM  ITER_UBUF  = {}",
        s.iter_ubuf
            .map_or("absent (iovec-only kernel, pre-6.7)".into(), |v| v
                .to_string())
    );

    if s.available() {
        println!("RESULT plaintext_capture=ENABLED (all fields and enums resolved)");
    } else {
        for m in &s.missing {
            println!("MISS  {m}");
        }
        println!(
            "RESULT plaintext_capture=DISABLED ({} item(s) missing)",
            s.missing.len()
        );
        std::process::exit(1);
    }
}
