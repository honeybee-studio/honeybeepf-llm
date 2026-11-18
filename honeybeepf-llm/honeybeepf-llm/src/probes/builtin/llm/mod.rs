mod aead;
mod background;
mod conn_filter;
#[cfg(feature = "k8s")]
pub(crate) mod coverage;
pub mod gotls;
pub mod http;
pub mod processor;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use aya::{
    Ebpf,
    maps::{HashMap as BpfHashMap, MapData, PerCpuArray},
};
use background::BackgroundCalls;
use conn_filter::ConnFilter;
use honeybeepf_llm_common::{LlmEventHeader, LlmStat};
use honeybeepf_llm_core::types::LlmDirection;
use log::{debug, info, warn};
use processor::{CallContext, Outcome, StreamProcessor};

use crate::audit;
use crate::telemetry::Transport;
use regex::Regex;

use crate::probes::{
    DynamicProbe, IdentityResolver, Probe, ProcessInfo, TracepointConfig, attach_kprobe,
    attach_tracepoint, attach_uprobe, detach_uprobe,
    discovery::{
        FileId, exported_function_offsets, file_identity, find_libraries_all,
        find_process_executables,
    },
    load_uprobe, spawn_ringbuf_handler_var,
};
// Re-export exec watch types and functions for lib.rs
pub use crate::probes::{
    ExecNotify, ExecPidQueue, LibOpenQueue, setup_exec_watch, setup_lib_open_watch,
};

// Timing constants
const CLEANUP_INTERVAL_SECS: u64 = 30; // How often to run cleanup
const CONNECTION_RETENTION_SECS: u64 = 300; // Keep idle connections for 5 minutes
const STATS_LOG_INTERVAL_SECS: u64 = 60; // How often to log event counters (only when changed)

/// Kernel-side event pipeline counters (LLM_STATS map), set once when the probe attaches.
static EVENT_STATS: OnceLock<PerCpuArray<MapData, u64>> = OnceLock::new();

/// Totals of the kernel event counters, summed over CPUs, in `LlmStat::ALL` order.
pub fn event_stats() -> Option<[u64; LlmStat::COUNT]> {
    let map = EVENT_STATS.get()?;
    let mut totals = [0u64; LlmStat::COUNT];
    for (i, total) in totals.iter_mut().enumerate() {
        let values = map.get(&(i as u32), 0).ok()?;
        *total = values.iter().sum();
    }
    Some(totals)
}

/// Log the kernel event counters as one line.
pub fn log_event_stats() {
    if let Some(totals) = event_stats() {
        let line = LlmStat::ALL
            .iter()
            .zip(totals.iter())
            .map(|(stat, v)| format!("{}={v}", stat.name()))
            .collect::<Vec<_>>()
            .join(" ");
        info!("[LLM stats] {line}");
    }
}

/// (entry program, return program, function); each pair only works together.
const REQUIRED_PROBES: [(&str, &str, &str); 2] = [
    ("probe_ssl_rw_enter", "probe_ssl_read_exit", "SSL_read"),
    ("probe_ssl_rw_enter", "probe_ssl_write_exit", "SSL_write"),
];

/// Handshake events are dropped in userspace; _ex variants are not in every OpenSSL build.
const OPTIONAL_PROBES: [(&str, &str, &str); 3] = [
    (
        "probe_ssl_do_handshake_enter",
        "probe_ssl_do_handshake_exit",
        "SSL_do_handshake",
    ),
    (
        "probe_ssl_rw_ex_enter",
        "probe_ssl_write_ex_exit",
        "SSL_write_ex",
    ),
    (
        "probe_ssl_rw_ex_enter",
        "probe_ssl_read_ex_exit",
        "SSL_read_ex",
    ),
];

/// (program, function) that ends a connection's stream; optional like the probes above.
const CLOSE_PROBE: (&str, &str) = ("probe_ssl_free", "SSL_free");

/// Clears an exiting thread's session entries; one per node, not per probed file.
const TASK_EXIT_PROBE: TracepointConfig = TracepointConfig {
    program_name: "probe_ssl_task_exit",
    category: "sched",
    name: "sched_process_exit",
};

/// Go `crypto/tls` programs, loaded at start so that a verifier rejection is logged on every node even
/// before a Go executable is attached.
const GO_TLS_PROGRAMS: [&str; 4] = [
    "probe_go_tls_write",
    "probe_go_tls_read_enter",
    "probe_go_tls_read_return",
    "probe_go_tls_close",
];

/// AEAD programs for a TLS library built into an executable: context init and cleanup, seal entry,
/// open entry and open return.
const AEAD_PROGRAMS: [&str; 5] = [
    "probe_aead_init",
    "probe_aead_cleanup",
    "probe_aead_seal",
    "probe_aead_open_enter",
    "probe_aead_open_exit",
];

/// Kernel functions that tie a sealed or opened record to its TCP socket, with their programs.
const AEAD_SOCKET_PROBES: [(&str, &str); 2] = [
    ("probe_aead_tcp_sendmsg", "tcp_sendmsg"),
    ("probe_aead_tcp_recvmsg", "tcp_recvmsg"),
];

/// Scans of one process: each AES-GCM table is filled at the first TLS handshake that uses its cipher.
const AEAD_SCANS: u32 = 20;
/// Scans at every poll; after them the wait doubles, so the last scan is about half an hour later.
const AEAD_DENSE_SCANS: u32 = 8;
/// Age of a process at its first scan: a loader relocates the table pointers before `main`.
const AEAD_SETTLE: Duration = Duration::from_millis(100);
/// Processes waiting for a scan; a burst of short-lived processes beyond it is not scanned.
const AEAD_PENDING: usize = 64;

/// Every function in the probe tables, resolved in one pass over the file's symbols.
const PROBED_FUNCTIONS: [&str; 6] = [
    "SSL_read",
    "SSL_write",
    "SSL_do_handshake",
    "SSL_write_ex",
    "SSL_read_ex",
    "SSL_free",
];

