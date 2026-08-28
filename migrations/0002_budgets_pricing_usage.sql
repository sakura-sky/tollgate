-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Andrew Stevens
--
-- Token-budget core: budgets (global AND per-key), model pricing, usage
-- events, and an append-only audit log.
--
-- Money is stored as integer MICROS - 1e-6 of the deployment's configured
-- currency unit - everywhere, to avoid floating-point drift. The currency is a
-- single deployment-level config value (ISO 4217); amounts are not tagged
-- per-row. A price of 1.25 per 1M tokens is 1_250_000 micros.

-- ---------------------------------------------------------------------------
-- budgets - one row per spend ceiling. scope_type unifies the global cap and
-- per-key caps (and leaves room for per-provider / per-model later), so the
-- hot-path enforcer treats them identically: resolve the budgets that apply to
-- a request, check each one's counter.
-- ---------------------------------------------------------------------------
CREATE TABLE budgets (
    id                UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    -- 'global'   -> scope_id IS NULL (one deployment-wide cap)
    -- 'api_key'  -> scope_id = api_keys.id::text (per-key cap)
    -- 'provider' -> scope_id = 'vertex' | 'anthropic' | ...
    -- 'model'    -> scope_id = 'provider:model'
    scope_type        TEXT        NOT NULL
                        CHECK (scope_type IN ('global', 'api_key', 'provider', 'model')),
    scope_id          TEXT,
    period            TEXT        NOT NULL DEFAULT 'monthly'
                        CHECK (period IN ('daily', 'weekly', 'monthly')),
    limit_micros  BIGINT      NOT NULL CHECK (limit_micros >= 0),
    -- true  -> breach REJECTS the request (hard cap)
    -- false -> breach only emits a notification (soft cap), request proceeds
    hard_stop         BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- 'global' must have NULL scope_id; every other scope must name one.
    CONSTRAINT budgets_scope_id_shape CHECK (
        (scope_type = 'global'  AND scope_id IS NULL) OR
        (scope_type <> 'global' AND scope_id IS NOT NULL)
    ),
    -- Shape checks so a typo can't create a valid-looking budget that silently
    -- never matches (fail-open). A 'provider' scope_id is a bare slug; a 'model'
    -- scope_id is 'provider:model'. NOTE: the 'api_key' case cannot be a real FK
    -- here (polymorphic scope_id), so the app layer MUST verify the api_keys row
    -- exists before inserting an api_key-scoped budget.
    CONSTRAINT budgets_provider_scope_shape CHECK (
        scope_type <> 'provider' OR scope_id ~ '^[a-z][a-z0-9_-]*$'
    ),
    CONSTRAINT budgets_model_scope_shape CHECK (
        scope_type <> 'model' OR scope_id ~ '^[a-z][a-z0-9_-]*:.+$'
    )
);

-- At most one budget per (scope, period). Global is a single row per period.
CREATE UNIQUE INDEX budgets_global_period_key
    ON budgets (scope_type, period)
    WHERE scope_type = 'global';
CREATE UNIQUE INDEX budgets_scoped_period_key
    ON budgets (scope_type, scope_id, period)
    WHERE scope_type <> 'global';

-- ---------------------------------------------------------------------------
-- model_prices - the tokens -> cost table. Cost is keyed on (provider, model);
-- effective_from/effective_to preserve historical pricing so past usage can be
-- costed at the rate that applied when it happened (cost replays, invoices).
-- ---------------------------------------------------------------------------
CREATE TABLE model_prices (
    provider              TEXT        NOT NULL,
    model                 TEXT        NOT NULL,
    -- Currency micros per 1,000,000 tokens (configured currency). 1.25/1M -> 1_250_000.
    input_per_1m_micros   BIGINT      NOT NULL CHECK (input_per_1m_micros >= 0),
    output_per_1m_micros  BIGINT      NOT NULL CHECK (output_per_1m_micros >= 0),
    effective_from        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    effective_to          TIMESTAMPTZ,          -- NULL = current
    source                TEXT,                 -- provenance note / pricing-page URL
    PRIMARY KEY (provider, model, effective_from),
    -- An effective window must be non-empty when closed.
    CONSTRAINT model_prices_effective_window CHECK (
        effective_to IS NULL OR effective_to > effective_from
    )
);

