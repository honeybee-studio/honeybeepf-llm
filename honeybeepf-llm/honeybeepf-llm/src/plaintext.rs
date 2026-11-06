//! Resolve plaintext (non-TLS) capture endpoints to the IP:port set the kernel watches. A source
//! (`specs_from_registry` from config; a Service annotation could be another) yields `EndpointSpec`s;
//! the source-agnostic core maps each to its Service ClusterIP and ready pod IPs, or DNS if external.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};

use honeybeepf_llm_core::providers::ProviderRegistry;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use log::warn;

/// Upper bound for the fixed-size BPF map; beyond it extra endpoints are dropped with a warning.
pub const MAX_ENDPOINTS: usize = 8192;

/// One plaintext target from a source: a host and the plain-HTTP port it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointSpec {
    pub host: String,
    pub port: u16,
    pub namespace: Option<String>,
}

/// The resolved IP:port set handed to a sink; sorted so change detection is a plain comparison.
pub type EndpointSet = BTreeSet<SocketAddr>;

/// Where resolved endpoints go; the real implementation writes the BPF endpoint map.
pub trait EndpointSink: Send {
    fn replace(&mut self, endpoints: &EndpointSet);
}

/// Config source: every provider marked `plaintext` contributes one spec per host. Empty = off.
pub fn specs_from_registry(registry: &ProviderRegistry) -> Vec<EndpointSpec> {
    let mut specs = Vec::new();
    for provider in &registry.providers {
        let Some(plain) = &provider.plaintext else {
            continue;
        };
        for host in &provider.hosts {
            let host = host.trim();
            if !host.is_empty() {
                specs.push(EndpointSpec {
                    host: host.to_string(),
                    port: plain.port,
                    namespace: plain.namespace.clone(),
                });
            }
        }
    }
    specs
}

/// How a host is reached, as decided by [`classify`].
pub enum Target {
    Service { name: String, namespace: String },
    External(String),
}

/// A single-label host, one carrying `.svc`, or any host with an explicit namespace is an
/// in-cluster Service; anything else (a public FQDN) is resolved by DNS.
pub fn classify(spec: &EndpointSpec) -> Target {
    let labels: Vec<&str> = spec.host.split('.').collect();
    let in_cluster = spec.namespace.is_some() || labels.len() == 1 || spec.host.contains(".svc");
    if !in_cluster {
        return Target::External(spec.host.clone());
    }
    let name = labels[0].to_string();
    let namespace = spec
        .namespace
        .clone()
        .or_else(|| labels.get(1).map(|s| s.to_string()))
        .unwrap_or_else(|| "default".to_string());
    Target::Service { name, namespace }
}

/// ClusterIP:port for a Service, or `None` for a headless Service (no/"None" ClusterIP).
pub fn cluster_ip_endpoint(svc: &Service, port: u16) -> Option<SocketAddr> {
    let ip = svc.spec.as_ref()?.cluster_ip.as_deref()?;
    if ip.is_empty() || ip == "None" {
        return None;
    }
    Some(SocketAddr::new(ip.parse::<IpAddr>().ok()?, port))
}

/// Name of the Service port that `port` maps to, for matching the EndpointSlice's pod port.
pub fn service_port_name(svc: &Service, port: u16) -> Option<String> {
    let ports = svc.spec.as_ref()?.ports.as_ref()?;
    ports
        .iter()
        .find(|p| p.port == i32::from(port))
        .and_then(|p| p.name.clone())
}

/// Ready pod IP:port from one EndpointSlice; `want` is the Service port name (none → the sole port).
pub fn slice_endpoints(slice: &EndpointSlice, want: Option<&str>) -> Vec<SocketAddr> {
    let ports = slice.ports.as_deref().unwrap_or(&[]);
    let pod_port = match want {
        Some(name) => ports
            .iter()
            .find(|p| p.name.as_deref() == Some(name))
            .and_then(|p| p.port),
        None if ports.len() == 1 => ports[0].port,
        None => return Vec::new(),
    };
    let Some(pod_port) = pod_port.and_then(|p| u16::try_from(p).ok()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ep in &slice.endpoints {
        let ready = ep.conditions.as_ref().and_then(|c| c.ready) != Some(false);
        if !ready {
            continue;
        }
        for addr in &ep.addresses {
            if let Ok(ip) = addr.parse::<IpAddr>() {
                out.push(SocketAddr::new(ip, pod_port));
            }
        }
    }
    out
}

/// Keep at most `MAX_ENDPOINTS` (the map is fixed-size); warn once per truncating pass.
pub fn cap(set: EndpointSet) -> EndpointSet {
    if set.len() <= MAX_ENDPOINTS {
        return set;
    }
    warn!(
        "plaintext endpoints {} exceed cap {}; watching the first {}",
        set.len(),
        MAX_ENDPOINTS,
        MAX_ENDPOINTS
    );
    set.into_iter().take(MAX_ENDPOINTS).collect()
}

/// Holds the configured specs, or `None` when nothing is configured for plaintext, so a TLS-only
/// deployment starts no k8s watch and no DNS lookup.
pub struct Resolver {
    specs: Vec<EndpointSpec>,
}

impl Resolver {
    pub fn new(specs: Vec<EndpointSpec>) -> Option<Self> {
        if specs.is_empty() {
            None
        } else {
            Some(Self { specs })
        }
    }

    pub fn specs(&self) -> &[EndpointSpec] {
        &self.specs
    }
}

#[cfg(test)]
mod tests;
