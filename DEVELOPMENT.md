# Development

## Toolchain

- Rust 1.88 (pinned in `rust-toolchain.toml`, and the MSRV in `Cargo.toml`)
  - It was 1.85. That pinned `redis` five minor versions back, because redis
    1.2.3 and later require 1.88, so dependabot kept opening updates CI had to
    reject.
- Docker 24+ for local Postgres/Valkey and image builds
- Terraform 1.7+ for infrastructure
- `gcloud` CLI for GCP interaction

## Local stack

`compose/docker-compose.yaml` publishes everything on loopback, on ports chosen
to stay clear of a Postgres, Redis or gateway already running on the machine:

| Service | Host address | Override | Inside the stack |
| --- | --- | --- | --- |
| Postgres | `127.0.0.1:23432` | `TOLLGATE_DEV_PG_PORT` | `postgres:5432` |
| Valkey | `127.0.0.1:23379` | `TOLLGATE_DEV_VALKEY_PORT` | `valkey:6379` |
| Tollgate (`app` profile) | `127.0.0.1:23080` | `TOLLGATE_DEV_HTTP_PORT` | `tollgate:8080` |

Inside its container the gateway binds `0.0.0.0:8080`, so the port can be
published.

`.env.example` points at the first two. Compose reads the overrides from the
shell first, then from `compose/.env`. A repo-root `.env` counts only once it is
exported into the shell (as the README quick start does with
`set -a; source .env; set +a`) or passed with `--env-file .env`, which then
replaces `compose/.env` for these settings; `compose/.env` still reaches the
`app` container through `env_file`. Do not use `--project-directory .` with
this file: it renames the Compose project, so it starts new, empty volumes, and
it moves the relative `build` and `env_file` paths. If you move a port, change
the matching URL in your `.env` as well.

Commands run on the host (`cargo run -- serve`, `cargo run -- admin ...`) do
not read `.env` themselves, so export it first with
`set -a; source .env; set +a`. Without it, and with neither URL already set in
the environment, each comes from the `--config` file (also `TOLLGATE_CONFIG`)
or else a `tollgate.toml` in the working directory, if that file sets it, and
otherwise from the built-in `127.0.0.1:5432` and `127.0.0.1:6379`, which no
longer match the compose stack.

```bash
TOLLGATE_DEV_PG_PORT=33432 docker compose -f compose/docker-compose.yaml up -d
```

## Day-to-day loop

```bash
# Format
cargo fmt --all

# Lint (treats warnings as errors, matches CI)
cargo clippy --all-targets --locked -- -D warnings

# Tests (--all-targets, because that is what CI and cloudbuild run)
cargo test --locked --all-targets

# Run the gateway
set -a; source .env; set +a
cargo run --bin tollgate -- serve
```

Two gates CI runs that the loop above does not. Both are cheap and both catch
things the ordinary suite cannot see.

The Valkey enforcement battery runs against a real server, because the ordinary
suite uses an in-memory budget backend and never executes the Lua that actually
reserves and settles budgets. The tests are `#[ignore]`d so nobody is blocked
without a server:

```bash
docker compose -f compose/docker-compose.yaml up -d valkey
TOLLGATE_TEST_REDIS_URL=redis://127.0.0.1:23379 \
  cargo test --lib redis_live -- --ignored --test-threads=1
```

CI fails if fewer than 12 of them run, so a battery that silently stops being
collected is caught rather than reported as green. Deleting one of these tests
therefore means lowering that floor in the workflow, which is a decision to make
on purpose rather than a side effect.

```bash
# Advisories against the dependencies we already have
cargo audit
```

`.cargo/audit.toml` already matches CI's settings, including its ignores and
its `--deny warnings`, so a local run gives the same answer CI gives. A gate
that disagrees with the one in CI is a gate people learn to ignore.

The ignores fall into two groups. RUSTSEC-2023-0071 (`rsa`) and
RUSTSEC-2026-0459 (`libsqlite3-sys`) each hold only while the crate it names
stays out of the build graph, and CI checks that on every run. The same check
by hand exits 0 when it passes. It exits non-zero if `cargo tree` fails, if
`sqlx-postgres` is missing from the listing (so the listing cannot be trusted),
or if one of the crates is in the graph, which it then prints:

