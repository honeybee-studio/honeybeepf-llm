use std::net::Ipv4Addr;

use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::api::discovery::v1::{Endpoint, EndpointConditions, EndpointPort, EndpointSlice};

use super::*;
use honeybeepf_llm_core::providers::ProviderRegistry;

fn reg(json: &str) -> ProviderRegistry {
    ProviderRegistry::from_json(json).expect("test registry parses")
}

fn svc(cluster_ip: Option<&str>, ports: Vec<ServicePort>) -> Service {
    let spec = ServiceSpec {
        cluster_ip: cluster_ip.map(String::from),
        ports: Some(ports),
        ..Default::default()
    };
    Service {
        spec: Some(spec),
        ..Default::default()
    }
}

fn sport(n: i32, name: Option<&str>) -> ServicePort {
    ServicePort {
        port: n,
        name: name.map(String::from),
        ..Default::default()
    }
}

fn slice(endpoints: Vec<Endpoint>, ports: Vec<EndpointPort>) -> EndpointSlice {
    EndpointSlice {
        address_type: "IPv4".into(),
        endpoints,
        ports: Some(ports),
        ..Default::default()
    }
}

fn ep(addr: &str, ready: Option<bool>) -> Endpoint {
    let conditions = Some(EndpointConditions {
        ready,
        ..Default::default()
    });
    Endpoint {
        addresses: vec![addr.into()],
        conditions,
        ..Default::default()
    }
}

fn eport(name: Option<&str>, n: i32) -> EndpointPort {
    EndpointPort {
        name: name.map(String::from),
        port: Some(n),
        ..Default::default()
    }
}

fn spec(host: &str, ns: Option<&str>) -> EndpointSpec {
    EndpointSpec {
        host: host.into(),
        port: 8000,
        namespace: ns.map(String::from),
    }
}

#[test]
fn specs_select_only_plaintext_providers_and_skip_blank_hosts() {
    // TLS-only config has no `plaintext` key, so the feature stays off.
    assert!(
        specs_from_registry(&reg(
            r#"{"providers":[{"name":"o","hosts":["api.openai.com"]}]}"#
        ))
        .is_empty()
    );
    assert!(
        specs_from_registry(&reg(
            r#"{"providers":[{"name":"v","hosts":[""," "],"plaintext":{"port":80}}]}"#
        ))
        .is_empty()
    );
    let specs = specs_from_registry(&reg(
        r#"{"providers":[{"name":"v","hosts":["vllm","mirror"],"plaintext":{"port":8000,"namespace":"models"}}]}"#,
    ));
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0], spec("vllm", Some("models")));
    assert_eq!(specs[1].host, "mirror");
}

#[test]
fn resolver_is_none_without_specs() {
    // Zero cost when disabled: no specs → nothing to run.
    assert!(Resolver::new(vec![]).is_none());
    assert!(Resolver::new(vec![spec("vllm", Some("models"))]).is_some());
}

#[test]
fn classify_picks_service_vs_external() {
    let cases = [
        ("vllm", None, Some(("vllm", "default"))), // single label
        (
            "vllm.models.svc.cluster.local",
            None,
            Some(("vllm", "models")),
        ), // FQDN carries ns
        ("vllm.models", Some("models"), Some(("vllm", "models"))), // explicit ns
        ("api.openai.com", None, None),            // public FQDN → external
        ("vllm.models", None, None),               // ambiguous → external
    ];
    for (host, ns, want) in cases {
        match (classify(&spec(host, ns)), want) {
            (Target::Service { name, namespace }, Some((n, s))) => {
                assert_eq!((name.as_str(), namespace.as_str()), (n, s))
            }
            (Target::External(_), None) => {}
            _ => panic!("classify({host}, {ns:?})"),
        }
    }
}

#[test]
fn cluster_ip_endpoint_uses_configured_port_and_skips_headless() {
    let s = svc(Some("10.0.0.1"), vec![sport(8000, Some("http"))]);
    assert_eq!(
        cluster_ip_endpoint(&s, 8000),
        Some("10.0.0.1:8000".parse().expect("addr"))
    );
    assert!(cluster_ip_endpoint(&svc(Some("None"), vec![]), 8000).is_none()); // headless
    assert!(cluster_ip_endpoint(&svc(None, vec![]), 8000).is_none());
}

