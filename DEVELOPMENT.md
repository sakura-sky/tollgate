# Development

## Toolchain

- Rust 1.88 (pinned in `rust-toolchain.toml`, and the MSRV in `Cargo.toml`)
  - It was 1.85. That pinned `redis` five minor versions back, because redis
    1.2.3 and later require 1.88, so dependabot kept opening updates CI had to
    reject.
- Docker 24+ for local Postgres/Valkey and image builds
- Terraform 1.7+ for infrastructure
- `gcloud` CLI for GCP interaction

## Day-to-day loop

```bash
# Format
cargo fmt --all

# Lint (treats warnings as errors, matches CI)
cargo clippy --all-targets --locked -- -D warnings

# Tests (--all-targets, because that is what CI and cloudbuild run)
cargo test --locked --all-targets

# Run the gateway
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
TOLLGATE_TEST_REDIS_URL=redis://127.0.0.1:6379 \
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

`.cargo/audit.toml` already matches CI's settings, including its ignore and its
`--deny warnings`, so a local run gives the same answer CI gives. A gate that
disagrees with the one in CI is a gate people learn to ignore.

## Database migrations

Migrations live in `migrations/` and are embedded into the binary by `sqlx::migrate!`. To add one:

```bash
# Filenames are scanned in lexical order; use a 4-digit prefix.
$EDITOR migrations/0011_your_change.sql

# Apply against your local Postgres
cargo run --bin tollgate -- admin migrate
```

Queries in this repo are checked at runtime, not compile time: there is no `sqlx::query!` or `query_as!` in `src/`, so `cargo build` needs no `DATABASE_URL` set and there is no `sqlx-data.json` or `.sqlx/` cache to regenerate. The one compile-time sqlx macro, `sqlx::migrate!("./migrations")` in `src/db.rs`, reads the migrations directory at build time rather than a live database. The Dockerfile still sets `SQLX_OFFLINE=true` defensively, though nothing in this repo currently requires it.

## Running with OTLP

If you have a local OpenTelemetry Collector listening on the default gRPC port:

```bash
TOLLGATE_TELEMETRY__OTLP_ENDPOINT=http://127.0.0.1:4317 \
  cargo run --bin tollgate -- serve
```

Without that env var set, traces stay in-process and only the JSON-formatted log layer is active.

## Container build

```bash
docker build -t tollgate:dev .
docker run --rm -p 8080:8080 \
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
