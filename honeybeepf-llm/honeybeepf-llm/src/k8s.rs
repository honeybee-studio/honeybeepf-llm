//! Kubernetes pod identity resolution.
//!
//! Maps PIDs from eBPF events to Kubernetes pod metadata by:
//! 1. Parsing `/proc/{pid}/cgroup` to extract container IDs
//! 2. Watching pods on the current node via the K8s API
//! 3. Caching the mapping: cgroup_id → container_id → PodInfo

use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::{Api, Client, runtime::watcher};
use log::{debug, warn};

/// Workload label value for a pod whose owner cannot be named.
pub const UNATTRIBUTED: &str = "unattributed";

/// Kubernetes pod identity resolved from a PID/cgroup_id.
#[derive(Debug, Clone)]
pub struct PodInfo {
    pub pod_name: String,
    pub namespace: String,
    /// Owner kind: "Deployment", "CronJob", "StatefulSet", "Pod" (no owner), etc.; None if unresolvable.
    pub workload_kind: Option<String>,
    /// Owner name, e.g. "my-app" (Deployment name).
    pub workload_name: Option<String>,
}

impl PodInfo {
    /// Returns the best "service" name: workload_name if available, otherwise pod_name.
    pub fn service_name(&self) -> &str {
        self.workload_name.as_deref().unwrap_or(&self.pod_name)
    }
}

/// Thread-safe resolver: PID/cgroup_id → PodInfo.
///
/// Designed to be wrapped in `Arc` and shared across probe handler closures
/// (which run in `spawn_blocking` threads).
pub struct PodResolver {
    /// Whether the host kernel uses cgroup v2 (unified hierarchy).
    ///
    /// On cgroup v2, `bpf_get_current_cgroup_id()` returns a unique ID per
    /// container, so we can cache `cgroup_id → container_id` for O(1) lookups.
    /// On cgroup v1, the helper always returns 1 (root), so caching would
    /// incorrectly map all PIDs to the first container seen.
    cgroup_v2: bool,
    /// cgroup_id → container_id (parsed from /proc/{pid}/cgroup, cached).
    /// Only populated when `cgroup_v2 = true`.
    cgroup_cache: RwLock<HashMap<u64, Option<String>>>,
    /// container_id (short 12-char prefix) → PodInfo.
    /// Populated and updated by the K8s watcher task.
    pod_store: RwLock<HashMap<String, Arc<PodInfo>>>,
}

impl Default for PodResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl PodResolver {
    pub fn new() -> Self {
        Self {
            cgroup_v2: detect_cgroup_v2(),
            cgroup_cache: RwLock::new(HashMap::new()),
            pod_store: RwLock::new(HashMap::new()),
        }
    }

    /// Resolve a PID + cgroup_id to pod metadata.
    ///
    /// Returns cached result in O(1) for known cgroup_ids.
    /// On cache miss, reads `/proc/{pid}/cgroup` to extract the container ID,
    /// then looks it up in the pod store.
    pub fn resolve(&self, pid: u32, cgroup_id: u64) -> Option<Arc<PodInfo>> {
        // Fast path: use the cgroup_id → container_id cache.
        // This is only valid on cgroup v2, where bpf_get_current_cgroup_id()
        // returns a unique ID per container. On cgroup v1 it always returns 1
        // (the root), which would poison the cache for all PIDs.
        if self.cgroup_v2
            && let Ok(cache) = self.cgroup_cache.read()
            && let Some(cached) = cache.get(&cgroup_id)
        {
            return match cached {
                Some(container_id) => self.pod_store.read().ok()?.get(container_id).cloned(),
                None => None, // Known non-container PID
            };
        }

        // Slow path: parse /proc/{pid}/cgroup.
        // Always correct on both cgroup v1 and v2.
        let container_id = extract_container_id(pid);

        // Cache the result only on cgroup v2 where IDs are meaningful.
        // Only cache if we actually found a container_id. If the process already
        // exited (/proc/{pid} gone), we'd cache None and overwrite a valid
        // pre-warmed entry.
        if self.cgroup_v2
            && container_id.is_some()
            && let Ok(mut cache) = self.cgroup_cache.write()
        {
            cache.insert(cgroup_id, container_id.clone());
        }

        // Look up in pod_store
        container_id.and_then(|cid| self.pod_store.read().ok()?.get(&cid).cloned())
    }

