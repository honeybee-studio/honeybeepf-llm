# Find the workloads that are not seen

A workload with no recorded calls may make none, or may make calls the agent cannot read. One metric tells them apart.

`honeybeepf_llm_llm_tls_coverage_pods` (gauge, Kubernetes builds) counts the pods on each node by whether the TLS
probes can read their calls, so a workload with no recorded calls can be told apart from one the agent cannot see.
Labels: `target_namespace`, `target_workload_kind`, `target_workload_name` and `tls`. Every minute the agent looks
at each pod's processes and counts the pod under its best one:

| `tls` | The pod has a process that |
|---|---|
| `hooked` | maps a libssl, or runs an executable (built-in OpenSSL or Go `crypto/tls`), that the probes are attached to |
| `openssl_not_hooked` | maps a libssl the probes are not attached to (the attach failed or has not run yet) |
| `go` | is a Go executable the probes are not attached to (Go before 1.20, or no `crypto/tls` linked in) |
| `java` | runs a JVM (JSSE is not hooked) |
| `other` | none of the above: rustls, a TLS library linked in statically, or no TLS at all |

A workload that is gone reads 0. Workloads whose calls the agent cannot see:

```promql
sum by (target_namespace, target_workload_name, tls) (honeybeepf_llm_llm_tls_coverage_pods{tls!="hooked"}) > 0
```

## What to do about each

| `tls` | What you can do |
|---|---|
| `openssl_not_hooked` | Check the agent log on that node for an attach error. A pod that started a moment ago is attached within a minute. |
| `go` | Go before 1.20 is not supported: a build with Go 1.20 or later is seen. |
| `java` | JSSE is not supported yet. |
| `other` | rustls and other statically linked TLS are not supported yet. A pod that calls a model server over plain HTTP is seen once that server is a [plaintext provider](plaintext.md). |
