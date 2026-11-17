---
title: Your LLM bill, split by team
description: Your LLM bill, split by team. Tokens and estimated cost per Kubernetes namespace, pod and model, read with eBPF. No code changes, no proxy, no SDK.
hide:
  - navigation
  - toc
---

<div class="hb-hero" markdown>

<div class="hb-hero__text" markdown>

<h1>Your LLM bill, <em>split by team.</em></h1>

<p class="hb-sub">Your provider sends one total. honeybeepf-llm shows which namespace, pod and model spent it, without
touching your apps.</p>

[Quickstart :material-arrow-right:](quickstart.md){ .md-button .md-button--primary }
[GitHub :fontawesome-brands-github:](https://github.com/honeybee-studio/honeybeepf-llm){ .md-button }

<p class="hb-meta">Open source, Apache-2.0 · eBPF DaemonSet for Kubernetes · OpenTelemetry metrics</p>

</div>

<div class="hb-receipt-wrap">
<div class="hb-receipt" data-hb-live role="img" aria-label="A receipt printing LLM calls as they happen on the demo cluster, with a subtotal per team: team-b $1.96, team-a $0.85, team-c $0.17, total $2.98">
  <div class="hb-receipt__head">
    <strong>honeybeepf-llm</strong>
    <span>cluster demo · node worker-1</span>
    <span class="hb-receipt__live"><i></i>printing live</span>
  </div>
  <ol class="hb-receipt__items" data-hb-log>
    <li><b>team-b/support-bot</b><span>claude-sonnet-4-5 × 1,840 tok</span><em>$0.011</em></li>
    <li><b>team-a/chat-api</b><span>gpt-5.4-mini × 2,310 tok</span><em>$0.002</em></li>
    <li><b>team-c/summarizer</b><span>gemini-2.5-flash × 3,020 tok</span><em>$0.001</em></li>
    <li><b>team-b/support-bot</b><span>claude-opus-5-5 × 960 tok</span><em>$0.019</em></li>
  </ol>
  <div class="hb-receipt__subs">
    <div data-hb-team="team-b" data-hb-usd="1.96"><span>team-b</span><i></i><span class="hb-amount">$1.96</span></div>
    <div data-hb-team="team-a" data-hb-usd="0.85"><span>team-a</span><i></i><span class="hb-amount">$0.85</span></div>
    <div data-hb-team="team-c" data-hb-usd="0.17"><span>team-c</span><i></i><span class="hb-amount">$0.17</span></div>
  </div>
  <div class="hb-receipt__total"><span>Total</span><span data-hb-total>$2.98</span></div>
  <div class="hb-receipt__foot">Application code changed: 0 lines<br>Simulated calls on the demo workloads</div>
</div>
</div>

</div>

## How you get it

<div class="hb-steps" markdown>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--code">$ helm install honeybeepf-llm \
    oci://ghcr.io/honeybee-studio/\
    charts/honeybeepf-llm

<span>DaemonSet ready: 1 agent per node</span></div>
<div class="hb-step__n">1</div>
<h3>Install one DaemonSet</h3>

One agent per node. Your apps, SDKs and API keys stay as they are.
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--log"><span>pod</span>    team-b/support-bot
<span>host</span>   api.anthropic.com
<span>model</span>  claude-sonnet-4-5
<span>tokens</span> 1,840  <b>$0.011</b></div>
<div class="hb-step__n">2</div>
<h3>It reads every LLM call</h3>

Each call is read in the kernel, where your app hands it to TLS, and matched to its pod.
</div>

<div class="hb-step" markdown>
<div class="hb-step__art hb-step__art--shot"><img src="images/grafana-cost-narrow.png" alt="Grafana panel: estimated spend per workload, team-b support-bot $1.96, team-a chat-api $0.85, team-c summarizer $0.17"></div>
<div class="hb-step__n">3</div>
<h3>See the cost per team</h3>

Tokens and dollars per namespace, pod and model in Prometheus, with a [Grafana dashboard](grafana/README.md) included.
</div>

</div>

## Try it

=== "Helm"

    Requires a Kubernetes cluster with Linux 5.10+ nodes, `kubectl` and Helm 3.

    ```bash
    kubectl create namespace honeybeepf
    helm install honeybeepf-llm oci://ghcr.io/honeybee-studio/charts/honeybeepf-llm -n honeybeepf
    kubectl logs -n honeybeepf ds/honeybeepf-llm -f | grep 'LLM SUCCESS'
    ```

    For the Grafana cost dashboard, add the bundled Prometheus and OpenTelemetry Collector: see [Install](install.md).

=== "Demo, no API keys"

    Runs a mock provider, LLM clients in three team namespaces and Grafana in any cluster (kind, k3d, minikube).

    ```bash
    git clone https://github.com/honeybee-studio/honeybeepf-llm.git && cd honeybeepf-llm
    examples/demo/demo.sh up
    kubectl -n monitoring port-forward svc/grafana 3000:80
    ```

    Then open <http://localhost:3000> and the "LLM Cost Observability" dashboard.

## Why not a gateway or an SDK?

<div class="hb-compare" markdown>

| | Gateway / proxy | SDK instrumentation | honeybeepf-llm (eBPF) |
|---|---|---|---|
| App change | new base URL | add code, redeploy | none |
| In the request path | yes | in-process | no: reads buffer copies |
| Calls it sees | only those routed through it | only instrumented code | every pod on the node |
| API keys | pass through it | in the app process | never logged (path, model, tokens only) |

</div>

## How it works

![uprobes on SSL_write/SSL_read copy plaintext into a ring buffer; the agent parses HTTP, matches the provider, maps the cgroup to a pod and exports OTLP metrics](images/how-it-works.svg)

1. The agent attaches uprobes to `SSL_write` and `SSL_read` in every `libssl` a process maps, and to `crypto/tls` in
   Go executables.
2. The uprobes copy the plaintext buffer, the pid and the cgroup id into a BPF ring buffer.
3. In user space, the HTTP/1.1, HTTP/2 and SSE parser reassembles each call, picks the provider from the host and
   path, and reads the usage fields from the response.
4. The cgroup maps the process to its pod, and the result is exported as OpenTelemetry metrics.

## Read next

<div class="hb-index" markdown>

- [Quickstart](quickstart.md): install the agent and watch the calls in a few minutes.
- [How it works](how-it-works.md): from a TLS buffer to dollars per pod, step by step.
- [What is supported](providers.md): providers, runtimes, protocols and kernels.
- [Find the workloads that are not seen](guides/coverage.md): one query lists them.
- [Add a provider](guides/custom-providers.md) or [your own prices](guides/prices.md).
- [Metrics](metrics.md): every metric and label, and why a call was not recorded.
- [Budgets and alerts](alerts.md): spend and data-quality rules shipped with the chart.
- [Troubleshooting](troubleshooting.md) and [FAQ](faq.md).
- [Security review](security-review.md): kernel hooks, privileges, data handling and known gaps.
- [Overhead benchmark](benchmark.md): CPU, memory and throughput with and without the agent.

</div>
