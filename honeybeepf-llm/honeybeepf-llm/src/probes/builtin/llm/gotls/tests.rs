use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

use object::{Object, ObjectSection, ObjectSymbol};

use super::*;

/// A program that links crypto/tls's Conn.Write, Read and Close.
const FIXTURE: &str = r#"package main

import "crypto/tls"

func main() {
	c, err := tls.Dial("tcp", "127.0.0.1:1", nil)
	if err != nil {
		return
	}
	c.Write([]byte("x"))
	c.Read(make([]byte, 1))
	c.Close()
}
"#;

/// Fixtures as (name, GOARCH, extra `go build` flags). The `-ext` ones are linked by the C linker, which puts
/// its own code before Go's in `.text`, as cgo and many distribution builds do.
const VARIANTS: &[(&str, &str, &[&str])] = &[
    ("fx-amd64", "amd64", &[]),
    ("fx-amd64-stripped", "amd64", &["-ldflags", "-s -w"]),
    ("fx-arm64", "arm64", &[]),
    ("fx-arm64-stripped", "arm64", &["-ldflags", "-s -w"]),
    (
        "fx-amd64-ext",
        "amd64",
        &["-buildmode=pie", "-ldflags", "-linkmode=external"],
    ),
    (
        "fx-amd64-ext-stripped",
        "amd64",
        &["-buildmode=pie", "-ldflags", "-linkmode=external -s -w"],
    ),
];

/// The Go toolchain from $GO or PATH. Without it the build tests are skipped, except in CI, where
/// the workflow installs Go and a missing toolchain must fail.
fn go_tool() -> Option<PathBuf> {
    let go = std::env::var_os("GO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("go"));
    if Command::new(&go)
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return Some(go);
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "Go toolchain not found in CI"
    );
    eprintln!("skipping: no Go toolchain (set GO or put go on PATH)");
    None
}

/// Whether the external-link fixtures can be built: on an x86-64 host with a C compiler.
fn c_linker() -> bool {
    cfg!(target_arch = "x86_64")
        && Command::new("cc")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
}

/// Builds the fixture variants once per test run.
fn fixtures() -> Option<&'static (PathBuf, PathBuf)> {
    static BUILT: OnceLock<Option<(PathBuf, PathBuf)>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let go = go_tool()?;
            let dir = std::env::temp_dir().join(format!("hb-gotls-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok()?;
            std::fs::write(dir.join("main.go"), FIXTURE).ok()?;
            std::fs::write(dir.join("go.mod"), "module fixture\n").ok()?;
            let external = c_linker();
            assert!(
                external || !cfg!(target_arch = "x86_64") || std::env::var_os("CI").is_none(),
                "C compiler not found in CI"
            );
            for (name, arch, flags) in VARIANTS {
                let cgo = name.contains("-ext");
                if cgo && !external {
                    continue;
                }
                let status = Command::new(&go)
                    .args(["build", "-trimpath"])
                    .args(*flags)
                    .arg("-o")
                    .arg(dir.join(name))
                    .arg(".")
                    .current_dir(&dir)
                    .env("GOOS", "linux")
                    .env("GOARCH", arch)
                    .env("CGO_ENABLED", if cgo { "1" } else { "0" })
                    .status()
                    .ok()?;
                assert!(status.success(), "go build {name} failed");
            }
            Some((go, dir))
        })
        .as_ref()
}

fn fixture(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

/// The fixtures that were built.
fn built(dir: &Path) -> impl Iterator<Item = &'static str> {
    VARIANTS
        .iter()
        .map(|(name, ..)| *name)
        .filter(|name| dir.join(name).exists())
}

/// File offset of `addr` in `.text`.
fn text_offset(text: &object::Section<'_, '_>, addr: u64) -> u64 {
    let (file_start, _) = text.file_range().expect(".text in the file");
    file_start + (addr - text.address())
}

