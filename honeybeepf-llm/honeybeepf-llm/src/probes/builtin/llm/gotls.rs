//! File offsets to probe Go's `crypto/tls` at: `(*Conn).Write`, `Read`, `Close`, and every `RET` in `Read`.
//! Returns get uprobes, not uretprobes: the Go runtime moves goroutine stacks, which breaks a uretprobe.
//! Functions come from the runtime's own tables (moduledata and pclntab), which stripping keeps, read in chunks.

use std::{fs::File, os::unix::fs::FileExt};

use anyhow::{Context, Result, bail};
use object::{
    Endianness, elf,
    read::{
        ReadCache,
        elf::{FileHeader, SectionHeader},
    },
};

const WRITE: &str = "crypto/tls.(*Conn).Write";
const READ: &str = "crypto/tls.(*Conn).Read";
const CLOSE: &str = "crypto/tls.(*Conn).Close";
const BUILDINFO_MAGIC: &[u8] = b"\xff Go buildinf:";
/// Build info with the version string inline after the 32-byte header (Go 1.18+); older ones point to it.
const BUILDINFO_INLINE: u8 = 0x2;
/// Enough of `.go.buildinfo` for the header and the version string.
const BUILDINFO_READ: u64 = 512;
/// pclntab header magic of Go 1.20 and later; earlier layouts differ.
const PCLNTAB_GO120: u32 = 0xffff_fff1;
/// Oldest Go whose tables this parser reads; it is also past the register ABI on amd64 and arm64.
const MIN_GO: (u32, u32) = (1, 20);
const ARM64_RET: u32 = 0xd65f_03c0;
/// Sections the linker puts the runtime's moduledata in: Go 1.26+, then older.
const MODULEDATA_SECTIONS: [&[u8]; 2] = [b".go.module", b".noptrdata"];
/// Word indexes in moduledata: the pclntab header, then the slices (pointer, length, capacity) and
/// addresses this parser reads.
const MD_FUNCNAMETAB: usize = 1;
const MD_PCLNTABLE: usize = 13;
const MD_FTAB: usize = 16;
const MD_MINPC: usize = 20;
const MD_MAXPC: usize = 21;
const MD_TEXT: usize = 22;
const MD_WORDS: usize = 23;
/// Bytes of a table read at a time, so a large binary never sits in memory whole.
const CHUNK: usize = 64 * 1024;

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

/// An allocated section that has bytes in the file.
struct Section {
    name: Vec<u8>,
    addr: u64,
    offset: u64,
    size: u64,
    exec: bool,
}

/// A 64-bit little-endian ELF executable: its section table, and the file to read data from.
struct Elf {
    file: File,
    arch: GoArch,
    sections: Vec<Section>,
}

/// The parts of the runtime's moduledata this parser reads: where the code starts, and the tables.
struct Moduledata {
    text: u64,
    maxpc: u64,
    funcnametab: (u64, u64),
    pclntable: (u64, u64),
    ftab: (u64, u64),
}

/// Probe targets in the Go executable at `path`; `Ok(None)` if it is not a Go binary this agent can probe
/// (not Go, Go before 1.20, another architecture, or `crypto/tls` not linked in).
pub fn inspect(path: &str) -> Result<Option<GoTlsTargets>> {
    let Some(elf) = Elf::open(path)? else {
        return Ok(None);
    };
    let Some(version) = go_version(&elf)? else {
        return Ok(None);
    };
    if parse_version(&version).is_none_or(|v| v < MIN_GO) {
        return Ok(None);
    }
    let md = moduledata(&elf)?.context("no Go moduledata")?;
    let funcs = find_funcs(&elf, &md, &[WRITE, READ, CLOSE])?;
    let (Some(write), Some(read)) = (funcs[0], funcs[1]) else {
        return Ok(None);
    };
    let read_returns = return_offsets(&elf, read)?;
    if read_returns.is_empty() {
        bail!("{READ} has no return instruction");
    }
    Ok(Some(GoTlsTargets {
        version,
        arch: elf.arch,
        write: elf.code_offset(write)?,
        read: elf.code_offset(read)?,
        read_returns,
        close: funcs[2].map(|f| elf.code_offset(f)).transpose()?,
    }))
}