/// Attach the SSL probes to `libssl_path`; `Ok(false)` if it does not export SSL_read and SSL_write.
pub fn attach_probes_to_path(bpf: &mut Ebpf, libssl_path: &str) -> Result<bool> {
    let offsets = exported_function_offsets(libssl_path, &PROBED_FUNCTIONS)?;
    let Some(required) = REQUIRED_PROBES
        .iter()
        .map(|&(enter, exit, func)| offsets.get(func).map(|&off| (enter, exit, func, off)))
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(false);
    };

    // Roll back on failure so a retry on the same file doesn't stack duplicate probes.
    let mut attached = Vec::with_capacity(REQUIRED_PROBES.len() * 2);
    for (enter, exit, func, offset) in required {
        for prog in [enter, exit] {
            match attach_uprobe(bpf, prog, func, libssl_path, offset) {
                Ok(link) => attached.push((prog, link)),
                Err(e) => {
                    for (prog, link) in attached {
                        if let Err(de) = detach_uprobe(bpf, prog, link) {
                            warn!("Failed to roll back {prog} on {libssl_path}: {de}");
                        }
                    }
                    return Err(e);
                }
            }
        }
    }

    for (enter, exit, func) in OPTIONAL_PROBES {
        let Some(&offset) = offsets.get(func) else {
            continue;
        };
        if let Ok(link) = attach_uprobe(bpf, enter, func, libssl_path, offset)
            && attach_uprobe(bpf, exit, func, libssl_path, offset).is_err()
        {
            let _ = detach_uprobe(bpf, enter, link);
        }
    }

    // Without it a connection's stream is only dropped after CONNECTION_RETENTION_SECS idle.
    let (prog, func) = CLOSE_PROBE;
    if let Err(e) = offsets
        .get(func)
        .context("not exported")
        .and_then(|&offset| attach_uprobe(bpf, prog, func, libssl_path, offset))
    {
        warn!("{func} not probed in {libssl_path}: {e}");
    }

    Ok(true)
}

/// Attach the Go `crypto/tls` probes to the Go executable at `path`: `Ok(None)` if it has no
/// `crypto/tls` this agent can probe, else the Go version and the number of returns probed in `Read`.
fn attach_go_tls(bpf: &mut Ebpf, path: &str) -> Result<Option<(String, usize)>> {
    // An executable whose Go tables cannot be read is left unprobed, not failed: nothing was attached.
    let targets = match gotls::inspect(path) {
        Ok(Some(targets)) => targets,
        Ok(None) => return Ok(None),
        Err(e) => {
            info!("Go executable not probed, its function tables could not be read: {path}: {e:#}");
            return Ok(None);
        }
    };
    let [write, read_enter, read_return, close] = GO_TLS_PROGRAMS;
    let mut sites = vec![
        (write, "crypto/tls.(*Conn).Write", targets.write),
        (read_enter, "crypto/tls.(*Conn).Read", targets.read),
    ];
    sites.extend(
        targets
            .read_returns
            .iter()
            .map(|&off| (read_return, "crypto/tls.(*Conn).Read return", off)),
    );
    // Roll back on failure, as for libssl, so a retry does not stack duplicate probes.
    let mut attached = Vec::with_capacity(sites.len());
    for (prog, func, offset) in sites {
        match attach_uprobe(bpf, prog, func, path, offset) {
            Ok(link) => attached.push((prog, link)),
            Err(e) => {
                for (prog, link) in attached {
                    if let Err(de) = detach_uprobe(bpf, prog, link) {
                        warn!("Failed to roll back {prog} on {path}: {de}");
                    }
                }
                return Err(e);
            }
        }
    }
    // Without it a connection's stream is only dropped after CONNECTION_RETENTION_SECS idle.
    match targets.close {
        Some(offset) => {
            if let Err(e) = attach_uprobe(bpf, close, "crypto/tls.(*Conn).Close", path, offset) {
                warn!("crypto/tls.(*Conn).Close not probed in {path}: {e}");
            }
        }
        None => warn!("crypto/tls.(*Conn).Close not found in {path}"),
    }
    Ok(Some((targets.version, targets.read_returns.len())))
}

/// One path per distinct file not in `known`, preferring host paths over `/proc/<pid>/root`.
fn unique_new_files<'a>(
    paths: impl IntoIterator<Item = &'a String>,
    known: &HashSet<FileId>,
) -> Vec<(&'a str, FileId)> {
    // A /proc/<pid>/root path disappears when that process exits before the attach.
    let mut paths: Vec<&String> = paths.into_iter().collect();
    paths.sort_by_key(|p| p.starts_with("/proc/"));
    let mut out: Vec<(&str, FileId)> = Vec::new();
    for path in paths {
        let Some(id) = file_identity(path) else {
            continue;
        };
        if !known.contains(&id) && !out.iter().any(|(_, seen)| *seen == id) {
            out.push((path, id));
        }
    }
    out
}

pub struct LlmProbe {
    /// Files already tried (attached, failed or exporting no SSL functions), so each is attempted once.
    known_targets: Arc<Mutex<HashSet<FileId>>>,
    /// Files the SSL probes are attached to, for the per-pod TLS coverage.
    attached: Arc<Mutex<HashSet<FileId>>>,
    /// Also probe main executables with OpenSSL built in (official Node.js builds).
    executables: bool,
    /// Set when executables are also scanned for the AEAD tables of a built-in BoringSSL or aws-lc.
    aead: Option<Mutex<AeadScans>>,
}

/// A process whose executable no SSL or Go probe fits, scanned until its AEAD tables show.
struct AeadPending {
    pid: u32,
    path: String,
    id: FileId,
    queued: Instant,
    scans: u32,
    /// Polls to skip before the next scan.
    wait: u32,
}

/// Whether the processes of an executable file have their memory scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileScan {
    /// No BoringSSL or aws-lc in the file, or no table at its first scan.
    Skip,
    Scan,
    /// A table filled at run time was found, so every table of the file is known.
    Done,
}

