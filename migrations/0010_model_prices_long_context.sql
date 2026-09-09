-- SPDX-License-Identifier: MIT
-- SPDX-FileCopyrightText: 2026 Andrew Stevens
--
-- Per-model long-context tiering.
--
-- Several models bill the WHOLE request at a higher rate once the prompt crosses
-- a size threshold: commonly around 200k tokens, commonly 2x on input and 1.5x
-- on output. A price is one rate per class and cannot express a rate that varies
-- with request size, so a long request was under-charged by the tier multiple.
--
-- These were briefly deployment-wide settings, which does not work: a gateway
-- fronting one model that tiers and one that bills flat had to choose which to
-- get wrong, over-charging the flat model or under-charging the tiered one. The
-- threshold and multiples belong to the model, exactly like the cache rates, so
-- they live on the price row with the deployment setting as a fallback.
--
-- NULL means "not configured for this model", which falls back to the
-- deployment default. That default ships as a no-op, so an unconfigured model is
-- billed flat and logs that it may be under-charged, rather than having a
-- multiple invented for it: a model that does not tier must not be charged as
-- though it does.

ALTER TABLE model_prices
    ADD COLUMN long_context_threshold_tokens BIGINT
        CHECK (long_context_threshold_tokens IS NULL OR long_context_threshold_tokens >= 0),
    -- Per-mille, so 2000 is 2.0x. Integer, because no float touches the money
    -- path. Below 1000 would make a long request CHEAPER than a short one,
    -- which is backwards, so it is refused here as well as at boot.
    ADD COLUMN long_context_input_permille INTEGER
        CHECK (long_context_input_permille IS NULL
               OR (long_context_input_permille >= 1000 AND long_context_input_permille <= 10000)),
    ADD COLUMN long_context_output_permille INTEGER
        CHECK (long_context_output_permille IS NULL
               OR (long_context_output_permille >= 1000 AND long_context_output_permille <= 10000));

COMMENT ON COLUMN model_prices.long_context_threshold_tokens IS
    'Prompt tokens above which this model re-rates the whole request. NULL falls '
    'back to the deployment default; 0 disables tiering for this model.';
COMMENT ON COLUMN model_prices.long_context_input_permille IS
    'Multiple applied to every prompt-side rate above the threshold, per-mille '
    '(2000 = 2.0x). NULL falls back to the deployment default.';
COMMENT ON COLUMN model_prices.long_context_output_permille IS
    'Multiple applied to the output rate above the threshold, per-mille '
    '(1500 = 1.5x). Separate from the input multiple because real tiers move the '
    'two by different amounts. NULL falls back to the deployment default.';
