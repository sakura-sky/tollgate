# Operations

Operational notes for running Tollgate in production: how spend state survives
restarts, what the network posture is, and the reference architectures for
locking egress down further.

## Budget durability and persistence

Tollgate keeps two copies of every charge.

The **ledger in Postgres** is the system of record. Every admitted request writes
an append-only usage row with its exact cost once the provider response is
settled. Postgres is the source of truth for spend and for billing.

The **counters in Valkey** are a fast cache used on the hot path. Reserving and
settling budget against Valkey with an atomic script is what lets the gateway
hard-stop before overspend in well under a millisecond, without a database round
trip per request.

Because the ledger is authoritative, the cache is disposable, and it is rebuilt
from the ledger in two places.

At startup the gateway runs `reconcile_counters`, which sums the ledger over each
budget's current period window and raises every counter to that figure. It only
ever raises: a ledger write can be lost after its counter was already
incremented, and setting the counter back down to the ledger sum would permit
overspend for the rest of the period.

At request time the reserve script refuses to act on a counter that is not there,
and the gateway rebuilds it from the ledger before retrying. An absent counter is
not a counter at zero, it is a counter whose history Valkey has lost, and an
`INCRBY` cannot tell those apart. Without this a flush, a failover to a cold
replica, or an eviction would silently restart every budget's period. If the
ledger cannot be reached to rebuild it, the request is refused with a `503`
rather than admitted unenforced.

Three consequences worth planning for. The first request of a new budget period
costs one ledger query per applicable budget, because that period's counter does
not exist yet. A Postgres outage during a period rollover refuses traffic rather
than admitting it, which is the correct direction but is a hard dependency.

And every request that was in flight at the moment of the loss settles against a
counter that has since been rebuilt, so its settlement is declined. The cost
still reaches the ledger, but the counter does not pick it up until the next
startup reconcile raises it. For that period the counter under-states spend by
the actual cost of whatever was in flight. That is bounded and it is the safe
direction relative to the alternative, which was losing the entire period, but it
is a reason to restart the gateway after a cache incident rather than leaving it.

**Run Valkey as a single shard.** The budget counter keys carry no hash tag, so
a request that touches several budgets at once, which is the normal case (a key
budget, a provider budget, a model budget, a global one), hands several keys to
one Lua script. In a sharded Valkey those keys hash to different slots and the
script is refused with `CROSSSLOT`, which surfaces as a `503` on every request
that spans more than one budget. The Terraform module pins `valkey_shard_count`
to 1 and validates it: any other value fails `plan`. Raising it would not scale
the gateway, it would break enforcement, so lifting the restriction means giving
the counter keys a common hash tag in the code first, which also moves every
existing counter to a new key name.

Run Valkey with `maxmemory-policy noeviction` and do not share the instance with
other workloads. Counters carry a 40-day expiry, so under any `volatile-*` policy
a live budget counter is an eviction candidate. Eviction is now repaired rather
than silently ignored, but repairing it costs a ledger query per affected budget,
and under memory pressure that can be every budget on every request.

Valkey is still configured for durability so a routine restart does not force a
full reconcile storm and a momentary cold window:

- **Local (compose):** `valkey-server --appendonly yes` with a named volume, so
  the append-only file survives `docker compose restart`.
- **GCP (Memorystore for Valkey):** `persistence_config` is set to `AOF` with
  `append_fsync = EVERY_SEC`, trading at most one second of cached counter
  writes on a hard node failure. The ledger still backs the reconcile, so even a
  total cache loss is recoverable, just slower on the next boot.

If you deliberately want a cold cache (for example after changing budget periods),
restart the gateway and let reconcile rebuild the counters from the ledger.

## API-key lookups

