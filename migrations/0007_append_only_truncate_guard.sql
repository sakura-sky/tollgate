-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Andrew Stevens
--
-- Close the TRUNCATE gap in the append-only guarantee. The existing trigger only
-- rejects UPDATE and DELETE; TRUNCATE would still empty the ledger or audit log.
-- Add statement-level BEFORE TRUNCATE triggers that reject it, using the same
-- reject function. (Partition retention uses DROP TABLE, not TRUNCATE, so this
-- does not interfere with dropping old usage_events partitions.)

CREATE TRIGGER usage_events_no_truncate
    BEFORE TRUNCATE ON usage_events
    FOR EACH STATEMENT EXECUTE FUNCTION tollgate_reject_mutation();

CREATE TRIGGER audit_log_no_truncate
    BEFORE TRUNCATE ON audit_log
    FOR EACH STATEMENT EXECUTE FUNCTION tollgate_reject_mutation();
