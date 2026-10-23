# syntax=docker/dockerfile:1.7
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens

# ---------- Stage 1: planner ----------
# cargo-chef computes a recipe so the dependency build can be cached
# independently of the application source.
#
# Pinned by tag, not by digest. A tag moves, so this build is only as
# reproducible as whatever `rust:1.88-slim-bookworm` points at today; a digest
# pin would fix that but has to be re-pinned by hand for every patch release of
# the base image. Tag for now, deliberately, and the lockfile plus --locked
# carry the reproducibility that actually affects the binary.
#
# The tag must track `rust-version` in Cargo.toml. Building the MSRV crate on
# an older toolchain than the manifest demands fails late and confusingly.
FROM rust:1.88-slim-bookworm AS chef
RUN cargo install cargo-chef --locked --version 0.1.71
WORKDIR /app

FROM chef AS planner
# `Cargo.lock`, not `Cargo.lock*`. The glob makes a missing lockfile a silent
# no-op, and the build then resolves fresh dependencies instead of failing,
# which is the opposite of what --locked is for.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo chef prepare --recipe-path recipe.json

# ---------- Stage 2: builder ----------
FROM chef AS builder
ARG SQLX_OFFLINE=true
ENV SQLX_OFFLINE=${SQLX_OFFLINE}

# No OpenSSL headers here. TLS is rustls over ring, which builds without a
# system OpenSSL, and Cargo.lock contains no `openssl`, `openssl-sys`,
# `native-tls` or `openssl-probe`. Installing libssl-dev anyway added packages
# to the builder that nothing linked against, and it invited a future
# dependency to quietly pick the OpenSSL path because it happened to be
# available. If a build ever fails for want of these, check what pulled in a
# native-tls backend before adding them back.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=planner /app/recipe.json recipe.json
# --locked here as well as on the real build below. The cook step is what
# actually resolves and compiles the dependency graph, so without it the cached
# layer can be built from versions the lockfile never named.
RUN cargo chef cook --release --locked --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY LICENSE ./LICENSE

RUN cargo build --release --locked --bin tollgate

# ---------- Stage 3: runtime ----------
# Distroless cc gives us libc + ca-certificates without a shell or package
# manager. Non-root by default; read-only filesystem friendly.
#
# Tag-pinned, with the same trade-off as the builder base above: `:nonroot`
# moves as distroless rebuilds, so this is not a reproducible pin. Moving to a
# digest would fix the image exactly, at the cost of hand-updating it to pick
# up base-image CVE fixes.
FROM gcr.io/distroless/cc-debian12:nonroot

# Version and revision come from the build, because the image cannot know them
# on its own. Defaults are deliberately not-a-real-version: an image labelled
# 0.0.0-dev is obviously unlabelled, whereas a stale hardcoded number reads as
# true and misidentifies what is running. CI should pass the real values, e.g.
# --build-arg VERSION=$(git describe --tags) --build-arg REVISION=$GITHUB_SHA.
ARG VERSION=0.0.0-dev
ARG REVISION=unknown

# OCI labels so a pulled image can say what it is without consulting a build
# log. Registries, scanners and `docker inspect` all read these, and an image
# in Artifact Registry otherwise carries nothing that ties it back to a commit.
LABEL org.opencontainers.image.title="tollgate" \
      org.opencontainers.image.description="AI gateway and spend-control proxy for LLM providers" \
      org.opencontainers.image.source="https://github.com/sakura-sky/tollgate" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}"

COPY --from=builder /app/target/release/tollgate /usr/local/bin/tollgate
COPY --from=builder /app/migrations /opt/tollgate/migrations
COPY --from=builder /app/LICENSE /opt/tollgate/LICENSE

USER nonroot:nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/tollgate"]
CMD ["serve"]