Every request under `/v1` and `/console/` is authenticated before anything else,
with one Postgres lookup by key prefix. `serve` runs those lookups on their own
pool, carved out of `TOLLGATE_DATABASE__MAX_CONNECTIONS`: a quarter of it, at
most 4, with the rest for the shared pool (ledger writes, budget rebuilds, config
reload, maintenance, the console). The default of 10 is 2 for lookups and 8
shared. `serve` refuses to start below 4. Admin commands are unaffected and use
the whole figure.

Sizing: instances times `TOLLGATE_DATABASE__MAX_CONNECTIONS` must fit under the
server's connection limit, which is low on small Cloud SQL tiers. This change
leaves the total per instance as it was. With 4 to 7 connections the lookup pool
has one connection, so one slow lookup holds up every other lookup for up to
`TOLLGATE_DATABASE__ACQUIRE_TIMEOUT`.

**Timeout.** Every lookup connection is opened with
`statement_timeout` = `TOLLGATE_DATABASE__ACQUIRE_TIMEOUT`, as a startup option,
so the server ends a stalled lookup. Dropping a query from the client does not
cancel it. At boot, before migrating, `serve` reads the setting back from
`pg_settings` on a lookup connection and refuses to start if it does not match.
That catches a connection pooler that drops startup options, which some
transaction-mode poolers do. Every lookup connection opened later is checked the
same way; one that does not match is discarded (sqlx logs `error returned from
after_connect` for each attempt) and the lookup gets a `503`. The shipped
Terraform connects to Cloud SQL directly; if you put a pooler in front, it must
pass startup options through.

`statement_timeout` does not cover a connection whose network path has gone
silent, and Tollgate sets no TCP keepalive on its connections (the shared pool
has the same exposure). The gate's own deadline, `TOLLGATE_DATABASE__ACQUIRE_TIMEOUT`
plus 250 ms, still bounds what the caller waits.

**When the store cannot answer.** The caller gets `503` with
`x-tollgate-reason: backend_error` in the route's error format, never `401`.
Each one is counted on
`tollgate_requests_total{decision="key_store_error"}`. SDKs retry a `503` (they do
not retry a `401`), so expect retry load during a Postgres outage. The failure is
logged at ERROR as `api key lookup failed; answering 503 until the key store
recovers`, with a fixed `kind` (`pool_timed_out`, `io`, `database`, `deadline`
and so on) and the SQLSTATE where there is one. At most one such line is written
per minute, carrying the count of lines skipped since the previous one, because
unauthenticated callers can trigger it. The metric is the authoritative count.
The lookup pool's own statement and slow-acquire logging is switched off for the
same reason. sqlx can still log a warning when it fails to test a returned
connection; set the log filter for `sqlx` to `error` if that line is noise.

**Saturation.** A flood of well-formed but unknown keys costs one lookup each and
can fill the lookup pool, and legitimate callers then get that `503` until it
drains. It cannot take the shared pool's connections, so the ledger and config
reload keep working. Put a rate limiter in front; per-key rate limiting and a
cache of unknown prefixes are planned.

After a Postgres restart or failover, the lookup pool tests each idle connection
before use and reconnects, so callers normally see no error. An idle connection
whose network path went silent instead stalls the next lookup until the acquire
timeout. A lookup the gate abandons at its deadline has its connection closed
(sqlx gives up on the close after 5 seconds) rather than returned to the pool,
because returning it would first test it with no time limit, and on a silent
network path that holds a lookup slot until the operating system gives up on the
connection.

## Provider refusals and request ids

Every response on `/v1` and `/console/*` carries `x-tollgate-request-id`. Ask a
client reporting a failure for it: the log lines written while an authenticated
request is handled carry it, including the stream relay and settlement, in the
`request_id` field of the line or of its `request` span. That span also records
the method (cut to 16 bytes) and the path without its query (cut to 256 bytes),
both the caller's own text.
Unauthenticated requests open no span, so their lines (and the key-store
failure line, which is rate-limited and aggregate) have none. A panic inside
the authentication gate itself is answered without the header.

