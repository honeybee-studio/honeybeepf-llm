//! Kubernetes pod identity resolution.
//!
//! Maps PIDs from eBPF events to Kubernetes pod metadata by:
//! 1. Parsing `/proc/{pid}/cgroup` to extract container IDs
//! 2. Watching pods on the current node via the K8s API
//! 3. Caching the mapping: cgroup_id → container_id → PodInfo

use std::{
    collections::HashMap,
    sync::{Arc, RwLock, atomic::Ordering},
    time::Duration,
};

use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::{Api, Client, runtime::watcher};
use log::warn;

/// Kubernetes pod identity resolved from a PID/cgroup_id.
#[derive(Debug, Clone)]
pub struct PodInfo {
    pub pod_name: String,
    pub namespace: String,
    /// Owner kind: "Deployment", "StatefulSet", "DaemonSet", "Job", etc.
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
    /// cgroup_id → container_id (parsed from /proc/{pid}/cgroup, cached).
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
        // Fast path: check cgroup_cache
        if let Ok(cache) = self.cgroup_cache.read()
            && let Some(cached) = cache.get(&cgroup_id)
        {
            return match cached {
                Some(container_id) => self.pod_store.read().ok()?.get(container_id).cloned(),
                None => None, // Known non-container PID
            };
        }

        // Slow path: parse /proc/{pid}/cgroup
        let container_id = extract_container_id(pid);

        // Cache the result (even None, to avoid re-reading /proc)
        if let Ok(mut cache) = self.cgroup_cache.write() {
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
                        resolver.apply_pod_event(&pod, false);
                    }
                    Ok(watcher::Event::Delete(pod)) => {
                        resolver.apply_pod_event(&pod, true);
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

    /// Process a pod event from the K8s API watcher.
    fn apply_pod_event(&self, pod: &Pod, is_delete: bool) {
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
            // Extract workload info from ownerReferences
            let (workload_kind, workload_name) = metadata
                .owner_references
                .as_ref()
                .and_then(|refs| refs.first())
                .map(|owner| {
                    if owner.kind == "ReplicaSet" {
                        // ReplicaSet is owned by Deployment; strip the hash suffix.
                        // "my-app-7d4b8c9f5" → "my-app"
                        // Note: This strips one suffix segment, which works for standard Deployment-generated
                        // RS names, but could misfire for custom names like "my-app-v2-7d4b8c9f5" -> "my-app-v2".
                        // True resolution would require following the ownerReferences chain up to Deployment.
                        let name = owner
                            .name
                            .rsplit_once('-')
                            .map(|(prefix, _)| prefix.to_string())
                            .unwrap_or_else(|| owner.name.clone());
                        ("Deployment".to_string(), name)
                    } else {
                        (owner.kind.clone(), owner.name.clone())
                    }
                })
                .map(|(k, n)| (Some(k), Some(n)))
                .unwrap_or((None, None));

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

                if let (Ok(pod_store), Ok(mut cgroup_cache)) =
                    (resolver.pod_store.read(), resolver.cgroup_cache.write())
                {
                    cgroup_cache.retain(|_, v| {
                        v.as_ref()
                            .map(|cid| pod_store.contains_key(cid))
                            .unwrap_or(true)
                    });
                }
            }
        });
    }
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
    fn test_resolver_caches_negative_result() {
        let resolver = PodResolver::new();
        // First call caches
        let _ = resolver.resolve(1, 99999);
        // Verify cache was populated
        assert!(resolver.cgroup_cache.read().unwrap().contains_key(&99999));
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
