-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Andrew Stevens
--
-- Analyst-friendly view over the usage ledger. Storage stays as exact integer
-- micros (no floating-point drift); this view exposes cost as a plain decimal
-- in the deployment's currency, with clear column names.

CREATE VIEW usage_report AS
SELECT
    id,
    api_key_id,
    provider,
    model,
    input_tokens,
    output_tokens,
    (cost_micros::numeric / 1000000)::numeric(18, 6) AS cost,
    decision,
    started_at
FROM usage_events;