When a provider answers with a non-2xx, the client gets a classified refusal
(see the README's reason table) and the log gets one WARN line,
`upstream refused the request`, with:

- `request_id`, the id the client was given, on the line itself, so a `warn`
  log filter (which drops the span) still finds it;
- `provider`, `model`, `status`, and `fault` (`request`, `rate_limited` or
  `provider`);
- `provider_error_type` and `provider_error_code`, read from the provider's
  error body and kept only when they are short, plain identifiers. They are
  empty when the body is not JSON, or, on a stream, when it is over 2 KiB or
  took more than 2 seconds to read; a stream's line is written by a background
  task up to 2 seconds after the response;
- `provider_request_id`, the provider's own id for the request, to quote to the
  provider's support.

The provider's message is not on that line, because it can echo part of the
request. Up to 2 KiB of the provider's body is logged at TRACE under the target
`tollgate::upstream_body`, which the default filter leaves off. To see it, add
`tollgate::upstream_body=trace` to `TOLLGATE_TELEMETRY__LOG_FILTER`. Filters match
targets by prefix, so `tollgate=trace` or a bare `trace` switches it on too. That
line is never exported over OTLP. It covers refusals from the moment it is on;
earlier ones cannot be recovered.

A burst of `bad_request` across many keys can be a deployment fault rather than
callers' bodies. Anthropic, for example, documents a `400` for a workspace spend
limit, and a `429` with no Retry-After and the error code
`enforced_spend_limit_reached` for its tier spend cap (check the provider's
current documentation). Read the WARN lines'
`provider_error_type` and `provider_error_code` before blaming clients.

Inside a stream that began with `200`, an error event (or anything the relay
cannot classify as safe) ends the client's stream with one fixed error event,
and one WARN line is logged per stream, `upstream stream error; ending the
client's stream and metering on`, with `request_id`, `provider`, `model`,
`reason` (`error_event`, `unclassifiable`, `unknown_event_name` or
`over_limit`), `event_name`, and the provider's cleaned error type and code.
The event's joined data so far (up to 2 KiB; for `over_limit`, without the
over-long line) is at TRACE under `tollgate::upstream_body`. The
`stream settled` line for that stream carries the same reason as
`client_ended`. On `/v1/messages`, `unknown_event_name` means Anthropic sent an
event name outside the eight it documents, usually a new event type; streams
using it end early until Tollgate is updated, which is the fail-closed choice.
On `/v1/chat/completions` any event name gives that reason, as neither OpenAI
nor Vertex names its events.

A `200` on a stream route that is not an event stream (or is compressed) is
logged at WARN, `upstream answered a stream request without a plain event
stream; charging the reservation`, with the reason and the cleaned media type.

The relay drops a provider's SSE comments (keepalives) and sends its own fixed
`:` comment instead, at most every 15 seconds.

A provider `401`, `403`, `404`, or any other non-2xx outside `400`, `413`, `422`
and `429`, reaches the client as `502`, so each one
also produces tower-http's ERROR `response failed` line, as Tollgate's own
`502`s always have.

With `TOLLGATE_TELEMETRY__OTLP_ENDPOINT` set, one Tollgate `request` span is now
exported per authenticated request on `/v1` and `/console/*`. Before, the per-request span
was at DEBUG in tower-http and filtered out by default. Unauthenticated
requests and the public paths (`/healthz`, `/readyz`, `/metrics`, `/console`)
open no span.

## Request rules: media sources, Gemini thinking, service tier

These settings change what a request may ask a provider to do. Each is
checked at startup, before the database is touched, and a refusal of a request
under these rules is fixed text that never repeats what the client sent. A body
that is not valid JSON is the exception: its `400` carries the parser's message,
which gives the line and column of the syntax error.