```bash
graph=$(cargo tree --locked --workspace --all-features --target all \
  -e normal,build,dev --prefix none --format '{p}') &&
  grep -q '^sqlx-postgres ' <<< "$graph" &&
  ! grep -E '^(sqlx-mysql|rsa|sqlx-sqlite|libsqlite3-sys) ' <<< "$graph"
```

RUSTSEC-2026-0543 (`chrono`) and RUSTSEC-2026-0542 (`humantime`) are
informational "unmaintained" notices, not vulnerabilities, against crates
Tollgate does compile in. They are ignored until Tollgate moves from chrono to
`time` and replaces humantime-serde, and that change removes both ignores. CI
checks the reverse for these two: it fails if either crate leaves the build
graph while the check still names it. By hand, against the same listing:

```bash
grep -q '^chrono ' <<< "$graph" && grep -q '^humantime ' <<< "$graph"
```

## Database migrations

Migrations live in `migrations/` and are embedded into the binary by `sqlx::migrate!`. To add one:

```bash
# Filenames are scanned in lexical order; use a 4-digit prefix.
$EDITOR migrations/0012_your_change.sql

# Apply against your local Postgres
set -a; source .env; set +a
cargo run --bin tollgate -- admin migrate
```

Queries in this repo are checked at runtime, not compile time: there is no `sqlx::query!` or `query_as!` in `src/`, so `cargo build` needs no `DATABASE_URL` set and there is no `sqlx-data.json` or `.sqlx/` cache to regenerate. The one compile-time sqlx macro, `sqlx::migrate!("./migrations")` in `src/db.rs`, reads the migrations directory at build time rather than a live database. The Dockerfile still sets `SQLX_OFFLINE=true` defensively, though nothing in this repo currently requires it.

## Running with OTLP

If you have a local OpenTelemetry Collector listening on the default gRPC port:

```bash
set -a; source .env; set +a
TOLLGATE_TELEMETRY__OTLP_ENDPOINT=http://127.0.0.1:4317 \
  cargo run --bin tollgate -- serve
```

Without that env var set, traces stay in-process and only the JSON-formatted log layer is active.

## Container build

```bash
docker build -t tollgate:dev .
docker run --rm -p 127.0.0.1:8080:8080 \
  -e TOLLGATE_DATABASE__URL=postgres://... \
  -e TOLLGATE_REDIS__URL=redis://... \
  -e TOLLGATE_SECURITY__API_KEY_PEPPER=... \
  tollgate:dev
```

The container will not start without `TOLLGATE_SECURITY__API_KEY_PEPPER` set to a fixed secret of at least 16 bytes that is not the `.env.example` placeholder.

## Conventions

- Error handling: `anyhow` for application errors, `thiserror` for typed errors that cross API boundaries (`AppError` in `src/error.rs`).
- Logging: structured `tracing` events; never `println!` from library code.
- Config: every new tunable lands in `src/config.rs` with a sensible default.
- IDs: UUID v4. Timestamps: `TIMESTAMPTZ` in Postgres, `chrono::DateTime<Utc>` in Rust.
- Money: store as integer micros (the deployment's configured currency × 1,000,000, see `TOLLGATE_BILLING__CURRENCY`) to avoid floating-point drift. The money path itself is currency-agnostic; the currency code is a display label only.

## What lives outside this repo

- Production Terraform state (per-customer GCS bucket).
- Model prices are operator-supplied, written with `admin price set` and stored in the `model_prices` table; Tollgate ships no price list.
- Customer API keys - issued by `admin key issue`, never checked in.

## Licence headers

This repo follows the [REUSE](https://reuse.software/) convention. Every source file carries:

```
SPDX-License-Identifier: MIT
SPDX-FileCopyrightText: 2026 Andrew Stevens
```

When adding a new file, copy that header (with the appropriate comment prefix for the file type) so the licensing remains machine-checkable.
