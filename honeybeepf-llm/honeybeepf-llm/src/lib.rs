pub mod audit;
pub mod health;
pub mod logging;
pub mod settings;
pub mod telemetry;

#[cfg(feature = "k8s")]
pub mod k8s;

// Plaintext (non-TLS) capture endpoint resolution (userspace).
#[cfg(feature = "k8s")]
pub mod plaintext;

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};

use anyhow::Result;
use aya::{Ebpf, EbpfLoader, util::KernelVersion};
use aya_log::EbpfLogger;
use log::{debug, info, warn};
use tokio::{signal, time::Instant};

use crate::settings::Settings;

pub mod probes;
use crate::probes::{
    DYNAMIC_POLL_MS, DynamicProbe, IdentityResolver, LibOpen, LibOpenSeen, ProcessInfo,
    builtin::llm::{
        ExecNotify, ExecPidQueue, InScope, LibOpenQueue, LlmProbe, setup_exec_watch,
        setup_lib_open_watch,
    },
    discovery::resolve_in_root,
    request_shutdown, shutdown_flag,
};

/// Inode of the root PID namespace (PROC_PID_INIT_INO); any other means the agent runs in a nested one.
const INIT_PIDNS_INO: u64 = 0xEFFF_FFFC;
/// In a nested PID namespace, the fewest seconds between two /proc rescans an exec or libssl open asks for.
const RESCAN_DEBOUNCE_SECS: u64 = 2;
/// In a nested PID namespace, a /proc rescan every this many discovery ticks (5 s), for libssl loaded after exec.
const RESCAN_EVERY_TICKS: u32 = 2;

pub struct HoneyBeeEngine {
    pub settings: Settings,
    bpf: Ebpf,
    dynamic_probes: Vec<Box<dyn DynamicProbe>>,
    resolver: IdentityResolver,
    /// The agent runs in a nested PID namespace (node in a container: kind, k3d, minikube). The kernel reports
    /// root-namespace PIDs, which this /proc does not show, so discovery rescans /proc instead of using them.
    nested_pidns: bool,
    /// The containers of a namespace scope, shared by the pod watcher and the AEAD scan.
    #[cfg(feature = "k8s")]
    scope_containers: Option<std::sync::Arc<k8s::ScopeContainers>>,
}