```toml
# tollgate.toml

[providers.media]
# Buckets a native Vertex request may name in fileData.fileUri (gs://bucket/...).
# Empty (the default) refuses every gs:// URI.
gcs_buckets = ["my-shared-media"]

[providers.vertex]
# Accept client `labels` on native Vertex requests. They land in your billing
# export. Default false: refused.
# allow_client_labels = true

# One row per (upstream, model). upstream: "vertex" (native generateContent) or
# "vertex-openai" (/v1/chat/completions with providers.openai.upstream = "vertex").
# model: the id as that route forwards it, matched without case; on
# vertex-openai a bare id means the google/ one.
[[providers.models]]
upstream = "vertex"
model = "gemini-2.5-pro"
thinking = "additive"
max_thinking_tokens = 24576

[[providers.models]]
upstream = "vertex-openai"
model = "google/gemini-2.5-pro"
thinking = "additive"
max_thinking_tokens = 24576

[providers.openai]
# Unset: on when upstream = "custom" and the base URL host is api.openai.com.
# pin_service_tier = true
```

The same settings as environment variables, in figment's inline form:

```sh
TOLLGATE_PROVIDERS__MEDIA__GCS_BUCKETS='[my-shared-media]'
TOLLGATE_PROVIDERS__MODELS='[{upstream="vertex",model="gemini-2.5-pro",thinking="additive",max_thinking_tokens=24576}]'
TOLLGATE_PROVIDERS__OPENAI__PIN_SERVICE_TIER=true
TOLLGATE_PROVIDERS__VERTEX__ALLOW_CLIENT_LABELS=true
```

A bucket name made only of digits must be quoted inside the list (`'["123"]'`),
or it parses as a number and startup fails.

**`gs://` allowlist.** Google's docs require a `gs://` object to be public or in
the project sending the request, so Vertex reads it on the operator's behalf,
not the caller's; which Google identity performs the read is unverified (see
[`SECURITY.md`](../SECURITY.md)). List only buckets
every key holder may read. A URI is accepted only when the text between `gs://`
and the next `/` equals a listed name exactly; `%`, `..`, `\`, an uppercase or
empty bucket, and any other scheme are refused. Entries
must be plain lowercase bucket names (no `gs://`, no path), or startup fails. A
refused request is a `403` with `x-tollgate-reason: forbidden_source`. So is an
`https://` URI on any `googleapis.com` host or `storage.cloud.google.com`
(compared without case or a trailing dot), which would reach Cloud Storage over
https and so a bucket off the allowlist, and any URI carrying credentials
(`user@` or `user:pass@`). Every other `https://` URI is a `400` until media
support lands: remote media has no size bound, and what a URL serves can change
between the pre-flight count and the generation. It is not
written to the ledger, like any other request refused before it resolves to a
priced model. The one exception is a refusal raised at the `exact` pre-flight
count step (the count re-runs these checks), which is recorded as `error` at
zero cost. Under `fast` admission any `fileData` is still refused with a
`400`, because its size cannot be estimated from the body; use `exact`.

**Gemini thinking rule.** Every Gemini model (`gemini-` ids, and
`google/gemini-` on `vertex-openai`) on the Vertex routes needs a row stating
`thinking`, or its requests are refused with a `400` that names this setting.
Built in, as `within_output_cap`: `gemini-3.8-flash` on `vertex`, and
`google/gemini-3.8-flash` on `vertex-openai`. Model ids are matched without
ASCII case. On `vertex-openai` a row is matched against the id Tollgate
forwards: a bare id gets the `google/` prefix first, so `gemini-x` and
`google/gemini-x` are one model and one row covers both; two rows naming the
same model that way refuse startup. A row for the same upstream and model
replaces the built-in one.

- `within_output_cap`: thinking tokens count against `maxOutputTokens` (or
  `max_tokens`), so the reservation is the cap. Set it only after checking: send
  the model a request with a small cap (say 64) and a high thinking setting; if
  it stops with `MAX_TOKENS` and the reported thoughts plus candidates stay
  within the cap, thinking is inside it.
