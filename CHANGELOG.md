<!--
SPDX-License-Identifier: MIT
SPDX-FileCopyrightText: 2026 Andrew Stevens
-->

# Changelog

Notable changes per release.

## v0.2.3

Telling the caller whose fault a refusal is, and leaving a trace of it.

### Fixed

- A failure of the pre-flight token count under `exact` admission is now reported
  as an upstream error (`502`, `upstream_error`) rather than a backend one
  (`503`, `backend_error`). The count is a round trip to the provider, so the old
  labelling sent an operator to look at Valkey and Postgres while the fault was
  upstream. Buffered and streaming paths agree.
- A count endpoint that REFUSES the request now surfaces as a `400`, not a `502`.
  Both count adapters discarded the HTTP status, so a body the provider rejected
  as malformed was reported as a provider outage, and SDKs retried it three times
  before surfacing what was really a client bug.
- Refusals made before forwarding now reach the ledger, at zero cost, on the
  streaming routes as well as the buffered one. Streaming previously recorded
  nothing for a refusal at all, so an hour of budget denials, unpriced models, or
  a provider's count endpoint being down read as an hour in which no requests
  arrived, and the ledger is the only place a refused request is visible. The
  decision words match the buffered path: `unpriced`, `rejected_budget`, `error`.
  A parse failure is still not recorded on either path, since the request never
  resolved to a model.
- **Provider calls use HTTP/2.** `reqwest` has been built without its default
  features since the first release, to make the TLS backend an explicit choice,
  and that quietly left `http2` out with it. Every outbound provider request in
  every release up to now was HTTP/1.1, so the busiest path in the product had
  no connection multiplexing.
- Vertex `:countTokens` is sent an allowlisted body rather than the caller's
  `generateContent` body unchanged. Verified against live Vertex: the endpoint
  rejects `safetySettings`, `labels`, `toolConfig` and `cachedContent` with a
  400, and an SDK-built body routinely carries `safetySettings`, so `exact`
  admission on Vertex refused every request. It accepts and counts `contents`,
  `systemInstruction`, `tools` and `generationConfig`, which is what is now sent.
- Structured-output requests on Vertex were going to be under-reserved. A
  `responseSchema` lives inside `generationConfig` and is counted as prompt
  material (live: 1 token bare, 51 with a small four-property schema attached),
  so the count has to include it. `system_instruction` in Google's snake-case
  spelling is now read as well as the camelCase form, for the same reason:
  missing it drops the system prompt from the reservation while the request
  still carries it.

### Changed

- **Outbound TLS now trusts the image's CA store rather than a bundle compiled
  into the binary.** This follows from moving to `reqwest` 0.13, whose rustls
  feature uses the platform verifier. Trust rotates when you rebase instead of
  needing a rebuild, and a private CA installed in the image is honoured, so a
  TLS-inspecting egress proxy works where it previously could not. The reverse
  is now also true: an image without CA certificates cannot reach any provider.
  The distroless runtime image ships them.
- **The minimum supported Rust version is 1.88**, up from 1.85. The old floor
  was holding the `redis` client five minor versions back, since 1.2.3 onward
  require 1.88, so every dependabot update to the client that talks to the
  budget store had to be rejected. The client itself is unchanged in this
  release; the bump is what makes updating it possible.
- **A panic fails its request instead of the process.** The release profile no
  longer sets `panic = "abort"`, and a catch-panic layer turns a panic into a
  `500` for the request that caused it. Previously a panic anywhere, including
  inside a dependency and on one request for one key, took the whole gateway
  down. Symbols are kept (`strip = "debuginfo"`) so an abort is diagnosable.
- **The dev compose file binds Postgres and Valkey to loopback.** They were
  published on every interface, which on a laptop on an untrusted network is an
  open database and an open unauthenticated Valkey. If you were reaching either
  from another host, that stops working on purpose.