-- Exactly ONE current price per (provider, model). UNIQUE (not just an index)
-- so a re-price MUST close the old row (set effective_to) before inserting the
-- new one - otherwise two "current" rows would make cost lookups nondeterministic.
CREATE UNIQUE INDEX model_prices_current_idx
    ON model_prices (provider, model)
    WHERE effective_to IS NULL;

-- ---------------------------------------------------------------------------
-- usage_events - one row per proxied (or rejected) request. System of record
-- for spend; Redis counters are the hot-path cache and are reconciled here.
-- ---------------------------------------------------------------------------
CREATE TABLE usage_events (
    id                UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    api_key_id        UUID        NOT NULL REFERENCES api_keys (id),
    provider          TEXT        NOT NULL,
    model             TEXT        NOT NULL,
    input_tokens      BIGINT      NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens     BIGINT      NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cost_micros   BIGINT      NOT NULL DEFAULT 0 CHECK (cost_micros >= 0),
    decision          TEXT        NOT NULL
                        CHECK (decision IN ('allowed', 'rejected_budget', 'rejected_policy', 'error')),
    request_id        TEXT,
    latency_ms        INTEGER,
    started_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX usage_events_key_time_idx ON usage_events (api_key_id, started_at);
CREATE INDEX usage_events_time_idx     ON usage_events (started_at);
-- Support provider- and model-scoped budget spend sums over a period without
-- scanning every row (budgets.scope_type may be 'provider' or 'model').
CREATE INDEX usage_events_provider_time_idx ON usage_events (provider, started_at);
CREATE INDEX usage_events_model_time_idx    ON usage_events (provider, model, started_at);

-- ---------------------------------------------------------------------------
-- audit_log - append-only trail of privileged actions (key.issue, budget.set,
-- request decisions).
--
-- Append-only is enforced HERE by a trigger that rejects UPDATE/DELETE, which
-- holds even against the table owner (unlike REVOKE, which the owner bypasses).
-- Operators SHOULD additionally run the app as a non-owner role and REVOKE
-- UPDATE, DELETE (see docs/OPERATIONS.md) as defence in depth.
-- ---------------------------------------------------------------------------
CREATE TABLE audit_log (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    principal     TEXT        NOT NULL,   -- api key id, admin user, or 'system'
    action        TEXT        NOT NULL,   -- 'key.issue', 'budget.set', 'request.allowed', ...
    resource      TEXT,                   -- key id / budget id / request id
    metadata      JSONB       NOT NULL DEFAULT '{}'::jsonb,
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX audit_log_time_idx ON audit_log (occurred_at);

-- ---------------------------------------------------------------------------
-- Triggers
-- ---------------------------------------------------------------------------

-- Keep budgets.updated_at honest (DEFAULT NOW() only fires on INSERT).
CREATE OR REPLACE FUNCTION tollgate_set_updated_at() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at := NOW();
    RETURN NEW;
END;
$$;

CREATE TRIGGER budgets_set_updated_at
    BEFORE UPDATE ON budgets
    FOR EACH ROW EXECUTE FUNCTION tollgate_set_updated_at();

-- Make append-only real: reject any UPDATE/DELETE on the billing ledger and the
-- audit trail. Enforced for ALL roles including the table owner.
CREATE OR REPLACE FUNCTION tollgate_reject_mutation() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'append-only table %: % is not permitted', TG_TABLE_NAME, TG_OP;
END;
$$;

CREATE TRIGGER usage_events_append_only
    BEFORE UPDATE OR DELETE ON usage_events
    FOR EACH ROW EXECUTE FUNCTION tollgate_reject_mutation();

CREATE TRIGGER audit_log_append_only
    BEFORE UPDATE OR DELETE ON audit_log
    FOR EACH ROW EXECUTE FUNCTION tollgate_reject_mutation();