    /// Start the K8s pod watcher background task.
    ///
    /// Watches pods on the specified node and keeps the pod_store updated.
    pub async fn start_k8s_watcher(
        self: &Arc<Self>,
        node_name: String,
    ) -> Result<tokio::task::JoinHandle<()>> {
        let client = Client::try_default()
            .await
            .context("Failed to create K8s client (not running in cluster?)")?;

        let jobs_client = client.clone();
        // Pods are namespaced resources, but we want to watch across all namespaces on this node.
        // Therefore, we use Api::all() to get cluster-wide visibility.
        let api: Api<Pod> = Api::all(client);
        let watcher_config = watcher::Config {
            field_selector: Some(format!("spec.nodeName={}", node_name)),
            ..Default::default()
        };

        let resolver = Arc::clone(self);

        let handle = tokio::spawn(async move {
            let stream = watcher(api, watcher_config);
            tokio::pin!(stream);

            while let Some(event) = stream.next().await {
                match event {
                    Ok(watcher::Event::Apply(pod) | watcher::Event::InitApply(pod)) => {
                        let cronjob = resolver.lookup_cronjob(&jobs_client, &pod).await;
                        resolver.apply_pod_event(&pod, false, cronjob);
                    }
                    Ok(watcher::Event::Delete(pod)) => {
                        resolver.apply_pod_event(&pod, true, None);
                    }
                    Ok(watcher::Event::Init | watcher::Event::InitDone) => {}
                    Err(e) => {
                        warn!("K8s pod watcher error: {}. Will retry.", e);
                        // kube's watcher automatically retries with backoff
                    }
                }
            }

            warn!("K8s pod watcher stream ended");
        });

        // Start cache cleanup task
        self.start_cache_cleanup_task();

        Ok(handle)
    }

    async fn lookup_cronjob(&self, client: &Client, pod: &Pod) -> Option<String> {
        let (namespace, job) = self.unresolved_job(pod)?;
        cronjob_of(client, namespace, job).await
    }

    /// Namespace and Job name of a Job pod not yet stored with its CronJob.
    fn unresolved_job<'a>(&self, pod: &'a Pod) -> Option<(&'a str, &'a str)> {
        let owner = controller_owner(&pod.metadata)
            .filter(|o| o.kind == "Job" && o.api_version.starts_with("batch/"))?;
        // Pending pods have no container IDs and so no store entry to fill.
        if extract_container_ids_from_pod(pod).is_empty() {
            return None;
        }
        // Stored as a Job: no CronJob owner, or the last lookup failed; ask again.
        let stored = self.stored_info(&pod.metadata);
        if stored.is_some_and(|i| i.workload_kind.as_deref() != Some("Job")) {
            return None;
        }
        Some((pod.metadata.namespace.as_deref()?, owner.name.as_str()))
    }

    /// Stored info of the same pod, also under an earlier container ID after a restart.
    fn stored_info(&self, metadata: &ObjectMeta) -> Option<Arc<PodInfo>> {
        let (name, namespace) = (metadata.name.as_deref()?, metadata.namespace.as_deref()?);
        let store = self.pod_store.read().ok()?;
        store
            .values()
            .find(|i| i.pod_name == name && i.namespace == namespace)
            .cloned()
    }

    /// Process a pod event from the K8s API watcher.
    fn apply_pod_event(&self, pod: &Pod, is_delete: bool, cronjob: Option<String>) {
        let metadata = &pod.metadata;
        let pod_name = match &metadata.name {
            Some(n) => n.clone(),
            None => return,
        };
        let namespace = metadata.namespace.clone().unwrap_or_default();

        // Extract container IDs from pod status
        let container_ids = extract_container_ids_from_pod(pod);

        if is_delete {
            if let Ok(mut store) = self.pod_store.write() {
                for cid in &container_ids {
                    store.remove(cid);
                }
            }
            // Clean cgroup_cache entries that pointed to deleted containers
            if let Ok(mut cache) = self.cgroup_cache.write() {
                cache.retain(|_, v| {
                    v.as_ref()
                        .map(|id| !container_ids.contains(id))
                        .unwrap_or(true)
                });
            }
        } else {
            // Pre-warm cgroup_id cache: resolve cgroup_id from cgroup path on disk
            // so that short-lived processes (e.g. cat, curl) can be mapped to pods
            // even after they exit.
            if self.cgroup_v2 {
                for cid in &container_ids {
                    if let Some(cgroup_id) = lookup_cgroup_id_by_container(cid)
                        && let Ok(mut cache) = self.cgroup_cache.write()
                    {
                        cache.insert(cgroup_id, Some(cid.clone()));
                    }
                }
            }
            // A pod's owner does not change: keep the stored workload unless a lookup just found the CronJob.
            let (workload_kind, workload_name) = match (cronjob, self.stored_info(metadata)) {
                (None, Some(prev)) => (prev.workload_kind.clone(), prev.workload_name.clone()),
                (cronjob, _) => workload_of(metadata, cronjob).unzip(),
            };

            let info = Arc::new(PodInfo {
                pod_name,
                namespace,
                workload_kind,
                workload_name,
            });

            if let Ok(mut store) = self.pod_store.write() {
                for cid in container_ids {
                    store.insert(cid, Arc::clone(&info));
                }
            }
        }
    }

    /// Periodically evict stale cgroup_cache entries whose container_id
    /// no longer exists in pod_store.
    fn start_cache_cleanup_task(self: &Arc<Self>) {
        let resolver = Arc::clone(self);
        let shutdown = crate::probes::shutdown_flag();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                resolver.evict_stale_cgroups();
            }
        });
    }

    /// Drop cgroup_cache entries whose container_id is missing from pod_store.
    fn evict_stale_cgroups(&self) {
        // Same order as resolve(): cgroup_cache before pod_store, or a queued watcher write deadlocks both.
        if let (Ok(mut cgroup_cache), Ok(pod_store)) =
            (self.cgroup_cache.write(), self.pod_store.read())
        {
            cgroup_cache.retain(|_, v| {
                v.as_ref()
                    .map(|cid| pod_store.contains_key(cid))
                    .unwrap_or(true)
            });
        }
    }
}