#[derive(Default)]
struct AeadScans {
    /// Executable names to scan; empty scans every executable with the library built in.
    names: Vec<String>,
    pending: Vec<AeadPending>,
    files: HashMap<FileId, FileScan>,
    /// Tables found in each file; its other processes share them.
    found: HashSet<(FileId, aead::Table)>,
    /// (file, program, offset) of each attached probe, since tables share functions.
    attached: HashSet<(FileId, &'static str, u64)>,
    /// Whether the socket probes attached; `None` until the first table is found.
    socket_probes: Option<bool>,
}

/// Whether the executable at `path` is one of `names`, with or without its extension (`claude.exe`).
fn named(names: &[String], path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.split('.').next().unwrap_or(file);
    names.is_empty() || names.iter().any(|n| n == file || n == stem)
}

impl LlmProbe {
    /// `aead`: scan executables for AEAD tables, only those named when the list is not empty.
    pub fn new(executables: bool, detect_by_shape: bool, aead: Option<Vec<String>>) -> Self {
        // Shape detection is read by the host-blind parser globals, so push it once here.
        http::set_detect_by_shape(detect_by_shape);
        if detect_by_shape {
            warn!(
                "LLM detection by request shape is on (experimental): unlisted hosts are recorded by path and usage shape"
            );
        }
        Self {
            known_targets: Arc::new(Mutex::new(HashSet::new())),
            attached: Arc::new(Mutex::new(HashSet::new())),
            executables,
            aead: aead.map(|names| {
                Mutex::new(AeadScans {
                    names,
                    ..AeadScans::default()
                })
            }),
        }
    }

    /// Queue the process for an AEAD scan unless a probe is attached to its executable already, or
    /// `limit` processes wait.
    fn queue_aead_scan(&self, bpf: &mut Ebpf, pid: u32, path: &str, limit: usize) {
        let Some(scans) = &self.aead else { return };
        let Some(id) = file_identity(path) else {
            return;
        };
        let probed = self
            .attached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&id);
        let mut scans = scans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if probed || scans.socket_probes == Some(false) {
            return;
        }
        if scans.files.get(&id) == Some(&FileScan::Done) {
            // Its probes are live in every process of the file; only the sockets are left to learn.
            track_receives(bpf, pid);
        } else if scans.pending.len() < limit
            && scans.pending.iter().all(|p| p.pid != pid)
            && scans.should_scan(id, path)
        {
            let mut process = AeadPending {
                pid,
                path: path.to_owned(),
                id,
                queued: Instant::now(),
                scans: 0,
                wait: 0,
            };
            scans.skip_known_tables(bpf, &mut process);
            scans.pending.push(process);
        }
    }

    fn attach_new<'a, I>(&self, bpf: &mut Ebpf, paths: I) -> Vec<(&'a str, FileId, Result<bool>)>
    where
        I: IntoIterator<Item = &'a String> + Clone,
    {
        let results = attach_new_files(&self.known_targets, paths, |path| {
            attach_probes_to_path(bpf, path)
        });
        let mut attached = self
            .attached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        attached.extend(
            results
                .iter()
                .filter(|(_, _, r)| matches!(r, Ok(true)))
                .map(|(_, id, _)| *id),
        );
        results
    }
}

/// The TLS code probed in a main executable.
enum ExeTls {
    OpenSsl,
    /// Go version and the number of returns probed in `(*Conn).Read`.
    Go(String, usize),
}

impl LlmProbe {
    /// Like `attach_new` for main executables: built-in OpenSSL first, then Go `crypto/tls`.
    fn attach_new_exes<'a, I>(
        &self,
        bpf: &mut Ebpf,
        paths: I,
    ) -> Vec<(&'a str, FileId, Result<Option<ExeTls>>)>
    where
        I: IntoIterator<Item = &'a String> + Clone,
    {
        let mut go = HashMap::new();
        let results = attach_new_files(&self.known_targets, paths, |path| {
            if attach_probes_to_path(bpf, path)? {
                return Ok(true);
            }
            let found = attach_go_tls(bpf, path)?;
            let attached = found.is_some();
            if let Some(info) = found {
                go.insert(path.to_owned(), info);
            }
            Ok(attached)
        });
        let mut attached = self
            .attached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        results
            .into_iter()
            .map(|(path, id, result)| {
                let result = result.map(|ok| {
                    ok.then(|| {
                        attached.insert(id);
                        go.remove(path)
                            .map_or(ExeTls::OpenSsl, |(v, n)| ExeTls::Go(v, n))
                    })
                });
                (path, id, result)
            })
            .collect()
    }
}

/// Rounds of picking another path to a file whose chosen path vanished during the attach.
const ATTACH_ROUNDS: usize = 3;

/// Try each file not tried before, holding the lock only to pick candidates and record them.
fn attach_new_files<'a, I>(
    known: &Mutex<HashSet<FileId>>,
    paths: I,
    mut attach: impl FnMut(&str) -> Result<bool>,
) -> Vec<(&'a str, FileId, Result<bool>)>
where
    I: IntoIterator<Item = &'a String> + Clone,
{
    let mut out = Vec::new();
    for _ in 0..ATTACH_ROUNDS {
        let candidates = {
            let known = known
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            unique_new_files(paths.clone(), &known)
        };
        let mut vanished = false;
        for (path, id) in candidates {
            let result = attach(path);
            // A /proc/<pid>/root path dies with its process; recording the file would skip it for good.
            if result.is_err() && !std::path::Path::new(path).exists() {
                vanished = true;
                continue;
            }
            known
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id);
            out.push((path, id, result));
        }
        if !vanished {
            break;
        }
    }
    out
}

/// Most executables have no TLS code to probe, so only an attach or an error gets a log line.
fn log_executable(path: &str, id: FileId, result: Result<Option<ExeTls>>) {
    match result {
        Ok(Some(ExeTls::OpenSsl)) => {
            audit::attach("exe_openssl", path, id.ino(), "built-in OpenSSL")
        }
        Ok(Some(ExeTls::Go(version, returns))) => audit::attach(
            "exe_go",
            path,
            id.ino(),
            &format!("{version}, crypto/tls, {returns} Read returns"),
        ),
        Ok(None) => {}
        Err(e) => audit::attach_failed(path, &e),
    }
}