- **Cloud Build no longer pushes a `:latest` tag.** A mutable tag makes a deploy
  irreproducible. A Cloud Deploy target referencing `:latest` needs repointing
  at the immutable tag.
- **`valkey_shard_count` now refuses any value but 1**, and the Terraform
  provider list no longer requires `google-beta`, which nothing used. An
  existing tfvars setting more than one shard will fail `plan`: budget counter
  keys are not hash-tagged, so a multi-shard Valkey refuses the reserve script
  with `CROSSSLOT` rather than scaling anything.
- The `redis` client is built without its default features, keeping only what
  the Lua reserve and settle paths need.
- `hmac` and `sha2` move to the 0.12/0.10 generation, which is the one `sqlx`
  already pulls in for SCRAM. That removes five duplicate crates and leaves one
  SHA-256 implementation in the binary. Key hashing is unchanged: still
  HMAC-SHA256 under the pepper, so no issued key is affected.

### Added

- **The audit trail is written.** `audit_log` has existed since migration 0002
  with append-only triggers, and nothing wrote to it, so key issuance,
  revocation, budget changes and price changes left no record beyond their
  effect. All four now append a row. The principal is the OS user and host the
  command ran as: enough to correlate a change with a shell history, not a claim
  that anyone was authenticated, and `SECURITY.md` says so rather than implying
  otherwise. No secret is recorded.
- **`/metrics` reports something.** `serve` previously exposed `tollgate_up`
  alone, so anyone who built a dashboard found nothing to put on it. It now
  exports
  `tollgate_requests_total` by decision, `tollgate_cost_micros_total`, and
  `tollgate_budget_limit_micros` per configured budget. A scrape reads process
  memory only: exporting spend would put the observability path on the same
  Postgres and Valkey that fail during the incident you are trying to see.
- CI runs the Valkey enforcement battery against a real server. Those tests cover
  the Lua that actually reserves and settles budgets, and until now were run only
  by hand, so nothing stopped the enforcement path regressing between releases.
  They stay `#[ignore]`d, so a developer without a server is not blocked.
- The advisory job runs on a weekly schedule and denies warning-class findings
  (unmaintained, unsound, yanked). `.cargo/audit.toml` mirrors those settings so
  a local `cargo audit` gives the same answer CI gives.
- **A Terraform module applied as shipped now has a way to enable a provider.**
  Every provider defaults to disabled, so a first `apply` produced a gateway
  that served health, metrics and the console and refused every proxied request.
  The new `provider_env` map sets `TOLLGATE_*` variables on the Cloud Run
  container, and refuses any secret-shaped name, because values passed that way
  are stored in Terraform state in plaintext.

## v0.2.2

Long-context pricing, and a budget counter that survives losing its cache.

There is no v0.2.1. The work was cut as one release rather than shipped in
pieces.

Upgrading from v0.2.0 requires migration 0010.

### Added

- **Long-context tiering, per model.** Several current models bill the whole
  request at a higher rate once the prompt crosses a size threshold, commonly 2x
  on input and 1.5x on output above 200k tokens. A price is one rate per class
  and cannot express that, so before this release every long request on such a
  model was under-charged by the tier multiple. Migration 0010 puts the threshold
  and separate input and output multiples on the price row, resolved field by
  field against a deployment default. Set them with `admin price set
  --long-context-threshold --long-context-input-permille
  --long-context-output-permille`. Off until set, because a model may not tier at
  all and defaulting to an uplift would invent a charge.
- `admin price set` prints the tier the model will actually run with, marking
  fields inherited from the deployment default.
- A soft budget now logs a WARN when it is exceeded, so `--hard-stop false`
  produces a cap that reports rather than one that is silently inert.

### Fixed

- **A lost Valkey counter no longer resets a budget.** The reserve script used
  `INCRBY`, which recreates a missing key at the reserve amount, so a flush, a
  failover to a cold replica, or an eviction silently restarted every budget's
  period until someone restarted the gateway. Reserve now refuses on an absent
  counter, rebuilds it from the ledger, and retries once; if the ledger cannot be
  reached the request is refused rather than admitted unenforced. Settle skips a
  counter that vanished instead of resurrecting it holding one request's delta.