/// Detects whether the host kernel uses cgroup v2 (unified hierarchy).
///
/// Returns `true` if `/sys/fs/cgroup/cgroup.controllers` exists, which is
/// only present on cgroup v2. On cgroup v1, that file does not exist and
/// the filesystem is split across per-subsystem mount points.
///
/// This detection matters for `bpf_get_current_cgroup_id()`: on v2, it
/// returns a unique ID per container; on v1, it always returns 1 (root).
fn detect_cgroup_v2() -> bool {
    std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
}

fn controller_owner(metadata: &ObjectMeta) -> Option<&OwnerReference> {
    let refs = metadata.owner_references.as_ref()?;
    refs.iter()
        .find(|r| r.controller == Some(true))
        .or_else(|| refs.first())
}

/// Owning workload (kind, name) of a pod; `cronjob` is the owner of its Job, if looked up.
fn workload_of(metadata: &ObjectMeta, cronjob: Option<String>) -> Option<(String, String)> {
    let Some(owner) = controller_owner(metadata) else {
        return Some(("Pod".to_string(), metadata.name.clone()?));
    };
    if owner.kind.is_empty() || owner.name.is_empty() {
        return None;
    }
    let workload = match owner.kind.as_str() {
        // A Deployment names its ReplicaSets "<deployment>-<pod-template-hash>".
        "ReplicaSet" => match metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("pod-template-hash"))
            .and_then(|hash| owner.name.strip_suffix(hash.as_str())?.strip_suffix('-'))
            .filter(|d| !d.is_empty())
        {
            Some(deployment) => ("Deployment".to_string(), deployment.to_string()),
            None => (owner.kind.clone(), owner.name.clone()),
        },
        "Job" => match cronjob {
            Some(cronjob) => ("CronJob".to_string(), cronjob),
            None => (owner.kind.clone(), owner.name.clone()),
        },
        _ => (owner.kind.clone(), owner.name.clone()),
    };
    Some(workload)
}

