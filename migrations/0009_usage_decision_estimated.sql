-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Andrew Stevens
--
-- Allow the 'estimated' decision on usage_events.
--
-- Until now 'error' meant two unrelated things: a request that failed and was
-- charged NOTHING, and a request charged its full RESERVATION because metering
-- failed. Worse, some reservation-charged rows were recorded as 'allowed' (a
-- clean stream that reported no usage, a 2xx with no usage block), so they were
-- invisible entirely.
--
-- Charging the reservation deliberately over-charges: it is the safe direction
-- when the true cost cannot be measured. But an operator reconciling against a
-- provider invoice is entitled to know which part of a period was measured and
-- which was assumed, and today they cannot tell.
--
-- After this, the vocabulary is:
--   allowed          - forwarded, charged its MEASURED cost
--   estimated        - forwarded (or possibly forwarded), charged its RESERVATION
--                      because usage could not be trusted or was never observed
--   error            - failed, charged nothing
--   rejected_budget  - refused by a hard cap, charged nothing
--   rejected_policy  - refused by policy, charged nothing
--   unpriced         - refused, no price configured, charged nothing
--
-- usage_events is range-partitioned by month (migration 0006). Adding a CHECK
-- normally takes ACCESS EXCLUSIVE on the parent AND scans every partition. NOT
-- VALID skips the scan and takes the lock only briefly; VALIDATE then re-checks
-- existing rows under a weaker lock. Existing rows satisfy the new constraint by
-- construction (it is a strict superset of the old one), so validation cannot
-- fail; this is purely about not locking the ledger while it does so.
SET lock_timeout = '3s';

ALTER TABLE usage_events DROP CONSTRAINT usage_events_decision_check;

ALTER TABLE usage_events ADD CONSTRAINT usage_events_decision_check
    CHECK (decision IN (
        'allowed',
        'estimated',
        'rejected_budget',
        'rejected_policy',
        'error',
        'unpriced'
    )) NOT VALID;

RESET lock_timeout;

ALTER TABLE usage_events VALIDATE CONSTRAINT usage_events_decision_check;