- `additive`: thinking comes on top of the cap. The output leg reserved is the
  cap plus `max_thinking_tokens`, or plus the request's own explicit budget
  (`thinkingConfig.thinkingBudget`, or `thinking_config.thinking_budget` under
  `google` / `extra_body.google`) when that is larger. The outbound cap is not
  changed. `max_thinking_tokens` is your assertion, not a bound Google enforces:
  it calls a thinking budget a soft limit, so a request can exceed it and settle
  above its reservation. Settlement always charges the real usage. `additive`
  without `max_thinking_tokens` refuses startup, and so does
  `max_thinking_tokens` on a `within_output_cap` row.

  Set `max_thinking_tokens` to cover the highest thinking effort a client can
  ask for without a number: `reasoning_effort: "high"` on
  `/v1/chat/completions`, `thinkingLevel: "HIGH"` (or `thinking_level`), and the
  dynamic budget `thinkingBudget: -1`. Each of those is reserved at exactly
  `max_thinking_tokens`, so a bound below what the model spends at its highest
  setting lets those requests settle above their reservation.

Non-Gemini models on these routes, and every model on a custom OpenAI-compatible
upstream, have no thinking rule.

**Service tier.** On `/v1/chat/completions`, a `service_tier` that is present
and not exactly the string `"default"` is refused, including `"auto"`, `null`,
numbers and arrays. On OpenAI's own API an
omitted tier means `auto`, so with `pin_service_tier` on, every outbound request
carries `service_tier: "default"`. Leave it off for an OpenAI-compatible server
that rejects the field. The Vertex upstream never gets it.

**Native Vertex allowlist.** The README's Limitations list the fields a native
`:generateContent` body may carry. Anything else is a `400`, so an SDK that adds
a new field will see refusals until it is reviewed and added. Keys in Google's
own objects must be strict lowerCamelCase or strict snake_case; other
spellings, and two keys in one object that differ only in case or underscores
(`fileDATA` beside `fileData`), are a `400`. `generationConfig.maxOutputTokens`
must be a positive integer: `0` is a `400`, since Google may read it as unset.
A `fileData` that is not an object is a `403`. `labels` are refused unless
`providers.vertex.allow_client_labels` is true, since they land in your
billing export; allowed, they are limited to 64 entries of at most 63
characters.

**Content parts on the Vertex OpenAI-compatible route.** A message `content`
array may carry only `{"type": "text", "text": ...}` parts until media support
lands; any other part type, or another key in a part, is a `400`.

**Key spelling on the Vertex OpenAI-compatible route.** A top-level key with an
uppercase ASCII letter is a `400`, and so is one inside `google`,
`extra_body`, `extra_body.google` or `thinking_config`. Vertex may read
camelCase aliases (`maxTokens`, `extraBody`, `candidateCount`,
`serviceTier`) that would bypass the snake_case checks and the reservation, so
clients must send the snake_case fields. In the same objects, a key that
matches a field Tollgate checks once case and underscores are ignored, but is
not spelled exactly as that field (`maxtokens`, `max__tokens`, `max_tokens_`,
`service__tier`, `cachedcontent`), is a `400`, and so are two keys in one
object that differ only in case or underscores. The checked fields are
`model`, `messages`, `max_tokens`, `max_completion_tokens`, `n`, `stream`,
`stream_options`, `service_tier`, `modalities`, `audio`, `prediction`,
`web_search_options`, `tools`, `tool_choice`, `reasoning_effort`, `google`,
`extra_body`, `thinking_config`, `thinking_budget`, `include_thoughts`,
`thinking_level`, `cached_content` and `candidate_count`. This is the set
protected against alias spellings, not a list of refused fields. Of these,
`cached_content` and `candidate_count` are refused whenever present, at the top
level and inside `google` or `extra_body.google`; `audio`, `prediction` and
`web_search_options` unless null; `modalities` unless text only; `n` unless an integer of at most 1;
`service_tier` other than `"default"`; and `tools` of any type but `function`.
The rest are read, pinned or rewritten. A custom upstream is unaffected.