/// Name of the CronJob that owns `job`; pods only reference their Job, so this needs an API get.
async fn cronjob_of(client: &Client, namespace: &str, job: &str) -> Option<String> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let api: Api<Job> = Api::namespaced(client.clone(), namespace);
    // Bounded: the pod watcher waits on this before it handles the next pod event.
    let error = match tokio::time::timeout(Duration::from_secs(5), api.get_opt(job)).await {
        Ok(Ok(found)) => {
            return controller_owner(&found?.metadata)
                .filter(|o| o.kind == "CronJob" && !o.name.is_empty())
                .map(|o| o.name.clone());
        }
        Ok(Err(e)) => e.to_string(),
        Err(_) => "timed out".to_string(),
    };
    if WARNED.swap(true, Ordering::Relaxed) {
        debug!("Cannot get Job {namespace}/{job}: {error}");
    } else {
        warn!(
            "Cannot get Job {namespace}/{job}: {error}; CronJob pods keep their Job label (chart grants jobs get)"
        );
    }
    None
}

/// Extract container IDs from a pod's status.
///
/// Returns short (12-char) container ID strings.
fn extract_container_ids_from_pod(pod: &Pod) -> Vec<String> {
    let mut ids = Vec::new();

    if let Some(status) = &pod.status {
        for containers in [&status.container_statuses, &status.init_container_statuses]
            .into_iter()
            .flatten()
        {
            for cs in containers {
                if let Some(cid) = &cs.container_id {
                    // Format: "containerd://<64hex>" or "docker://<64hex>"
                    if let Some(hex_id) = cid.rsplit("://").next()
                        && hex_id.len() >= 12
                    {
                        ids.push(hex_id[..12].to_string());
                    }
                }
            }
        }
    }

    ids
}

/// Parse `/proc/{pid}/cgroup` to extract the container ID.
///
/// Supports both cgroup v1 and v2 formats:
/// - v2: `0::/kubepods/besteffort/pod<uid>/<64-hex-container-id>`
/// - v1: `12:memory:/kubepods/besteffort/pod<uid>/<64-hex-container-id>`
/// - systemd scope: `cri-containerd-<64hex>.scope` or `docker-<64hex>.scope`
fn extract_container_id(pid: u32) -> Option<String> {
    let path = format!("/proc/{}/cgroup", pid);
    let content = std::fs::read_to_string(&path).ok()?;

    for line in content.lines() {
        if let Some(id) = parse_container_id_from_cgroup_line(line) {
            return Some(id);
        }
    }
    None
}

/// Extract a container ID from a single cgroup line.
///
/// Returns the first 12 characters of a 64-char hex container ID.
fn parse_container_id_from_cgroup_line(line: &str) -> Option<String> {
    // Get the path portion after "hierarchy-ID:controller-list:"
    let path = line.splitn(3, ':').nth(2)?;

    // Skip lines that don't look like container cgroups
    if !path.contains("kubepods") && !path.contains("docker") && !path.contains("containerd") {
        return None;
    }

    // Get the last path segment
    let last_segment = path.rsplit('/').next()?;

    // Check for scope-style (systemd/cri): "cri-containerd-<64hex>.scope" or "docker-<64hex>.scope"
    if last_segment.ends_with(".scope") {
        let inner = last_segment.trim_end_matches(".scope");
        // Extract the hex ID after the last '-'
        if let Some(hex_id) = inner.rsplit('-').next()
            && is_container_id(hex_id)
        {
            return Some(hex_id[..12].to_string());
        }
    }

    // Check for plain container ID as last path segment
    if is_container_id(last_segment) {
        return Some(last_segment[..12].to_string());
    }

    None
}