#[test]
fn slice_maps_ready_pod_ips_to_the_service_target_port() {
    // Service port 8000 is named "http"; the slice's "http" port is the pod's 8080 (targetPort).
    let s = svc(Some("10.0.0.1"), vec![sport(8000, Some("http"))]);
    assert_eq!(service_port_name(&s, 8000).as_deref(), Some("http"));
    let sl = slice(
        vec![
            ep("10.1.0.5", Some(true)),
            ep("10.1.0.6", None),
            ep("10.1.0.7", Some(false)),
        ],
        vec![eport(Some("http"), 8080), eport(Some("metrics"), 9090)],
    );
    let mut got = slice_endpoints(&sl, Some("http"));
    got.sort();
    // The not-ready 10.1.0.7 is dropped; the matching port 8080 is used, not 9090.
    assert_eq!(
        got,
        vec![
            "10.1.0.5:8080".parse().expect("addr"),
            "10.1.0.6:8080".parse().expect("addr")
        ]
    );
}

#[test]
fn slice_falls_back_to_a_sole_unnamed_port_and_tracks_readiness_change() {
    let one = |ready| slice(vec![ep("10.1.0.5", Some(ready))], vec![eport(None, 8080)]);
    assert_eq!(
        slice_endpoints(&one(true), None),
        vec!["10.1.0.5:8080".parse().expect("addr")]
    );
    // A pod going not-ready changes the set (endpoint change is observable).
    assert_ne!(
        slice_endpoints(&one(true), None),
        slice_endpoints(&one(false), None)
    );
    // No name and several ports → nothing, rather than a wrong guess.
    let many = slice(
        vec![ep("10.1.0.5", Some(true))],
        vec![eport(None, 8080), eport(None, 9090)],
    );
    assert!(slice_endpoints(&many, None).is_empty());
}

#[test]
fn cap_truncates_to_the_map_bound() {
    let addrs = |n: u32| -> EndpointSet {
        (0..n)
            .map(|i| SocketAddr::new(Ipv4Addr::from(i).into(), 80))
            .collect()
    };
    assert_eq!(cap(addrs(10)), addrs(10));
    assert_eq!(cap(addrs(MAX_ENDPOINTS as u32 + 5)).len(), MAX_ENDPOINTS);
}

#[test]
fn endpoint_key_encodes_ipv4_and_skips_ipv6() {
    // 10.0.0.1:8000 → IP network-order bits in the high 32, htons(port) in the low 16.
    let ip = u32::from(Ipv4Addr::new(10, 0, 0, 1)).to_be() as u64;
    let want = (ip << 16) | (8000u16.to_be() as u64);
    assert_eq!(
        endpoint_key(&"10.0.0.1:8000".parse().expect("addr")),
        Some(want)
    );
    assert_eq!(endpoint_key(&"[::1]:8000".parse().expect("addr")), None);
}

#[test]
fn cgroup_gate_picks_unified_then_hybrid_else_none() {
    let unified = "cgroup2 /sys/fs/cgroup cgroup2 rw 0 0";
    assert_eq!(
        cgroup_v2_root_from(unified),
        Some(PathBuf::from("/sys/fs/cgroup"))
    );
    let hybrid = "tmpfs /sys/fs/cgroup tmpfs rw 0 0\n\
                  cgroup2 /sys/fs/cgroup/unified cgroup2 rw 0 0\n\
                  cgroup /sys/fs/cgroup/memory cgroup rw 0 0";
    assert_eq!(
        cgroup_v2_root_from(hybrid),
        Some(PathBuf::from("/sys/fs/cgroup/unified"))
    );
    let v1 = "tmpfs /sys/fs/cgroup tmpfs rw 0 0\n\
              cgroup /sys/fs/cgroup/memory cgroup rw,memory 0 0";
    assert_eq!(cgroup_v2_root_from(v1), None);
}