- **`--hard-stop` could not be set to false.** A bare `bool` in the CLI derive is
  a presence flag, so `--hard-stop false` was an error and absence gave true.
  Soft budgets existed in the schema and in the enforcement path and could not be
  created.
- A reservation on an upstream whose cache convention is unverified now sizes the
  prompt at double, matching what the billing already assumes there. Without it,
  such a request could settle at the long-context multiple against a reservation
  taken at 1x.
- A deployment-wide long-context multiple above 1.0x with a threshold of 0 is
  refused at boot. It re-rated nothing and suppressed the under-charge warning
  too, so it looked configured and did neither.
- `admin price set --long-context-threshold 0`, the documented per-model disable,
  was refused on any model that had ever been given a multiple.

### Changed

- `--hard-stop` now takes a value. A script passing it as a bare flag will break;
  pass `--hard-stop true` or drop it, since budgets are hard by default.
- `x-tollgate-reason` now uses one vocabulary on every route. The native
  Anthropic path used to emit the SDK's error kind, so a budget refusal on
  `/v1/messages` reported `permission_error` while the same refusal on
  `/v1/chat/completions` reported `budget_exceeded`, and an alert keyed on the
  latter missed every Claude refusal. The response body is unchanged: it still
  carries Anthropic's vocabulary so SDK exception classes resolve.
- A price book loads whole or not at all. One model resolving to an invalid tier
  is fatal at boot, and on a reload keeps the previous book rather than swapping
  in a partial one.
- The Cloud Run request timeout in the Terraform module goes from 60s to 960s. It
  predated streaming, so every stream was being killed by the platform at one
  minute and charged its full reservation. The Valkey instance now sets
  `maxmemory-policy noeviction`.
- Every path that creates a budget counter now goes through one script, which
  refuses to overwrite a counter live traffic just created and stamps the
  creation with a random generation. A reservation records the generation it saw
  and a settlement declines if it has moved, so a settlement in flight across a
  cache loss cannot subtract a reservation the rebuilt counter never held.
- A prompt above the long-context threshold on a model with no tier configured
  logs a possible under-charge on the streaming path as well as the buffered one.
- A price row whose own multiples are stranded against a zero threshold is warned
  about on every config reload.
- An Anthropic stream that reports an error before generating anything is charged
  its reserved prompt leg rather than its full worst-case reservation. Restricted
  to Anthropic, which has a pre-generation signal; on the OpenAI path a client
  that disconnects before the terminal usage chunk is indistinguishable from one
  that never started, so every abnormal end there still charges the reservation.

### Upgrading

Complete the rollout before relying on the counter repair. A v0.2.0 instance
still recreates a missing counter at the reserve amount and settles without
checking a generation, so during a mixed rollout the old behaviour is live on the
old instances. Restart once the rollout is finished.

### Documentation

A full pass for accuracy. The README no longer claims `exact` admission is a
strict cap without qualification, the routes and their accepted auth headers are
documented, the price example sets cache rates rather than silently falling back
to a 10x over-charge, and several statements that were never true have been
corrected or removed. `docs/OPERATIONS.md` gains the table-ownership step its
least-privilege recipe was missing, without which partition maintenance fails
silently and the ledger stops recording weeks later. `SECURITY.md` gains a threat
model.

## v0.2.0

Streaming, an OpenAI-compatible endpoint, a native Anthropic route, and
prompt-cache pricing. See the release notes for detail.

## v0.1.2

Metering corrections. Vertex thinking and tool-use tokens were dropped from the
output leg, and Anthropic cached requests were under-counted by roughly 99%. Both
were live under-charges.

## v0.1.1 and earlier

The budget-enforcement core, provider adapters, console, config hot-reload and
ledger retention. See the Git history.
