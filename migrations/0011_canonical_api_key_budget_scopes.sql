-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Andrew Stevens
--
-- Store every per-key budget's key id in canonical form, and refuse any other.
--
-- A budget scoped to `api_key:<id>` is matched against the request's key id by
-- string comparison, and the request carries the lowercase hyphenated form.
-- `admin budget set` accepted any form the uuid parser does (upper case, no
-- hyphens, braces, a urn:uuid: prefix) and stored it as typed, so a budget
-- entered any other way never matched its key: the key ran under the wider
-- budgets only. That is a silent fail-open.
--
-- Those budgets then START enforcing, against the spend already recorded this
-- period, as soon as an instance loads them: any running instance at its next
-- config reload after this migration is applied, or at restart if reload is
-- disabled. (A binary that ships this migration refuses to start until it is
-- applied.) A key that was over its intended cap is refused straight away.
--
-- It refuses, rather than guesses, in two cases. Fix the rows and run it again:
--   * an api_key id that is not a UUID at all (only possible for a row written
--     outside the CLI);
--   * two budgets for the same key and period once both are canonical: keeping
--     either one silently would change an enforced limit.
--
-- Migration 0002 put shape CHECKs on provider and model scope ids but none on
-- api_key, leaving its form to the application; this one enforces it.

-- 1. Ids that are not UUIDs. Checked row by row, catching the cast error, so the
--    message lists every bad row instead of failing on the first.
DO $$
DECLARE
    r     RECORD;
    bad   TEXT := '';
    probe UUID;
BEGIN
    FOR r IN SELECT id, scope_id FROM budgets WHERE scope_type = 'api_key' LOOP
        BEGIN
            probe := regexp_replace(r.scope_id, '^urn:uuid:', '', 'i')::uuid;
        EXCEPTION WHEN invalid_text_representation THEN
            bad := bad || format(' budget %s has api_key scope id %L;', r.id, r.scope_id);
        END;
    END LOOP;
    IF bad <> '' THEN
        RAISE EXCEPTION '%', 'per-key budgets whose key id is not a UUID; fix or delete them, '
            'then run the migration again:' || bad;
    END IF;
END
$$;

-- 2. Budgets that would collide once canonical. Every id casts now (step 1).
DO $$
DECLARE
    dup TEXT;
BEGIN
    SELECT string_agg(format(' key %s, period %s: budgets %s;', canon, period, ids), '')
      INTO dup
      FROM (
          SELECT regexp_replace(scope_id, '^urn:uuid:', '', 'i')::uuid::text AS canon,
                 period,
                 string_agg(format('%s (%L)', id, scope_id), ', ' ORDER BY id) AS ids
            FROM budgets
           WHERE scope_type = 'api_key'
           GROUP BY 1, 2
          HAVING count(*) > 1
      ) c;
    IF dup IS NOT NULL THEN
        RAISE EXCEPTION '%', 'per-key budgets that name the same key and period in different '
            'forms; delete all but one of each, then run the migration again:' || dup;
    END IF;
END
$$;

-- 3. Canonicalise, then make any other form impossible to store.
SET lock_timeout = '3s';

-- The WHERE clause casts nothing: PostgreSQL may evaluate AND terms in any
-- order, so a cast there could run on a provider or model row and abort. The
-- regular expression cannot fail, and the cast in SET runs only on rows that
-- passed it, all of which step 1 proved valid.
UPDATE budgets
   SET scope_id = regexp_replace(scope_id, '^urn:uuid:', '', 'i')::uuid::text
 WHERE scope_type = 'api_key'
   AND scope_id !~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$';

ALTER TABLE budgets ADD CONSTRAINT budgets_api_key_scope_canonical CHECK (
    scope_type <> 'api_key'
    OR scope_id ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
);

RESET lock_timeout;