/// Track the TCP receives of `pid`, so a cipher context it keys is bound to the socket just read.
fn track_receives(bpf: &mut Ebpf, pid: u32) {
    let tracked = bpf
        .map_mut("AEAD_TGIDS")
        .context("AEAD_TGIDS map not found")
        .and_then(|map| Ok(BpfHashMap::<_, u32, u8>::try_from(map)?))
        .and_then(|mut map| Ok(map.insert(pid, 1, 0)?));
    if let Err(e) = tracked {
        debug!("TCP receives of PID {pid} not tracked: {e:#}");
    }
}

impl AeadScans {
    /// Whether processes of the file are scanned: it is named (when names are set), has a BoringSSL or
    /// aws-lc built in, and not all of its tables are known. The file is read once, no process memory.
    fn should_scan(&mut self, id: FileId, path: &str) -> bool {
        named(&self.names, path)
            && *self.files.entry(id).or_insert_with(|| {
                if aead::has_library(path) {
                    FileScan::Scan
                } else {
                    FileScan::Skip
                }
            }) == FileScan::Scan
    }

    /// The constant tables of a file are known from its first scan: only the tables this process fills
    /// are left to find.
    fn skip_known_tables(&self, bpf: &mut Ebpf, p: &mut AeadPending) {
        if p.scans == 0 && self.found.iter().any(|(file, _)| *file == p.id) {
            track_receives(bpf, p.pid);
            p.scans = 1;
        }
    }

    /// Scan each process that is due and probe the tables found.
    fn scan_pending(&mut self, bpf: &mut Ebpf) {
        for mut p in std::mem::take(&mut self.pending) {
            if self.files.get(&p.id) != Some(&FileScan::Scan) {
                continue;
            }
            self.skip_known_tables(bpf, &mut p);
            if p.wait > 0 || (p.scans == 0 && p.queued.elapsed() < AEAD_SETTLE) {
                p.wait = p.wait.saturating_sub(1);
                if std::path::Path::new(&format!("/proc/{}", p.pid)).exists() {
                    self.pending.push(p);
                }
                continue;
            }
            // The first scan covers all data of the file, where a library keeps its constant tables;
            // later ones only what the process wrote.
            let started = Instant::now();
            let Ok(tables) = aead::scan(p.pid, p.id.ino(), p.scans > 0) else {
                continue;
            };
            debug!(
                "AEAD scan {} of PID {} ({}) took {:?}, {} tables",
                p.scans,
                p.pid,
                p.path,
                started.elapsed(),
                tables.len()
            );
            p.scans += 1;
            if p.scans == 1 {
                if tables.is_empty() {
                    self.files.insert(p.id, FileScan::Skip);
                    continue;
                }
                track_receives(bpf, p.pid);
            }
            for &table in &tables {
                if !self.found.insert((p.id, table)) {
                    continue;
                }
                match self.attach(bpf, &p, &table) {
                    Ok(0) => {}
                    Ok(_) => audit::attach(
                        "exe_aead",
                        &p.path,
                        p.id.ino(),
                        &format!(
                            "built-in {}, seal {:#x}, open {:#x}, PID {}",
                            table.library.name(),
                            table.seal,
                            table.open,
                            p.pid
                        ),
                    ),
                    Err(e) => audit::attach_failed(&p.path, &e),
                }
            }
            if tables.iter().any(|t| t.filled_at_run_time) {
                self.files.insert(p.id, FileScan::Done);
                continue;
            }
            if p.scans < AEAD_SCANS && self.socket_probes != Some(false) {
                p.wait = (1 << p.scans.saturating_sub(AEAD_DENSE_SCANS)) - 1;
                self.pending.push(p);
            }
        }
    }
}

impl AeadScans {
    /// Attach the programs to the table's functions not probed yet; returns how many were attached.
    fn attach(&mut self, bpf: &mut Ebpf, p: &AeadPending, table: &aead::Table) -> Result<usize> {
        if self.socket_probes.is_none() {
            let attached = AEAD_SOCKET_PROBES
                .iter()
                .try_for_each(|(prog, func)| attach_kprobe(bpf, prog, func));
            self.socket_probes = Some(attached.is_ok());
            attached.context("AEAD probes need tcp_sendmsg and tcp_recvmsg")?;
        }
        if self.socket_probes != Some(true) {
            return Ok(0);
        }
        let [init, cleanup, seal, open_enter, open_exit] = AEAD_PROGRAMS;
        // Seal and open come first: they work without the other two.
        let sites = [
            (seal, "AEAD seal", table.seal),
            (open_enter, "AEAD open", table.open),
            (open_exit, "AEAD open", table.open),
            (init, "AEAD init", table.init[0]),
            (init, "AEAD init", table.init[1]),
            (cleanup, "AEAD cleanup", table.cleanup),
        ];
        let mut attached = 0;
        for (prog, func, offset) in sites {
            if offset != 0 && self.attached.insert((p.id, prog, offset)) {
                attach_uprobe(bpf, prog, func, &p.path, offset)?;
                attached += 1;
            }
        }
        Ok(attached)
    }
}

impl DynamicProbe for LlmProbe {
    fn poll(&self, bpf: &mut Ebpf) {
        if let Some(scans) = &self.aead {
            scans
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .scan_pending(bpf);
        }
    }

    fn on_exec(&self, bpf: &mut Ebpf, process_info: &ProcessInfo) -> Result<()> {
        let ssl_libs = process_info.libs.iter().filter(|path| {
            // Match against basename with a strict prefix check
            std::path::Path::new(path)
                .file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.starts_with("libssl.so"))
        });
        for (path, id, result) in self.attach_new(bpf, ssl_libs) {
            match result {
                Ok(true) => audit::attach(
                    "libssl",
                    path,
                    id.ino(),
                    &format!("loaded later, PID {}", process_info.pid),
                ),
                Ok(false) => audit::attach_failed(path, &"SSL_read/SSL_write not exported"),
                Err(e) => audit::attach_failed(path, &e),
            }
        }

