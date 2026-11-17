# Troubleshooting

## The agent does not start

**It exits with `Operation not permitted` while creating a map.** The node's kernel needs a capability the pod does
not have. `NET_ADMIN` is needed before Linux 6.9 and `SYS_RESOURCE` before 5.11; if you trimmed the capability list in
your values, add them back. See [Permissions and resources](install.md#permissions-and-resources).

**The pod is pending or not scheduled on some nodes.** The DaemonSet runs on Linux nodes only
(`nodeSelector: kubernetes.io/os: linux`) and has no tolerations by default. To run on tainted nodes, set
`tolerations` in the values.

## No `LLM SUCCESS` lines

Work down this list; most cases are one of the first three.

1. **Look at the right node.** `kubectl logs ds/honeybeepf-llm` follows one agent pod. The calls of a pod are in the
   log of the agent on the same node:

    ```bash
    kubectl get pod -n <namespace> <pod> -o wide        # the NODE column
    kubectl get pod -n honeybeepf -o wide --field-selector spec.nodeName=<node>
    ```

2. **Check that the workload is seen.** With metrics on, the coverage metric says whether the agent can read a
   workload's TLS; see [Find the workloads that are not seen](guides/coverage.md).

    ```promql
    sum by (target_namespace, target_workload_name, tls) (honeybeepf_llm_llm_tls_coverage_pods{tls!="hooked"}) > 0
    ```

3. **Check that the provider is known.** The agent records calls to the hosts it knows; see
   [What is supported](providers.md). A self-hosted server, a company gateway or a provider that is not listed needs
   an entry: [Add a provider](guides/custom-providers.md), or a [plain-HTTP provider](guides/plaintext.md) for a server
   called without TLS.
4. **Check the protocol.** WebSocket (such as OpenAI Realtime) and HTTP/3 are not read yet.
5. **On Ubuntu nodes, keep the AppArmor setting.** The containerd AppArmor profile on Ubuntu denies the agent access to
   other containers' `libssl`, and then only pods that share the agent image's `libssl` are recorded. The chart sets
   `securityContext.appArmorProfile: Unconfined` for this; keep it if you override `securityContext`.

## Calls are seen but not recorded

`llm_responses_unparsed_total` counts calls the agent saw but could not read, by `reason`; the
[metrics reference](metrics.md) lists every reason. The common ones:

| `reason` | Usual cause | What to do |
|---|---|---|
| `usage_absent` | a streaming call without `stream_options.include_usage`, or an API that returns no usage (some embeddings) | send `stream_options: {"include_usage": true}` on OpenAI-compatible streams |
| `capture_lost` | the agent fell behind and the node's ring buffer was full, or one read was over 16 KB | check `llm_events_total{stage="reserve_fail"}`; the [benchmark](benchmark.md) shows the CPU it needs at a given call rate |
| `connection_closed`, `next_request` | the response ended before its usage, such as a cancelled request | usually expected; alert only on a sudden rise |

The chart's `LLMCallsNotRecorded` [alert](alerts.md) fires when the share of such calls passes a threshold.

## Calls are recorded but cost nothing

A model that is not in the price table is not costed as $0: its tokens are counted in `llm_unpriced_tokens_total`
with `reason="unknown_model"`. Add its price in the values; see [Add your own prices](guides/prices.md).

```promql
sum by (model, reason) (increase(honeybeepf_llm_llm_unpriced_tokens_total[1d]))
```

## No metrics in Prometheus

- `output.otlp.endpoint` is empty by default, and the agent then records calls in its log only. Set it to your
  Collector, as in [Install](install.md).
- An `https://` endpoint is verified against the CA in `output.otlp.caSecret`, else the system roots. For a private CA,
  create the Secret as in [A collector outside the cluster](install.md#a-collector-outside-the-cluster).
- Metric names in Prometheus carry the `honeybeepf_llm_` prefix: `honeybeepf_llm_llm_tokens_total`, not
  `llm_tokens_total`.

## Spend is counted twice

With [detection by shape](guides/detect-by-shape.md) on, a gateway inside the cluster reached over TLS (LiteLLM, a
company proxy) is recorded twice: the app's call to the gateway and the gateway's call to the provider. Leave one side
out of totals, for example with `{match!="shape"}`.

## `cosign verify` says `no signatures found`

The signatures use cosign 3's bundle format; use cosign 3 or later. Releases before 0.3.1 are not fully signed. See
[Verify the image, chart and binaries](verify.md).

## Still stuck?

Open an [issue](https://github.com/honeybee-studio/honeybeepf-llm/issues) with the agent log of the node, the kernel
version (`uname -r`), the app's language and TLS library, and the provider it calls. Leave out API keys and prompts.