impl HoneyBeeEngine {
    pub fn new(settings: Settings, bytecode: &[u8]) -> Result<Self> {
        bump_memlock_rlimit()?;
        let tail_call_ok = u8::from(KernelVersion::current().is_ok_and(tail_call_allowed));
        info!(
            "LLM probes keep the task stack (empty tail call): {}",
            tail_call_ok == 1
        );
        // Not required to exist: test objects without the LLM programs still load.
        let mut loader = EbpfLoader::new();
        loader.set_global("TAIL_CALL_OK", &tail_call_ok, false);
        let nested_pidns = nested_pidns().is_some();
        if nested_pidns {
            info!(
                "Agent runs in a nested PID namespace (node in a container): new processes are found by rescanning /proc"
            );
        }
        // The plaintext hash/sock maps prealloc ~8 MB at full size (PLAIN_CONNS ~6.5 MB); keep them
        // at one slot unless capture is configured, which only the k8s build resolves endpoints for.
        #[cfg(feature = "k8s")]
        let plaintext_on = !plaintext::configured_specs().is_empty();
        #[cfg(not(feature = "k8s"))]
        let plaintext_on = false;
        let plain_maps = honeybeepf_llm_common::plain_map_entries(plaintext_on);
        loader.set_max_entries("PLAIN_ENDPOINTS", plain_maps.endpoints);
        loader.set_max_entries("PLAIN_SOCKS", plain_maps.socks);
        loader.set_max_entries("PLAIN_CONNS", plain_maps.conns);
        // Same per-connection cardinality as PLAIN_CONNS: the connect4 → sockops cookie handoff.
        loader.set_max_entries("PLAIN_CONNECT_META", plain_maps.conns);
        // When on, give PLAIN_EVENTS its full ring and raise the verifier log level so a
        // socket-program rejection prints its full cause.
        #[cfg(feature = "k8s")]
        if plaintext_on {
            loader.set_max_entries("PLAIN_EVENTS", plaintext::PLAIN_EVENTS_FULL_BYTES);
            loader
                .verifier_log_level(aya::VerifierLogLevel::VERBOSE | aya::VerifierLogLevel::STATS);
        }
        // Only the pods of some namespaces: their cgroups fill SCOPE_CGROUPS, and nothing else is copied.
        let scope_on = u8::from(scope_namespaces(&settings).is_some());
        loader.set_global("SCOPE_ON", &scope_on, false);
        if scope_on == 0 {
            loader.set_max_entries("SCOPE_CGROUPS", 1);
        }
        let mut bpf = loader.load(bytecode)?;

        // Initialize the eBPF Logger only in debug mode
        if settings.debug.unwrap_or(false) {
            if let Err(e) = EbpfLogger::init(&mut bpf) {
                warn!("Failed to initialize eBPF logger: {e}");
            } else {
                debug!("eBPF logger enabled (debug mode)");
            }
        } else {
            debug!("eBPF logger disabled (production mode)");
        }
        let mut dynamic_probes: Vec<Box<dyn DynamicProbe>> = Vec::new();
        #[cfg(feature = "k8s")]
        let scope_containers = settings
            .builtin_probes
            .llm_namespaces
            .is_some()
            .then(|| std::sync::Arc::new(k8s::ScopeContainers::default()));
        #[cfg(feature = "k8s")]
        let in_scope = scope_containers.clone().map(|containers| -> InScope {
            std::sync::Arc::new(move |pid| containers.contains_pid(pid))
        });
        #[cfg(not(feature = "k8s"))]
        let in_scope: Option<InScope> = None;
        if settings.builtin_probes.llm.unwrap_or(false) {
            let executables = settings.builtin_probes.llm_executables.unwrap_or(true);
            let detect_by_shape = settings.builtin_probes.llm_detect_by_shape.unwrap_or(false);
            let aead = settings.builtin_probes.llm_aead.unwrap_or(false).then(|| {
                settings
                    .builtin_probes
                    .llm_aead_executables
                    .clone()
                    .unwrap_or_default()
            });
            if let Some(on) = experimental_on(detect_by_shape, aead.is_some()) {
                info!("{on}");
            }
            dynamic_probes.push(Box::new(LlmProbe::new(
                executables,
                detect_by_shape,
                aead,
                in_scope,
            )));
        }

        let resolver = IdentityResolver::none();

        Ok(Self {
            settings,
            bpf,
            dynamic_probes,
            resolver,
            nested_pidns,
            #[cfg(feature = "k8s")]
            scope_containers,
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let _shutdown = ShutdownOnDrop;
        if let Err(e) = telemetry::init_metrics() {
            warn!(
                "Failed to initialize OpenTelemetry metrics: {e:#}. Metrics will not be exported."
            );
        }

        // Start K8s pod watcher if available
        #[cfg(feature = "k8s")]
        {
            use std::sync::Arc;

            // In restrictive enterprise environments, the Downward API might be blocked or missing.
            // Fall back to HOSTNAME or reading the system hostname directly (since agent runs with hostNetwork: true).
            let resolved_node_name = std::env::var("K8S_NODE_NAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
                .or_else(|_| {
                    std::fs::read_to_string("/proc/sys/kernel/hostname")
                        .map(|s| s.trim().to_string())
                });

            if let Ok(node_name) = resolved_node_name {
                info!("Starting K8s pod watcher for node: {node_name}");
                let pod_labels = self.settings.builtin_probes.llm_pod_labels.as_deref();
                let mut pod_resolver =
                    k8s::PodResolver::with_pod_labels(pod_labels.unwrap_or_default());
                if self.nested_pidns {
                    pod_resolver = pod_resolver.without_pid_lookup();
                }
                if let Some(namespaces) = scope_namespaces(&self.settings) {
                    let containers = self.scope_containers.clone().unwrap_or_default();
                    match scope_of(&mut self.bpf, namespaces, containers) {
                        Ok(scope) => pod_resolver = pod_resolver.with_scope(scope),
                        Err(e) => {
                            warn!("Namespace scope not set up: {e:#}; no TLS data is recorded")
                        }
                    }
                }
                let pod_resolver = Arc::new(pod_resolver);
                match pod_resolver.start_k8s_watcher(node_name).await {
                    Ok(_handle) => {
                        info!("K8s pod watcher started");
                        self.resolver = IdentityResolver::with_pod_resolver(pod_resolver);
                    }
                    Err(e) => {
                        warn!("K8s pod watcher failed to start: {e}. Pod resolution disabled.");
                        if scope_namespaces(&self.settings).is_some() {
                            warn!("Namespace scope without pods: no TLS data is recorded");
                        }
                    }
                }
            } else {
                info!(
                    "K8S_NODE_NAME and hostname not found. Pod resolution disabled (running outside K8s?)."
                );
                if scope_namespaces(&self.settings).is_some() {
                    warn!("Namespace scope without pods: no TLS data is recorded");
                }
            }
        }

        // Plaintext (non-TLS) capture: attach socket-level BPF only when endpoints are configured
        // and the node has a cgroup v2 hierarchy on 5.10+; otherwise it stays off and logs why.
        #[cfg(feature = "k8s")]
        {
            let specs = plaintext::configured_specs();
            if !specs.is_empty() {
                let (resolver, endpoints) = (self.resolver.clone(), specs.len());
                match plaintext::start(&mut self.bpf, specs, resolver).await {
                    Ok(cgroup) => {
                        audit::plaintext_capture_on(&cgroup.display().to_string(), endpoints)
                    }
                    Err(e) => audit::plaintext_capture_off(&format!("{e:#}")),
                }
            }
        }

        self.attach_probes()?;
        health::spawn_writer();

        // Start dynamic probe discovery if any dynamic probes are enabled
        if self.dynamic_probes.is_empty() {
            info!("Monitoring active. Press Ctrl-C to exit.");
            signal::ctrl_c().await?;
        } else {
            let exec_watch = setup_exec_watch(&mut self.bpf)?;
            let lib_watch = setup_lib_open_watch(&mut self.bpf)?;
            self.run_discovery(exec_watch, lib_watch).await?;
        }

        info!("Exiting...");
        Ok(())
    }

    /// Run the dynamic discovery loop that monitors for new processes and notifies probes.
    async fn run_discovery(
        &mut self,
        (queue, notify): (ExecPidQueue, ExecNotify),
        (lib_queue, lib_notify, mut lib_seen): (LibOpenQueue, ExecNotify, LibOpenSeen),
    ) -> Result<()> {
        const BATCH_WAIT_MS: u64 = 50;

        let shutdown = shutdown_flag();
        // A deadline branch rather than a sleep inside a branch, so opens are handled while it waits.
        let mut exec_due: Option<Instant> = None;

        info!("Dynamic discovery active. Press Ctrl-C to exit.");

        let beat = health::register("discovery loop");
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let mut poll = tokio::time::interval(Duration::from_millis(DYNAMIC_POLL_MS));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Nested PID namespace only: event PIDs are not this /proc's, so events just ask for a rescan.
        let mut ticks = 0u32;
        let mut rescan = RescanGate::default();
        loop {
            tokio::select! {
                _ = signal::ctrl_c() => break,
                _ = tick.tick() => {
                    beat.beat();
                    ticks = ticks.wrapping_add(1);
                    if self.nested_pidns && ticks.is_multiple_of(RESCAN_EVERY_TICKS) {
                        rescan.request();
                    }
                },
                _ = poll.tick() => {
                    for probe in &self.dynamic_probes {
                        probe.poll(&mut self.bpf);
                    }
                },
                () = lib_notify.notified(), if self.nested_pidns => {
                    lib_queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
                    rescan.request();
                }
                () = lib_notify.notified(), if !self.nested_pidns => {
                    let opens: Vec<LibOpen> = {
                        let mut q = lib_queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        q.drain(..).collect()
                    };
                    for open in opens {
                        debug!("libssl open: {} (PID: {})", open.path, open.pid);
                        let root = PathBuf::from(format!("/proc/{}/root", open.pid));
                        // Process gone: not marked seen, so the kernel reports the next open again.
                        let Some(host_path) = resolve_in_root(&root, Path::new(&open.path)) else {
                            continue;
                        };
                        let libs = HashSet::from([host_path.to_string_lossy().into_owned()]);
                        self.notify_probes(&ProcessInfo { pid: open.pid, libs, exe: None });
                        let _ = lib_seen.insert(open.seen_key, open.ts, 0);
                    }
                }
                () = notify.notified(), if exec_due.is_none() => {
                    // Brief delay to batch rapid exec events
                    exec_due = Some(Instant::now() + Duration::from_millis(BATCH_WAIT_MS));
                }
                () = tokio::time::sleep_until(exec_due.unwrap_or_else(Instant::now)), if exec_due.is_some() => {
                    exec_due = None;
                    let pids: Vec<u32> = {
                        let mut q = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        q.drain(..).collect()
                    };
                    if self.nested_pidns && !pids.is_empty() {
                        rescan.request();
                    }

                    if !self.nested_pidns && !pids.is_empty() {
                        for pid in pids {
                            // Pre-fetch libraries once per process to avoid redundant I/O
                            let libs = match crate::probes::discovery::get_process_libraries(pid) {
                                Ok(l) => l,
                                Err(e) => {
                                    // Debug level because this can happen for short-lived processes
                                    log::debug!("Failed to get libraries for PID {pid}: {e}");
                                    continue;
                                }
                            };

                            // Check if useful
                            if libs.is_empty() {
                                continue;
                            }

                            let exe = crate::probes::discovery::get_process_exe(pid);
                            self.notify_probes(&ProcessInfo { pid, libs, exe });
                        }
                    }
                }
            }

            if rescan.take(std::time::Instant::now()) {
                self.rescan_processes();
            }

            if shutdown.load(Ordering::Relaxed) {
                break;
            }
        }

        crate::probes::builtin::llm::log_event_stats();
        telemetry::shutdown_metrics();

        Ok(())
    }

    /// Offer every process in /proc to the probes as its exec would; files already tried are skipped there.
    fn rescan_processes(&mut self) {
        let Ok(procs) = procfs::process::all_processes() else {
            return;
        };
        for process in procs.flatten() {
            let pid = process.pid as u32;
            let Ok(libs) = crate::probes::discovery::get_process_libraries(pid) else {
                continue;
            };
            if libs.is_empty() {
                continue;
            }
            let exe = crate::probes::discovery::get_process_exe(pid);
            self.notify_probes(&ProcessInfo { pid, libs, exe });
        }
    }

    fn notify_probes(&mut self, process_info: &ProcessInfo) {
        for probe in &self.dynamic_probes {
            if let Err(e) = probe.on_exec(&mut self.bpf, process_info) {
                warn!("Probe on_exec error: {e}");
            }
        }
    }

    fn attach_probes(&mut self) -> Result<()> {
        for probe in &self.dynamic_probes {
            probe.attach(&mut self.bpf, self.resolver.clone())?;
            telemetry::record_active_probe("llm", 1);
        }
        Ok(())
    }
}

/// Stops the ringbuf readers on every return from `run`: the runtime's drop waits for blocking tasks forever.
struct ShutdownOnDrop;

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        request_shutdown();
    }
}

/// A /proc rescan asked for by events, run at most once per `RESCAN_DEBOUNCE_SECS`.
#[derive(Default)]
struct RescanGate {
    due: bool,
    last: Option<std::time::Instant>,
}

impl RescanGate {
    fn request(&mut self) {
        self.due = true;
    }

