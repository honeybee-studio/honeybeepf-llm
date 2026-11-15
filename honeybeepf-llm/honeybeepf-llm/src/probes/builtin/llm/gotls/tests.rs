use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

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

/// Builds the fixture for amd64 and arm64, with and without symbols, once per test run.
fn fixtures() -> Option<&'static (PathBuf, PathBuf)> {
    static BUILT: OnceLock<Option<(PathBuf, PathBuf)>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let go = go_tool()?;
            let dir = std::env::temp_dir().join(format!("hb-gotls-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok()?;
            std::fs::write(dir.join("main.go"), FIXTURE).ok()?;
            std::fs::write(dir.join("go.mod"), "module fixture\n").ok()?;
            for arch in ["amd64", "arm64"] {
                for (suffix, ldflags) in [("", ""), ("-stripped", "-s -w")] {
                    let out = dir.join(format!("fx-{arch}{suffix}"));
                    let status = Command::new(&go)
                        .args(["build", "-trimpath", "-ldflags", ldflags, "-o"])
                        .arg(&out)
                        .arg(".")
                        .current_dir(&dir)
                        .env("GOOS", "linux")
                        .env("GOARCH", arch)
                        .env("CGO_ENABLED", "0")
                        .status()
                        .ok()?;
                    assert!(status.success(), "go build {arch}{suffix} failed");
                }
            }
            Some((go, dir))
        })
        .as_ref()
}

fn fixture(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
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
            file_offset(&section, addr).expect("in .text")
        })
        .collect()
}

#[test]
fn stripped_binaries_resolve_like_unstripped_ones() -> Result<()> {
    let Some((_, dir)) = fixtures() else {
        return Ok(());
    };
    for arch in ["amd64", "arm64"] {
        let full = inspect(&fixture(dir, &format!("fx-{arch}")))?.context("probe targets")?;
        let stripped = inspect(&fixture(dir, &format!("fx-{arch}-stripped")))?
            .context("probe targets from .gopclntab")?;
        assert_eq!(full, stripped, "{arch}: .symtab and .gopclntab disagree");
        assert!(full.version.starts_with("go1."), "{}", full.version);
        assert!(full.close.is_some());
        let expected = if arch == "amd64" {
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
    for name in [
        "fx-amd64",
        "fx-amd64-stripped",
        "fx-arm64",
        "fx-arm64-stripped",
    ] {
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
    let path = fixture(dir, "fx-amd64");
    let data = std::fs::read(&path)?;
    let obj = object::File::parse(&*data)?;
    let text = obj.section_by_name(".text").context(".text")?;
    let symbol = |name: &str| -> Result<u64> {
        let sym = obj
            .symbols()
            .find(|s| s.name() == Ok(name))
            .context(name.to_owned())?;
        file_offset(&text, sym.address())
    };
    let targets = inspect(&path)?.context("probe targets")?;
    assert_eq!(targets.write, symbol(WRITE)?);
    assert_eq!(targets.read, symbol(READ)?);
    assert_eq!(targets.close, Some(symbol(CLOSE)?));
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
