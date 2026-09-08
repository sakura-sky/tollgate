-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Andrew Stevens
--
-- Cache-aware pricing: per-class rates for prompt-cache tokens, and the token
-- counts to go with them in the ledger.
--
-- Background. Providers report prompt-side tokens in several classes and charge
-- a different rate for each. A cache READ typically costs about a tenth of the
-- base input rate; a cache WRITE costs MORE than it (1.25x for a short TTL, 2x
-- for a long one on Anthropic). Until now Tollgate had one input rate and one
-- output rate, so v0.1.2 counted every prompt-side class at the base input rate:
-- correct in direction but up to ~10x over on cache-heavy reads.

-- ---------------------------------------------------------------------------
-- Rates. NULLABLE on purpose.
--
-- DEFAULT 0 would be a fail-OPEN default: every price row that exists today
-- would start pricing cache reads and writes at FREE, with no operator action,
-- and an Anthropic long-TTL cache write really costs 2x the input rate. That is
-- a silent under-charge, which is the one direction this product may never err
-- in. NULL instead means "the operator has not priced this class", and the
-- application substitutes a deliberately conservative multiple of the base input
-- rate (see pricing.rs). Over-charging is recoverable; under-charging is not.
-- ---------------------------------------------------------------------------
ALTER TABLE model_prices
    ADD COLUMN cache_read_per_1m_micros  BIGINT
        CHECK (cache_read_per_1m_micros  >= 0),
    ADD COLUMN cache_write_per_1m_micros BIGINT
        CHECK (cache_write_per_1m_micros >= 0);

COMMENT ON COLUMN model_prices.cache_read_per_1m_micros IS
    'Currency micros per 1M cache-READ tokens. NULL = unpriced; the application '
    'substitutes a conservative multiple of input_per_1m_micros and warns.';
COMMENT ON COLUMN model_prices.cache_write_per_1m_micros IS
    'Currency micros per 1M cache-WRITE tokens. NULL = unpriced; the application '
    'substitutes a conservative multiple of input_per_1m_micros and warns. Note '
    'providers may charge more than the input rate to write a cache entry.';

-- ---------------------------------------------------------------------------
-- Ledger columns. DEFAULT 0 IS correct here, unlike the rates above: these are
-- observations rather than price inputs, and rows written before this migration
-- genuinely have no breakdown to record.
--
-- usage_events is range-partitioned by month (migration 0006). ALTER on the
-- partitioned parent recurses into every attached partition, including the
-- create-ahead ones tollgate_usage_maintain() has already made, so no per-
-- partition work is needed. The append-only trigger fires on row UPDATE/DELETE,
-- not on DDL, so it does not block this.
--
-- On PostgreSQL 16 ADD COLUMN ... DEFAULT is metadata-only, so no table rewrite,
-- but the statement still takes ACCESS EXCLUSIVE on the parent and all
-- partitions. lock_timeout keeps that from queueing behind a long reader and
-- stalling the gateway; re-run the migration if it trips.
-- ---------------------------------------------------------------------------
SET lock_timeout = '3s';

ALTER TABLE usage_events
    ADD COLUMN cache_read_tokens  BIGINT NOT NULL DEFAULT 0
        CHECK (cache_read_tokens  >= 0),
    ADD COLUMN cache_write_tokens BIGINT NOT NULL DEFAULT 0
        CHECK (cache_write_tokens >= 0);

RESET lock_timeout;

-- ---------------------------------------------------------------------------
-- Analyst view.
--
-- NOTE the meaning of usage_events.input_tokens CHANGES from here on for
-- providers whose cache tokens are reported INSIDE the prompt count (Gemini,
-- OpenAI): it now excludes cached tokens, which are carried in their own column.
-- Anthropic rows were always fresh-input-only. An operator watching an
-- input-token series will therefore see a step at this boundary, so the view
-- exposes total_prompt_tokens to restore a continuous series.
--
-- CREATE OR REPLACE VIEW can append trailing columns but cannot reorder or
-- retype existing ones, so the first ten are reproduced verbatim from 0006.
-- Do not DROP and CREATE: 0006 already showed that path is fiddly.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE VIEW usage_report AS
SELECT
    id,
    api_key_id,
    provider,
    model,
    input_tokens,
    output_tokens,
    (cost_micros::numeric / 1000000)::numeric(18, 6) AS cost,
    gateway_micros AS gateway_overhead_us,
    decision,
    started_at,
    cache_read_tokens,
    cache_write_tokens,
    (input_tokens + cache_read_tokens + cache_write_tokens) AS total_prompt_tokens
FROM usage_events;