## Ledger retention

`usage_events` is append-only: a trigger rejects `UPDATE` and `DELETE` for every
role, including the table owner, so spend history cannot be quietly rewritten.
That immutability is deliberate, but it means the ledger cannot be trimmed with a
`DELETE` purge. Retention is therefore done by time partitioning.

The table is `RANGE`-partitioned by `started_at` into monthly partitions. The
gateway runs a maintenance routine at startup and every few hours that:

- creates the current month's partition and the two after it, so inserts
  always have a home; and
- drops any partition whose whole month is older than
  `TOLLGATE_RETENTION__WINDOW` (default 90 days). Dropping a partition is DDL, so
  it bypasses the append-only row trigger while the rows that remain stay
  immutable.

Set `TOLLGATE_RETENTION__WINDOW=0s` to keep everything (create-ahead only, no
drops). Granularity is monthly, so at least the current month is always retained
and the effective retention rounds up to a whole month. The budget counters are
unaffected: they are driven by the current period's rows, which are always well
within any sane retention window, and Postgres partition pruning on `started_at`
keeps the spend-sum queries fast as the ledger grows.

For very high volume, reduce the window or switch the partition granularity to
weekly or daily in `tollgate_usage_maintain` (migration `0006`); the same
create-ahead and drop-old logic applies.

## Network and egress posture (baseline)

The default Terraform deploys a private posture, not an open one:

- Cloud Run reaches Cloud SQL and Valkey over internal ranges through the
  Serverless VPC Access connector (`egress = PRIVATE_RANGES_ONLY`). Provider
  traffic still leaves over Google's default public path; the application
  restricts destinations, the network does not.
- The provider adapters only ever connect to fixed provider hostnames. A caller
  supplies a provider name and a path suffix, both allowlisted; they never supply
  a destination host. There is no user-controlled URL in the forward path.
- The outbound HTTP client refuses redirects (`redirect::Policy::none()`), so a
  `3xx` from an upstream cannot bounce the request, and its credentials, to a
  different host.
- Outbound TLS is validated against the CA store in the running image, not
  against a bundle compiled into the binary. Trust rotates when you rebase, and
  a private CA added to the image is honoured, which is what makes the Secure
  Web Proxy architecture below workable. The flip side is that an image without
  CA certificates cannot reach any provider; the distroless runtime image ships
  them, so treat a base-image change as a change to trust.
- Secrets (database URL, API-key pepper) are read from Secret Manager by
  reference and scoped to the runtime service account per-secret, not granted
  project-wide.

One gap in that posture, stated plainly because the module ships it this way:
the Valkey connection has neither AUTH nor in-transit encryption. Budget counters
travel unauthenticated and in cleartext, and the only thing keeping the instance
off other networks is Private Service Connect. Anyone who reaches that address
can read and write the counters that decide whether spending continues. Enable Memorystore AUTH and carry the credential in `TOLLGATE_REDIS__URL` if an
attacker inside the VPC is in your threat model. In-transit encryption is not
available in this build: the Valkey client is compiled without a TLS feature, so
a `rediss://` URL is refused at startup rather than silently downgraded. Adding
it is open work. The ledger
bounds the consequence: counters are rebuilt from Postgres at startup and
whenever one goes missing, so tampering suppresses enforcement until the next
rebuild rather than corrupting the record of what was spent.

The practical consequence is that Tollgate's server-side request forgery surface
is small by construction: the set of hosts it will connect to is fixed in code
and configuration. The reference architecture below closes the residual gap,
which is that the *network* itself still permits arbitrary outbound TLS.

### Reference architecture A: baseline (shipped default)

