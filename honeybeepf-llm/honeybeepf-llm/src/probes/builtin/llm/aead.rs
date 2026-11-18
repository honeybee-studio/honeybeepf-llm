//! Finds the AEAD method tables of a TLS library built into an executable, in a running process's memory.
//! A table is 4 size bytes (key, nonce, overhead, tag) and its function pointers; the AES-GCM tables are filled
//! at run time, so their functions cannot be read from the file and have no symbol in a stripped build.

use std::{fs::File, os::unix::fs::FileExt};

use anyhow::{Context, Result};
use object::{Object, ObjectSection, read::ReadCache};
use procfs::process::{MMPermissions, MMapPath, MemoryMap, Process};

/// Nonce, overhead and tag sizes of the AEADs TLS 1.3 uses, after a key size of 16 or 32.
const SIZES: [u8; 3] = [12, 16, 16];
/// Indexes of the probed functions; both libraries keep them in the same order.
const INIT: usize = 0;
const INIT_WITH_DIRECTION: usize = 1;
const CLEANUP: usize = 2;
const SEAL: usize = 4;
const OPEN: usize = 5;
/// A table has at least init, cleanup, seal and open.
const MIN_FUNCTIONS: usize = 4;
/// Longest table of any library; chunks overlap by it.
const MAX_TABLE_LEN: usize = 16 + 10 * 8;
const CHUNK: usize = 4 << 20;
/// Bits of a `/proc/<pid>/pagemap` entry: the page is in memory or in swap, and it is a page of a file
/// or of shared memory. A private page the process wrote has the first and not the second.
const PAGE_PRESENT_OR_SWAPPED: u64 = 0b11 << 62;
const PAGE_OF_FILE_OR_SHARED: u64 = 1 << 61;
/// Pagemap entries per read.
const PAGEMAP_CHUNK: usize = 4096;
/// Text each library keeps in its version string; only executables with one have their memory scanned.
const LIBRARY_MARKERS: [&[u8]; 2] = [b"BoringSSL", b"AWS-LC"];

/// The library a table belongs to, which sets where its function pointers are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Library {
    /// A 16-bit id and a flag word follow the sizes, then 10 pointers.
    AwsLc,
    /// 8 pointers follow the sizes.
    BoringSsl,
}

impl Library {
    /// Tried in this order: only aws-lc can be told by its header.
    const ALL: [Self; 2] = [Self::AwsLc, Self::BoringSsl];

    pub const fn name(self) -> &'static str {
        match self {
            Self::AwsLc => "aws-lc",
            Self::BoringSsl => "BoringSSL",
        }
    }

    /// (offset of the first pointer, number of pointers).
    const fn pointers(self) -> (usize, usize) {
        match self {
            Self::AwsLc => (16, 10),
            Self::BoringSsl => (8, 8),
        }
    }

    /// Whether the bytes between the sizes and the pointers fit; `table` holds at least 16 bytes.
    fn header_fits(self, table: &[u8]) -> bool {
        match self {
            Self::AwsLc => {
                table[4..6] != [0, 0]
                    && table[6..8] == [0, 0]
                    && matches!(table[8..16], [0 | 1, 0, 0, 0, 0, 0, 0, 0])
            }
            Self::BoringSsl => true,
        }
    }
}

/// File offsets of the probed functions of one table; 0 for one the table leaves unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Table {
    pub library: Library,
    /// `init` and `init_with_direction`, which key a context.
    pub init: [u64; 2],
    pub cleanup: u64,
    pub seal: u64,
    pub open: u64,
    /// Found in writable memory: an AES-GCM table, filled at the first TLS handshake that uses it.
    pub filled_at_run_time: bool,
}

/// An executable mapping of the file: [start, end) and the file offset of `start`.
type ExecRange = (u64, u64, u64);

fn file_offset(execs: &[ExecRange], ptr: u64) -> Option<u64> {
    execs
        .iter()
        .find(|&&(start, end, _)| (start..end).contains(&ptr))
        .map(|&(start, _, offset)| ptr - start + offset)
}

