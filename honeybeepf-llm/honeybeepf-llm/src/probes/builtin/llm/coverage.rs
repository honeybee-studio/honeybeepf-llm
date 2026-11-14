//! Which pods the TLS probes can see. A pod is `hooked` when one of its processes maps a library or runs an
//! executable the probes are attached to; otherwise its best process tells why not, so a dashboard can
//! tell "no LLM calls" from "calls the agent cannot read".

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use object::{Object, read::ReadCache};
use regex::Regex;

use crate::k8s::{PodInfo, UNATTRIBUTED};
use crate::probes::IdentityResolver;
use crate::probes::discovery::{FileId, file_identity, get_process_exe, resolve_host_path};

/// How often every process on the node is classified again.
const SCAN_INTERVAL_SECS: u64 = 60;

/// What the probes can do with one process's TLS, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Tls {
    /// Maps a libssl, or runs an executable, the probes are attached to.
    Hooked,
    /// Maps a libssl the probes are not attached to (attach failed, or not seen yet).
    OpensslNotHooked,
    /// A Go executable: crypto/tls is not hooked.
    Go,
    /// A JVM: JSSE is not hooked.
    Java,
    /// No OpenSSL, Go or JVM found (rustls, a static TLS library, or no TLS at all).
    Other,
}

impl Tls {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Tls::Hooked => "hooked",
            Tls::OpensslNotHooked => "openssl_not_hooked",
            Tls::Go => "go",
            Tls::Java => "java",
            Tls::Other => "other",
        }
    }
}

/// Pods per workload and TLS state: (namespace, workload kind, workload name, state) → pods.
pub(crate) type Coverage = HashMap<(String, String, String, Tls), u64>;

/// Classify every process now and then, keeping the result in `out` for the coverage gauge.
pub(super) fn spawn(
    attached: Arc<Mutex<HashSet<FileId>>>,
    resolver: IdentityResolver,
    out: &'static Mutex<Coverage>,
) {
    let shutdown = crate::probes::shutdown_flag();
    std::thread::spawn(move || {
        let mut go_exes = HashMap::new();
        while !shutdown.load(Ordering::Relaxed) {
            let attached = attached.lock().map(|a| a.clone()).unwrap_or_default();
            let now = scan(&attached, &resolver, &mut go_exes);
            if let Ok(mut out) = out.lock() {
                merge(&mut out, now);
            }
            std::thread::sleep(Duration::from_secs(SCAN_INTERVAL_SECS));
        }
    });
}

/// Replace the counts with `now`, keeping a key seen before at 0, so a gone workload reads 0, not its last count.
fn merge(out: &mut Coverage, now: Coverage) {
    for count in out.values_mut() {
        *count = 0;
    }
    out.extend(now);
}

/// Pods per workload and state for every pod process on the node right now.
fn scan(
    attached: &HashSet<FileId>,
    resolver: &IdentityResolver,
    go_exes: &mut HashMap<FileId, bool>,
) -> Coverage {
    let ssl = Regex::new(r"^libssl\.so").expect("static regex");
    let mut pods: HashMap<String, (Arc<PodInfo>, Tls)> = HashMap::new();
    let Ok(procs) = procfs::process::all_processes() else {
        return Coverage::new();
    };
    for process in procs.flatten() {
        let pid = process.pid as u32;
        let Some(pod) = resolver.pod_of_pid(pid) else {
            continue;
        };
        let key = format!("{}/{}", pod.namespace, pod.pod_name);
        if pods.get(&key).is_some_and(|(_, t)| *t == Tls::Hooked) {
            continue;
        }
        let tls = classify(&process, attached, &ssl, go_exes);
        pods.entry(key)
            .and_modify(|(_, best)| *best = (*best).min(tls))
            .or_insert((pod, tls));
    }
    let mut out = Coverage::new();
    for (pod, tls) in pods.into_values() {
        let or_unattributed = |v: &Option<String>| {
            v.as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(UNATTRIBUTED)
                .to_string()
        };
        let key = (
            pod.namespace.clone(),
            or_unattributed(&pod.workload_kind),
            or_unattributed(&pod.workload_name),
            tls,
        );
        *out.entry(key).or_default() += 1;
    }
    out
}

fn classify(
    process: &procfs::process::Process,
    attached: &HashSet<FileId>,
    ssl: &Regex,
    go_exes: &mut HashMap<FileId, bool>,
) -> Tls {
    // The host path through /proc/<pid>/root, as the probes attached it, so the identities compare.
    let exe_path = get_process_exe(process.pid as u32);
    let exe = exe_path.as_deref().and_then(file_identity);
    if exe.as_ref().is_some_and(|id| attached.contains(id)) {
        return Tls::Hooked;
    }
    let mut best = Tls::Other;
    for map in process.maps().into_iter().flatten() {
        let procfs::process::MMapPath::Path(path) = map.pathname else {
            continue;
        };
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if ssl.is_match(name) {
            let host = resolve_host_path(process.pid, &path);
            if file_identity(&host.to_string_lossy()).is_some_and(|id| attached.contains(&id)) {
                return Tls::Hooked;
            }
            best = best.min(Tls::OpensslNotHooked);
        } else if name.starts_with("libjvm.so") {
            best = best.min(Tls::Java);
        }
    }
    if best == Tls::Other
        && let Some(id) = exe
        && *go_exes
            .entry(id)
            .or_insert_with(|| exe_path.as_deref().is_some_and(is_go))
    {
        return Tls::Go;
    }
    best
}

/// Whether the executable at `path` is a Go binary (it has the `.go.buildinfo` section).
fn is_go(path: &str) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let cache = ReadCache::new(file);
    object::File::parse(&cache).is_ok_and(|f| f.section_by_name(".go.buildinfo").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, tls: Tls) -> (String, String, String, Tls) {
        ("ns".into(), "Deployment".into(), name.into(), tls)
    }

    #[test]
    fn a_pod_counts_under_its_best_process() {
        let mut order = [
            Tls::Other,
            Tls::Go,
            Tls::Hooked,
            Tls::Java,
            Tls::OpensslNotHooked,
        ];
        order.sort();
        assert_eq!(
            order,
            [
                Tls::Hooked,
                Tls::OpensslNotHooked,
                Tls::Go,
                Tls::Java,
                Tls::Other
            ]
        );
    }

    #[test]
    fn a_gone_workload_reads_zero() {
        let mut out = Coverage::from([(key("old", Tls::Go), 2), (key("api", Tls::Hooked), 1)]);
        merge(&mut out, Coverage::from([(key("api", Tls::Hooked), 3)]));
        assert_eq!(
            out,
            Coverage::from([(key("old", Tls::Go), 0), (key("api", Tls::Hooked), 3)])
        );
    }

    #[test]
    fn this_test_binary_is_not_go() {
        assert!(!is_go("/proc/self/exe"));
        assert!(!is_go("/nonexistent"));
    }
}
