<!--
SPDX-License-Identifier: MIT
SPDX-FileCopyrightText: 2026 Andrew Stevens
-->

# Changelog

Notable changes per release.

## Unreleased

Authentication before anything else, a key store that fails as a `503`
rather than a wave of `401`s, and provider errors that no longer reach clients
verbatim.

Upgrading: `serve` now refuses `TOLLGATE_DATABASE__MAX_CONNECTIONS` below 4, an
`ACQUIRE_TIMEOUT` outside 1 ms to 2147483647 ms, and a Postgres connection that
drops startup options (see Changed).

### Security

- Every request to `/v1`, any path under `/v1/`, and any path under `/console/`
  is authenticated before any route, extractor or handler runs. Without a valid
  key the answer is `401` (or `503` if the key store cannot answer), shaped for
  the path's family, and it no longer
  reveals which providers are enabled (the streaming routes returned `503`
  "upstream is not configured" first), the admission mode (`501` on a streaming
  chat request under `exact`), the `anthropic-beta` allowlist, the route table
  (`404` and `405` with `Allow`) or body-size limits (`413`).
- Only the exact key shape Tollgate issues is looked up. Anything else is refused
  without a database query.
- A provider's non-2xx response no longer reaches the client. The buffered
  routes returned the provider's status and body verbatim (on the
  `/v1/{provider}/{path}` route inside its `{"response": ..}` envelope), which
  could name the cloud project, region and resource path, and the provider's
  account state. Every route, buffered or streaming, now answers with a status
  classified by fault, a reason header and a fixed message (see Changed). The
  provider's error type and code are logged, and the first 2 KiB of its body
  only at TRACE on request. The same applies to a pre-flight count refusal,
  whose message `exact` admission used to pass back inside a `400`.
- Error events inside a stream that began with `200` no longer reach the
  client verbatim. The relay used to forward the provider's bytes unchanged;
  it now forwards only events it can classify as safe (parsing CR, LF and CRLF
  line ends and multi-line data per the SSE specification), and ends the
  client's stream with one fixed error event on an error or anything it cannot
  classify. Metering still reads the provider's original stream to its end
  (within the idle and maximum-duration limits), even if the client
  disconnects after that.