    /// Whether to rescan now; a request within the debounce waits for a later call.
    fn take(&mut self, now: std::time::Instant) -> bool {
        let ready = self
            .last
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(RESCAN_DEBOUNCE_SECS));
        if self.due && ready {
            (self.due, self.last) = (false, Some(now));
            return true;
        }
        false
    }
}

/// The agent's PID namespace (st_dev, st_ino) when it is not the root one: on a node in a container (kind, k3d,
/// minikube), the kernel's PIDs differ from the ones this /proc shows.
/// The namespaces recording is limited to; never outside a Kubernetes build, which knows no pods.
#[cfg(not(feature = "k8s"))]
fn scope_namespaces(settings: &Settings) -> Option<Vec<String>> {
    if settings.builtin_probes.llm_namespaces.is_some() {
        warn!("A namespace scope needs the Kubernetes build: every process is recorded");
    }
    None
}

/// The namespaces recording is limited to. Without cgroup v2 or the pod watcher nothing is in scope, so
/// nothing is recorded rather than everything.
#[cfg(feature = "k8s")]
fn scope_namespaces(settings: &Settings) -> Option<Vec<String>> {
    let namespaces = settings.builtin_probes.llm_namespaces.clone()?;
    if !k8s::detect_cgroup_v2() {
        warn!("A namespace scope needs cgroup v2 to tell pods apart: no TLS data is recorded");
    }
    Some(namespaces)
}