/// The table of `library` at the start of `bytes`, if its pointers make one.
fn table_of(library: Library, bytes: &[u8], execs: &[ExecRange], writable: bool) -> Option<Table> {
    let (at, count) = library.pointers();
    let table = bytes.get(..at + count * 8)?;
    if !library.header_fits(table) {
        return None;
    }
    // A null pointer is an unset function; any other must point into the executable's code.
    let offsets: Vec<u64> = table[at..]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|p| match u64::from_le_bytes(*p) {
            0 => Some(0),
            ptr => file_offset(execs, ptr),
        })
        .collect::<Option<_>>()?;
    (offsets[SEAL] != 0
        && offsets[OPEN] != 0
        && offsets.iter().filter(|&&o| o != 0).count() >= MIN_FUNCTIONS)
        .then_some(Table {
            library,
            init: [offsets[INIT], offsets[INIT_WITH_DIRECTION]],
            cleanup: offsets[CLEANUP],
            seal: offsets[SEAL],
            open: offsets[OPEN],
            filled_at_run_time: writable,
        })
}

/// Every table in `buf`, which starts at an 8-aligned address.
fn tables_in(buf: &[u8], execs: &[ExecRange], writable: bool) -> Vec<Table> {
    buf.chunks(8)
        .enumerate()
        .filter(|(_, head)| matches!(head[0], 16 | 32) && head.get(1..4) == Some(&SIZES))
        .filter_map(|(i, _)| {
            Library::ALL
                .into_iter()
                .find_map(|library| table_of(library, &buf[i * 8..], execs, writable))
        })
        .collect()
}

/// Add to `runs` the [start, end) of each page with a written entry; `entries` start at address `at`.
fn add_written(runs: &mut Vec<(u64, u64)>, entries: &[u8], at: u64, page: u64) {
    let pages = (at..).step_by(page as usize);
    for (entry, addr) in entries.as_chunks::<8>().0.iter().zip(pages) {
        let entry = u64::from_le_bytes(*entry);
        if entry & PAGE_PRESENT_OR_SWAPPED == 0 || entry & PAGE_OF_FILE_OR_SHARED != 0 {
            continue;
        }
        match runs.last_mut() {
            Some(run) if run.1 == addr => run.1 += page,
            _ => runs.push((addr, addr + page)),
        }
    }
}

/// The parts of [start, end) the process has written, or all of it if its page map cannot be read.
fn written_runs(pagemap: Option<&File>, (start, end): (u64, u64)) -> Vec<(u64, u64)> {
    let Some(pagemap) = pagemap else {
        return vec![(start, end)];
    };
    let page = procfs::page_size();
    let mut runs = Vec::new();
    let mut entries = vec![0u8; PAGEMAP_CHUNK * 8];
    let mut at = start;
    while at < end {
        let want =
            usize::try_from((end - at) / page).map_or(PAGEMAP_CHUNK, |n| n.min(PAGEMAP_CHUNK));
        let got = read_at(pagemap, &mut entries[..want * 8], at / page * 8) / 8;
        if got == 0 {
            return vec![(start, end)];
        }
        add_written(&mut runs, &entries[..got * 8], at, page);
        at += got as u64 * page;
    }
    runs
}

/// Whether the executable at `path` has a BoringSSL or aws-lc built in, by its version text in
/// `.rodata`; read from the file, not from any process.
pub fn has_library(path: &str) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let cache = ReadCache::new(file);
    let Ok(obj) = object::File::parse(&cache) else {
        return false;
    };
    obj.sections()
        .filter(|s| s.name() == Ok(".rodata"))
        .filter_map(|s| s.data().ok())
        .any(has_marker)
}

fn has_marker(data: &[u8]) -> bool {
    (0..data.len()).any(|i| LIBRARY_MARKERS.iter().any(|m| data[i..].starts_with(m)))
}

/// The code ranges of the executable with inode `ino`, and the memory to scan: the file's data and the
/// one zero-filled mapping right after it, as (start, end, writable). The heap and other memory are not read.
fn regions_of(maps: &[MemoryMap], ino: u64) -> (Vec<ExecRange>, Vec<(u64, u64, bool)>) {
    let (mut execs, mut regions) = (Vec::new(), Vec::new());
    let mut file_end = None;
    for map in maps {
        let (start, end) = map.address;
        let writable = map.perms.contains(MMPermissions::WRITE);
        let of_file = matches!(map.pathname, MMapPath::Path(_)) && map.inode == ino;
        if of_file && map.perms.contains(MMPermissions::EXECUTE) {
            execs.push((start, end, map.offset));
        } else if of_file && map.perms.contains(MMPermissions::READ) {
            regions.push((start, end, writable));
        } else if writable && file_end == Some(start) && matches!(map.pathname, MMapPath::Anonymous)
        {
            regions.push((start, end, true));
        }
        file_end = of_file.then_some(end);
    }
    (execs, regions)
}

