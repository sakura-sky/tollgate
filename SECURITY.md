# Security Policy

## Reporting a vulnerability

If you discover a security issue in Tollgate, please **do not open a public GitHub issue**.

Preferred channel: open a [GitHub private vulnerability report](https://github.com/sakura-sky/tollgate/security/advisories/new) on the repository.

If GitHub PVR is not available to you, email **security@sakurasky.com** with:

- A clear description of the vulnerability and the impact you believe it has.
- Steps to reproduce (PoC code is welcome but not required).
- The version / commit you tested against.
- Whether you'd like to be credited in the advisory, and how.

You should expect an acknowledgement within **3 business days**. We aim to send a triage update (severity assessment + remediation timeline) within **10 business days**.

## Scope

In scope:

- The `tollgate` Rust binary and library crates in this repository.
- The Dockerfile and the resulting container image.
- The Terraform module under [`infra/terraform/`](./infra/terraform/) - but only configuration that ships in this repo, not operator modifications.

Out of scope:

- Issues that require physical access to a deployed instance.
- Bugs in upstream dependencies (`axum`, `sqlx`, `tokio`, etc.) - please report those to the relevant project. We're happy to coordinate if a vendored fix is needed in the meantime.
- Findings in the operator's own GCP configuration that are outside what the Terraform module installs.
- Denial-of-service via resource exhaustion in unconfigured deployments where token budgets / rate limits have not been set.

## Disclosure

We follow coordinated disclosure. After a fix is shipped:

- We publish a [GitHub Security Advisory](https://github.com/sakura-sky/tollgate/security/advisories) with a CVE ID where appropriate.
- The release notes link to the advisory.
- Reporters who wish to be credited are named in the advisory.

If you have a deadline you'd like us to work within (e.g. a conference talk), tell us up front and we'll do our best to align.

## Hardening guidance

Deployment hardening guidance (persistence, egress lockdown, least-privilege database roles, and reference architectures) lives in [`docs/OPERATIONS.md`](./docs/OPERATIONS.md).
