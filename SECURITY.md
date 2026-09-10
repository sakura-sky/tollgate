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
- Denial-of-service via resource exhaustion in unconfigured deployments where token budgets have not been set. Note that Tollgate ships no rate limiter of its own, by design: deploy it behind one.

## Threat model

What the design assumes, so you can tell whether it matches yours.

**A deployment is one trust boundary.** Any valid API key can read the whole deployment's budgets and usage through `/console` and the `/console/*` endpoints, including the labels and spend of every other key. Budget *enforcement* is per key; visibility is not. If your keys belong to parties who should not see each other's spend, run separate deployments. Per-tenant scoping is an Enterprise-edition concern, not an open-source-core one.

**The pepper protects against a database read, and nothing else.** API keys are stored as HMAC-SHA256 digests under a server-side pepper held outside the database, so an attacker who reads `api_keys` cannot derive usable keys without also holding the pepper. An attacker who has both, or who has the running process's environment, has everything.

**Rotating the pepper invalidates every issued key at once.** There is no dual-pepper verification window, so a rotation is a full re-issue, not a rolling one. Plan it as an outage.

**Tollgate refuses to start without a real pepper.** A pepper shorter than 16 bytes, or the placeholder value shipped in `.env.example`, is rejected at boot rather than accepted with a warning. Starting with an ephemeral pepper would make every already-issued key fail to verify while the service still reported healthy: a silent, total auth outage.

**The ledger is append-only against accident, not against its owner.** Triggers reject `UPDATE` and `DELETE`, but the table owner can disable a trigger or drop a partition, and retention depends on that. The control that matters is the role split described in [`docs/OPERATIONS.md`](./docs/OPERATIONS.md).

**Privileged CLI actions are not audited.** An `audit_log` table exists with the same append-only triggers, and nothing writes to it in this release. Key issuance, revocation, budget changes and price changes leave no record in the database beyond their effect.

## Disclosure

We follow coordinated disclosure. After a fix is shipped:

- We publish a [GitHub Security Advisory](https://github.com/sakura-sky/tollgate/security/advisories) with a CVE ID where appropriate.
- The release notes link to the advisory.
- Reporters who wish to be credited are named in the advisory.

If you have a deadline you'd like us to work within (e.g. a conference talk), tell us up front and we'll do our best to align.

## Hardening guidance

Deployment hardening guidance (persistence, egress lockdown, least-privilege database roles, and reference architectures) lives in [`docs/OPERATIONS.md`](./docs/OPERATIONS.md).