/// The tables in the memory of `pid` that belongs to the executable with inode `ino` (see `regions_of`).
/// With `written_only`, only the pages the process has written, where it fills a table at run time.
pub fn scan(pid: u32, ino: u64, written_only: bool) -> Result<Vec<Table>> {
    let (execs, mut regions) = regions_of(&Process::new(pid as i32)?.maps()?.0, ino);
    let mem = File::open(format!("/proc/{pid}/mem")).context("process memory not readable")?;
    if written_only {
        let pagemap = File::open(format!("/proc/{pid}/pagemap")).ok();
        regions = regions
            .into_iter()
            .filter(|&(.., writable)| writable)
            .flat_map(|(start, end, _)| written_runs(pagemap.as_ref(), (start, end)))
            .map(|(start, end)| (start, end, true))
            .collect();
    }
    let mut tables: Vec<Table> = Vec::new();
    let mut buf = vec![0u8; CHUNK + MAX_TABLE_LEN];
    for (start, end, writable) in regions {
        let mut pos = start;
        while pos < end {
            // Chunks overlap by one table, so a table across a chunk boundary is still found.
            let want = usize::try_from(end - pos).map_or(buf.len(), |left| left.min(buf.len()));
            let got = read_at(&mem, &mut buf[..want], pos);
            for table in tables_in(&buf[..got], &execs, writable) {
                if !tables.contains(&table) {
                    tables.push(table);
                }
            }
            pos += CHUNK as u64;
        }
    }
    Ok(tables)
}

/// Read as much of `buf` as the process has mapped at `pos`; an unreadable page ends the read.
fn read_at(mem: &File, buf: &mut [u8], pos: u64) -> usize {
    let mut got = 0;
    while got < buf.len() {
        match mem.read_at(&mut buf[got..], pos + got as u64) {
            Ok(n) if n > 0 => got += n,
            _ => break,
        }
    }
    got
}

#[cfg(test)]
mod tests {
    use super::*;
    use procfs::FromBufRead;

    const EXECS: [ExecRange; 1] = [(0x40_0000, 0x50_0000, 0x1000)];

    fn boringssl(key_len: u8, pointers: [u64; 8]) -> Vec<u8> {
        let mut out = vec![key_len, 12, 16, 16, 0, 0, 0, 0];
        out.extend(pointers.iter().flat_map(|p| p.to_le_bytes()));
        out
    }

    fn aws_lc(key_len: u8, id: u16, extra_in: u8, pointers: [u64; 10]) -> Vec<u8> {
        let mut out = vec![key_len, 12, 16, 16];
        out.extend(id.to_le_bytes());
        out.extend([0, 0, extra_in, 0, 0, 0, 0, 0, 0, 0]);
        out.extend(pointers.iter().flat_map(|p| p.to_le_bytes()));
        out
    }

    #[test]
    fn test_boringssl_table_found_with_its_functions_as_file_offsets() {
        let mut buf = vec![0u8; 24];
        buf.extend(boringssl(
            32,
            [0x40_0100, 0, 0x40_0200, 0, 0x40_0300, 0x40_0400, 0, 0],
        ));
        buf.extend([0u8; 16]);
        let expected = Table {
            library: Library::BoringSsl,
            init: [0x1100, 0],
            cleanup: 0x1200,
            seal: 0x1300,
            open: 0x1400,
            filled_at_run_time: false,
        };
        assert_eq!(tables_in(&buf, &EXECS, false), [expected]);
        let filled = Table {
            filled_at_run_time: true,
            ..expected
        };
        assert_eq!(tables_in(&buf, &EXECS, true), [filled]);
    }

    #[test]
    fn test_aws_lc_table_found_with_pointers_after_its_id_and_flag() {
        // Read as a BoringSSL table, the flag word would be `init` and `open` (unset) would be `seal`.
        for extra_in in [0, 1] {
            let buf = aws_lc(
                32,
                0x18,
                extra_in,
                [
                    0x40_0100, 0, 0x40_0200, 0, 0x40_0300, 0x40_0400, 0, 0x40_0500, 0, 0,
                ],
            );
            let expected = Table {
                library: Library::AwsLc,
                init: [0x1100, 0],
                cleanup: 0x1200,
                seal: 0x1300,
                open: 0x1400,
                filled_at_run_time: false,
            };
            assert_eq!(tables_in(&buf, &EXECS, false), [expected]);
        }
    }

