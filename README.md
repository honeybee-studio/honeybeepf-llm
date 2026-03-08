# eBPF Observability Platform

**Lightweight eBPF observability for AI workloads**

---

## 1. Project Overview
A lightweight, eBPF-based observability platform designed to identify cost and performance bottlenecks in AI workloads by selectively collecting essential data such as LLM token usage and system metrics.

---

## 2. Background / Introduction
Traditional observability tools often introduce significant operational overhead due to excessive resource consumption, required application code changes, and complex configuration processes.  

To address these limitations, our platform is built around a Rust-based eBPF agent that collects only essential data at the kernel level without any code modifications.  

The agent can be deployed via Helm Charts in Kubernetes environments or as a standalone binary in traditional AI data centers, enabling cost reduction and performance optimization across heterogeneous infrastructures.

---

## 3. Core Values
- We practice selective observability—collecting only decision-driving data directly from the kernel.  
- Minimal overhead by design  
- Infrastructure-agnostic: works on Kubernetes and traditional AI data centers  
- Built for AI efficiency: enabling cheaper, faster, and more efficient AI workloads  

---

## 4. Team

| Name   | ID | Role       | SNS | Responsibilities                 |
|--------|----|------------|-----|---------------------------------|
| Jundorok |    | Team Leader | TBU | Roadmap & Feature Development   |
| pmj-chosim |    | Core Dev   | TBU | CI/CD & Observability           |
| sammiee5311 |    | Core Dev   | TBU | Feature Development             |
| vanillaturtlechips |    | Core Dev   | TBU | CI/CD & Observability           |

---

## 5. Tech Stack
- **Languages:** eBPF, Kernel, Rust  
- **Infrastructure:** Kubernetes, Helm, OpenTelemetry, Prometheus, Grafana  
- **Communication:** Discord, GitHub Discussions  

---

## 6. Roadmap
- **Phase 1:** CI/CD and Observability Setup  
- **Phase 2:** Core Module Development  
- **Phase 3:** Monitoring and Testing  
- **Phase 4:** Release & Operator Integrations  

> We track roadmap execution via GitHub Projects and release multi-architecture
> container images using `publish.sh` once CI pipelines pass.

---

## 7. How to Contribute
- **Issues:** Use GitHub Issues for bug reports or feature requests  
- **PRs:** Contributions must open PRs  
- **Guide:** Follow [`CONTRIBUTING.md`](CONTRIBUTING.md) for coding standards and
	review expectations  

---

## 8. Resources & Links
- GitHub Repository: [github.com/jundorok/honeybeePF](https://github.com/jundorok/honeybeePF)
- Helm Charts: [`charts/honeybeepf`](charts/honeybeepf)
- Governance: [`GOVERNANCE.md`](GOVERNANCE.md)

---

## 9. Governance & Community
- **Code of Conduct:** See [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md). Report
	incidents privately via [GitHub Issues](https://github.com/jundorok/honeybeePF/issues).
- **Decision Process:** Maintainers document proposals via Issues/Discussions
	with a 72-hour community review window before landing major changes.  
- **Meetings:** We host quarterly community syncs announced in GitHub
	Discussions. Notes are published alongside meeting issues.  
- **Membership:** Active contributors who review and merge work over two
	consecutive releases are invited to join the maintainer group.

## 10. Licensing
- **Source Code:** MIT License (`LICENSE`).  
- **Documentation:** MIT License unless otherwise noted within the document.  
- **Third-Party Assets:** Refer to each component's directory for licensing
	notices.

---

