-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Andrew Stevens
--
-- Retention for the usage ledger. usage_events is append-only (a trigger blocks
-- UPDATE/DELETE, even for the owner), so rows cannot be pruned with DELETE. The
-- only way to trim the ledger while keeping in-window rows immutable is time
-- partitioning: drop whole old partitions (DROP TABLE is DDL and bypasses the
-- row-level trigger).
--
-- This migration converts usage_events into a monthly RANGE-partitioned table,
-- copies existing rows, and installs a maintenance function that creates
-- partitions ahead of need and drops partitions older than a retention window.

-- The analyst view depends on usage_events; drop and recreate it at the end.
DROP VIEW IF EXISTS usage_report;

-- Move the existing table aside (its append-only trigger moves with it).
ALTER TABLE usage_events RENAME TO usage_events_legacy;

-- Index names are schema-wide unique and are NOT renamed with the table, so free
-- the originals for the new table while the legacy table still exists for the copy.
ALTER INDEX usage_events_pkey               RENAME TO usage_events_legacy_pkey;
ALTER INDEX usage_events_key_time_idx       RENAME TO usage_events_legacy_key_time_idx;
ALTER INDEX usage_events_time_idx           RENAME TO usage_events_legacy_time_idx;
ALTER INDEX usage_events_provider_time_idx  RENAME TO usage_events_legacy_provider_time_idx;
ALTER INDEX usage_events_model_time_idx     RENAME TO usage_events_legacy_model_time_idx;

-- New partitioned parent. started_at must be part of the primary key for range
-- partitioning on it. Columns, checks and the FK mirror the original exactly.
CREATE TABLE usage_events (
    id             UUID        NOT NULL DEFAULT gen_random_uuid(),
    api_key_id     UUID        NOT NULL REFERENCES api_keys (id),
    provider       TEXT        NOT NULL,
    model          TEXT        NOT NULL,
    input_tokens   BIGINT      NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens  BIGINT      NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cost_micros    BIGINT      NOT NULL DEFAULT 0 CHECK (cost_micros >= 0),
    gateway_micros BIGINT      NOT NULL DEFAULT 0 CHECK (gateway_micros >= 0),
    decision       TEXT        NOT NULL
                     CONSTRAINT usage_events_decision_check
                     CHECK (decision IN ('allowed', 'rejected_budget', 'rejected_policy', 'error', 'unpriced')),
    request_id     TEXT,
    latency_ms     INTEGER,
    started_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, started_at)
) PARTITION BY RANGE (started_at);

-- Indexes on the parent cascade to every current and future partition.
CREATE INDEX usage_events_key_time_idx      ON usage_events (api_key_id, started_at);
CREATE INDEX usage_events_time_idx          ON usage_events (started_at);
CREATE INDEX usage_events_provider_time_idx ON usage_events (provider, started_at);
CREATE INDEX usage_events_model_time_idx    ON usage_events (provider, model, started_at);

-- Create monthly partitions covering all existing rows, plus the current and
-- next month so inserts have a home immediately after this migration.
DO $$
DECLARE
    lo date;
    hi date;
    d  date;
    pname text;
BEGIN
    -- Align month boundaries to UTC so partition ranges match the UTC period
    -- windows the budget spend queries use.
    SET LOCAL timezone = 'UTC';

    SELECT date_trunc('month', COALESCE(min(started_at), now()))::date,
           date_trunc('month', COALESCE(max(started_at), now()))::date
      INTO lo, hi
      FROM usage_events_legacy;

    -- Always extend two months ahead so inserts have a home well past any month
    -- boundary even if maintenance is delayed.
    hi := GREATEST(hi, (date_trunc('month', now()) + interval '2 month')::date);

    d := lo;
    WHILE d <= hi LOOP
        pname := 'usage_events_' || to_char(d, 'YYYYMM');
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS %I PARTITION OF usage_events FOR VALUES FROM (%L) TO (%L)',
            pname, d, (d + interval '1 month')::date
        );
        d := (d + interval '1 month')::date;
    END LOOP;
END $$;

-- Copy the existing ledger into the partitioned table.
INSERT INTO usage_events
    (id, api_key_id, provider, model, input_tokens, output_tokens, cost_micros,
     gateway_micros, decision, request_id, latency_ms, started_at)
SELECT
    id, api_key_id, provider, model, input_tokens, output_tokens, cost_micros,
    gateway_micros, decision, request_id, latency_ms, started_at
FROM usage_events_legacy;

-- DROP TABLE is DDL, so the append-only row trigger does not block it.
DROP TABLE usage_events_legacy;

-- Reinstate append-only immutability. A row-level BEFORE trigger on a partitioned
-- table applies to all partitions (PostgreSQL 13+; this deployment targets 16).
CREATE TRIGGER usage_events_append_only
    BEFORE UPDATE OR DELETE ON usage_events
    FOR EACH ROW EXECUTE FUNCTION tollgate_reject_mutation();

-- Recreate the analyst view (unchanged from 0005).
CREATE VIEW usage_report AS
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
    started_at
FROM usage_events;

-- Maintenance: ensure the current and next month partitions exist, then drop any
-- partition whose whole month is older than the retention window. Called at
-- startup and periodically by the gateway. A retention of 0 keeps everything
-- (create-ahead only). Granularity is monthly: at least the current month is
-- always retained.
CREATE OR REPLACE FUNCTION tollgate_usage_maintain(retention interval)
    RETURNS void
    LANGUAGE plpgsql AS $$
DECLARE
    m      date;
    pname  text;
    cutoff date;
    r      record;
BEGIN
    -- UTC-align month boundaries; cap lock waits so partition DDL never stalls
    -- the insert hot path (it backs off and retries next sweep instead).
    SET LOCAL timezone = 'UTC';
    SET LOCAL lock_timeout = '3s';

    -- Create-ahead: current, next, and the month after, so inserts always have a
    -- home even if a sweep is missed near a month boundary.
    FOR m IN
        SELECT generate_series(
            date_trunc('month', now()),
            date_trunc('month', now()) + interval '2 month',
            interval '1 month'
        )::date
    LOOP
        pname := 'usage_events_' || to_char(m, 'YYYYMM');
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS %I PARTITION OF usage_events FOR VALUES FROM (%L) TO (%L)',
            pname, m, (m + interval '1 month')::date
        );
    END LOOP;

    IF retention > interval '0' THEN
        cutoff := date_trunc('month', now() - retention)::date;
        FOR r IN
            SELECT c.relname AS name
            FROM pg_inherits i
            JOIN pg_class c ON c.oid = i.inhrelid
            JOIN pg_class p ON p.oid = i.inhparent
            WHERE p.relname = 'usage_events'
              AND c.relname ~ '^usage_events_[0-9]{6}$'
              AND to_date(right(c.relname, 6), 'YYYYMM') < cutoff
        LOOP
            EXECUTE format('DROP TABLE IF EXISTS %I', r.name);
        END LOOP;
    END IF;
END $$;