/// The scope that adds the cgroups of `namespaces`' containers to `SCOPE_CGROUPS` as their pods show.
#[cfg(feature = "k8s")]
fn scope_of(
    bpf: &mut Ebpf,
    namespaces: Vec<String>,
    containers: std::sync::Arc<k8s::ScopeContainers>,
) -> Result<k8s::Scope> {
    use anyhow::Context as _;

    let map = bpf
        .take_map("SCOPE_CGROUPS")
        .context("SCOPE_CGROUPS map not found")?;
    let cgroups = std::sync::Mutex::new(aya::maps::HashMap::<_, u64, u8>::try_from(map)?);
    info!(
        "Recording only the TLS calls of pods in: {}",
        namespaces.join(", ")
    );
    Ok(k8s::Scope::new(
        namespaces,
        containers,
        move |cgroup_id, add| {
            let mut cgroups = cgroups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let result = if add {
                cgroups.insert(cgroup_id, 1, 0)
            } else {
                cgroups.remove(&cgroup_id)
            };
            if let Err(e) = result {
                debug!("Scope cgroup {cgroup_id} (add: {add}) not updated: {e}");
            }
        },
    ))
}

fn nested_pidns() -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let ns = std::fs::metadata("/proc/self/ns/pid").ok()?;
    nested(ns.dev(), ns.ino())
}