        let exe = process_info.exe.iter().filter(|_| self.executables);
        for (path, id, result) in self.attach_new_exes(bpf, exe) {
            log_executable(path, id, result);
        }
        if let Some(path) = process_info.exe.as_ref().filter(|_| self.executables) {
            self.queue_aead_scan(bpf, process_info.pid, path, AEAD_PENDING);
        }
        Ok(())
    }
}

/// (pid, SSL*): one stream per TLS connection, whichever threads read and write it.
type StreamKey = (u32, u64);
type StreamMap = Arc<Mutex<HashMap<StreamKey, StreamProcessor>>>;
/// Locked after the StreamMap whenever both are held.
type BackgroundMap = Arc<Mutex<BackgroundCalls>>;

/// Feed one ringbuf event to its connection's stream, or end the stream on SSL_free.
fn route_event(
    streams: &mut HashMap<StreamKey, StreamProcessor>,
    background: &mut BackgroundCalls,
    header: &LlmEventHeader,
    data: &[u8],
    transport: Transport,
    // Called only for events that use it: a pod lookup miss reads /proc.
    #[cfg(feature = "k8s")] pod_info: impl FnOnce() -> Option<Arc<crate::k8s::PodInfo>>,
) -> Outcome {
    let key = (header.metadata.pid, header.ssl_ptr);
    if header.closed != 0 {
        return match streams.remove(&key) {
            Some(mut stream) if stream.is_llm() => stream.close(&CallContext {
                pid: header.metadata.pid,
                #[cfg(feature = "k8s")]
                pod_info: pod_info(),
                by_shape: false,
            }),
            _ => Outcome::Pending,
        };
    }
    if data.is_empty() {
        return Outcome::Pending;
    }
    let stream = streams.entry(key).or_default();
    stream.set_transport(transport);
    let ctx = CallContext {
        pid: header.metadata.pid,
        #[cfg(feature = "k8s")]
        pod_info: pod_info(),
        by_shape: false,
    };
    stream.handle_fragment(
        LlmDirection::from(header.rw),
        data,
        header.offset,
        header.len,
        header.truncated != 0,
        background,
        &ctx,
    )
}

/// Parse plaintext (non-TLS) capture records with the same stream machinery as the SSL path. The
/// records already belong to watched providers, so no connection filter runs; a `closed` record
/// (sent on TCP close) ends the stream, and idle streams are swept like the SSL ones.
#[cfg(feature = "k8s")]
pub(crate) fn spawn_plaintext_parser(bpf: &mut Ebpf, resolver: IdentityResolver) -> Result<()> {
    let state: StreamMap = Arc::new(Mutex::new(HashMap::new()));
    let background: BackgroundMap = Arc::default();
    let handler_state = state.clone();
    let handler_background = background.clone();
    spawn_ringbuf_handler_var(
        bpf,
        "PLAIN_EVENTS",
        move |header: &LlmEventHeader, data: &[u8]| {
            debug!(
                target: "honeybeepf_llm::plaintext",
                "plain record: pid={} stream={:#x} rw={} len={} closed={}",
                header.metadata.pid, header.ssl_ptr, header.rw, data.len(), header.closed
            );
            let mut map = handler_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut calls = handler_background
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            route_event(&mut map, &mut calls, header, data, Transport::Plain, || {
                resolver.resolve_pod(header.metadata.pid, header.metadata.cgroup_id)
            });
        },
    )?;
    start_cleanup_task(state, background);
    Ok(())
}

impl Probe for LlmProbe {
    fn attach(&self, bpf: &mut Ebpf, _resolver: IdentityResolver) -> Result<()> {
        http::report_provider_config();
        let targets = find_all_targets()?;

        // Keep going: libssl opened later is attached by re-discovery and needs the reader below.
        if targets.is_empty() {
            warn!("No libssl loaded yet; waiting for processes that load it.");
        }

        let stats = PerCpuArray::try_from(
            bpf.take_map("LLM_STATS")
                .context("Failed to get LLM_STATS map")?,
        )?;
        let _ = EVENT_STATS.set(stats);

        // Without it, every thread killed inside an SSL call keeps its session map entries until restart.
        if let Err(e) = attach_tracepoint(bpf, TASK_EXIT_PROBE) {
            warn!("{e:#}; session entries of killed threads are not cleared");
        }
        for prog in GO_TLS_PROGRAMS {
            if let Err(e) = load_uprobe(bpf, prog) {
                warn!("Go TLS probe {prog} failed to load: {e:#}");
            }
        }
        if let Some(scans) = &self.aead {
            let names = &scans
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .names;
            warn!(
                "AEAD probes for built-in BoringSSL and aws-lc are on (experimental); executables: {}",
                if names.is_empty() {
                    "any with the library".to_owned()
                } else {
                    names.join(", ")
                }
            );
            for prog in AEAD_PROGRAMS {
                if let Err(e) = load_uprobe(bpf, prog) {
                    warn!("AEAD probe {prog} failed to load: {e:#}");
                }
            }
        }

        // Skip libcrypto for SSL_* probes as they usually don't contain them
        let ssl_libs = targets.iter().filter(|path| !path.contains("libcrypto"));
        for (path, id, result) in self.attach_new(bpf, ssl_libs) {
            match result {
                Ok(true) => audit::attach("libssl", path, id.ino(), ""),
                Ok(false) => audit::attach_failed(path, &"SSL_read/SSL_write not exported"),
                Err(e) => audit::attach_failed(path, &e),
            }
        }

        if self.executables {
            let processes = find_process_executables();
            let exes: HashSet<&String> = processes.iter().map(|(_, path)| path).collect();
            for (path, id, result) in self.attach_new_exes(bpf, exes) {
                log_executable(path, id, result);
            }
            // Every running process is queued: the first scan of a file drops the others of it.
            for (pid, path) in &processes {
                if *pid != std::process::id() {
                    self.queue_aead_scan(bpf, *pid, path, usize::MAX);
                }
            }
        }

        #[cfg(feature = "k8s")]
        coverage::spawn(
            self.attached.clone(),
            _resolver.clone(),
            crate::telemetry::tls_coverage(),
        );
        let state: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let background: BackgroundMap = Arc::default();
        let handler_state = state.clone();
        let handler_background = background.clone();
        let mut conn_filter = ConnFilter::new(bpf)?;

        spawn_ringbuf_handler_var(
            bpf,
            "SSL_EVENTS",
            move |header: &LlmEventHeader, data: &[u8]| {
                if header.is_handshake == 1
                    || conn_filter.skip_if_foreign(header, data, &handler_state)
                {
                    return;
                }

                let mut map = handler_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut calls = handler_background
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                route_event(
                    &mut map,
                    &mut calls,
                    header,
                    data,
                    Transport::Tls,
                    #[cfg(feature = "k8s")]
                    || _resolver.resolve_pod(header.metadata.pid, header.metadata.cgroup_id),
                );
            },
        )?;

        start_stats_log_task(state.clone());
        start_cleanup_task(state, background);

        Ok(())
    }
}

