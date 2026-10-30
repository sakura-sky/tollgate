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

**Authentication comes first, and says nothing about the deployment.** Every request to `/v1`, a path under `/v1/`, or a path under `/console/` is authenticated before any route, extractor or handler runs, including on unknown paths and with the wrong method, and no request body is read first. Without a valid key, the status, headers and body depend only on the path's family (Anthropic, OpenAI, the enveloped `/v1/{provider}` routes, or the console) and on whether the key store answered: never on which providers are enabled, the admission mode, prices, budgets, the method, the sub-path or the body. Edge cases: `/v1/messagesx` is in the enveloped family and does not read `x-api-key`, and `/v1x` is not gated (no route serves it, so it is a `404` like any unknown path). `/healthz`, `/readyz` (which already reports store health), `/metrics` (which reports budget limits, request counters and the unmatchable-budgets gauge) and the `/console` page are public by design. HEAD responses carry no body on the wire (hyper, the HTTP server, strips it). Whether a connection is kept alive after a refusal can vary with the size of the unread body: hyper attempts one drain and otherwise closes the read side, never buffering it.

**The cost of an unauthenticated request depends only on what was sent.** No key, or one that is not exactly the issued shape (`tgk_`, 16 lowercase hex characters, `_`, 48 lowercase hex characters), is refused with no store access. A well-formed key costs one Postgres lookup and one HMAC verification, against a fixed dummy hash when the prefix is unknown or the store fails, so a hit and a miss do the same hashing. `x-api-key` moves a request into the lookup class only on `/v1/messages` paths. During a store stall the `503` arrives within `TOLLGATE_DATABASE__ACQUIRE_TIMEOUT` plus 250 ms, which anyone can measure without a key; it reveals the health of the key store, much as `/readyz` reveals the health of the shared pool. The lookups run on their own small pool, so a flood of well-formed junk keys can make legitimate callers wait for it and get that `503` (the `503` reflects store health or key-lookup saturation), but it cannot take the connections the ledger and config reload use.

**Provider error responses do not reach a client.** When a provider answers with a non-2xx status, buffered or streaming, the client gets a status classified by fault (the caller's body: `400`, `413` or `422` unchanged; the provider's rate limit: `429`; everything else, including the provider rejecting Tollgate's own credential: `502`), a reason header and a fixed message. None of the provider's body or headers is passed on, because a provider message can name the cloud project, region and resource path. A `Retry-After` on a `429` is re-written from a whole number of seconds, clamped to 1 to 60, never copied. Inside a stream that began with `200`, only events Tollgate can positively classify as safe are forwarded, and it fails closed on everything else. Lines are parsed as the SSE specification says (CR, LF and CRLF line ends, `data:` lines joined per event), so an error split across lines or line endings is still recognised. Raw U+2028, U+2029 and U+0085 inside a forwarded event are escaped, so a client that also splits lines on those reads the same events. Only `data:` and `event:` lines are kept; comments, `id:`, `retry:` and unknown fields are dropped. An event must have no data, data that is `[DONE]` (OpenAI only), or one JSON object that parses strictly; anything else (plain text, `NaN`, arrays, invalid UTF-8, an unknown or repeated event name, an event over 1 MiB) is treated like an error. An error is a top-level `error` that is not `null`, an event named `error`, or (Anthropic) `"type": "error"`. It ends the client's stream with one fixed error event carrying the request id, and nothing more is forwarded; metering continues to the provider's end (within the idle and maximum-duration limits) even if the client then disconnects. A `200` that is not an event stream, or is compressed, is never relayed. Text in any other field of a safe event (generated output, or an error nested inside a choice) is relayed, with line ends rewritten to LF and U+2028, U+2029 and U+0085 escaped. A `429` with `upstream_rate_limited` tells any authenticated caller that the provider is limiting the deployment as a whole, and its `Retry-After` is derived from the provider's value.

**What the request span records.** For an authenticated request, the `request` span carries the request id, the method (cut to 16 bytes) and the path without its query string (cut to 256 bytes). Both of the last two are the caller's own text, so they appear on that request's log lines and, with OTLP set, in the exported span.

**Request ids carry nothing.** Every response on `/v1` and `/console/*` carries `x-tollgate-request-id`, a random UUID v4 generated for that request. A value the client sends is never used, so a caller cannot plant an id in the operator's logs.

**There is no rate limiter.** Deploy behind one. Per-key rate limiting and a cache of unknown prefixes are planned. An unauthenticated request with a well-formed key still costs a Postgres lookup, and a key that is over budget writes one refusal row to the ledger per attempt, so a client in a retry loop turns into ledger volume.

## Disclosure

We follow coordinated disclosure. After a fix is shipped:

- We publish a [GitHub Security Advisory](https://github.com/sakura-sky/tollgate/security/advisories) with a CVE ID where appropriate.
- The release notes link to the advisory.
- Reporters who wish to be credited are named in the advisory.

If you have a deadline you'd like us to work within (e.g. a conference talk), tell us up front and we'll do our best to align.

## Hardening guidance

Deployment hardening guidance (persistence, egress lockdown, least-privilege database roles, and reference architectures) lives in [`docs/OPERATIONS.md`](./docs/OPERATIONS.md).
