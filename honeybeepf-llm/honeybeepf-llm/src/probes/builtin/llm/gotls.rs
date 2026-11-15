//! File offsets to probe Go's `crypto/tls` at: `(*Conn).Write`, `Read`, `Close`, and every `RET` in `Read`.
//! Returns get uprobes, not uretprobes: the Go runtime moves goroutine stacks, which breaks a uretprobe.

use anyhow::{Context, Result, bail};
use object::{Architecture, Object, ObjectSection, ObjectSymbol, read::ReadCache};

const WRITE: &str = "crypto/tls.(*Conn).Write";
const READ: &str = "crypto/tls.(*Conn).Read";
const CLOSE: &str = "crypto/tls.(*Conn).Close";
const BUILDINFO_MAGIC: &[u8] = b"\xff Go buildinf:";
/// Build info with the version string inline after the 32-byte header (Go 1.18+); older ones point to it.
const BUILDINFO_INLINE: u8 = 0x2;
/// `.gopclntab` header magic of Go 1.20 and later; earlier layouts differ.
const PCLNTAB_GO120: u32 = 0xffff_fff1;
/// Oldest Go whose `.gopclntab` this parser reads; it is also past the register ABI on amd64 and arm64.
const MIN_GO: (u32, u32) = (1, 20);
const ARM64_RET: u32 = 0xd65f_03c0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoArch {
    Amd64,
    Arm64,
}

/// File offsets to attach the Go TLS probes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoTlsTargets {
    pub version: String,
    pub arch: GoArch,
    pub write: u64,
    pub read: u64,
    /// Every return instruction in `Read`, where its result and buffer are read.
    pub read_returns: Vec<u64>,
    pub close: Option<u64>,
}

/// A function's address range in the executable's address space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Func {
    addr: u64,
    size: u64,
}

/// Probe targets in the Go executable at `path`; `Ok(None)` if it is not a Go binary this agent can probe
/// (not Go, Go before 1.20, another architecture, or `crypto/tls` not linked in).
pub fn inspect(path: &str) -> Result<Option<GoTlsTargets>> {
    let cache = ReadCache::new(std::fs::File::open(path)?);
    let obj = object::File::parse(&cache)?;
    let arch = match obj.architecture() {
        Architecture::X86_64 => GoArch::Amd64,
        Architecture::Aarch64 => GoArch::Arm64,
        _ => return Ok(None),
    };
    let Some(version) = go_version(&obj)? else {
        return Ok(None);
    };
    if parse_version(&version).is_none_or(|v| v < MIN_GO) {
        return Ok(None);
    }
    let funcs = find_funcs(&obj, &[WRITE, READ, CLOSE])?;
    let (Some(write), Some(read)) = (funcs[0], funcs[1]) else {
        return Ok(None);
    };
    let text = obj.section_by_name(".text").context("no .text section")?;
    let read_returns = return_offsets(&text, read, arch)?;
    if read_returns.is_empty() {
        bail!("{READ} has no return instruction");
    }
    Ok(Some(GoTlsTargets {
        version,
        arch,
        write: file_offset(&text, write.addr)?,
        read: file_offset(&text, read.addr)?,
        read_returns,
        close: funcs[2].map(|f| file_offset(&text, f.addr)).transpose()?,
    }))
}

/// The Go version from `.go.buildinfo`, e.g. `go1.27.3`; `None` if not Go or built before Go 1.18.
fn go_version<'d, R: object::ReadRef<'d>>(obj: &object::File<'d, R>) -> Result<Option<String>> {
    let Some(section) = obj.section_by_name(".go.buildinfo") else {
        return Ok(None);
    };
    let data = section.data()?;
    let Some(header) = data.get(..32) else {
        return Ok(None);
    };
    if !header.starts_with(BUILDINFO_MAGIC) || header[15] & BUILDINFO_INLINE == 0 {
        return Ok(None);
    }
    let rest = &data[32..];
    let (len, n) = uvarint(rest).context("bad build info length")?;
    let version = rest
        .get(n..n + len as usize)
        .context("truncated build info")?;
    Ok(Some(String::from_utf8_lossy(version).into_owned()))
}

/// (major, minor) of a version like `go1.27.3` or `go1.24rc1`.
fn parse_version(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.strip_prefix("go")?.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?;
    let digits = minor
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(minor.len());
    Some((major, minor[..digits].parse().ok()?))
}