impl Elf {
    /// `Ok(None)` if `path` is not a 64-bit little-endian x86-64 or arm64 ELF file.
    fn open(path: &str) -> Result<Option<Self>> {
        let file = File::open(path)?;
        let mut ident = [0u8; 6];
        if file.read_exact_at(&mut ident, 0).is_err()
            || ident[..4] != elf::ELFMAG
            || ident[4] != elf::ELFCLASS64
            || ident[5] != elf::ELFDATA2LSB
        {
            return Ok(None);
        }
        let cache = ReadCache::new(file);
        let (arch, sections) = {
            let header = elf::FileHeader64::<Endianness>::parse(&cache)?;
            let endian = header.endian()?;
            let arch = match header.e_machine(endian) {
                elf::EM_X86_64 => GoArch::Amd64,
                elf::EM_AARCH64 => GoArch::Arm64,
                _ => return Ok(None),
            };
            let table = header.sections(endian, &cache)?;
            let mut sections = Vec::new();
            for s in table.iter() {
                let flags = s.sh_flags(endian);
                if flags & u64::from(elf::SHF_ALLOC) == 0 {
                    continue;
                }
                let Some((offset, size)) = s.file_range(endian) else {
                    continue;
                };
                sections.push(Section {
                    name: table.section_name(endian, s)?.to_vec(),
                    addr: s.sh_addr(endian),
                    offset,
                    size,
                    exec: flags & u64::from(elf::SHF_EXECINSTR) != 0,
                });
            }
            (arch, sections)
        };
        Ok(Some(Self {
            file: cache.into_inner(),
            arch,
            sections,
        }))
    }

    fn section(&self, name: &[u8]) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// The section holding the `len` bytes at `addr`.
    fn section_at(&self, addr: u64, len: u64) -> Option<&Section> {
        self.sections.iter().find(|s| {
            addr >= s.addr
                && addr
                    .checked_add(len)
                    .is_some_and(|end| end <= s.addr + s.size)
        })
    }

    fn file_offset(&self, addr: u64, len: u64) -> Option<u64> {
        self.section_at(addr, len)
            .map(|s| s.offset + (addr - s.addr))
    }

    /// File offset of `func`, which must lie in executable code.
    fn code_offset(&self, func: Func) -> Result<u64> {
        let s = self
            .section_at(func.addr, func.size)
            .filter(|s| s.exec)
            .context("function outside the executable code")?;
        Ok(s.offset + (func.addr - s.addr))
    }

    fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; len];
        self.file.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }

    fn read_addr(&self, addr: u64, len: usize) -> Result<Vec<u8>> {
        let offset = self
            .file_offset(addr, len as u64)
            .context("address outside the file")?;
        self.read(offset, len)
    }
}

/// A table of `len` bytes in the file, read one chunk at a time.
struct Chunked<'e> {
    elf: &'e Elf,
    base: u64,
    len: u64,
    start: u64,
    buf: Vec<u8>,
}

impl<'e> Chunked<'e> {
    fn at_addr(elf: &'e Elf, (addr, len): (u64, u64)) -> Result<Self> {
        let base = elf
            .file_offset(addr, len)
            .context("Go table outside the file")?;
        Ok(Self::at_offset(elf, base, len))
    }

    fn at_offset(elf: &'e Elf, base: u64, len: u64) -> Self {
        Self {
            elf,
            base,
            len,
            start: 0,
            buf: Vec::new(),
        }
    }

    /// The `n` bytes at `off` into the table.
    fn get(&mut self, off: u64, n: usize) -> Result<&[u8]> {
        let end = off
            .checked_add(n as u64)
            .filter(|&end| end <= self.len)
            .context("read past the end of a Go table")?;
        if off < self.start || end > self.start + self.buf.len() as u64 {
            let take = usize::try_from(self.len - off)?.min(CHUNK.max(n));
            self.buf.resize(take, 0);
            self.elf
                .file
                .read_exact_at(&mut self.buf, self.base + off)?;
            self.start = off;
        }
        let i = usize::try_from(off - self.start)?;
        Ok(&self.buf[i..i + n])
    }

    fn u32_at(&mut self, off: u64) -> Result<u32> {
        Ok(u32::from_le_bytes(self.get(off, 4)?.try_into()?))
    }
}

