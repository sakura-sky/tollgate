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

**A deployment is one trust boundary.** Any valid API key can read the whole deployment's budgets and usage through the `/console/*` endpoints, including the spend and limit of every per-key budget, identified by the first eight characters of that key's id. Key labels are not exposed, and `/console/usage` rows carry no key attribution. Budget *enforcement* is per key; visibility is not. If your keys belong to parties who should not see each other's spend, run separate deployments. Per-tenant scoping is an Enterprise-edition concern, not an open-source-core one.

**The pepper protects against a database read, and nothing else.** API keys are stored as HMAC-SHA256 digests under a server-side pepper held outside the database, so an attacker who reads `api_keys` cannot derive usable keys without also holding the pepper. An attacker who has both, or who has the running process's environment, has everything.

**Rotating the pepper invalidates every issued key at once.** There is no dual-pepper verification window, so a rotation is a full re-issue, not a rolling one. Plan it as an outage.

**Tollgate refuses to start without a real pepper.** A pepper shorter than 16 bytes, or the placeholder value shipped in `.env.example`, is rejected at boot rather than accepted with a warning. Starting with an ephemeral pepper would make every already-issued key fail to verify while the service still reported healthy: a silent, total auth outage.

**The ledger is append-only against accident, not against its owner.** Triggers reject `UPDATE` and `DELETE`, but the table owner can disable a trigger or drop a partition, and retention depends on that. The control that matters is the role split described in [`docs/OPERATIONS.md`](./docs/OPERATIONS.md).

**Privileged CLI actions are recorded, and the principal is self-asserted.** `admin key issue`, `key revoke`, `budget set` and `price set` each append a row to `audit_log`, which the same append-only triggers protect. The `principal` is the OS user and host the command ran as, taken from the environment, written literally as `cli:<user>@<host>`. The `cli:` prefix is part of the value: it marks the row as a command line rather than an identity, so an operator querying for `alice@host` finds nothing without it. It correlates a change with a shell history or a bastion log. It does not authenticate anyone: the CLI holds the database URL and the pepper, so whoever can run it is already trusted by everything downstream, and they can set those variables to whatever they like. The control that binds is who can reach the database and read the secrets. The trail records what changed and when; treat who as a hint.

The row is written in the same transaction as the change it describes, so there is no privileged change without a trail row: if the audit insert fails, the change is rolled back and the command reports it. That is deliberately stronger than logging a failure and carrying on, which is what this did first, and which relied on someone alerting on the stderr of a CLI run from a laptop. No secret is written: `key issue` records the key's id, prefix and label, never the key itself.

**The budget cache is protected by the network alone.** The Terraform module provisions Memorystore for Valkey with neither AUTH nor in-transit encryption, so budget counters travel unauthenticated and in cleartext, relying entirely on Private Service Connect keeping the instance off any other network. Anyone who reaches that address can read and write the counters that decide whether spending is allowed to continue. If your threat model includes an attacker inside the VPC, enable Memorystore AUTH and put the credential in the URL. In-transit encryption is not available in this build: the binary is compiled without a TLS feature for the Valkey client, so a `rediss://` URL is refused at startup. Adding it is open work, and it needs the enforcement battery rerun against a TLS server before anyone relies on it. The ledger limits the damage: counters are rebuilt from Postgres on restart and on any counter that goes missing, so tampering suppresses enforcement until the next rebuild rather than corrupting the spend record.

**TLS trust comes from the host, not from the binary.** The HTTP client uses rustls with the platform verifier, so outbound provider connections are validated against the CA store in the running image. Trust therefore rotates with the base image rather than requiring a rebuild, and a private CA installed in the image is honoured, which is what lets a TLS-inspecting egress proxy work. The consequence to plan for is the reverse: an image without CA certificates cannot establish any provider connection at all. The distroless runtime image ships them; a change of base image is a change to trust.

**Outbound destinations are fixed in code.** A caller supplies a provider name and a path suffix, both allowlisted, never a destination host, and the client refuses redirects, so an upstream `3xx` cannot bounce a request and its credentials elsewhere. There is no user-controlled URL on the forward path, which is what keeps the server-side request forgery surface small. The exception is an operator-configured custom OpenAI-compatible base URL, which is trusted configuration rather than caller input.

**A panic fails its request, not the process.** The binary unwinds rather than aborting, and a catch-panic layer turns a panic in a handler into a `500` with `x-tollgate-reason: backend_error` while the gateway keeps serving. A panic after a streaming response has begun cannot be turned into a status code, so the connection drops and the settlement guard charges that request its reservation. There is no known panic on any request path; this is about the shape of the failure if one appears.

**There is no rate limiter.** Deploy behind one. An unauthenticated request with a well-formed key still costs a Postgres lookup, and a key that is over budget writes one refusal row to the ledger per attempt, so a client in a retry loop turns into ledger volume.

## Disclosure

We follow coordinated disclosure. After a fix is shipped:

- We publish a [GitHub Security Advisory](https://github.com/sakura-sky/tollgate/security/advisories) with a CVE ID where appropriate.
- The release notes link to the advisory.
- Reporters who wish to be credited are named in the advisory.

If you have a deadline you'd like us to work within (e.g. a conference talk), tell us up front and we'll do our best to align.

## Hardening guidance

Deployment hardening guidance (persistence, egress lockdown, least-privilege database roles, and reference architectures) lives in [`docs/OPERATIONS.md`](./docs/OPERATIONS.md).
