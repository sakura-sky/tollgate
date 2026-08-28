# Development

## Toolchain

- Rust 1.85 (pinned in `rust-toolchain.toml`)
- Docker 24+ for local Postgres/Redis and image builds
- Terraform 1.7+ for infrastructure
- `gcloud` CLI for GCP interaction

## Day-to-day loop

```bash
# Format
cargo fmt --all

# Lint (treats warnings as errors, matches CI)
cargo clippy --all-targets --locked -- -D warnings

# Tests
cargo test --locked

# Run the gateway
cargo run --bin tollgate -- serve
```

## Database migrations

Migrations live in `migrations/` and are embedded into the binary by `sqlx::migrate!`. To add one:

```bash
# Filenames are scanned in lexical order; use a 4-digit prefix.
$EDITOR migrations/0002_budgets.sql

# Apply against your local Postgres
cargo run --bin tollgate -- admin migrate
```

`sqlx`'s compile-time query checks (Phase 1+) require `DATABASE_URL` to be set when running `cargo build`, **or** a checked-in `sqlx-data.json` produced by `cargo sqlx prepare`. The Dockerfile sets `SQLX_OFFLINE=true` so production builds use the prepared cache.

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
  tollgate:dev
```

## Conventions

- Error handling: `anyhow` for application errors, `thiserror` for typed errors that cross API boundaries (`AppError` in `src/error.rs`).
- Logging: structured `tracing` events; never `println!` from library code.
- Config: every new tunable lands in `src/config.rs` with a sensible default.
- IDs: UUID v4. Timestamps: `TIMESTAMPTZ` in Postgres, `chrono::DateTime<Utc>` in Rust.
- Money: store as integer micros (USD × 1,000,000) to avoid floating-point drift.

## What lives outside this repo

- Production Terraform state (per-customer GCS bucket).
- Gemini/Vertex pricing manifest (Phase 1) - fetched at runtime.
- Customer API keys - issued by `admin key issue`, never checked in.

## Licence headers

This repo follows the [REUSE](https://reuse.software/) convention. Every source file carries:

```
SPDX-License-Identifier: MIT
SPDX-FileCopyrightText: 2026 Andrew Stevens
```

When adding a new file, copy that header (with the appropriate comment prefix for the file type) so the licensing remains machine-checkable.