/// File offsets of the RET instructions `go tool objdump` lists in Conn.Read.
fn objdump_returns(go: &Path, path: &str) -> BTreeSet<u64> {
    let out = Command::new(go)
        .args([
            "tool",
            "objdump",
            "-s",
            r"^crypto/tls\.\(\*Conn\)\.Read$",
            path,
        ])
        .output()
        .expect("go tool objdump");
    let text = String::from_utf8_lossy(&out.stdout);
    let data = std::fs::read(path).expect("read fixture");
    let obj = object::File::parse(&*data).expect("parse fixture");
    let section = obj.section_by_name(".text").expect(".text");
    text.lines()
        .filter(|l| l.split_whitespace().any(|w| w == "RET"))
        .map(|l| {
            let addr = l.split_whitespace().nth(1).expect("address column");
            let addr = u64::from_str_radix(addr.trim_start_matches("0x"), 16).expect("hex address");
            text_offset(&section, addr)
        })
        .collect()
}

#[test]
fn stripped_binaries_resolve_like_unstripped_ones() -> Result<()> {
    let Some((_, dir)) = fixtures() else {
        return Ok(());
    };
    for name in built(dir).filter(|n| !n.ends_with("-stripped")) {
        let full = inspect(&fixture(dir, name))?.context("probe targets")?;
        let stripped = inspect(&fixture(dir, &format!("{name}-stripped")))?
            .context("probe targets without symbols")?;
        assert_eq!(full, stripped, "{name}: stripping changed the targets");
        assert!(full.version.starts_with("go1."), "{}", full.version);
        assert!(full.close.is_some());
        let expected = if name.contains("amd64") {
            GoArch::Amd64
        } else {
            GoArch::Arm64
        };
        assert_eq!(full.arch, expected);
    }
    Ok(())
}

#[test]
fn read_returns_are_the_ret_instructions_objdump_lists() -> Result<()> {
    let Some((go, dir)) = fixtures() else {
        return Ok(());
    };
    for name in built(dir) {
        let path = fixture(dir, name);
        let targets = inspect(&path)?.context("probe targets")?;
        let ours: BTreeSet<u64> = targets.read_returns.iter().copied().collect();
        // objdump needs symbols, so the stripped binary is checked against its unstripped twin.
        let reference = objdump_returns(go, &fixture(dir, name.trim_end_matches("-stripped")));
        assert!(!reference.is_empty(), "{name}: objdump listed no RET");
        assert_eq!(ours, reference, "{name}");
    }
    Ok(())
}

#[test]
fn entry_offsets_point_at_the_functions() -> Result<()> {
    let Some((_, dir)) = fixtures() else {
        return Ok(());
    };
    for name in built(dir).filter(|n| !n.ends_with("-stripped")) {
        let path = fixture(dir, name);
        let data = std::fs::read(&path)?;
        let obj = object::File::parse(&*data)?;
        let text = obj.section_by_name(".text").context(".text")?;
        let symbol = |sym: &str| -> Result<u64> {
            let s = obj
                .symbols()
                .find(|s| s.name() == Ok(sym))
                .context(sym.to_owned())?;
            Ok(text_offset(&text, s.address()))
        };
        let targets = inspect(&path)?.context("probe targets")?;
        assert_eq!(targets.write, symbol(WRITE)?, "{name}");
        assert_eq!(targets.read, symbol(READ)?, "{name}");
        assert_eq!(targets.close, Some(symbol(CLOSE)?), "{name}");
    }
    Ok(())
}

#[test]
fn non_go_executables_have_no_targets() -> Result<()> {
    let me = std::env::current_exe()?;
    assert_eq!(inspect(&me.to_string_lossy())?, None);
    Ok(())
}

#[test]
fn versions_parse_with_suffixes() {
    assert_eq!(parse_version("go1.27.3"), Some((1, 27)));
    assert_eq!(parse_version("go1.24rc1"), Some((1, 24)));
    assert_eq!(parse_version("go1.20"), Some((1, 20)));
    assert_eq!(parse_version("devel go1.28-abc"), None);
    assert!(parse_version("go1.19.13").is_some_and(|v| v < MIN_GO));
}

#[test]
fn uvarint_matches_go_encoding() {
    assert_eq!(uvarint(&[0x08]), Some((8, 1)));
    assert_eq!(uvarint(&[0xac, 0x02]), Some((300, 2)));
    assert_eq!(uvarint(&[0x80]), None);
}
