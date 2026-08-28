-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Andrew Stevens
--
-- Allow the 'unpriced' decision on usage_events (a request refused because no
-- price is configured for its provider/model; fail closed).

ALTER TABLE usage_events DROP CONSTRAINT usage_events_decision_check;
ALTER TABLE usage_events ADD CONSTRAINT usage_events_decision_check
    CHECK (decision IN ('allowed', 'rejected_budget', 'rejected_policy', 'error', 'unpriced'));
