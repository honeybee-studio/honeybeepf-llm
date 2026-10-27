# Security Policy

honeybeepf-llm runs on every node with elevated privileges (eBPF capabilities, the host PID namespace and a read-only view of the host filesystem; see `securityContext` in `charts/honeybeepf-llm/values.yaml`) and reads TLS plaintext of LLM API calls, including request bodies and headers that may carry API keys. A security bug here can expose secrets or affect the whole node, so please report it privately.

## Reporting a vulnerability

- Use GitHub's private reporting: **Security → Report a vulnerability** on this repository ([direct link](https://github.com/honeybee-studio/honeybeepf-llm/security/advisories/new)).
- Please do not open a public issue, pull request or discussion for a suspected vulnerability.
- Include the version or commit, the kernel and distribution, how the agent was deployed (Helm chart values, binary), and steps or a proof of concept if you have one. Do not include real API keys or real prompt data; redact them.

We will acknowledge the report, keep you updated while we investigate, and credit you in the advisory unless you prefer otherwise. Please give us a reasonable time to release a fix before any public disclosure.

## Supported versions

There is no tagged release yet. Fixes go to `main` and to the images built from it (`ghcr.io/honeybee-studio/honeybeepf-llm:latest` and `sha-<commit>`). Once releases start, the latest minor release will receive security fixes.

## Scope

In scope:

- Plaintext, API keys or other secrets leaving the agent through logs, metrics, OpenTelemetry attributes or files.
- Memory safety, crashes or unbounded memory/CPU use in the agent caused by traffic it parses (a malicious server or client response).
- eBPF programs that can crash, hang or corrupt state in the kernel, or that read or write more than they should.
- Privilege issues in the Helm chart or the container image (capabilities, host mounts, service account permissions).
- The build and release pipeline: GitHub Actions workflows, published images and charts.

Out of scope:

- Vulnerabilities in LLM providers or in the applications being observed.
- Vulnerabilities in third-party dependencies that do not affect this project. Please report those upstream; if one does affect us, we want to know.
- Findings that need an attacker who already has root or `CAP_SYS_ADMIN` on the node.
