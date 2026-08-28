# Tollgate

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/brand/tollgate-logo-dark.svg">
  <img alt="Tollgate" src="assets/brand/tollgate-logo-light.svg" width="300">
</picture>

[![CI](https://github.com/sakura-sky/tollgate/actions/workflows/ci.yml/badge.svg)](https://github.com/sakura-sky/tollgate/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)
[![REUSE compliant](https://img.shields.io/badge/REUSE-compliant-success.svg)](https://reuse.software/)

Tollgate is an open-source AI gateway and spend-control proxy. It sits between an organisation's applications and their LLM providers, enforcing per-key and global token budgets and recording spend before requests reach the model.

This is an early foundation: a runnable axum skeleton with health probes, structured logging, an OTLP-capable telemetry pipeline, a `serve`/`admin` CLI, and database migrations.

It draws on the proxy and cost-control patterns popularised by [LiteLLM](https://github.com/BerriAI/litellm), implemented in Rust under MIT.

## Status

Early development. Not production-ready.

## Demo (zero infrastructure)

No Postgres, Redis, or cloud credentials required. Boot an in-memory Tollgate
with a built-in mock LLM provider:

```bash
cargo run -- demo
```

It prints a demo API key and two budgets (a small per-key cap and a global
backstop), then proxies `/v1/mock/...` to the mock. Send requests and watch the
per-key budget meter by token cost and **hard-stop before overspend**:

```bash
curl -s localhost:8080/v1/mock/generate \
  -H "x-tollgate-key: <printed-key>" \
  -H 'content-type: application/json' \
  -d '{"model":"demo","prompt":"hello","max_output_tokens":1000}'
```

The first few requests return `200` with a cost breakdown; once the budget is
exhausted the gateway returns `429` **before the request reaches the provider**.
Inspect spend with the admin endpoints (which require the key):
`curl -s -H "x-tollgate-key: <key>" localhost:8080/admin/budgets` and
`/admin/usage`. The demo binds to loopback only. Or run the whole narrated
sequence at once:

```bash
./scripts/demo.sh
```

### Console (read-only web UI)

The demo also serves a self-contained web console. After `cargo run -- demo`
starts, open the printed URL:

```
http://localhost:8080/console
```

It shows the per-key and global budget meters, live spend, the tokens-to-cost
breakdown, and recent usage, refreshing every second. Click **Send test request**
(or **Auto-send**) and watch the per-key meter climb green to amber to red and
the hard-stop banner appear when the budget is exhausted, while the global
backstop barely moves. In the demo the console is seeded with the demo key so it
connects on load; when the gateway serves it in production the viewer supplies
their own key. The console is deliberately observe-only: it reads the same
key-authenticated JSON endpoints you would call with curl.

### Monitoring

The gateway exposes standard operational endpoints for your SRE stack:

- `GET /healthz` - liveness ping
- `GET /readyz` - readiness
- `GET /metrics` - Prometheus text format. Currently exposes `tollgate_up`; per-decision request counters and budget spend/limit gauges are on the roadmap. Postgres is the system of record for spend in the meantime

Traces export via OTLP when `TOLLGATE_TELEMETRY__OTLP_ENDPOINT` is set.

## Quick start (local)

Requires Rust 1.85, Docker, and Docker Compose.

```bash
# 1. Bring up Postgres + Valkey
docker compose -f compose/docker-compose.yaml up -d

# 2. Configure
cp .env.example .env
set -a; source .env; set +a

# 3. Apply migrations
cargo run --bin tollgate -- admin migrate

# 4. Run the gateway
cargo run --bin tollgate -- serve

# 5. Smoke test
curl -s http://localhost:8080/healthz | jq
curl -s http://localhost:8080/readyz  | jq
```

## Managing keys and budgets

Keys and budgets are managed with the `tollgate admin` CLI against your Postgres.
From a source checkout the binary is not on your `PATH`, so either prefix with
`cargo run --` or install it once with `cargo install --path .`. The admin
commands need Postgres running, `TOLLGATE_DATABASE__URL` and a fixed
`TOLLGATE_SECURITY__API_KEY_PEPPER` (at least 16 bytes) set, and migrations
applied:

```bash
docker compose -f compose/docker-compose.yaml up -d
cp .env.example .env            # set TOLLGATE_SECURITY__API_KEY_PEPPER to a real secret
set -a; source .env; set +a
cargo run -- admin migrate
```

Issue a key (the plaintext is shown once and never stored; the command also
prints the key's id, which you need for a per-key budget):

```bash
cargo run -- admin key issue --label "team-alpha"
```

Set a global backstop and a per-key cap (amounts are in your configured currency;
budgets hard-stop by default):

```bash
cargo run -- admin budget set --scope global --period monthly --limit 500
cargo run -- admin budget set --scope api_key:<id> --period monthly --limit 25
```

Scopes are `global`, `api_key:<uuid>`, `provider:<name>`, or
`model:<provider:model>`; periods are `daily`, `weekly`, or `monthly`. Revoke a
key with `admin key revoke --key <key-or-prefix>`. Set model prices (per 1,000,000
tokens) so spend can be costed:

```bash
cargo run -- admin price set --provider anthropic --model claude-3-5-sonnet \
  --input-per-1m 3 --output-per-1m 15
```

In a running deployment the gateway also serves the read-only web console at
`/console`, along with the key-authenticated JSON it reads: `GET /console/budgets`
(current-period spend and limit per budget) and `GET /console/usage` (recent
ledger rows). Open `/console`, paste an API key, and you get the live budget
meters and usage that the demo shows. These endpoints are observe-only; there are
no write endpoints in the open-source core.

Authorization is deliberately flat: any valid key may view the deployment's
budgets and usage, because Tollgate is single-tenant per deployment (one trust
boundary). If you need per-tenant scoping, roles, or SSO on these views, that is
the commercial console's job, not the open-source core.

Every proxied request records the gateway's own added latency (the admission
path only, excluding the upstream provider call). The console shows the median
and p95, and each response carries an `x-tollgate-overhead-us` header, so you can
see exactly how little overhead Tollgate adds.

Budget and price changes apply within the reload interval
(`TOLLGATE_RELOAD__INTERVAL`, default 15s) without restarting the gateway; set it
to `0s` to read config only at startup. A changed limit applies to the existing
spend counter; a brand-new budget starts counting when it is first picked up.

Web and API based management of keys and budgets, with roles, SSO, approval
workflows, and audit, is part of the commercial console rather than this
open-source core, which is single-operator by design.

## Configuration

Configuration is layered: built-in defaults → optional `tollgate.toml` → environment variables prefixed `TOLLGATE_`. Nested fields use double underscores (e.g. `TOLLGATE_HTTP__PORT=8080`). See [`src/config.rs`](./src/config.rs) for the full schema.

## Deployment (Cloud Run)

The Terraform module under [`infra/terraform/`](./infra/terraform/) provisions the full stack: Cloud SQL Postgres, Memorystore for Valkey (over Private Service Connect), Artifact Registry, a Cloud Run v2 service, and a runtime service account with least-privilege roles for Vertex AI / Gemini, Cloud SQL, Cloud Logging, Trace, and Monitoring. The database URL and API-key pepper are held in Secret Manager and injected by reference, not as plaintext env values.

See [`docs/OPERATIONS.md`](./docs/OPERATIONS.md) for persistence, egress hardening, and reference architectures.

```bash
cd infra/terraform
cp example.tfvars terraform.tfvars   # then edit
terraform init
terraform apply
```

`region` is a required variable with no default. Pick the region closest to your customers and to the Vertex models you intend to call.

## CI

Every push and pull request runs [`.github/workflows/ci.yml`](./.github/workflows/ci.yml): `cargo fmt --check`, `cargo clippy -D warnings`, and `cargo test` at the 1.85 MSRV, plus a REUSE licence-compliance check. No cloud credentials are needed.

Separately, [`cloudbuild.yaml`](./cloudbuild.yaml) runs the same fmt/clippy/test gates and then builds the container image and pushes it to Artifact Registry. Trigger substitutions: `_REGION`, `_AR_REPO`.

## Layout

```
src/
  main.rs         - binary entrypoint
  lib.rs          - module surface
  cli.rs          - clap CLI dispatcher
  config.rs       - layered config (figment)
  telemetry.rs    - tracing + optional OTLP
  app.rs          - axum app, state, graceful shutdown
  db.rs           - Postgres pool + migration runner
  error.rs        - HTTP error type
  pricing.rs      - token → cost engine (integer micros, currency-agnostic)
  apikey.rs       - API-key generation, hashing (HMAC-SHA256), verification
  budget.rs       - budget resolution + reserve-then-settle enforcement
  gateway.rs      - shared request flow (demo and production) + admission modes
  provider.rs     - provider trait + built-in mock
  providers.rs    - Anthropic and Vertex/Gemini pass-through adapters
  backends.rs     - Postgres key/usage stores + Valkey budget backend
  console.rs      - read-only web console (serves assets/console.html)
  demo.rs         - zero-infra demo mode
  routes/
    health.rs     - /healthz, /readyz
assets/           - console.html and brand assets
migrations/       - sqlx migrations
infra/terraform/  - GCP infrastructure module
compose/          - local dev dependencies
docs/             - operations runbook and reference architectures
scripts/demo.sh   - narrated end-to-end demo runner
```

See [`DEVELOPMENT.md`](./DEVELOPMENT.md) for the dev loop.

## Contributing

Contributions are welcome. Please read [`CONTRIBUTING.md`](./CONTRIBUTING.md) before opening a PR, and note the [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md). The repo follows the [REUSE](https://reuse.software) convention for licence headers - every source file carries an SPDX header.

## Security

To report a vulnerability, please follow [`SECURITY.md`](./SECURITY.md). Do not open public issues for security problems.

## Licence

[MIT](./LICENSE) © 2026 Andrew Stevens and Tollgate contributors.