- A `200` on a stream route that is not an event stream (or is compressed) is
  a `502` `upstream_error` before the stream starts, charged its reservation.
  A buffered `200` whose body is an error (not a JSON object, a non-null
  top-level `error`, or Anthropic's `"type": "error"`) is a `502` too.

### Added

- `x-tollgate-request-id` on every response on `/v1` and `/console/*`: a random
  id generated per request, never taken from the client, and on every log line
  for an authenticated request (not on `tollgate demo`, which has no gate, nor
  on the `500` for a panic in the gate itself).
- `x-tollgate-reason: upstream_rate_limited` on a `429` when the provider is
  rate limiting the deployment, with `Retry-After` when the provider sent a whole
  number of seconds (clamped to 1 to 60, re-written rather than copied).
- A WARN log line, `upstream refused the request`, for every provider refusal,
  with the request id, the provider's error type and code, and the provider's
  own request id. The provider's body is opt-in at TRACE under
  `tollgate::upstream_body` and never exported over OTLP.

### Fixed

- A key store that cannot answer (Postgres down, the lookup pool exhausted, a
  lookup over its deadline) is a `503` with `x-tollgate-reason: backend_error`,
  not a `401`. A lookup that hit a schema mismatch is the same `503` rather than
  a panic.

### Changed

- Streams: the client receives only `data:` and `event:` lines from the
  provider, re-terminated with LF; provider comments, `id:` and `retry:` lines
  are dropped (Tollgate
  sends its own `:` keepalive, at most every 15 seconds, when it drops a
  provider's). An event name outside the protocol's known set ends the
  client's stream; on `/v1/chat/completions` any event name does, as neither
  OpenAI nor Vertex sends one. The `stream settled` log line gains
  `client_ended`.
- The stream meters' line splitting is linear in the bytes received however
  they are chunked (it rescanned the held line on every chunk), and the Anthropic meter now
  discards the rest of an over-long line, as the OpenAI meter did.
- Provider refusals are classified by fault on every route:
  - `400`, `413` and `422` keep their status, with `bad_request`;
  - `429` stays `429`, with `upstream_rate_limited`;
  - `401`, `403`, `404` and every other non-2xx become `502` with
    `upstream_error`. The streaming routes used to pass the provider's status
    through, so a rotated provider key showed clients a `401` that looked like
    their own key had failed.
  - On `/v1/{provider}/{path}`, a provider error that used to come back as
    `{"response": {"error": ..}, "tollgate": {..}}` with the provider's status is
    now, for example, a `400` with `{"error": "the provider rejected this
    request; ...", "status": 400}` and `x-tollgate-reason: bad_request`.
  - These responses carry no `x-tollgate-cost` or `x-tollgate-overhead-us`; the
    settlement and ledger row are unchanged.
- With OTLP set, one span is exported per authenticated request on `/v1` and
  `/console/*` (the per-request span was previously at DEBUG and filtered out).
- `tollgate_requests_total` gains the `key_store_error` decision. It is not
  exposed by `tollgate demo`.
- `unauthenticated` now also counts unknown paths, wrong methods and
  `/v1/messages/` sub-paths refused for their key, and no longer counts a store
  failure.
- API-key lookups run on their own pool, a quarter of
  `TOLLGATE_DATABASE__MAX_CONNECTIONS` (at most 4). The total is unchanged, so
  the shared pool is smaller: the default 10 becomes 8 shared and 2 for lookups.
  `serve` refuses a value below 4 (admin commands do not).
- `TOLLGATE_DATABASE__ACQUIRE_TIMEOUT` is also the lookup connections'
  server-side `statement_timeout`, and the gate answers `503` at most 250 ms
  after it. `serve` refuses a value outside 1 ms to 2147483647 ms.
- `serve` refuses to start if the lookup connections do not report that
  `statement_timeout`, which happens when a connection pooler drops startup
  options. Connect directly, or pass startup options through.
- `/v1/messages/` sub-paths (`count_tokens`, `batches`) answered `404` to
  anyone; they now need a key like every `/v1` path, and read `x-api-key` like
  `/v1/messages`, so an Anthropic SDK with a valid key still gets the `404`.
- Every `/console/*` refusal from `serve` carries `x-tollgate-reason` (the
  `tollgate demo` console is unchanged): `unauthenticated` on
  the `401`, `backend_error` on every `503` (including the request timeout's,
  which now has a JSON body), `bad_request` on a `404` or `405`.
- The console page reports "Gateway temporarily unavailable." on any refusal
  other than `401`, rather than showing an empty dashboard.
- Store failures are logged at most once a minute, with a count of the ones
  skipped, instead of once per request.
- `KeyStore::lookup` returns `Result<Option<KeyRecord>, KeyStoreError>` and
  `GatewayCore::authenticate` returns `Result<String, AuthError>`. The crate is
  not published, so this only matters to a fork.

## v0.3.0

Streams charged what they measured, Vertex locations that resolve, and budgets
that are enforced as set or reported when they cannot be.

Upgrading requires migration 0011, which can refuse to run and starts enforcing
per-key budgets that were previously ignored. Read the upgrade note under Fixed
first.

### Added

- `admin budget set --allow-unpriced`, to set a model's cap before its price.
- The `tollgate_unmatchable_budgets` gauge on `/metrics` (not exposed by
  `tollgate demo`), counting budgets that cannot match some or all of their
  requests.
- The compose file can run Tollgate itself: `docker compose -f
  compose/docker-compose.yaml --profile app up -d --build`. The service is behind
  the `app` profile, so a plain `up -d` still starts only Postgres and Valkey and
  nothing crash-loops before `.env` exists. It publishes on `127.0.0.1:8080`, like
  the dependencies.
- `TOLLGATE_DATABASE__AUTO_MIGRATE` (default off, dev only): `serve` applies
  pending migrations at boot, after all config checks (billing, pepper, the
  OpenAI-compatible upstream, the Vertex project and location, admission mode,
  Redis URL), so a mistake in any of those stops the process before it touches
  the schema. Migrations do run before the Redis connection, the schema and
  budget checks and binding the port, and against whatever database URL is
  configured. The compose `app` profile sets it.
  Production should keep running `admin migrate` explicitly.
- `TOLLGATE_DEMO_BIND_ALL`: `tollgate demo` honours the IP in
  `TOLLGATE_HTTP__BIND` instead of forcing loopback, which a container needs. It
  prints a warning banner whenever the demo is not on loopback. The bind address
  defaults to `0.0.0.0:8080`, so the flag on its own listens on every interface.
- `scripts/demo.sh` runs the demo in Docker when cargo is missing or
  `USE_DOCKER=1`, publishing on `127.0.0.1` only.

### Changed

- When a `/v1/chat/completions` stream reports usage more than once, it is
  charged the dearest of its reports, each costed whole, rather than the last.
  A stream whose prompt or output count falls from one report to the next is
  treated as self-contradictory and charged the reservation.
- Every stream settled by the relay logs a `stream settled` line with how it
  ended (`done`, `eof`, `client_gone`, `slow_client`, `idle_or_max`,
  `upstream_error`, `drain_expired`, `drain_aborted`), whether the finish, usage
  and end marker were seen, and the decision: at `debug` for a stream that was
  measured, ended at the provider's end marker and was not drained, `info`
  otherwise. Client disconnects were previously logged only at `debug`.
- When Vertex is in use, `serve` refuses a location that is not lowercase
  letters, digits and hyphens, and a project that is neither a project number
  nor a project ID, before connecting to anything. Both are built into the URL,
  and the location into the hostname, of requests that carry Tollgate's Google
  access token. Native Vertex with an empty project or location is now refused
  at boot too, instead of failing on the first request.
- OpenTelemetry moves to 0.33 (`tracing-opentelemetry` 0.34). A
  `TOLLGATE_TELEMETRY__OTLP_ENDPOINT` without a scheme, such as `collector:4317`,
  is rewritten to `https://` by the new exporter, and this build has no TLS, so
  Tollgate now refuses any endpoint that is not `http://host:port` or
  `unix:///path`, with a message saying what to set. The message names only the
  scheme of the refused value, never the value itself, since an endpoint can
  carry credentials or a token. An empty value now disables
  export; before, it fell back to the `OTEL_EXPORTER_OTLP_*` variables and then
  `http://localhost:4317`. Transient export failures are now retried (up to
  three times, with backoff).
- `serve` now validates the billing config, the API-key pepper, the
  OpenAI-compatible upstream settings, the Vertex project and location, the
  admission mode and the Redis URL before it connects to Postgres or Valkey, so
  a config mistake is reported as such rather than behind a connection error.

### Fixed

- A per-key budget set with its key id in any form the uuid parser accepts
  other than lowercase with hyphens (upper case, no hyphens, braces, a
  `urn:uuid:` prefix) was accepted
  and stored as typed, and never enforced: the key ran under the wider budgets
  only. Ids are now stored and loaded in canonical form, and migration 0011
  converts existing rows and adds a constraint so no other form can be stored.
  **Upgrade note:** budgets that were silently not enforced start enforcing,
  against the spend already recorded this period, as soon as an instance loads
  them: an instance already running at its next config reload after 0011 is
  applied, or at restart if `TOLLGATE_RELOAD__INTERVAL` is `0s`. An instance of
  this version cannot start until 0011 is applied. A key over its intended cap
  is refused straight away. To see which budgets this affects before migrating:
  `SELECT id, scope_id, period, limit_micros FROM budgets WHERE scope_type =
  'api_key' AND scope_id !~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$';`.
  The migration refuses, listing the rows, if an id is not a UUID at all or two
  budgets name the same key and period in different forms; fix those and run it
  again. Two rows for the same scope and period that still reach the gateway are
  merged to the stricter one, with a warning.
- `admin budget set` refuses `provider:` and `model:` scopes that could never
  match: a provider this build does not have, or a model with no current price
  (`--allow-unpriced` stages a model's cap before its price). Every budget that
  cannot match some or all of its requests is logged at startup and when the set
  changes on reload, and counted by the new gauge. That includes a budgeted
  model also priced under a second name with no budget at least as hard of its
  own. A Vertex
  budget while `/v1/chat/completions` also reaches Vertex is logged but not
  counted, since no budget can clear it: those requests are scoped as `openai`. The `budget.set` audit row now records the scope as
  stored, with the typed form, `--allow-unpriced` and whether the model was
  priced (null for other scopes) in its metadata. A rejected key id is no longer echoed back.
- Streams on `/v1/chat/completions` that finished but whose client then
  disconnected or stopped reading are now charged their measured usage instead
  of their full reservation. The common case is a client that stops reading as
  soon as it sees `finish_reason`, before `data: [DONE]`: once the finish has
  arrived, Tollgate now keeps reading the upstream for up to 10 seconds after
  it notices the client has stopped, to receive the usage and `[DONE]`. An
  upstream error or timeout after the finish still charges the reservation.
- A stream on `/v1/chat/completions` is now measured only when the upstream
  sent a non-empty `finish_reason` with no output after it, a usage report in or
  after that chunk, and `data: [DONE]`. A connection that closed cleanly without
  them used to be treated as finished, which under-charged a stream cut
  mid-generation on an upstream that reports usage on every chunk. Some rows
  that read `allowed` before may now read `estimated`, and a custom upstream
  that never sends `finish_reason` or `[DONE]` is charged its reservation on
  every stream. Usage is assumed to be cumulative across a stream.
- Vertex requests for the `global`, `us` and `eu` locations now go to the hosts
  Google documents for them: `aiplatform.googleapis.com` for `global` and
  `aiplatform.{us,eu}.rep.googleapis.com` for the multi-regions, instead of a
  `{location}-aiplatform.googleapis.com` host Google does not list. The native
  adapter, the token count and the OpenAI-compatible endpoint all use the same
  host. Thanks to Florian Valéry for the `global` fix.

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
