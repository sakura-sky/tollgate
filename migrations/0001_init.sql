-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Andrew Stevens
--
-- Baseline: api_keys.
-- Tollgate is single-tenant by design: each customer runs the container in
-- their own GCP project; the deployment IS the tenancy boundary. There is no
-- tenants table and no tenant_id columns anywhere.
-- Later migrations add budgets, usage, model_prices, and an audit log.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE api_keys (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Hex HMAC-SHA256 (under a server-side pepper) of the secret portion of the
    -- key. Never stores the plaintext. See src/apikey.rs for why a fast keyed
    -- hash, not argon2id, is correct for high-entropy tokens.
    key_hash        TEXT        NOT NULL,
    -- Public, non-secret prefix used to look up the row before verifying the hash.
    -- e.g. "tgk_1a2b3c4d" - the key tag plus the public handle.
    prefix          TEXT        NOT NULL,
    label           TEXT        NOT NULL,
    -- Optional scope restrictions (model allow-list, route allow-list, etc.).
    scopes          JSONB       NOT NULL DEFAULT '{}'::jsonb,
    revoked_at      TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at    TIMESTAMPTZ
);

CREATE UNIQUE INDEX api_keys_prefix_key ON api_keys (prefix);
CREATE INDEX api_keys_active_idx ON api_keys (created_at) WHERE revoked_at IS NULL;
