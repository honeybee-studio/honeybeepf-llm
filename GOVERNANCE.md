# honeybeepf-llm Governance Model

This document outlines how we collaboratively maintain honeybeepf-llm in line with
open source best practices.

## Roles and Responsibilities

- **Maintainers**: The core team listed in `README.md`. Maintainers are
  responsible for code reviews, release planning, roadmap stewardship, and
  shepherding community contributions.
- **Contributors**: Anyone submitting issues, pull requests, documentation, or
  operational feedback. Contributors help expand the project by sharing
  real-world requirements and improvements.
- **Users and Adopters**: Practitioners running honeybeepf-llm in production or
  evaluating it in proof-of-concept environments. Their feedback informs
  prioritization decisions.

## Decision Making

We favor consensus but are pragmatic:

1. **Discussion**: Product and architectural changes begin in GitHub Issues or
   Discussions. Maintainers ensure proposals are visible for community review.
2. **Implementation**: Pull requests must reference the related discussion or
   issue. At least one maintainer review plus one additional reviewer is
   required for substantive changes.
3. **Escalation**: If consensus is not reached, maintainers will summarize the
   viewpoints, request feedback with a minimum 72-hour window, and make a final
   decision that keeps the project aligned with its mission.

All decisions are recorded publicly through the associated GitHub artifacts.

## Release Process

- Releases are cut from the `main` branch once the CI pipeline is green.
- Each release tags source code and Docker images, and updates the Helm chart
  templates as needed.
- A release checklist issue tracks validation steps such as eBPF probe
  compatibility, regression tests, and documentation updates.

## Community Contributions

We welcome contributions of all sizes. Contributors are encouraged to:

- Review the `CONTRIBUTING.md` guide before opening a pull request.
- Participate in design discussions through GitHub Discussions and Issues.
- Join periodic community sync calls announced in the repository discussion
  forum.

Maintainers will acknowledge new contributors in release notes and invite active
contributors to become reviewers. Reviewer status is reviewed quarterly.

## Code of Conduct

All community spaces and interactions are governed by the
[`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md). Incidents can be reported privately via
[GitHub Issues](https://github.com/honeybee-studio/honeybeepf-llm/issues).

## Changes to This Document

Governance updates follow the same process as other project changes—proposed via
pull request, open for community review, and merged by a maintainer once
consensus is reached.
