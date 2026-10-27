# Security Policy

honeybeepf-llm runs as a DaemonSet with elevated privileges on every node. It has eBPF capabilities, the host PID namespace and a read-only view of the host filesystem; see `securityContext` in `charts/honeybeepf-llm/values.yaml` for the exact list. It reads the TLS plaintext of LLM API calls made on the node, including request headers that carry API keys. A bug in the agent can therefore leak secrets or affect the whole node. Please report suspected vulnerabilities privately.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting: **Security → Report a vulnerability** on this repository ([direct link](https://github.com/honeybee-studio/honeybeepf-llm/security/advisories/new)). Please do not open a public issue, pull request or discussion about it.

A useful report includes:

- the version or commit, and the image tag or digest if you used a published image;
- the kernel version and distribution of the node, and the Kubernetes distribution;
- how the agent was deployed (Helm values that differ from the defaults, or the binary and its settings);
- what an attacker needs (for example: runs a pod on the node, controls an LLM endpoint the pod calls, has access to the metrics backend) and what they gain;
- steps or a proof of concept.

Please replace real API keys, prompts and responses with placeholders. Test only against clusters and accounts you own.

## What happens next

We will acknowledge the report, work with you on a fix, and keep you updated. When the fix is released we publish a GitHub security advisory, request a CVE where it applies, and credit you unless you prefer otherwise. Please keep the details private until the advisory is published.

## Supported versions

There is no tagged release yet. Security fixes go to `main` and to the images built from it (`latest` and `sha-<commit>` tags). Once tagged releases start, fixes go to the latest release.

## Scope

In scope:

- **Leaks:** plaintext, API keys or other secrets leaving the agent through logs, metrics, OpenTelemetry attributes or anything else it writes or sends.
- **Untrusted input:** the agent parses traffic that workloads and remote endpoints control. A crafted request or response that crashes the agent, makes it use unbounded memory or CPU, or creates an unbounded number of metric series (for example through label values such as the model name) is in scope.
- **Crossing boundaries:** a workload using the agent to learn about or affect another workload on the node beyond what the metrics are meant to show.
- **Kernel side:** eBPF programs that can crash, hang or corrupt kernel state, or read or write memory they should not.
- **Deployment:** privileges in the Helm chart or the container image that are broader than the agent needs (capabilities, host mounts, host namespaces, RBAC).
- **Supply chain:** the GitHub Actions workflows and the images and charts they publish.

Not vulnerabilities (by design):

- The agent reading TLS plaintext of LLM calls on its own node. That is how it works; what matters is that the plaintext does not leave the agent.
- The agent needing the privileges listed in the chart. Reports that one of them is not needed are welcome as normal issues.
- Metrics showing namespaces, pod names, model names and token counts. That is their purpose; access to the metrics backend should be restricted like any other monitoring data.

Out of scope:

- Vulnerabilities in LLM providers, in the observed applications, or in the metrics backend.
- Vulnerabilities in dependencies that do not affect this project. Please report them upstream; if one does affect us, report it here as well.
- Attacks that need root or `CAP_SYS_ADMIN` on the node already.