/// The namespace when it is not the root one, whose PIDs the kernel reports.
fn nested(dev: u64, ino: u64) -> Option<(u64, u64)> {
    (ino != INIT_PIDNS_INO).then_some((dev, ino))
}

fn bump_memlock_rlimit() -> Result<()> {
    let rlim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };
    if ret != 0 {
        warn!("Failed to increase rlimit");
    }
    Ok(())
}

/// Whether the verifier accepts a tail call in a program with bpf-to-bpf calls (x86-64 since 5.10, every JIT since 6.0).
fn tail_call_allowed(kernel: KernelVersion) -> bool {
    kernel >= KernelVersion::new(6, 0, 0)
        || cfg!(target_arch = "x86_64") && kernel >= KernelVersion::new(5, 10, 0)
}

/// The startup line naming the experimental features that are on, by their chart names; none when all are off.
fn experimental_on(detect_by_shape: bool, aead: bool) -> Option<String> {
    let on: Vec<&str> = [("detectByShape", detect_by_shape), ("aead", aead)]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect();
    (!on.is_empty()).then(|| format!("experimental features on: {}", on.join(", ")))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{probes::POLL_INTERVAL_MS, settings::BuiltinProbes};

    #[test]
    fn experimental_features_that_are_on_are_named_at_startup() {
        assert_eq!(experimental_on(false, false), None);
        assert_eq!(
            experimental_on(false, true).as_deref(),
            Some("experimental features on: aead")
        );
        assert_eq!(
            experimental_on(true, true).as_deref(),
            Some("experimental features on: detectByShape, aead")
        );
    }

    #[test]
    fn rescans_run_once_per_debounce_and_only_when_asked() {
        let mut gate = RescanGate::default();
        let t0 = std::time::Instant::now();
        assert!(!gate.take(t0), "nothing asked");
        gate.request();
        assert!(gate.take(t0));
        gate.request();
        gate.request();
        assert!(
            !gate.take(t0 + Duration::from_secs(1)),
            "within the debounce"
        );
        assert!(
            gate.take(t0 + Duration::from_secs(RESCAN_DEBOUNCE_SECS)),
            "kept until then"
        );
        assert!(!gate.take(t0 + Duration::from_secs(10)), "served");
    }

    #[test]
    fn only_a_pid_namespace_other_than_the_root_one_is_nested() {
        assert_eq!(nested(4, INIT_PIDNS_INO), None);
        // kind v0.29: the node container's namespace, as the agent pod with hostPID sees it.
        assert_eq!(nested(4, 4_026_532_239), Some((4, 4_026_532_239)));
    }

    #[test]
    fn test_tail_call_allowed_by_kernel() {
        let x86 = cfg!(target_arch = "x86_64");
        for (version, want) in [
            ((4, 18, 0), false),
            ((5, 4, 0), false),
            ((5, 10, 0), x86),
            ((5, 19, 17), x86),
            ((6, 0, 0), true),
            ((7, 0, 0), true),
        ] {
            let (major, minor, patch) = version;
            assert_eq!(
                tail_call_allowed(KernelVersion::new(major, minor, patch)),
                want,
                "{version:?}"
            );
        }
    }

    /// A 64-bit little-endian BPF ELF header with no sections, which aya loads without the kernel.
    fn empty_bpf_object() -> Vec<u8> {
        let mut elf = vec![0x7f, b'E', b'L', b'F', 2, 1, 1];
        elf.resize(16, 0);
        // e_type ET_REL, e_machine EM_BPF, e_version 1; entry, offsets and flags stay 0.
        elf.extend_from_slice(&[1, 0, 247, 0, 1, 0, 0, 0]);
        elf.resize(52, 0);
        // e_ehsize 64, no program headers, e_shentsize 64, no sections.
        elf.extend_from_slice(&[64, 0, 0, 0, 0, 0, 64, 0]);
        elf.resize(64, 0);
        elf
    }

    // Leaves the process-wide shutdown flag set; no other test in this binary reads it.
    #[test]
    fn test_startup_error_stops_blocking_readers() {
        let settings = Settings {
            builtin_probes: BuiltinProbes {
                // The object has no LLM maps, so attaching the LLM probe fails inside run().
                llm: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        let engine =
            HoneyBeeEngine::new(settings, &empty_bpf_object()).expect("empty object loads");
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

        let result = rt.block_on(async {
            let shutdown = shutdown_flag();
            // The loop shape of spawn_ringbuf_handler, which needs a kernel ringbuf map.
            tokio::task::spawn_blocking(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
                }
            });
            engine.run().await
        });
        let err = result.expect_err("attaching the LLM probe fails");
        assert!(format!("{err:#}").contains("LLM_STATS"), "{err:#}");

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop(rt);
            let _ = done_tx.send(());
        });
        // Generous: with a kubeconfig, the pod watcher's DNS lookup also runs on a blocking thread.
        assert!(
            done_rx.recv_timeout(Duration::from_secs(30)).is_ok(),
            "runtime drop is still waiting on the reader loop"
        );
    }
}