fn start_cleanup_task(state: StreamMap, background: BackgroundMap) {
    let shutdown = crate::probes::shutdown_flag();
    std::thread::spawn(move || {
        while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS));
            let now = std::time::Instant::now();
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|_, v| {
                    now.duration_since(v.last_activity()).as_secs() < CONNECTION_RETENTION_SECS
                });
            // Counted after both locks are released: the ringbuf handler waits on them.
            let expired: Vec<_> = background
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .expire(now)
                .collect();
            for call in expired {
                processor::give_up(call);
            }
        }
    });
}

/// Log the kernel event counters and the open stream count every STATS_LOG_INTERVAL_SECS when they changed.
fn start_stats_log_task(state: StreamMap) {
    let shutdown = crate::probes::shutdown_flag();
    std::thread::spawn(move || {
        let (mut last, mut last_streams) = (None, 0);
        while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(STATS_LOG_INTERVAL_SECS));
            let current = event_stats();
            if current != last {
                log_event_stats();
                last = current;
            }
            let streams = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len();
            if streams != last_streams {
                info!("[LLM streams] {streams} open");
                last_streams = streams;
            }
        }
    });
}

/// Find all SSL libraries across running processes and system defaults.
pub fn find_all_targets() -> Result<HashSet<String>> {
    let ssl_pattern = Regex::new(r"libssl\.so\..*|libcrypto\.so\..*")?;
    find_libraries_all(&ssl_pattern, Some("libssl.so"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use conn_filter::opens_foreign_request;
    use honeybeepf_llm_common::{EventMetadata, LlmEvent, MAX_SSL_BUF_SIZE};
    use processor::UnparsedReason;

    const OPENAI_REQUEST: &[u8] = b"POST /v1/chat/completions HTTP/1.1\r\nHost: api.openai.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"gpt-4o\",\"stream\":true,\"messages\":[]}";
    const ANTHROPIC_REQUEST: &[u8] = b"POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\nContent-Type: application/json\r\n\r\n{\"model\":\"claude-test\",\"stream\":true,\"messages\":[]}";
    const SSE_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
    const OPENAI_DELTA: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
    const OPENAI_END: &[u8] = b"data: {\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\ndata: [DONE]\n\n";
    const ANTHROPIC_START: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":125,\"output_tokens\":1}}}\n\n";
    const ANTHROPIC_END: &[u8] = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":15}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    const OPENAI: u64 = 0x5555_0000_1000;
    const ANTHROPIC: u64 = 0x5555_0000_2000;

    /// One SSL call of `tid` on connection `ssl`, sent as a single event.
    fn call(tid: u32, ssl: u64, rw: LlmDirection, data: &[u8]) -> LlmEvent {
        let mut buf = [0u8; MAX_SSL_BUF_SIZE];
        buf[..data.len()].copy_from_slice(data);
        LlmEvent {
            header: LlmEventHeader {
                metadata: EventMetadata {
                    pid: 7,
                    _pad: tid,
                    ..Default::default()
                },
                rw: rw as u8,
                len: data.len() as u32,
                buf_filled: 1,
                ssl_ptr: ssl,
                ..Default::default()
            },
            buf,
        }
    }

    fn free(tid: u32, ssl: u64) -> LlmEvent {
        let mut e = call(tid, ssl, LlmDirection::Unknown, b"");
        e.header.closed = 1;
        e
    }

    fn route_all(events: &[LlmEvent]) -> Vec<Outcome> {
        let mut streams = HashMap::new();
        let mut background = BackgroundCalls::default();
        events
            .iter()
            .map(|e| {
                route_event(
                    &mut streams,
                    &mut background,
                    &e.header,
                    e.payload(),
                    Transport::Tls,
                    #[cfg(feature = "k8s")]
                    || None,
                )
            })
            .collect()
    }

    fn recorded(out: &[Outcome]) -> usize {
        out.iter().filter(|o| **o == Outcome::Recorded).count()
    }

    /// Node's event loop or asyncio: two streaming calls on one thread, their reads interleaved.
    #[test]
    fn test_aead_names_match_the_executable_with_or_without_its_extension() {
        let names = vec!["claude".to_string(), "codex".to_string()];
        let path = "/proc/7/root/usr/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe";
        assert!(named(&names, path));
        assert!(named(&names, "/usr/local/bin/codex"));
        assert!(!named(&names, "/usr/sbin/sshd"));
        assert!(!named(&names, "/opt/claude-tools/bin/helper"));
        assert!(named(&[], "/usr/sbin/sshd"));
    }

    #[test]
    fn test_aead_scan_skips_an_executable_without_the_library_and_reads_it_once() {
        let mut scans = AeadScans::default();
        // Not this test binary: it holds the library names as constants.
        let path = "/bin/sh";
        let id = file_identity(path).expect("file identity");
        assert!(!scans.should_scan(id, path));
        assert_eq!(scans.files.get(&id), Some(&FileScan::Skip));
        scans.files.insert(id, FileScan::Scan);
        assert!(
            scans.should_scan(id, path),
            "the cached state is used, not the file"
        );
    }

    #[test]
    fn test_two_connections_on_one_thread_are_both_recorded() {
        use LlmDirection::{Read, Write};
        let out = route_all(&[
            call(1, OPENAI, Write, OPENAI_REQUEST),
            call(1, ANTHROPIC, Write, ANTHROPIC_REQUEST),
            call(1, OPENAI, Read, SSE_HEAD),
            call(1, ANTHROPIC, Read, SSE_HEAD),
            call(1, ANTHROPIC, Read, ANTHROPIC_START),
            call(1, OPENAI, Read, OPENAI_DELTA),
            call(1, ANTHROPIC, Read, ANTHROPIC_END),
            call(1, OPENAI, Read, OPENAI_END),
        ]);
        assert_eq!(recorded(&out), 2, "{out:?}");
    }

    /// One connection, request written by one thread and response read by another.
    #[test]
    fn test_connection_shared_by_two_threads_is_recorded() {
        use LlmDirection::{Read, Write};
        let out = route_all(&[
            call(1, OPENAI, Write, OPENAI_REQUEST),
            call(2, OPENAI, Read, SSE_HEAD),
            call(2, OPENAI, Read, OPENAI_DELTA),
            call(2, OPENAI, Read, OPENAI_END),
        ]);
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    /// SSL_free ends an aborted stream (unparsed), so a reused SSL* starts a new one.
    #[test]
    fn test_freed_connection_does_not_leak_into_reused_address() {
        use LlmDirection::{Read, Write};
        let out = route_all(&[
            call(1, OPENAI, Write, OPENAI_REQUEST),
            call(1, OPENAI, Read, SSE_HEAD),
            call(1, OPENAI, Read, OPENAI_DELTA),
            free(1, OPENAI),
            call(1, OPENAI, Write, ANTHROPIC_REQUEST),
            call(1, OPENAI, Read, SSE_HEAD),
            call(1, OPENAI, Read, ANTHROPIC_START),
            call(1, OPENAI, Read, ANTHROPIC_END),
        ]);
        assert_eq!(
            out[3],
            Outcome::Unparsed(UnparsedReason::ConnectionClosed),
            "{out:?}"
        );
        assert_eq!(out.last(), Some(&Outcome::Recorded), "{out:?}");
    }

    #[test]
    fn test_free_removes_the_stream() {
        let mut streams = HashMap::new();
        for e in [
            call(
                1,
                OPENAI,
                LlmDirection::Write,
                b"GET /healthz HTTP/1.1\r\n\r\n",
            ),
            free(1, OPENAI),
        ] {
            route_event(
                &mut streams,
                &mut BackgroundCalls::default(),
                &e.header,
                e.payload(),
                Transport::Tls,
                #[cfg(feature = "k8s")]
                || None,
            );
        }
        assert!(streams.is_empty());
    }

    #[test]
    fn test_only_a_request_head_to_a_non_provider_host_skips_the_connection() {
        use LlmDirection::{Read, Write};
        let noise =
            b"POST /api/data HTTP/1.1\r\nHost: noise.internal\r\nContent-Length: 2\r\n\r\nxx";
        let opener = call(1, OPENAI, Write, noise);
        assert!(opens_foreign_request(&opener.header, opener.payload()));
        let mut continued = call(1, OPENAI, Write, noise);
        continued.header.offset = MAX_SSL_BUF_SIZE as u32;
        for e in [
            continued,
            call(1, OPENAI, Write, OPENAI_REQUEST),
            call(
                1,
                OPENAI,
                Write,
                b"GET /v1/models HTTP/1.1\r\nHost: api.openai.com\r\n\r\n",
            ),
            call(
                1,
                OPENAI,
                Write,
                b"POST /api/data HTTP/1.1\r\nHost: noise.int",
            ),
            call(1, OPENAI, Write, b"POST /api/data HTTP/1.1\r\n\r\n"),
            call(1, OPENAI, Write, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"),
            call(
                1,
                OPENAI,
                Read,
                b"HTTP/1.1 200 OK\r\nHost: noise.internal\r\n\r\n",
            ),
        ] {
            assert!(
                !opens_foreign_request(&e.header, e.payload()),
                "{:?}",
                e.payload()
            );
        }
    }

    /// Two paths to one file (like two processes' `/proc/<pid>/root/...`) attach once.
    #[test]
    fn test_unique_new_files_dedupes_same_inode() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("hb-llm-dedupe-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let lib = dir.join("libssl.so.3");
        let link = dir.join("libssl.so.3.hardlink");
        let other = dir.join("libssl.so.1.1");
        std::fs::write(&lib, b"lib")?;
        let _ = std::fs::remove_file(&link);
        std::fs::hard_link(&lib, &link)?;
        std::fs::write(&other, b"other")?;

        let paths: Vec<String> = [&lib, &link, &other]
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let new = unique_new_files(&paths, &HashSet::new());
        assert_eq!(new.len(), 2, "same inode attached twice: {new:?}");
        assert_eq!(new[0].0, paths[0]);
        assert_eq!(new[1].0, paths[2]);

        // Re-discovery for a new process skips files already attached.
        let known: HashSet<FileId> = new.iter().map(|(_, id)| *id).collect();
        assert!(unique_new_files(&paths, &known).is_empty());

        std::fs::remove_dir_all(&dir)
    }

    /// A byte-identical copy is a separate inode with its own uprobes, so it must not merge.
    #[test]
    fn test_unique_new_files_keeps_identical_copy() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("hb-llm-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let lib = dir.join("libssl.so.3");
        let copy = dir.join("libssl.so.3.copy");
        std::fs::write(&lib, b"lib")?;
        std::fs::copy(&lib, &copy)?;

        let paths: Vec<String> = [&lib, &copy]
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        assert_eq!(unique_new_files(&paths, &HashSet::new()).len(), 2);

        std::fs::remove_dir_all(&dir)
    }

    /// When the chosen path's process exits mid-attach, another path to the same file is tried.
    #[test]
    fn test_attach_new_files_retries_vanished_path() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("hb-llm-vanish-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let first = dir.join("a-node");
        let second = dir.join("b-node");
        std::fs::write(&first, b"elf")?;
        let _ = std::fs::remove_file(&second);
        std::fs::hard_link(&first, &second)?;
        let paths: Vec<String> = [&first, &second]
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let known = Mutex::new(HashSet::new());
        let mut tried = Vec::new();
        let out = attach_new_files(&known, &paths, |path| {
            tried.push(path.to_string());
            if path == paths[0] {
                std::fs::remove_file(path)?;
                anyhow::bail!("No such file or directory");
            }
            Ok(true)
        });

        assert_eq!(tried, paths);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, paths[1]);
        assert!(matches!(out[0].2, Ok(true)));
        assert_eq!(known.lock().map_or(0, |k| k.len()), 1);

        // A failure on a path that still exists is recorded, so the file is not retried.
        let known = Mutex::new(HashSet::new());
        let out = attach_new_files(&known, &paths[1..], |_| anyhow::bail!("verifier"));
        assert_eq!(out.len(), 1);
        assert_eq!(known.lock().map_or(0, |k| k.len()), 1);

        std::fs::remove_dir_all(&dir)
    }

    /// Each uprobe program in the eBPF object is tabled, and its function is in PROBED_FUNCTIONS.
    #[test]
    fn test_probe_tables_cover_every_uprobe_program() -> Result<()> {
        use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};

        let mut tabled: HashMap<&str, &str> = HashMap::from([CLOSE_PROBE]);
        for &(enter, exit, func) in REQUIRED_PROBES.iter().chain(&OPTIONAL_PROBES) {
            tabled.insert(enter, func);
            tabled.insert(exit, func);
        }
        for func in tabled.values() {
            assert!(PROBED_FUNCTIONS.contains(func), "{func} is never resolved");
        }

        let data = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/honeybeepf-llm"));
        let obj = object::File::parse(data)?;
        let mut programs = HashSet::new();
        for sym in obj.symbols().filter(|s| s.kind() == SymbolKind::Text) {
            let Some(index) = sym.section_index() else {
                continue;
            };
            let section = obj.section_by_index(index)?.name()?;
            if section.starts_with("uprobe") || section.starts_with("uretprobe") {
                programs.insert(sym.name()?);
            }
        }
        let tabled: HashSet<&str> = tabled
            .into_keys()
            .chain(GO_TLS_PROGRAMS)
            .chain(AEAD_PROGRAMS)
            .collect();
        assert_eq!(programs, tabled, "uprobe programs vs probe tables");
        Ok(())
    }

    /// Each uprobe program tail-calls into NO_PRIV_STACK, so a 6.13+ verifier gives it no per-CPU private stack.
    #[test]
    fn test_every_uprobe_program_has_a_tail_call() -> Result<()> {
        use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget, SymbolKind};
        const BPF_CALL: u8 = 0x85;
        const BPF_FUNC_TAIL_CALL: i32 = 12;

        let data = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/honeybeepf-llm"));
        let obj = object::File::parse(data)?;
        let (mut checked, mut without) = (0, Vec::new());
        for sym in obj.symbols().filter(|s| s.kind() == SymbolKind::Text) {
            let Some(index) = sym.section_index() else {
                continue;
            };
            let section = obj.section_by_index(index)?;
            let name = section.name()?;
            if !name.starts_with("uprobe") && !name.starts_with("uretprobe") {
                continue;
            }
            let start = usize::try_from(sym.address())?;
            let end = start + usize::try_from(sym.size())?;
            let code = section.data()?.get(start..end).unwrap_or_default();
            // A helper call has src_reg 0; a bpf-to-bpf call has src_reg 1.
            let (insns, _) = code.as_chunks::<8>();
            let tail_call = insns.iter().any(|insn| {
                insn[0] == BPF_CALL
                    && insn[1] >> 4 == 0
                    && i32::from_le_bytes([insn[4], insn[5], insn[6], insn[7]])
                        == BPF_FUNC_TAIL_CALL
            });
            let uses_map = section.relocations().any(|(at, rel)| {
                (start as u64..end as u64).contains(&at)
                    && matches!(rel.target(), RelocationTarget::Symbol(i)
                        if obj.symbol_by_index(i).and_then(|s| s.name()).is_ok_and(|n| n == "NO_PRIV_STACK"))
            });
            checked += 1;
            if !tail_call || !uses_map {
                without.push(sym.name()?);
            }
        }
        assert!(checked > 0, "no uprobe program found in the eBPF object");
        // The loader sets this global by name and does not fail when it is missing.
        assert!(
            obj.symbols().any(|s| s.name() == Ok("TAIL_CALL_OK")),
            "TAIL_CALL_OK not in the eBPF object"
        );
        assert!(
            without.is_empty(),
            "uprobe programs without a tail call into NO_PRIV_STACK: {without:?}"
        );
        Ok(())
    }

    /// The task exit program is in the eBPF object as a tracepoint program.
    #[test]
    fn test_task_exit_probe_is_a_tracepoint_program() -> Result<()> {
        use object::{Object, ObjectSection, ObjectSymbol};

        let data = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/honeybeepf-llm"));
        let obj = object::File::parse(data)?;
        let sym = obj
            .symbols()
            .find(|s| s.name() == Ok(TASK_EXIT_PROBE.program_name))
            .context("task exit program not in the eBPF object")?;
        let index = sym.section_index().context("symbol without a section")?;
        let section = obj.section_by_index(index)?.name()?;
        assert!(section.starts_with("tracepoint"), "section {section}");
        Ok(())
    }

    #[test]
    fn test_unique_new_files_skips_missing_path() {
        let paths = vec!["/nonexistent/libssl.so.3".to_string()];
        assert!(unique_new_files(&paths, &HashSet::new()).is_empty());
    }
}