```
                 Serverless VPC Access connector
   ┌────────────┐   (private ranges only)      ┌──────────────────┐
   │ Cloud Run  │ ───────────────────────────► │ Cloud SQL (PSA)  │
   │  Tollgate  │ ───────────────────────────► │ Valkey (PSC)     │
   │            │                               └──────────────────┘
   │            │        public egress (TLS)     ┌──────────────────┐
   │            │ ─────────────────────────────► │ api.anthropic.com│
   │            │ ─────────────────────────────► │ *.googleapis.com │
   └────────────┘                                │ metadata server  │
                                                 └──────────────────┘
```

Private dependencies are pinned to the VPC. Provider egress leaves over Google's
default path; the *application* restricts destinations, the network does not.

### Reference architecture B: pinned egress (Secure Web Proxy)

For deployments that need the network itself to enforce the destination
allowlist (regulated environments, strict data-exfiltration controls), route all
egress through a Secure Web Proxy (SWP) and deny everything not on the list.

```
   ┌────────────┐   all egress via connector    ┌──────────────────────┐
   │ Cloud Run  │ ────────────────────────────► │ Secure Web Proxy     │
   │  Tollgate  │  (egress = ALL_TRAFFIC)        │  FQDN allowlist:     │
   │            │                                │   api.anthropic.com  │
   └────────────┘                                │   *.googleapis.com   │
        │ private ranges                         │   metadata server    │
        ▼                                        └───────────┬──────────┘
   ┌──────────────────┐                                      │ allow
   │ Cloud SQL (PSA)  │                          deny all ◄──┘
   │ Valkey (PSC)     │                          other hosts
   └──────────────────┘
```

To deploy this lockdown:

1. Set the Cloud Run `vpc_access` egress to `ALL_TRAFFIC` so every outbound
   flow, not just private ranges, traverses the connector.
2. Provision a Cloud NAT on the connector subnet (public providers need a source
   for their return path) and a Secure Web Proxy gateway
   (`google_network_services_gateway` of type `SECURE_WEB_GATEWAY`) with a
   `google_network_security_gateway_security_policy`.
3. Add policy rules that **allow** TLS to `api.anthropic.com`,
   `*.googleapis.com`, and the metadata server `169.254.169.254`, and a
   lowest-priority rule that **denies** everything else. If you have configured
   an OpenAI-compatible upstream with
   `TOLLGATE_PROVIDERS__OPENAI__UPSTREAM=custom`, add that host too, or every
   request through it fails at the proxy rather than at the gateway.
4. Point the workload at the proxy and confirm a call to any other host is
   refused while provider calls still succeed.

This is deliberately left out of the default module: it is billable, always-on
infrastructure, and the allowlist must be validated against your own project and
the exact provider endpoints you call. Treat architecture A as the secure
default and architecture B as the hardened profile you enable when the threat
model calls for it.

Note that a pure VPC firewall cannot substitute for the proxy here: firewall
rules match on IP, and Anthropic's API is served from a large rotating set of
CDN addresses, so IP allowlisting can pin the Google APIs (which have a stable
restricted VIP) but not the Anthropic path. Domain-level pinning needs SWP.

## Database privileges (defence in depth)

The `usage_events` and `audit_log` tables are append-only, and that guarantee is
enforced inside the database by triggers that reject `UPDATE` and `DELETE`.

The trigger stops accidental and application-level mutation. It is not proof
against the table owner, who can `ALTER TABLE ... DISABLE TRIGGER`, drop the
trigger, or drop a whole partition in one statement. Retention relies on exactly
that last ability. So the real control is the role split below, plus database
audit logging; the trigger is what makes tampering deliberate rather than
possible by accident.

As a second layer, run the application against a non-owner Postgres role rather
than the role that owns the schema, and revoke the mutating grants it does not
need:

```sql
-- Run migrations as the owner, then create a least-privilege app role.
CREATE ROLE tollgate_app LOGIN PASSWORD '...';
GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA public TO tollgate_app;
-- No UPDATE on api_keys: the gateway only reads them, and revocation is a CLI
-- operation run as the owner. Granting it here would hand the application the
-- one table an attacker with this role would most want to write.
REVOKE UPDATE, DELETE ON usage_events, audit_log FROM tollgate_app;

-- REQUIRED, and easy to miss. Partition maintenance runs on the gateway's
-- schedule but must execute with the OWNER's rights, because creating and
-- dropping partitions of usage_events requires owning the parent and the
-- partitions. Marking the function SECURITY DEFINER does that without handing
-- the application role any ownership of its own.
ALTER FUNCTION tollgate_usage_maintain(interval) SECURITY DEFINER;
ALTER FUNCTION tollgate_usage_maintain(interval) SET search_path = public;
REVOKE ALL ON FUNCTION tollgate_usage_maintain(interval) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION tollgate_usage_maintain(interval) TO tollgate_app;
```

The application only ever inserts into the ledger, so it never needs `UPDATE` or
`DELETE` on it. With the trigger, the revoked grant, and no ownership, rewriting
history requires compromising the owner role, not just the application. On Cloud
SQL, create the app role separately from the migration role and point
`TOLLGATE_DATABASE__URL` at the app role.

**Do not skip the `ALTER FUNCTION` lines, and do not reach for
`ALTER TABLE ... OWNER` instead.** `tollgate_usage_maintain` ships as a plain
`plpgsql` function and runs as whoever calls it. Transferring the table to the
app role looks like the fix and is not: on Postgres 15 and later, which includes
the Postgres 16 this project ships, an ordinary role no longer holds `CREATE` on
the `public` schema, so partition creation still fails; `ALTER TABLE ... OWNER`
does not recurse to existing partitions, so retention's `DROP TABLE` fails on
every partition the migration made; and it hands the application the ownership
that lets it disable the append-only trigger, which is the thing this section
exists to prevent.

Get it wrong and the failure is quiet. Every maintenance sweep logs a `WARN`
while the gateway serves normally. Migration 0006 creates the current month's
partition and the two after it, so nothing breaks for roughly three months. After
that every ledger insert fails with "no partition found", and ledger writes are
best effort: the failure is logged and the request still succeeds. Budgets go on
binding from Valkey while the billing record silently stops, the console goes
stale, and retention never runs. Worse, a later cache loss rebuilds counters from
a ledger missing everything since the sweeps stopped.

Alert on two log messages. `usage ledger maintenance failed` is the early
warning, weeks before anything visibly breaks. `failed to write usage event` is
the ledger already losing rows. Neither affects the response a client gets, which
is exactly why they need an alert rather than a dashboard.

Alert on `tollgate_requests_total{decision="key_store_error"}` increasing. It
means the key store (Postgres, or the key-lookup pool) could not answer and
callers are getting `503`, not that anyone presented a bad key. See API-key
lookups above.

Alert too on `tollgate_unmatchable_budgets` being above zero. Each budget it
counts fails to cover some or all of the requests it appears to. For a non-UUID
key id or a model priced under a second name, those requests run under the other
budgets that match them (or are denied if none does); for a disabled provider or an unpriced model they are refused
anyway. With `upstream = vertex`, `/v1/chat/completions` prices, budgets and
records a bare model id under the `google/` id it forwards, so an `openai` model
budget with a bare id matches nothing and is counted. A Vertex budget while `/v1/chat/completions` also reaches Vertex is
logged but not counted, because no budget can clear it. The warning "a configured budget cannot match some
or all of its requests" names each budget and the reason, and is logged at
startup and whenever the set changes on reload. A budget staged on purpose for a
disabled provider or an unpriced model (`admin budget set --allow-unpriced`)
keeps the gauge raised until it can match.

For the same reason, an `openai` price row with a bare model id matches no
request while `/v1/chat/completions` reaches Vertex. The warning "openai price
rows with a bare model id match no request" names those models, at startup and
whenever the set changes on reload. Re-price each as `google/<id>`
(`admin price set --provider openai --model google/<id>`) and remove the bare
row.