/// Unsigned LEB128 varint, as Go's encoding/binary writes it: (value, bytes read).
fn uvarint(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &b) in data.iter().take(10).enumerate() {
        value |= u64::from(b & 0x7f) << (7 * i);
        if b < 0x80 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Each name's function from `.symtab`, or from `.gopclntab` when the binary is stripped.
fn find_funcs<'d, R: object::ReadRef<'d>>(
    obj: &object::File<'d, R>,
    names: &[&str],
) -> Result<Vec<Option<Func>>> {
    let mut found = vec![None; names.len()];
    for sym in obj.symbols() {
        if let Ok(name) = sym.name()
            && let Some(i) = names.iter().position(|n| *n == name)
            && sym.size() > 0
        {
            found[i] = Some(Func {
                addr: sym.address(),
                size: sym.size(),
            });
        }
    }
    if found.iter().any(Option::is_some) {
        return Ok(found);
    }
    let pclntab = obj
        .section_by_name(".gopclntab")
        .context("no .symtab or .gopclntab")?;
    let text = obj.section_by_name(".text").context("no .text section")?;
    pclntab_funcs(pclntab.data()?, text.address(), names)
}

/// Looks `names` up in a Go 1.20+ `.gopclntab`. Function addresses are relative to the start of
/// `.text`, since the header's text start is only filled in at run time.
fn pclntab_funcs(tab: &[u8], text_start: u64, names: &[&str]) -> Result<Vec<Option<Func>>> {
    let u32_at = |off: usize| -> Result<u32> {
        let b = tab.get(off..off + 4).context("pclntab out of range")?;
        Ok(u32::from_le_bytes(b.try_into()?))
    };
    if u32_at(0)? != PCLNTAB_GO120 {
        bail!("unsupported .gopclntab format {:#x}", u32_at(0)?);
    }
    let ptr = usize::from(*tab.get(7).context("short pclntab")?);
    if ptr != 8 {
        bail!("unsupported pointer size {ptr}");
    }
    let word = |i: usize| -> Result<usize> {
        let off = 8 + i * ptr;
        let b = tab.get(off..off + 8).context("pclntab out of range")?;
        Ok(usize::try_from(u64::from_le_bytes(b.try_into()?))?)
    };
    // Header words after the magic: nfunc, nfiles, textStart, funcnameOffset, cuOffset,
    // filetabOffset, pctabOffset, pclnOffset.
    let nfunc = word(0)?;
    let funcnames = word(3)?;
    let functab = word(7)?;

    let mut found = vec![None; names.len()];
    for i in 0..nfunc {
        let entry = u32_at(functab + i * 8)?;
        let func = functab + usize::try_from(u32_at(functab + i * 8 + 4)?)?;
        let name_off = usize::try_from(u32_at(func + 4)?)?;
        let start = funcnames + name_off;
        let end = tab
            .get(start..)
            .and_then(|s| s.iter().position(|&b| b == 0))
            .context("unterminated function name")?;
        let Some(slot) = names
            .iter()
            .position(|n| n.as_bytes() == &tab[start..start + end])
        else {
            continue;
        };
        // The table ends with one more entry, the end of the last function.
        let next = u32_at(functab + (i + 1) * 8)?;
        found[slot] = Some(Func {
            addr: text_start + u64::from(entry),
            size: u64::from(next.saturating_sub(entry)),
        });
    }
    Ok(found)
}

/// File offsets of the return instructions in `func`.
fn return_offsets<'d, 'f, R: object::ReadRef<'d>>(
    text: &object::Section<'d, 'f, R>,
    func: Func,
    arch: GoArch,
) -> Result<Vec<u64>> {
    let start = usize::try_from(
        func.addr
            .checked_sub(text.address())
            .context("function outside .text")?,
    )?;
    let code = text
        .data()?
        .get(start..start + usize::try_from(func.size)?)
        .context("function past the end of .text")?;
    let base = file_offset(text, func.addr)?;
    Ok(match arch {
        GoArch::Amd64 => x86_returns(code, func.addr)
            .into_iter()
            .map(|addr| base + (addr - func.addr))
            .collect(),
        GoArch::Arm64 => code
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(_, w)| u32::from_le_bytes(**w) == ARM64_RET)
            .map(|(i, _)| base + 4 * i as u64)
            .collect(),
    })
}

/// Addresses of `RET` instructions in x86-64 code that starts at `addr`.
fn x86_returns(code: &[u8], addr: u64) -> Vec<u64> {
    use iced_x86::{Code, Decoder, DecoderOptions};
    let mut decoder = Decoder::with_ip(64, code, addr, DecoderOptions::NONE);
    let mut out = Vec::new();
    for ins in &mut decoder {
        // Near returns, with and without a stack adjustment; Go emits the plain C3 form.
        if matches!(ins.code(), Code::Retnq | Code::Retnq_imm16) {
            out.push(ins.ip());
        }
    }
    out
}

fn file_offset<'d, 'f, R: object::ReadRef<'d>>(
    text: &object::Section<'d, 'f, R>,
    addr: u64,
) -> Result<u64> {
    let (file_start, _) = text.file_range().context(".text has no file data")?;
    Ok(file_start
        + addr
            .checked_sub(text.address())
            .context("address below .text")?)
}

#[cfg(test)]
mod tests;