/// The Go version from `.go.buildinfo`, e.g. `go1.27.3`; `None` if not Go or built before Go 1.18.
fn go_version(elf: &Elf) -> Result<Option<String>> {
    let Some(s) = elf.section(b".go.buildinfo") else {
        return Ok(None);
    };
    let data = elf.read(s.offset, usize::try_from(s.size.min(BUILDINFO_READ))?)?;
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

/// The runtime's moduledata, found by its first word: a pointer to the pclntab header, whose offsets
/// must match the moduledata's own table pointers. Function entries are relative to its `text`, which
/// is not the start of `.text` when a C linker put its own code first.
fn moduledata(elf: &Elf) -> Result<Option<Moduledata>> {
    for name in MODULEDATA_SECTIONS {
        let Some(s) = elf.section(name) else {
            continue;
        };
        let mut data = Chunked::at_offset(elf, s.offset, s.size);
        let span = (MD_WORDS * 8) as u64;
        let mut off = 0;
        while off + span <= s.size {
            let mut w = [0u64; MD_WORDS];
            for (word, b) in w
                .iter_mut()
                .zip(data.get(off, MD_WORDS * 8)?.as_chunks::<8>().0)
            {
                *word = u64::from_le_bytes(*b);
            }
            off += 8;
            let names_at = w[MD_FUNCNAMETAB].wrapping_sub(w[0]);
            if w[0] == 0 || !(1..=0x100).contains(&names_at) || w[MD_FTAB] != w[MD_PCLNTABLE] {
                continue;
            }
            let Ok(header) = elf.read_addr(w[0], 72) else {
                continue;
            };
            // Header words after the magic: nfunc, nfiles, (unused), funcnameOffset, cuOffset,
            // filetabOffset, pctabOffset, pclnOffset.
            let (h, _) = header[8..].as_chunks::<8>();
            let h: Vec<u64> = h.iter().map(|b| u64::from_le_bytes(*b)).collect();
            if u32::from_le_bytes(header[..4].try_into()?) != PCLNTAB_GO120
                || header[7] != 8
                || h[3] != names_at
                || h[7] != w[MD_PCLNTABLE].wrapping_sub(w[0])
                || h[0].wrapping_add(1) != w[MD_FTAB + 1]
                || w[MD_MINPC] != w[MD_TEXT]
            {
                continue;
            }
            return Ok(Some(Moduledata {
                text: w[MD_TEXT],
                maxpc: w[MD_MAXPC],
                funcnametab: (w[MD_FUNCNAMETAB], w[MD_FUNCNAMETAB + 1]),
                pclntable: (w[MD_PCLNTABLE], w[MD_PCLNTABLE + 1]),
                ftab: (w[MD_FTAB], w[MD_FTAB + 1].saturating_mul(8)),
            }));
        }
    }
    Ok(None)
}

/// Each name's function, looked up in the pclntab tables the moduledata points to.
fn find_funcs(elf: &Elf, md: &Moduledata, names: &[&str]) -> Result<Vec<Option<Func>>> {
    let wanted = name_offsets(elf, md, names)?;
    let mut ftab = Chunked::at_addr(elf, md.ftab)?;
    let mut pcln = Chunked::at_addr(elf, md.pclntable)?;
    // The table ends with one more entry, the end of the last function.
    let nfunc = md.ftab.1 / 8 - 1;
    let mut found = vec![None; names.len()];
    for i in 0..nfunc {
        let func = u64::from(ftab.u32_at(i * 8 + 4)?);
        let name = pcln.u32_at(func + 4)?;
        let Some(slot) = wanted.iter().position(|offs| offs.contains(&name)) else {
            continue;
        };
        let entry = ftab.u32_at(i * 8)?;
        let next = ftab.u32_at((i + 1) * 8)?;
        found[slot] = Some(Func {
            addr: md.text + u64::from(entry),
            size: u64::from(next.saturating_sub(entry)),
        });
    }
    if md.text + u64::from(ftab.u32_at(nfunc * 8)?) != md.maxpc {
        bail!("Go moduledata and pclntab disagree on the end of the code");
    }
    Ok(found)
}

/// Offsets in the function name table of each of `names`.
fn name_offsets(elf: &Elf, md: &Moduledata, names: &[&str]) -> Result<Vec<Vec<u32>>> {
    let mut tab = Chunked::at_addr(elf, md.funcnametab)?;
    let len = md.funcnametab.1;
    let mut found = vec![Vec::new(); names.len()];
    let mut off = 0;
    while off < len {
        let n = usize::try_from((len - off).min(CHUNK as u64))?;
        let chunk = tab.get(off, n)?;
        // Names are NUL-terminated; one cut by the chunk's end is read again with the next chunk.
        let Some(last) = chunk.iter().rposition(|&b| b == 0) else {
            off += n as u64;
            continue;
        };
        let mut start = 0;
        for name in chunk[..last].split(|&b| b == 0) {
            if let Some(slot) = names.iter().position(|n| n.as_bytes() == name) {
                found[slot].push(u32::try_from(off + start as u64)?);
            }
            start += name.len() + 1;
        }
        off += last as u64 + 1;
    }
    Ok(found)
}

/// File offsets of the return instructions in `func`.
fn return_offsets(elf: &Elf, func: Func) -> Result<Vec<u64>> {
    let base = elf.code_offset(func)?;
    let code = elf.read(base, usize::try_from(func.size)?)?;
    Ok(match elf.arch {
        GoArch::Amd64 => x86_returns(&code, func.addr)
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

#[cfg(test)]
mod tests;