/// Check if a string looks like a 64-char hex container ID.
fn is_container_id(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Look up the cgroup_id (inode number) for a container by scanning cgroup paths.
///
/// On cgroup v2, each container has a directory under /sys/fs/cgroup whose inode
/// number matches `bpf_get_current_cgroup_id()`. We find it by searching for the
/// container ID in the cgroup filesystem.
fn lookup_cgroup_id_by_container(short_container_id: &str) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    fn walk_dir(dir: &std::path::Path, needle: &str) -> Option<u64> {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.contains(needle)
                && let Ok(meta) = std::fs::metadata(&path)
            {
                return Some(meta.ino());
            }
            if let Some(id) = walk_dir(&path, needle) {
                return Some(id);
            }
        }
        None
    }

    // In Kubernetes, host root is mounted at /host. Try that first,
    // falling back to /sys/fs/cgroup for non-containerized environments.
    let host_path = std::path::Path::new("/host/sys/fs/cgroup");
    let path = if host_path.exists() {
        host_path
    } else {
        std::path::Path::new("/sys/fs/cgroup")
    };
    walk_dir(path, short_container_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cgroup_v2_plain() {
        let line = "0::/kubepods/besteffort/podabc123/a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let id = parse_container_id_from_cgroup_line(line);
        assert_eq!(id, Some("a1b2c3d4e5f6".to_string()));
    }

    #[test]
    fn test_parse_cgroup_v1() {
        let line = "12:memory:/kubepods/besteffort/podxyz/a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let id = parse_container_id_from_cgroup_line(line);
        assert_eq!(id, Some("a1b2c3d4e5f6".to_string()));
    }

    #[test]
    fn test_parse_cgroup_systemd_scope_containerd() {
        let line = "0::/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod123.slice/cri-containerd-a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2.scope";
        let id = parse_container_id_from_cgroup_line(line);
        assert_eq!(id, Some("a1b2c3d4e5f6".to_string()));
    }

    #[test]
    fn test_parse_cgroup_systemd_scope_docker() {
        let line = "0::/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod456.slice/docker-a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2.scope";
        let id = parse_container_id_from_cgroup_line(line);
        assert_eq!(id, Some("a1b2c3d4e5f6".to_string()));
    }

    #[test]
    fn test_parse_non_container_cgroup() {
        let line = "0::/user.slice/user-1000.slice/session-1.scope";
        let id = parse_container_id_from_cgroup_line(line);
        assert_eq!(id, None);
    }

    #[test]
    fn test_parse_empty_cgroup() {
        let line = "0::/";
        let id = parse_container_id_from_cgroup_line(line);
        assert_eq!(id, None);
    }

    #[test]
    fn test_resolver_returns_none_without_k8s() {
        let resolver = PodResolver::new();
        // pid 1 (init) won't have a container cgroup
        assert!(resolver.resolve(1, 12345).is_none());
    }

    #[test]
    fn test_resolver_does_not_cache_exited_pid() {
        let resolver = PodResolver {
            cgroup_v2: true,
            ..PodResolver::new()
        };
        // u32::MAX is above pid_max, so /proc/<pid>/cgroup never exists.
        assert!(resolver.resolve(u32::MAX, 99999).is_none());
        assert!(!resolver.cgroup_cache.read().unwrap().contains_key(&99999));
    }

    /// resolve() holds cgroup_cache(R) while it reads pod_store; if cleanup held pod_store(R)
    /// while waiting for cgroup_cache(W), a queued watcher write would block resolve() for good.
    #[test]
    fn test_cache_cleanup_does_not_hold_pod_store_while_waiting() {
        let resolver = Arc::new(PodResolver {
            cgroup_v2: true,
            ..PodResolver::new()
        });
        let resolve_guard = resolver.cgroup_cache.read().expect("cgroup_cache lock");

        let cleanup = {
            let resolver = Arc::clone(&resolver);
            std::thread::spawn(move || resolver.evict_stale_cgroups())
        };
        // Give cleanup time to park on the cgroup_cache write lock.
        std::thread::sleep(Duration::from_millis(200));

        let watcher_can_write = resolver.pod_store.try_write().is_ok();
        drop(resolve_guard);
        cleanup.join().expect("cleanup thread");
        assert!(
            watcher_can_write,
            "cleanup holds pod_store while waiting for cgroup_cache"
        );
    }

    fn owner(kind: &str, name: &str, controller: Option<bool>) -> OwnerReference {
        OwnerReference {
            kind: kind.to_string(),
            name: name.to_string(),
            controller,
            ..Default::default()
        }
    }

    fn pod_meta(owners: Vec<OwnerReference>, hash: Option<&str>) -> ObjectMeta {
        ObjectMeta {
            name: Some("p-1".to_string()),
            namespace: Some("team-a".to_string()),
            owner_references: (!owners.is_empty()).then_some(owners),
            labels: hash.map(|h| [("pod-template-hash".to_string(), h.to_string())].into()),
            ..Default::default()
        }
    }

    fn workload(kind: &str, name: &str) -> Option<(String, String)> {
        Some((kind.to_string(), name.to_string()))
    }

    #[test]
    fn test_workload_owner_chains() {
        let meta = |kind: &str, name: &str, hash: Option<&str>| {
            pod_meta(vec![owner(kind, name, Some(true))], hash)
        };
        let rs = |name: &str, hash| meta("ReplicaSet", name, hash);
        // The controller reference wins over an earlier non-controller owner.
        let two = vec![
            owner("Secret", "s", None),
            owner("StatefulSet", "db", Some(true)),
        ];
        let cases = [
            (rs("api-h1", Some("h1")), None, Some("Deployment/api")),
            (rs("app-v2-h1", Some("h1")), None, Some("Deployment/app-v2")),
            // A ReplicaSet not made by a Deployment: no hash label, or a name that does not end in it.
            (rs("web-abc", None), None, Some("ReplicaSet/web-abc")),
            (rs("web-abc", Some("xyz")), None, Some("ReplicaSet/web-abc")),
            (rs("h1", Some("h1")), None, Some("ReplicaSet/h1")),
            (meta("Job", "r-1", None), Some("r"), Some("CronJob/r")),
            (meta("Job", "r-1", None), None, Some("Job/r-1")),
            (meta("DaemonSet", "ds", None), None, Some("DaemonSet/ds")),
            (meta("Job", "", None), None, None),
            (pod_meta(vec![], None), None, Some("Pod/p-1")),
            (pod_meta(two, None), None, Some("StatefulSet/db")),
        ];
        for (meta, cronjob, want) in cases {
            let got = workload_of(&meta, cronjob.map(str::to_string));
            let got = got.map(|(kind, name)| format!("{kind}/{name}"));
            assert_eq!(got.as_deref(), want, "{:?}", meta.owner_references);
        }
    }

    #[test]
    fn test_job_pod_cronjob_retried_until_found_then_kept() {
        use k8s_openapi::api::core::v1::{ContainerStatus, PodStatus};
        let resolver = PodResolver {
            cgroup_v2: false,
            ..PodResolver::new()
        };
        let mut job = owner("Job", "report-29345678", Some(true));
        job.api_version = "batch/v1".to_string();
        let mut pod = Pod {
            metadata: pod_meta(vec![job], None),
            ..Default::default()
        };
        let with_container = |pod: &mut Pod, hex: &str| {
            pod.status = Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    container_id: Some(format!("containerd://{}", hex.repeat(32))),
                    ..Default::default()
                }]),
                ..Default::default()
            })
        };
        let stored = |r: &PodResolver| {
            r.stored_info(&pod_meta(vec![], None))
                .and_then(|i| i.workload_kind.clone().zip(i.workload_name.clone()))
        };
        // Pending: no container yet, so nothing to look up.
        assert_eq!(resolver.unresolved_job(&pod), None);

        with_container(&mut pod, "ab");
        let pending = Some(("team-a", "report-29345678"));
        assert_eq!(resolver.unresolved_job(&pod), pending);
        // A failed lookup labels the Job for now and is retried on the next event.
        resolver.apply_pod_event(&pod, false, None);
        assert_eq!(stored(&resolver), workload("Job", "report-29345678"));
        assert_eq!(resolver.unresolved_job(&pod), pending);
        resolver.apply_pod_event(&pod, false, Some("report".to_string()));
        assert_eq!(resolver.unresolved_job(&pod), None);

        // Later events, also after a container restart, keep the CronJob without a lookup.
        with_container(&mut pod, "cd");
        assert_eq!(resolver.unresolved_job(&pod), None);
        resolver.apply_pod_event(&pod, false, None);
        assert_eq!(stored(&resolver), workload("CronJob", "report"));

        let mut other = pod;
        other.metadata.name = Some("p-2".to_string());
        other.metadata.owner_references = Some(vec![owner("Job", "vj", Some(true))]);
        assert_eq!(resolver.unresolved_job(&other), None, "not a batch/v1 Job");
    }

    #[test]
    fn test_pod_info_service_name() {
        let info = PodInfo {
            pod_name: "my-app-7d4b8c9f5-abc12".to_string(),
            namespace: "default".to_string(),
            workload_kind: Some("Deployment".to_string()),
            workload_name: Some("my-app".to_string()),
        };
        assert_eq!(info.service_name(), "my-app");
    }

    #[test]
    fn test_pod_info_service_name_fallback() {
        let info = PodInfo {
            pod_name: "standalone-pod".to_string(),
            namespace: "default".to_string(),
            workload_kind: None,
            workload_name: None,
        };
        assert_eq!(info.service_name(), "standalone-pod");
    }
}
