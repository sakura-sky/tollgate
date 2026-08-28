-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Andrew Stevens
--
-- Record the gateway's own added latency per request: the time spent in
-- Tollgate's admission path (auth, parse, price, reserve, settle) EXCLUDING the
-- upstream provider call. This is the "how much overhead does the proxy add"
-- number, surfaced in the console and the x-tollgate-overhead-us response header.

ALTER TABLE usage_events
    ADD COLUMN gateway_micros BIGINT NOT NULL DEFAULT 0 CHECK (gateway_micros >= 0);

-- Refresh the analyst view to expose it alongside cost.
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
    started_at
FROM usage_events;