    #[test]
    fn test_aws_lc_table_with_all_ten_pointers_set_is_not_read_as_boringssl() {
        let mut pointers = [0u64; 10];
        for (i, p) in pointers.iter_mut().enumerate() {
            *p = 0x40_0100 + 0x100 * i as u64;
        }
        let tables = tables_in(&aws_lc(16, 0x10, 0, pointers), &EXECS, false);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].library, Library::AwsLc);
        assert_eq!((tables[0].seal, tables[0].open), (0x1500, 0x1600));
    }

    #[test]
    fn test_table_with_a_pointer_outside_the_code_is_not_a_table() {
        let buf = boringssl(
            16,
            [
                0x40_0100,
                0x7f00_0000,
                0x40_0200,
                0,
                0x40_0300,
                0x40_0400,
                0,
                0,
            ],
        );
        assert_eq!(tables_in(&buf, &EXECS, false), []);
    }

    #[test]
    fn test_table_without_an_open_function_is_not_a_table() {
        let buf = boringssl(16, [0x40_0100, 0x40_0150, 0x40_0200, 0, 0x40_0300, 0, 0, 0]);
        assert_eq!(tables_in(&buf, &EXECS, false), []);
    }

    #[test]
    fn test_written_pages_are_joined_into_runs_and_file_or_absent_pages_skipped() {
        let (present, swapped, of_file) = (1u64 << 63, 1u64 << 62, 1u64 << 61);
        let entries = [present, swapped, 0, present | of_file, present, present];
        let bytes: Vec<u8> = entries.iter().flat_map(|e| e.to_le_bytes()).collect();
        let mut runs = vec![(0x7000, 0x1_0000)];
        add_written(&mut runs, &bytes, 0x1_0000, 0x1000);
        assert_eq!(runs, [(0x7000, 0x1_2000), (0x1_4000, 0x1_6000)]);
    }

    #[test]
    fn test_sizes_at_an_unaligned_address_or_cut_short_are_skipped() {
        let mut buf = vec![0u8; 4];
        buf.extend(boringssl(
            32,
            [0x40_0100, 0, 0x40_0200, 0, 0x40_0300, 0x40_0400, 0, 0],
        ));
        assert_eq!(tables_in(&buf, &EXECS, false), []);
        let cut = boringssl(32, [0x40_0100; 8]);
        assert_eq!(tables_in(&cut[..cut.len() - 1], &EXECS, false), []);
    }

    #[test]
    fn test_only_the_file_data_and_the_zero_filled_mapping_after_it_are_read() {
        let maps = "\
00400000-00500000 r-xp 00001000 08:01 42 /usr/bin/tool
00500000-00600000 r--p 00101000 08:01 42 /usr/bin/tool
00600000-00700000 rw-p 00201000 08:01 42 /usr/bin/tool
00700000-00800000 rw-p 00000000 00:00 0 
00800000-00900000 rw-p 00000000 00:00 0 
00900000-00a00000 rw-p 00000000 00:00 0                                  [heap]
7f0000000000-7f0000100000 r--p 00000000 08:01 7 /usr/lib/libc.so.6
7f0000100000-7f0000200000 rw-p 00000000 00:00 0 
";
        let maps = procfs::process::MemoryMaps::from_buf_read(maps.as_bytes()).unwrap();
        let (execs, regions) = regions_of(&maps.0, 42);
        assert_eq!(execs, [(0x40_0000, 0x50_0000, 0x1000)]);
        assert_eq!(
            regions,
            [
                (0x50_0000, 0x60_0000, false),
                (0x60_0000, 0x70_0000, true),
                (0x70_0000, 0x80_0000, true),
            ]
        );
    }

    #[test]
    fn test_library_marker_found_anywhere_in_the_data() {
        assert!(has_marker(
            b"\0\0OpenSSL 1.1.1 (compatible; AWS-LC 1.66.2)\0"
        ));
        assert!(has_marker(b"BoringSSL"));
        assert!(!has_marker(b"OpenSSL 3.0.13 30 Jan 2024\0BoringSS"));
    }
}
