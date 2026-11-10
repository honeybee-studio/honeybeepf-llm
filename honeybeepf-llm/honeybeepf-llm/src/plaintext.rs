//! Resolve plaintext (non-TLS) capture endpoints to the IP:port set the kernel watches. A source
//! (`specs_from_registry` from config; a Service annotation could be another) yields `EndpointSpec`s;
//! the source-agnostic core maps each to its Service ClusterIP and ready pod IPs, or DNS if external.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use aya::util::KernelVersion;
use honeybeepf_llm_common::plain_endpoint_key;
use honeybeepf_llm_core::providers::ProviderRegistry;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::{Api, Client, api::ListParams};
use log::{debug, info, warn};

use crate::probes::{IdentityResolver, shutdown_flag};

/// Full ring size once plaintext is enabled; the eBPF map declares a tiny default so a TLS-only
/// node spends almost nothing on it.
pub const PLAIN_EVENTS_FULL_BYTES: u32 = 4 * 1024 * 1024;
/// Seconds between reconcile passes (re-list Services, re-resolve external DNS).
const RECONCILE_SECONDS: u64 = 30;
const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";

/// Upper bound for the fixed-size BPF map; beyond it extra endpoints are dropped with a warning.
pub const MAX_ENDPOINTS: usize = 8192;
/// Where the cgroup v2 root may be mounted, in order of preference. The host's tree under `/host` (the chart
/// mounts the host root there) comes first: a pod has its own cgroup namespace, so its own `/sys/fs/cgroup`
/// is only the agent pod's cgroup and a program attached there sees no other pod's sockets. Unified before
/// hybrid's `unified` subdirectory.
const CGROUP_V2_ROOTS: [&str; 4] = [
    "/host/sys/fs/cgroup",
    "/host/sys/fs/cgroup/unified",
    "/sys/fs/cgroup",
    "/sys/fs/cgroup/unified",
];

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
        .or_else(|| labels.get(1).map(std::string::ToString::to_string))
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

    /// Reconcile the endpoint set every `RECONCILE_SECONDS` until shutdown, pushing changes to the
    /// sink. A periodic pass covers pod churn within one interval; a streaming watch is a refinement.
    pub async fn run(self, client: Client, mut sink: impl EndpointSink) {
        let shutdown = shutdown_flag();
        let mut last: Option<EndpointSet> = None;
        loop {
            let set = cap(resolve(&client, &self.specs).await);
            if last.as_ref() != Some(&set) {
                sink.replace(&set);
                last = Some(set);
            }
            for _ in 0..RECONCILE_SECONDS {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// One resolution pass. A single bad host is logged and skipped so it never drops the rest.
async fn resolve(client: &Client, specs: &[EndpointSpec]) -> EndpointSet {
    let mut set = EndpointSet::new();
    for spec in specs {
        match classify(spec) {
            Target::External(host) => {
                match tokio::net::lookup_host((host.as_str(), spec.port)).await {
                    Ok(addrs) => set.extend(addrs),
                    Err(e) => warn!("plaintext DNS {host}:{} failed: {e}", spec.port),
                }
            }
            Target::Service { name, namespace } => {
                let svc: Api<Service> = Api::namespaced(client.clone(), &namespace);
                let mut port_name = None;
                match svc.get(&name).await {
                    Ok(service) => {
                        if let Some(addr) = cluster_ip_endpoint(&service, spec.port) {
                            set.insert(addr);
                        }
                        port_name = service_port_name(&service, spec.port);
                    }
                    Err(e) => warn!("plaintext Service {namespace}/{name} failed: {e}"),
                }
                let slices: Api<EndpointSlice> = Api::namespaced(client.clone(), &namespace);
                let lp = ListParams::default().labels(&format!("{SERVICE_NAME_LABEL}={name}"));
                match slices.list(&lp).await {
                    Ok(list) => {
                        for slice in list {
                            set.extend(slice_endpoints(&slice, port_name.as_deref()));
                        }
                    }
                    Err(e) => warn!("plaintext EndpointSlices {namespace}/{name} failed: {e}"),
                }
            }
        }
    }
    set
}

/// Key for the `PLAIN_ENDPOINTS` map; IPv6 is not matched yet (pod IPs are IPv4 in scope).
pub fn endpoint_key(addr: &SocketAddr) -> Option<u64> {
    match addr {
        SocketAddr::V4(a) => Some(plain_endpoint_key(
            u32::from(*a.ip()).to_be(),
            a.port().to_be(),
        )),
        SocketAddr::V6(_) => None,
    }
}

/// Writes the resolved set into the `PLAIN_ENDPOINTS` BPF map, inserting new keys and removing gone
/// ones so the kernel always watches exactly the current endpoints.
pub struct MapEndpointSink {
    map: aya::maps::HashMap<aya::maps::MapData, u64, u8>,
    keys: BTreeSet<u64>,
}

impl MapEndpointSink {
    pub fn new(map: aya::maps::HashMap<aya::maps::MapData, u64, u8>) -> Self {
        Self {
            map,
            keys: BTreeSet::new(),
        }
    }
}

impl EndpointSink for MapEndpointSink {
    fn replace(&mut self, endpoints: &EndpointSet) {
        let want: BTreeSet<u64> = endpoints.iter().filter_map(endpoint_key).collect();
        for key in want.difference(&self.keys) {
            let _ = self.map.insert(key, 1u8, 0);
        }
        for key in self.keys.difference(&want) {
            let _ = self.map.remove(key);
        }
        self.keys = want;
    }
}

/// The cgroup v2 hierarchy root to attach at: the first of `CGROUP_V2_ROOTS` mounted as cgroup2.
fn cgroup_v2_root_from(mounts: &str) -> Option<PathBuf> {
    let v2: Vec<&str> = mounts
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let (_src, mount, fstype) = (f.next()?, f.next()?, f.next()?);
            (fstype == "cgroup2").then_some(mount)
        })
        .collect();
    CGROUP_V2_ROOTS
        .iter()
        .find(|root| v2.contains(root))
        .map(PathBuf::from)
}

fn cgroup_v2_root() -> Option<PathBuf> {
    cgroup_v2_root_from(&std::fs::read_to_string("/proc/mounts").ok()?)
}

/// Plaintext endpoints declared in the configured providers (built-ins are never plaintext, so the
/// built-in merge is skipped). Empty = feature off. Reads the same env the provider registry does.
pub fn configured_specs() -> Vec<EndpointSpec> {
    let json = std::env::var("LLM_PROVIDERS_CONFIG_FILE")
        .ok()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .or_else(|| std::env::var("LLM_PROVIDERS_CONFIG").ok());
    let registry = json
        .and_then(|j| ProviderRegistry::from_json(&j).ok())
        .unwrap_or_default();
    specs_from_registry(&registry)
}

/// Gate on cgroup v2 + kernel 5.10, attach the socket programs, start the parser, fill the endpoint
/// map, and run the reconcile driver; a failure after the sockops attach undoes it. Returns Err
/// (logged as disabled) when the node cannot support it.
pub async fn start(
    bpf: &mut aya::Ebpf,
    specs: Vec<EndpointSpec>,
    resolver: IdentityResolver,
) -> Result<()> {
    use aya::programs::{CgroupAttachMode, CgroupSockAddr, SkMsg, SkSkb, SockOps};

    if !KernelVersion::current().is_ok_and(|v| v >= KernelVersion::new(5, 10, 0)) {
        anyhow::bail!("kernel older than 5.10");
    }
    let cgroup = cgroup_v2_root().context("no cgroup v2 hierarchy (pure cgroup v1 node)")?;
    let cgroup_fd = std::fs::File::open(&cgroup)?;
    info!("Plaintext programs attach at cgroup {}", cgroup.display());

    // Verify every program before attaching any: a verifier rejection then leaves nothing attached.
    {
        let p: &mut CgroupSockAddr = bpf
            .program_mut("plaintext_connect4")
            .context("plaintext_connect4 program missing")?
            .try_into()?;
        p.load().map_err(|e| load_error("plaintext_connect4", e))?;
    }
    {
        let p: &mut SockOps = bpf
            .program_mut("plaintext_sockops")
            .context("plaintext_sockops program missing")?
            .try_into()?;
        p.load().map_err(|e| load_error("plaintext_sockops", e))?;
    }
    {
        let p: &mut SkMsg = bpf
            .program_mut("plaintext_skmsg")
            .context("plaintext_skmsg program missing")?
            .try_into()?;
        p.load().map_err(|e| load_error("plaintext_skmsg", e))?;
    }
    {
        let p: &mut SkSkb = bpf
            .program_mut("plaintext_skskb")
            .context("plaintext_skskb program missing")?
            .try_into()?;
        p.load().map_err(|e| load_error("plaintext_skskb", e))?;
    }

    // connect4 first, so a connection's pid/cgroup is recorded before sockops reads it at establish.
    let connect4: &mut CgroupSockAddr = bpf
        .program_mut("plaintext_connect4")
        .context("plaintext_connect4 program missing")?
        .try_into()?;
    let connect4_link = connect4.attach(&cgroup_fd, CgroupAttachMode::Single)?;

    let sockops: &mut SockOps = bpf
        .program_mut("plaintext_sockops")
        .context("plaintext_sockops program missing")?
        .try_into()?;
    let sockops_link = match sockops.attach(&cgroup_fd, CgroupAttachMode::Single) {
        Ok(link) => link,
        Err(e) => {
            detach_connect4(bpf, connect4_link);
            return Err(e.into());
        }
    };

    if let Err(e) = attach_programs_and_read(bpf, resolver) {
        detach_sockops(bpf, sockops_link);
        detach_connect4(bpf, connect4_link);
        return Err(e);
    }

    let map = aya::maps::HashMap::<_, u64, u8>::try_from(
        bpf.take_map("PLAIN_ENDPOINTS")
            .context("PLAIN_ENDPOINTS map missing")?,
    )?;
    let client = Client::try_default().await?;
    let Some(resolver) = Resolver::new(specs) else {
        return Ok(());
    };
    tokio::spawn(resolver.run(client, MapEndpointSink::new(map)));
    Ok(())
}

/// Attach the sk_msg (send) and sk_skb (receive) programs to the sockhash of watched sockets and
/// start parsing PLAIN_EVENTS into LLM calls.
fn attach_programs_and_read(bpf: &mut aya::Ebpf, resolver: IdentityResolver) -> Result<()> {
    use aya::programs::{SkMsg, SkSkb};
    let socks = aya::maps::SockHash::<_, u64>::try_from(
        bpf.map("PLAIN_SOCKS").context("PLAIN_SOCKS map missing")?,
    )?
    .fd()
    .try_clone()?;
    {
        let skmsg: &mut SkMsg = bpf
            .program_mut("plaintext_skmsg")
            .context("plaintext_skmsg program missing")?
            .try_into()?;
        skmsg.attach(&socks)?;
    }
    {
        let skskb: &mut SkSkb = bpf
            .program_mut("plaintext_skskb")
            .context("plaintext_skskb program missing")?
            .try_into()?;
        skskb.attach(&socks)?;
    }
    crate::probes::builtin::llm::spawn_plaintext_parser(bpf, resolver)
}

/// Detach the sockops program so a failed later step leaves nothing attached to the cgroup.
fn detach_sockops(bpf: &mut aya::Ebpf, link: aya::programs::sock_ops::SockOpsLinkId) {
    use aya::programs::SockOps;
    let prog = bpf
        .program_mut("plaintext_sockops")
        .expect("plaintext_sockops present after it was attached");
    if let Ok(prog) = TryInto::<&mut SockOps>::try_into(prog) {
        let _ = prog.detach(link);
    }
}

/// Detach the connect4 program so a failed later step leaves nothing attached to the cgroup.
fn detach_connect4(
    bpf: &mut aya::Ebpf,
    link: aya::programs::cgroup_sock_addr::CgroupSockAddrLinkId,
) {
    use aya::programs::CgroupSockAddr;
    let prog = bpf
        .program_mut("plaintext_connect4")
        .expect("plaintext_connect4 present after it was attached");
    if let Ok(prog) = TryInto::<&mut CgroupSockAddr>::try_into(prog) {
        let _ = prog.detach(link);
    }
}

/// Log a program's verifier rejection — the last lines at warn, the whole log at debug so the kernel
/// matrix shows the cause — and return a short error for the caller.
fn load_error(name: &str, e: aya::programs::ProgramError) -> anyhow::Error {
    use aya::programs::ProgramError;
    if let ProgramError::LoadError { verifier_log, .. } = &e {
        let full = verifier_log.to_string();
        let lines: Vec<&str> = full.lines().collect();
        let keep = lines.len().min(25);
        let tail = lines[lines.len() - keep..].join("\n");
        warn!(
            "{name}: verifier rejected the program; last {keep} of {} log lines:\n{tail}",
            lines.len()
        );
        debug!("{name}: full verifier log:\n{full}\n--- end verifier log ---");
        return anyhow::anyhow!("{name}: verifier rejected the program");
    }
    anyhow::anyhow!("{name}: {e}")
}

#[cfg(test)]
mod tests;
