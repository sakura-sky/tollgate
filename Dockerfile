# syntax=docker/dockerfile:1.7
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens

# ---------- Stage 1: planner ----------
# cargo-chef computes a recipe so the dependency build can be cached
# independently of the application source.
FROM rust:1.85-slim-bookworm AS chef
RUN cargo install cargo-chef --locked --version 0.1.71
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY migrations ./migrations
RUN cargo chef prepare --recipe-path recipe.json

# ---------- Stage 2: builder ----------
FROM chef AS builder
ARG SQLX_OFFLINE=true
ENV SQLX_OFFLINE=${SQLX_OFFLINE}

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY migrations ./migrations
COPY LICENSE ./LICENSE

RUN cargo build --release --locked --bin tollgate

# ---------- Stage 3: runtime ----------
# Distroless cc gives us libc + ca-certificates without a shell or package
# manager. Non-root by default; read-only filesystem friendly.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /app/target/release/tollgate /usr/local/bin/tollgate
COPY --from=builder /app/migrations /opt/tollgate/migrations
COPY --from=builder /app/LICENSE /opt/tollgate/LICENSE

USER nonroot:nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/tollgate"]
CMD ["serve"]
