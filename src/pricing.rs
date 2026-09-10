// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Token → cost translation.
//!
//! This is the mechanism that turns a provider's reported token usage into a
//! cost figure, keyed on `(provider, model)`. It backs both the budget enforcer
//! (does this request fit under the cap?) and the `usage_events` ledger (what
//! did this request actually cost?).
//!
//! ## Money is currency-agnostic integer micros
//!
//! All money is **micros** - 1e-6 of the deployment's configured currency unit -
//! stored as `i64`. This module is currency-agnostic: it does integer arithmetic
//! on micros and never assumes a currency or symbol. The currency (ISO 4217) and
//! its display formatting are a deployment config / UI concern, applied at the
//! edge. There is no floating point on the money path, so costs never drift. A
//! price of `1.25` per 1M tokens is `1_250_000` micros.
//!
//! Prices and model list are **operator configuration**, not baked into this
//! binary: they live in the `model_prices` table (managed via the admin CLI /
//! UI) and are loaded into a [`PriceBook`]. Nothing here ships a canonical price.
//!
//! ## The formula
//!
//! For a request that consumed `input_tokens` and `output_tokens`, with a price
//! of `input_per_1m_micros` / `output_per_1m_micros` for its model:
//!
//! ```text
//! cost_micros =
//!     round(input_tokens  × input_per_1m_micros  / 1_000_000)
//!   + round(output_tokens × output_per_1m_micros / 1_000_000)
//! ```
//!
//! i.e. each side is `tokens × (price per million) ÷ one million`, rounded to
//! the nearest micro, then summed. Token counts come from
//! `Provider::parse_usage` (Gemini `usageMetadata`, Anthropic `usage`), so the
//! same table costs every provider uniformly and one budget can span them.
//!
//! Prices live in the `model_prices` table (see migration 0002) with
//! `effective_from`/`effective_to`, so historical usage is always costed at the
//! rate that applied when it happened. [`PriceBook`] is the in-memory view of
//! the currently-effective rows.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The "per 1,000,000 tokens" denominator - one million *tokens*, not a money
/// unit. Dividing `tokens × price_per_1M` by this yields micros.
const TOKENS_PER_MILLION: i128 = 1_000_000;

/// The price for one `(provider, model)`, in micros per 1,000,000 tokens.
///
/// The two cache rates are EFFECTIVE rates: resolved once when the price book is
/// built, either from the operator's configured value or from a conservative
/// fallback multiple of the input rate. Resolving here rather than per request
/// keeps the hot path to integer multiplication, and means a reservation and its
/// settlement always use the same snapshot even across a hot reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub provider: String,
    pub model: String,
    /// Micros per 1M fresh input tokens.
    pub input_per_1m_micros: i64,
    /// Micros per 1M output tokens.
    pub output_per_1m_micros: i64,
    /// Micros per 1M cache-read tokens, already resolved.
    pub cache_read_per_1m_micros: i64,
    /// Micros per 1M cache-write tokens, already resolved.
    pub cache_write_per_1m_micros: i64,
    /// True when either cache rate came from the fallback rather than from the
    /// operator. Surfaced so a deployment can see it is being over-charged on
    /// purpose instead of discovering it in a variance review.
    pub cache_rates_are_fallback: bool,
    /// Long-context re-rating, resolved at price-book build time like the cache
    /// rates. Carried on the price rather than passed to each call site so that
    /// every cost and every reservation inherits it, and a new call site cannot
    /// forget to apply it.
    #[serde(default)]
    pub long_context: LongContextTier,
    /// True when this model's OWN row set an UPLIFT, meaning a multiple above
    /// 1.0x, rather than inheriting one from the deployment default.
    ///
    /// Needed to tell apart tiers that resolve to the same shape but mean
    /// opposite things. A model deliberately disabled with
    /// `--long-context-threshold 0` inherits the deployment multiples and looks
    /// exactly like a row whose own multiples have been stranded against a zero
    /// threshold. Only the second is a misconfiguration, and warning about the
    /// first on every reload, every fifteen seconds by default, would drown the
    /// signal the warning exists to carry.
    ///
    /// "Uplift" rather than "set" because a row is allowed to write an explicit
    /// 1.0x, which is a deliberate no-op and has nothing to strand.
    #[serde(default)]
    pub long_context_uplift_from_row: bool,
}

/// Token usage for a single request, normalised across providers by the
/// adapters into four DISJOINT classes.
///
/// Disjoint is the whole point. Providers disagree about whether cached tokens
/// are counted inside the prompt total or alongside it: Anthropic reports them
/// alongside, Gemini and OpenAI report them inside. Each adapter converts to
/// this one representation at the edge, so nothing downstream has to know a
/// provider's convention and no class can be double-counted or dropped.
///
/// `input_tokens` therefore means FRESH prompt tokens only, excluding anything
/// served from or written to a cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Fresh prompt tokens. Cache classes are NOT included here.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt tokens served from a provider-side cache. Cheaper than fresh
    /// input, typically about a tenth of the rate.
    pub cache_read_tokens: u64,
    /// Prompt tokens written into a provider-side cache. Can cost MORE than
    /// fresh input (1.25x to 2x on Anthropic, by TTL).
    pub cache_write_tokens: u64,
    /// Set when the upstream's own numbers contradicted each other, so these
    /// counts are a guess rather than a measurement. Callers must charge the
    /// reservation instead of costing them: see [`Usage::is_untrustworthy`].
    #[serde(default)]
    pub suspect: bool,
    /// Set when the prompt classes OVERLAP rather than partitioning the prompt.
    ///
    /// True only on an upstream whose cache convention is unverified, where the
    /// adapter deliberately bills the full prompt AND the cached count because
    /// it cannot know whether they are disjoint. The billing is right, but the
    /// classes may then sum to more than the provider's own prompt.
    ///
    /// Nothing on the money path branches on this: sizing resolves upward on
    /// every path, exactly as billing does, and an earlier version that sized
    /// downward here is described in [`Usage::threshold_prompt_tokens`]. It is
    /// kept, and serialised, because a persisted usage record otherwise cannot
    /// say whether its prompt classes were a measurement or a double bill.
    #[serde(default)]
    pub classes_overlap: bool,
}

/// Ceiling on the tokens one leg of a single request can plausibly consume.
///
/// Frontier context windows are on the order of 10M tokens, so 50M is far above
/// anything a real request can report while still leaving `u64` arithmetic
/// nowhere near saturation. A response above this is malformed or hostile, not
/// expensive.
pub const MAX_PLAUSIBLE_TOKENS_PER_LEG: u64 = 50_000_000;

impl Usage {
    /// Usage with no cache activity. Kept two-argument so every existing call
    /// site and adapter that predates cache classes still compiles unchanged.
    #[must_use]
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            suspect: false,
            classes_overlap: false,
        }
    }

    /// Mark these counts as derived from a self-contradictory response.
    #[must_use]
    pub fn into_suspect(mut self) -> Self {
        self.suspect = true;
        self
    }

    /// Usage including prompt-cache classes. `input_tokens` must already EXCLUDE
    /// both cache classes: callers normalise at the adapter, not here.
    #[must_use]
    pub fn with_cache(
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            suspect: false,
            classes_overlap: false,
        }
    }

    /// Every prompt-side token the provider billed for, across all classes.
    /// Restores the pre-cache-split meaning of "input tokens" for reporting.
    #[must_use]
    pub fn total_prompt_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// How large the prompt was, for deciding whether it crossed a size
    /// threshold.
    ///
    /// The same as [`Self::total_prompt_tokens`], and deliberately so even where
    /// the classes overlap.
    ///
    /// The tempting alternative on an unverified upstream is to take the larger
    /// class rather than the sum: a 150k prompt reporting 100k cached bills as
    /// 250k, and testing a 200k threshold against that re-rates a request the
    /// provider may never have tiered. But "unverified" means precisely that we
    /// do not know whether the cached count sits INSIDE the prompt or beside it.
    /// If it sits beside it, the real prompt IS the sum, and taking the larger
    /// class under-sizes the request and skips a tier the provider applied: an
    /// under-charge, on exactly the requests this feature exists to catch.
    ///
    /// So it resolves upward, the same way BILLING already resolves on that
    /// path. An earlier version billed conservatively and sized optimistically,
    /// which is the worst of both.
    #[must_use]
    pub fn threshold_prompt_tokens(&self) -> u64 {
        self.total_prompt_tokens()
    }

    /// Mark the prompt classes as overlapping rather than partitioning.
    #[must_use]
    pub fn with_overlapping_classes(mut self) -> Self {
        self.classes_overlap = true;
        self
    }

    /// True when a leg exceeds [`MAX_PLAUSIBLE_TOKENS_PER_LEG`].
    ///
    /// Costing an implausible count is not merely wrong, it is corrupting.
    /// `cost_micros` saturates to `i64::MAX`, the budget backend then adds that
    /// to a counter that cannot be lowered by any code path (reconcile only
    /// raises), and the ledger row it writes makes `SUM(cost_micros)` overflow
    /// `bigint` on every subsequent startup reconciliation. One bad response
    /// would wedge a deployment until the period rolled over.
    ///
    /// Callers MUST treat this as a metering failure and charge the reservation
    /// rather than the computed cost. This is the guard `round_to_micros`
    /// documents as the metering layer's responsibility.
    #[must_use]
    pub fn is_implausible(&self) -> bool {
        self.input_tokens > MAX_PLAUSIBLE_TOKENS_PER_LEG
            || self.output_tokens > MAX_PLAUSIBLE_TOKENS_PER_LEG
            || self.cache_read_tokens > MAX_PLAUSIBLE_TOKENS_PER_LEG
            || self.cache_write_tokens > MAX_PLAUSIBLE_TOKENS_PER_LEG
    }

    /// True when these counts must NOT be costed: either implausible, or derived
    /// from a response whose own numbers contradicted each other.
    ///
    /// Both cases are metering failures rather than expensive requests. Costing
    /// a contradictory report means picking one reading of it, and the readings
    /// differ by the size of the cache: guessing cheap under-charges, guessing
    /// expensive settles above a reservation that was computed from the honest
    /// numbers. Charging the reservation is the only answer that breaks neither
    /// invariant.
    #[must_use]
    pub fn is_untrustworthy(&self) -> bool {
        self.suspect || self.is_implausible()
    }
}

/// Long-context tiering: several current models bill the WHOLE prompt at a
/// higher rate once it crosses a threshold (commonly 200k tokens, commonly 2x).
///
/// A price is one rate per class, so it cannot express a rate that changes with
/// size. Ignoring that under-charges every large request by the tier multiple,
/// and under-charging is the one direction this product may never err in.
///
/// Rather than refuse large prompts, or wait for tiered rate rows in the schema,
/// apply a conservative multiple above the threshold. Same shape as the
/// unpriced-cache-class fallback: an unmodelled dimension costs more, not less.
/// Integer per-mille, so no float touches the money path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongContextTier {
    /// Prompt tokens above which the multiples apply. Zero disables it.
    pub threshold_tokens: u64,
    /// Applied to every prompt-side rate once the threshold is crossed.
    pub multiple_permille: u32,
    /// Applied to the OUTPUT rate once the threshold is crossed.
    ///
    /// Separate from the prompt multiple because real tiers move the two by
    /// different amounts: Gemini 2.5 Pro goes 2x on input and 1.5x on output,
    /// Sonnet's 1M tier likewise. An earlier version re-rated only the prompt,
    /// which meant enabling the feature still under-charged the output leg of
    /// every long request: it failed at the one thing it exists to fix.
    pub output_multiple_permille: u32,
}

impl Default for LongContextTier {
    /// OFF by default: a threshold to notice at, and no uplift.
    ///
    /// Defaulting to an uplift would invent a charge. Unlike an unpriced cache
    /// class, where the class demonstrably exists and demonstrably costs more,
    /// a given model may not tier at all, and a 2x default would silently double
    /// the bill for large prompts on a model that bills flat. It would also
    /// break the definition of the unit: one million tokens would no longer cost
    /// the per-million rate.
    ///
    /// So the default leaves cost unchanged and instead makes the gap visible:
    /// a prompt above the threshold logs that it may be under-charged. An
    /// operator who knows their model tiers sets the multiple.
    fn default() -> Self {
        Self {
            threshold_tokens: 200_000,
            multiple_permille: 1_000,
            output_multiple_permille: 1_000,
        }
    }
}

/// How far below the threshold a reservation still reserves at the tier rate.
///
/// The reservation tests the COUNTED prompt; the settlement tests the prompt the
/// provider REPORTS. They differ: token counters are approximate, and a request
/// that counts at 199,995 can report 200,005. Without a guard band that flips a
/// 1x reservation into a 2x settlement of the entire prompt leg, so a ten-token
/// discrepancy breaks a hard cap by the whole multiple.
///
/// Five percent costs transient headroom on requests near the threshold and
/// removes the cliff. Over-reserving is released at settle; under-reserving is
/// an overspend.
///
/// Note what this does NOT fix: a provider reporting more tokens than we counted
/// overshoots its reservation on any request, tiered or not, which is the
/// documented limit of `exact` admission. The band keeps that drift from being
/// MULTIPLIED, so the overshoot stays proportional to the discrepancy rather
/// than re-rating the entire prompt.
const TIER_RESERVE_GUARD_PERMILLE: u64 = 950;

impl LongContextTier {
    /// Resolve one model's stored columns against the deployment default, field
    /// by field.
    ///
    /// Per-field rather than all-or-nothing so an operator can set just the
    /// threshold for a model and inherit the multiples, or vice versa.
    ///
    /// The single definition of that rule. `admin price set` prints the tier a
    /// re-price will produce, and it has to print what the LOADER will build,
    /// not its own reading of the same columns: two copies of a resolution rule
    /// drifting apart is how the long-context default came to be on in
    /// production while every test said it was off.
    ///
    /// A stored multiple below 1.0x is clamped rather than honoured. The CLI
    /// refuses one outright; this is the backstop for a row that reached the
    /// database another way, and a long request must never cost less than a
    /// short one.
    #[must_use]
    pub fn resolve_row(
        threshold: Option<i64>,
        input_permille: Option<i32>,
        output_permille: Option<i32>,
        fallback: Self,
    ) -> Self {
        Self {
            threshold_tokens: threshold
                .map_or(fallback.threshold_tokens, |v| u64::try_from(v).unwrap_or(0)),
            multiple_permille: input_permille.map_or(fallback.multiple_permille, |v| {
                u32::try_from(v).unwrap_or(1_000).max(1_000)
            }),
            output_multiple_permille: output_permille
                .map_or(fallback.output_multiple_permille, |v| {
                    u32::try_from(v).unwrap_or(1_000).max(1_000)
                }),
        }
    }

    /// Whether this tier asks for an uplift that no threshold will ever apply.
    ///
    /// Checked on the DEPLOYMENT default at boot, not on a resolved per-model
    /// tier. A model deliberately disabled with `--long-context-threshold 0`
    /// resolves to exactly this shape by inheriting the deployment multiples,
    /// and refusing that would take the whole price book down for a
    /// configuration that is working as intended.
    ///
    /// The deployment default has no such excuse. Nothing inherits into it, so
    /// an uplift against a zero threshold there can only be a mistake, and it is
    /// a mistake in the under-charging direction: no request is re-rated, and
    /// because `is_unpriced_long_context` needs a threshold to test against, no
    /// request is even logged as a possible under-charge. The operator would see
    /// a multiple in their environment and no signal anywhere that it does
    /// nothing. `admin price set` already refuses the same pair on a row.
    #[must_use]
    pub fn has_stranded_uplift(&self) -> bool {
        self.threshold_tokens == 0
            && (self.multiple_permille > 1_000 || self.output_multiple_permille > 1_000)
    }

    /// Whether this tier does anything at all.
    #[must_use]
    fn configured(&self) -> bool {
        self.threshold_tokens > 0
            && (self.multiple_permille > 1_000 || self.output_multiple_permille > 1_000)
    }

    /// Whether an uplift applies to this prompt. False when no multiple is
    /// configured, even above the threshold.
    #[must_use]
    pub fn applies_to(&self, prompt_tokens: u64) -> bool {
        self.configured() && prompt_tokens > self.threshold_tokens
    }

    /// Whether a RESERVATION should be taken at the tier rate.
    ///
    /// Deliberately looser than [`Self::applies_to`]: it triggers slightly below
    /// the threshold, because the counted prompt and the reported prompt differ
    /// and a request that straddles the line would otherwise settle above its
    /// reservation by the whole multiple.
    #[must_use]
    pub fn reserve_applies_to(&self, prompt_tokens: u64) -> bool {
        if !self.configured() {
            return false;
        }
        let guard = self
            .threshold_tokens
            .saturating_mul(TIER_RESERVE_GUARD_PERMILLE)
            / 1_000;
        prompt_tokens > guard
    }

    /// Whether this prompt is large enough that the provider may be tiering it
    /// while Tollgate is not. Used to log the gap rather than silently accept it.
    #[must_use]
    pub fn is_unpriced_long_context(&self, prompt_tokens: u64) -> bool {
        self.threshold_tokens > 0 && !self.configured() && prompt_tokens > self.threshold_tokens
    }

    /// Reject a multiple that would make a large request CHEAPER, which would
    /// invert the whole point.
    ///
    /// # Errors
    /// Returns a message when the multiple is below 1.0x.
    /// Validates the MULTIPLES regardless of the threshold.
    ///
    /// An earlier version returned early when the threshold was zero, which left
    /// a hole: a deployment default of `(0, 2_000_000, 1000)` booted fine, and a
    /// price row that set a threshold but inherited the multiples then picked up
    /// a 2000x uplift that no CHECK constraint had seen, because the value never
    /// went near the database.
    pub fn validate(&self) -> Result<(), String> {
        if self.multiple_permille < MIN_FALLBACK_PERMILLE {
            return Err(format!(
                "long_context_multiple_permille is {}, below the minimum {MIN_FALLBACK_PERMILLE} \
                 (1.0x). A multiple under 1.0x would make a long-context request cheaper than a \
                 short one, when providers charge MORE for it.",
                self.multiple_permille
            ));
        }
        // Real tiers are 1.5x to 2x. A fat-fingered value (2_000_000 meaning
        // 2x) saturates costs to i64::MAX, and the budget counter has no path
        // back down from that: it wedges the deployment until the period rolls.
        if self.output_multiple_permille < MIN_FALLBACK_PERMILLE {
            return Err(format!(
                "long_context_output_multiple_permille is {}, below the minimum \
                 {MIN_FALLBACK_PERMILLE} (1.0x).",
                self.output_multiple_permille
            ));
        }
        if self.output_multiple_permille > MAX_LONG_CONTEXT_PERMILLE {
            return Err(format!(
                "long_context_output_multiple_permille is {}, above the maximum \
                 {MAX_LONG_CONTEXT_PERMILLE} (10x).",
                self.output_multiple_permille
            ));
        }
        if self.multiple_permille > MAX_LONG_CONTEXT_PERMILLE {
            return Err(format!(
                "long_context_multiple_permille is {}, above the maximum \
                 {MAX_LONG_CONTEXT_PERMILLE} (10x). Real long-context tiers are 1.5x to 2x; a \
                 value this large is almost certainly a units mistake, and it would saturate \
                 costs to the integer maximum, which no budget counter can recover from.",
                self.multiple_permille
            ));
        }
        Ok(())
    }
}

/// What a provider's worst case looks like on the prompt side, used to size a
/// reservation before the response reveals how the prompt actually split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PromptReserveProfile {
    /// This adapter can report `cache_write_tokens > 0`, so the prompt might be
    /// billed at the write rate, which can exceed the input rate.
    pub can_cache_write: bool,
    /// This adapter may bill a single prompt token on TWO legs: it counts the
    /// full prompt as fresh AND the cached count on top, because the upstream's
    /// convention is unverified and billing both is the only reading that cannot
    /// under-charge. The reservation has to cover the sum, or a cache hit
    /// settles above what was admitted.
    pub may_double_bill_prompt: bool,
}

/// Fallback multiples applied to a model's base input rate when the operator has
/// not priced a cache class, expressed in integer PER-MILLE so no float ever
/// touches the money path.
///
/// These are not shipped prices. Tollgate ships no price list; they are safety
/// backstops on the operator's own rate, chosen so that an unpriced class is
/// over-charged rather than under-charged:
///
/// - Reads default to 1000 per-mille (1.0x). Real read rates are 0.1x to 0.25x,
///   so this over-charges. Safe.
/// - Writes default to 2000 per-mille (2.0x), which covers the most expensive
///   real write rate seen (Anthropic long-TTL at 2x) and therefore also the
///   cheaper ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheRateFallback {
    pub read_permille: u32,
    pub write_permille: u32,
}

impl Default for CacheRateFallback {
    fn default() -> Self {
        Self {
            read_permille: 1_000,
            write_permille: 2_000,
        }
    }
}

/// Smallest fallback multiple that is not a silent discount. A multiple below
/// 1.0x would price an UNPRICED class below the operator's own input rate, which
/// is guessing downward on a number nobody has supplied: the same fail-open the
/// nullable rate columns exist to avoid.
pub const MIN_FALLBACK_PERMILLE: u32 = 1_000;

/// Largest long-context multiple accepted. Real tiers are 1.5x to 2x, so
/// anything above 10x is a units mistake, and a large enough one saturates the
/// money path to `i64::MAX`, which no budget counter can come back from.
pub const MAX_LONG_CONTEXT_PERMILLE: u32 = 10_000;

impl CacheRateFallback {
    /// Reject a configuration that would make an unpriced class cheap or free.
    ///
    /// # Errors
    /// Returns a message naming the offending field when either multiple is
    /// below [`MIN_FALLBACK_PERMILLE`].
    pub fn validate(&self) -> Result<(), String> {
        for (name, v) in [
            ("cache_read_fallback_permille", self.read_permille),
            ("cache_write_fallback_permille", self.write_permille),
        ] {
            if v < MIN_FALLBACK_PERMILLE {
                return Err(format!(
                    "{name} is {v}, below the minimum {MIN_FALLBACK_PERMILLE} (1.0x). \
                     A fallback under 1.0x would price an UNPRICED cache class below the \
                     model's own input rate, which under-charges silently. Set a real rate \
                     with `admin price set` instead of discounting the fallback."
                ));
            }
        }
        Ok(())
    }

    /// Apply a multiple to a base rate, rounding UP, saturating at [`i64::MAX`].
    /// Rounding up keeps the fallback on the over-charging side of exact.
    #[must_use]
    fn apply(base: i64, permille: u32) -> i64 {
        let scaled = i128::from(base) * i128::from(permille);
        let rounded = scaled.div_euclid(1_000) + i128::from(scaled.rem_euclid(1_000) != 0);
        i64::try_from(rounded).unwrap_or(i64::MAX)
    }
}

/// Divide a numerator of `Σ tokens × price_per_1M × permille` by one million and
/// a further thousand, rounding half-up to the nearest micro.
///
/// The per-mille scale exists so a long-context multiple can be folded into the
/// sum and the WHOLE thing rounded once. Rounding a base cost and then rounding
/// a separate uplift is the round-each-leg pattern this module forbids: it
/// differs from a single rounding by up to a micro, and truncation runs in the
/// cheap direction. A tier that does not apply passes a multiple of 1000, so
/// the arithmetic is identical for an ordinary request.
///
/// The `i128` intermediate cannot overflow for any `u64` token count times `i64`
/// price times a multiple in range. As a last resort the result saturates into
/// `i64` rather than wrapping, but callers MUST reject implausible token counts
/// upstream (see the metering layer) so saturation never occurs on the money
/// path.
#[must_use]
fn round_scaled_to_micros(numerator: i128) -> i64 {
    const SCALE: i128 = TOKENS_PER_MILLION * 1_000;
    let rounded = numerator.saturating_add(SCALE / 2) / SCALE;
    i64::try_from(rounded).unwrap_or(i64::MAX)
}

impl ModelPrice {
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        input_per_1m_micros: i64,
        output_per_1m_micros: i64,
    ) -> Self {
        // Clamp to non-negative at RUNTIME. A negative rate would manufacture
        // budget (metering would DECREMENT the spend counter). This must hold in
        // release builds, so it is a real `.max(0)` - not a `debug_assert!`,
        // which is stripped in release. The DB also CHECKs >= 0; this is the
        // in-memory backstop for a PriceBook built from an external manifest.
        let input = input_per_1m_micros.max(0);
        let fb = CacheRateFallback::default();
        Self {
            provider: provider.into(),
            model: model.into(),
            input_per_1m_micros: input,
            output_per_1m_micros: output_per_1m_micros.max(0),
            cache_read_per_1m_micros: CacheRateFallback::apply(input, fb.read_permille),
            cache_write_per_1m_micros: CacheRateFallback::apply(input, fb.write_permille),
            cache_rates_are_fallback: true,
            long_context: LongContextTier::default(),
            long_context_uplift_from_row: false,
        }
    }

    /// Attach operator-configured cache rates. `None` for a class leaves the
    /// conservative fallback in place, so an unpriced class is never free.
    #[must_use]
    pub fn with_cache_rates(
        mut self,
        cache_read: Option<i64>,
        cache_write: Option<i64>,
        fallback: CacheRateFallback,
    ) -> Self {
        self.cache_read_per_1m_micros = cache_read.map_or_else(
            || CacheRateFallback::apply(self.input_per_1m_micros, fallback.read_permille),
            |v| v.max(0),
        );
        self.cache_write_per_1m_micros = cache_write.map_or_else(
            || CacheRateFallback::apply(self.input_per_1m_micros, fallback.write_permille),
            |v| v.max(0),
        );
        self.cache_rates_are_fallback = cache_read.is_none() || cache_write.is_none();
        self
    }

    /// Attach a long-context tier. Applied by every cost and every reservation.
    #[must_use]
    pub fn with_long_context(mut self, tier: LongContextTier) -> Self {
        self.long_context = tier;
        self
    }

    /// Resolve this model's tier from its own columns, falling back per field to
    /// the deployment default.
    ///
    /// Per-field rather than all-or-nothing so an operator can set just the
    /// threshold for a model and inherit the multiples, or vice versa.
    #[must_use]
    pub fn with_long_context_row(
        mut self,
        threshold: Option<i64>,
        input_permille: Option<i32>,
        output_permille: Option<i32>,
        fallback: LongContextTier,
    ) -> Self {
        self.long_context_uplift_from_row =
            input_permille.is_some_and(|v| v > 1_000) || output_permille.is_some_and(|v| v > 1_000);
        let tier =
            LongContextTier::resolve_row(threshold, input_permille, output_permille, fallback);
        self.with_long_context(tier)
    }

    /// Cost of `usage` at this price, in micros, INCLUDING long-context
    /// re-rating.
    ///
    /// The multiples are folded into the numerator and the whole sum is rounded
    /// ONCE, at a scale of a thousandth of a micro. Computing a base cost and
    /// then adding a separately-rounded uplift is the round-each-leg pattern the
    /// module comment forbids: it differs from a single rounding by up to a
    /// micro, and the truncation runs in the cheap direction.
    ///
    /// A crossed threshold re-rates the WHOLE request, not just the excess,
    /// because that is how providers bill it.
    #[must_use]
    pub fn cost_micros(&self, usage: Usage) -> i64 {
        let tier = self.long_context;
        let (prompt_m, output_m) = if tier.applies_to(usage.threshold_prompt_tokens()) {
            (
                i128::from(tier.multiple_permille),
                i128::from(tier.output_multiple_permille),
            )
        } else {
            (1_000, 1_000)
        };
        let leg = |tokens: u64, rate: i64| i128::from(tokens) * i128::from(rate);
        let prompt = leg(usage.input_tokens, self.input_per_1m_micros)
            .saturating_add(leg(usage.cache_read_tokens, self.cache_read_per_1m_micros))
            .saturating_add(leg(
                usage.cache_write_tokens,
                self.cache_write_per_1m_micros,
            ))
            .saturating_mul(prompt_m);
        let output = leg(usage.output_tokens, self.output_per_1m_micros).saturating_mul(output_m);
        round_scaled_to_micros(prompt.saturating_add(output))
    }

    /// The rate to reserve prompt-side tokens at, given whether this request's
    /// provider can actually report a cache write.
    ///
    /// A reservation is made before we know how the prompt will split across the
    /// three prompt-side classes, so the honest worst case is the most expensive
    /// applicable prompt rate. Two subtleties:
    ///
    /// - The write rate is only included when the adapter can report a cache
    ///   write. Because an unpriced write class always resolves to a fallback
    ///   above the input rate, including it unconditionally would inflate EVERY
    ///   reservation on every model in every deployment, including adapters that
    ///   structurally never report writes. That is a denial vector, not caution:
    ///   traffic that fit under a cap before an upgrade would start being
    ///   refused.
    /// - The read rate is always included. Nothing stops an operator setting a
    ///   read rate above the input rate by mistake, and the database only checks
    ///   it is non-negative. Without this, such a model would settle above its
    ///   reservation and break the `exact` admission hard cap.
    #[must_use]
    pub fn reserve_prompt_rate(&self, profile: PromptReserveProfile) -> i64 {
        // Where a prompt token can be billed on two legs at once, the worst case
        // is the SUM of those legs, not the larger of them.
        if profile.may_double_bill_prompt {
            let both = self
                .input_per_1m_micros
                .saturating_add(self.cache_read_per_1m_micros);
            return if profile.can_cache_write {
                both.max(self.cache_write_per_1m_micros)
            } else {
                both
            };
        }
        let rate = self.input_per_1m_micros.max(self.cache_read_per_1m_micros);
        if profile.can_cache_write {
            rate.max(self.cache_write_per_1m_micros)
        } else {
            rate
        }
    }

    /// Worst-case cost to reserve for a request, at the rate from
    /// [`Self::reserve_prompt_rate`].
    #[must_use]
    pub fn reserve_micros(
        &self,
        prompt_tokens: u64,
        max_output_tokens: u64,
        profile: PromptReserveProfile,
    ) -> i64 {
        // A prompt that will be re-rated for length must be RESERVED at the
        // re-rated price, on BOTH legs, or settle exceeds reserve on exactly the
        // largest requests and walks a hard cap.
        //
        // `reserve_applies_to` triggers slightly below the threshold on purpose:
        // this sees the COUNTED prompt while settlement sees the REPORTED one,
        // and a request that straddles the line would otherwise flip a 1x
        // reservation into a 2x settlement.
        //
        // The size tested must match the size SETTLEMENT will test. On an
        // upstream whose cache convention is unverified the adapter bills the
        // prompt on two legs, so `Usage::threshold_prompt_tokens` resolves
        // upward to the sum, which is up to twice the counted prompt. Testing
        // the counted prompt here against a five percent guard band cannot cover
        // a factor-of-two sizing gap: a 150k prompt reporting 100k cached sizes
        // as 250k at settle, crosses a 200k threshold the reservation never saw,
        // and settles at the full multiple against a 1x reservation.
        //
        // So size the same way the RATE already does: `reserve_prompt_rate`
        // charges the sum of both legs under this profile, and the sizing
        // assumes the same worst case. It over-reserves an unverified upstream
        // between half the threshold and the threshold, which is released at
        // settle.
        let tier = self.long_context;
        let reserve_size = if profile.may_double_bill_prompt {
            prompt_tokens.saturating_mul(2)
        } else {
            prompt_tokens
        };
        let (prompt_m, output_m) = if tier.reserve_applies_to(reserve_size) {
            (
                i128::from(tier.multiple_permille),
                i128::from(tier.output_multiple_permille),
            )
        } else {
            (1_000, 1_000)
        };
        // saturating_mul, matching the cost path. The per-mille factor is what
        // makes overflow reachable here at all: tokens times rate cannot exceed
        // i128 on its own, but tokens times rate times a multiple of up to 10x
        // can. A wrapped negative would floor to a 1-micro reservation, which is
        // the cheap direction.
        let prompt = i128::from(prompt_tokens)
            .saturating_mul(i128::from(self.reserve_prompt_rate(profile)))
            .saturating_mul(prompt_m);
        let output = i128::from(max_output_tokens)
            .saturating_mul(i128::from(self.output_per_1m_micros))
            .saturating_mul(output_m);
        round_scaled_to_micros(prompt.saturating_add(output))
    }
}

/// In-memory view of the currently-effective prices, keyed `(provider, model)`.
#[derive(Debug, Clone, Default)]
pub struct PriceBook {
    by_key: HashMap<(String, String), ModelPrice>,
}

impl PriceBook {
    /// Build a price book from a set of prices (e.g. the current rows of
    /// `model_prices`). Later entries win on a duplicate key.
    pub fn from_prices(prices: impl IntoIterator<Item = ModelPrice>) -> Self {
        let mut by_key = HashMap::new();
        for p in prices {
            by_key.insert((p.provider.clone(), p.model.clone()), p);
        }
        Self { by_key }
    }

    /// The price for a `(provider, model)`, if known.
    #[must_use]
    pub fn lookup(&self, provider: &str, model: &str) -> Option<&ModelPrice> {
        self.by_key.get(&(provider.to_owned(), model.to_owned()))
    }

    /// Cost of `usage` for a `(provider, model)`, in USD micros. `None` when the
    /// model is not priced - callers MUST treat an unpriced model as a failure
    /// (fail closed), never as free.
    #[must_use]
    pub fn cost_micros(&self, provider: &str, model: &str, usage: Usage) -> Option<i64> {
        self.lookup(provider, model).map(|p| p.cost_micros(usage))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

/// Format micros as a plain decimal amount, e.g. `1_250_000` → `"1.250000"`.
///
/// Currency-agnostic on purpose: no symbol, no thousands separators, no locale.
/// The currency symbol and locale formatting are a display/UI concern applied at
/// the edge, driven by the deployment's configured currency.
#[must_use]
pub fn format_micros(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let abs = micros.unsigned_abs();
    format!("{sign}{}.{:06}", abs / 1_000_000, abs % 1_000_000)
}

// NOTE: Tollgate ships NO canonical price list. Models and their prices are
// operator configuration (the `model_prices` table, managed via the admin CLI /
// UI), loaded into a `PriceBook` at runtime. Test prices below are fixtures
// only - they are not shipped defaults and are not authoritative.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worked_example_sonnet() {
        // 1,000 input + 500 output on claude-3-5-sonnet ($3 / $15 per 1M).
        // input:  1_000  × 3_000_000  / 1e6 =  3_000 micros
        // output:   500  × 15_000_000 / 1e6 =  7_500 micros
        // total = 10_500 micros = $0.0105
        let price = ModelPrice::new("anthropic", "claude-3-5-sonnet", 3_000_000, 15_000_000);
        let cost = price.cost_micros(Usage::new(1_000, 500));
        assert_eq!(cost, 10_500);
        assert_eq!(format_micros(cost), "0.010500");
    }

    #[test]
    fn one_million_tokens_equals_the_per_million_rate() {
        // Exactly 1M input tokens must cost exactly the input rate, and 1M
        // output tokens exactly the output rate - the definition of the unit.
        let price = ModelPrice::new("vertex", "gemini-1.5-pro", 1_250_000, 5_000_000);
        assert_eq!(price.cost_micros(Usage::new(1_000_000, 0)), 1_250_000);
        assert_eq!(price.cost_micros(Usage::new(0, 1_000_000)), 5_000_000);
        assert_eq!(
            price.cost_micros(Usage::new(1_000_000, 1_000_000)),
            6_250_000
        );
    }

    #[test]
    fn zero_usage_is_zero_cost() {
        let price = ModelPrice::new("vertex", "gemini-1.5-flash", 75_000, 300_000);
        assert_eq!(price.cost_micros(Usage::default()), 0);
    }

    #[test]
    fn rounds_half_up_to_nearest_micro() {
        // 1 token at 1_500_000 micros/1M = 1.5 micros -> rounds to 2.
        let price = ModelPrice::new("p", "m", 1_500_000, 0);
        assert_eq!(price.cost_micros(Usage::new(1, 0)), 2);
        // 1 token at 500_000 micros/1M = 0.5 micros -> rounds to 1.
        let price = ModelPrice::new("p", "m", 500_000, 0);
        assert_eq!(price.cost_micros(Usage::new(1, 0)), 1);
        // 1 token at 400_000 micros/1M = 0.4 micros -> rounds to 0.
        let price = ModelPrice::new("p", "m", 400_000, 0);
        assert_eq!(price.cost_micros(Usage::new(1, 0)), 0);
    }

    #[test]
    fn cost_is_rounded_once_over_the_summed_legs() {
        // Both legs individually round to 0.5 micros. Rounding each leg
        // independently would give 1 + 1 = 2; rounding the SUM (1.0 micro) gives
        // 1. The summed-once result is correct and must be 1.
        let price = ModelPrice::new("p", "m", 500_000, 500_000);
        assert_eq!(price.cost_micros(Usage::new(1, 1)), 1);
    }

    #[test]
    fn saturates_instead_of_wrapping_on_absurd_inputs() {
        // A pathological price × token count whose true cost exceeds i64::MAX
        // must saturate to i64::MAX, never wrap to a small/negative number.
        let price = ModelPrice::new("p", "m", i64::MAX, i64::MAX);
        assert_eq!(price.cost_micros(Usage::new(u64::MAX, u64::MAX)), i64::MAX);
    }

    #[test]
    fn negative_prices_are_clamped_to_zero() {
        // A negative rate must never survive: it would manufacture budget.
        let price = ModelPrice::new("p", "m", -5_000_000, -1);
        assert_eq!(price.input_per_1m_micros, 0);
        assert_eq!(price.output_per_1m_micros, 0);
        assert_eq!(price.cost_micros(Usage::new(1_000_000, 1_000_000)), 0);
    }

    #[test]
    fn zero_price_model_is_representable_and_free() {
        // A free / loss-leader model prices at 0 and never panics.
        let price = ModelPrice::new("p", "free-model", 0, 0);
        assert_eq!(price.cost_micros(Usage::new(1_000_000, 1_000_000)), 0);
    }

    #[test]
    fn large_usage_does_not_overflow() {
        // 1 billion tokens at $15/1M output - far beyond any real request -
        // must still compute without overflow: 1e9 × 15e6 / 1e6 = 1.5e10 micros.
        let price = ModelPrice::new("anthropic", "claude-3-5-sonnet", 3_000_000, 15_000_000);
        assert_eq!(
            price.cost_micros(Usage::new(0, 1_000_000_000)),
            15_000_000_000
        );
    }

    #[test]
    fn pricebook_lookup_and_cost() {
        // Fixtures only - not shipped prices.
        let book = PriceBook::from_prices(vec![
            ModelPrice::new("vertex", "flash", 75_000, 300_000),
            ModelPrice::new("anthropic", "sonnet", 3_000_000, 15_000_000),
        ]);
        assert_eq!(book.len(), 2);
        let cost = book
            .cost_micros("vertex", "flash", Usage::new(1_000_000, 1_000_000))
            .expect("flash is priced");
        assert_eq!(cost, 375_000); // 0.075 + 0.30 = 0.375
        // Unknown model returns None - callers fail closed, not free.
        assert!(
            book.cost_micros("vertex", "unpriced", Usage::new(1, 1))
                .is_none()
        );
        assert!(book.lookup("openai", "gpt-5").is_none());
    }

    #[test]
    fn long_context_prompts_are_re_rated_not_under_charged() {
        // Several current models bill the WHOLE prompt at a higher rate above a
        // threshold. A single rate per class cannot express that, and ignoring
        // it under-charges every large request by the tier multiple.
        // Tiering rides on the price, so every cost AND every reservation
        // inherits it and a new call site cannot forget to apply it.
        //
        // OFF by default: `off` is a plain price. Defaulting to an uplift would
        // invent a charge on models that bill flat, and would break the
        // definition of the unit (one million tokens costing the per-million
        // rate), which two existing tests correctly pin.
        let off = ModelPrice::new("anthropic", "m", 1_000_000, 5_000_000);
        assert_eq!(
            off.cost_micros(Usage::new(1_000_000, 0)),
            1_000_000,
            "the default must not change what a rate means"
        );
        // 2x input, 1.5x output: the real shape of a provider tier.
        let on = off.clone().with_long_context(tier(200_000, 2_000, 1_500));

        // Under the threshold: identical either way.
        let small = Usage::new(100_000, 1_000);
        assert_eq!(on.cost_micros(small), off.cost_micros(small));

        // Over it: BOTH legs are re-rated, each by its own multiple. Re-rating
        // only the prompt was the earlier bug: it left the output leg of every
        // long request under-charged, which is the thing this exists to fix.
        // 300k input at 1/1M doubled = 600_000; 1k output at 5/1M by 1.5 = 7_500.
        let big = Usage::new(300_000, 1_000);
        assert_eq!(on.cost_micros(big), 600_000 + 7_500);
        assert_eq!(off.cost_micros(big), 300_000 + 5_000);

        // Cache classes count toward the threshold and are re-rated with it: a
        // 300k prompt served from cache is still a 300k prompt to the provider.
        let cached = Usage::with_cache(0, 1_000, 300_000, 0);
        assert!(on.cost_micros(cached) > off.cost_micros(cached));

        // The RESERVATION is re-rated on both legs too, or settle exceeds
        // reserve on exactly the largest requests and walks a hard cap.
        let prof = profile(false, false);
        let reserved = on.reserve_micros(300_000, 1_000, prof);
        assert!(reserved > off.reserve_micros(300_000, 1_000, prof));
        let settled = on.cost_micros(Usage::new(300_000, 1_000));
        assert!(
            settled <= reserved,
            "settle {settled} exceeded reserve {reserved}"
        );
    }

    #[test]
    fn a_request_straddling_the_threshold_cannot_settle_above_its_reservation() {
        // The reservation sees the COUNTED prompt; settlement sees the prompt the
        // provider REPORTS. Token counters are approximate, so a request counted
        // at 199,995 can report 200,005. Without a guard band that flips a 1x
        // reservation into a 2x settlement of the whole prompt leg: a ten-token
        // discrepancy breaching a hard cap by the entire multiple.
        let p = ModelPrice::new("anthropic", "m", 1_000_000, 5_000_000)
            .with_long_context(tier(200_000, 2_000, 1_500));
        let prof = profile(false, false);

        let reserved = p.reserve_micros(199_995, 1_000, prof);
        let settled = p.cost_micros(Usage::new(200_005, 1_000));

        // What the band guarantees is NOT settle <= reserve outright. A provider
        // reporting more tokens than we counted overshoots on any request, tier
        // or no tier, which is the documented limit of `exact` admission. What
        // it prevents is that drift being MULTIPLIED: the overshoot stays
        // proportional to the ten-token discrepancy instead of the whole prompt
        // being re-rated.
        let overshoot = settled - reserved;
        assert!(
            overshoot <= 10 * 2 * 5,
            "overshoot {overshoot} is larger than the token drift can explain"
        );

        // Without the band this request sits just under the threshold, so it
        // would reserve at 1x and settle at 2x: the breach would be the entire
        // prompt leg rather than a rounding of it.
        let untiered = ModelPrice::new("anthropic", "m", 1_000_000, 5_000_000);
        let without_band = untiered.reserve_micros(199_995, 1_000, prof);
        assert!(
            reserved > without_band,
            "the band must reserve at the tier rate below the threshold"
        );
        assert!(
            settled - without_band > 100_000,
            "without the band the breach would be the whole multiple"
        );

        // Well below the band, no uplift is reserved: it costs transient
        // headroom near the threshold, not everywhere.
        let far_below = p.reserve_micros(100_000, 1_000, prof);
        assert_eq!(far_below, 100_000 + 5_000);
    }

    #[test]
    fn a_double_billed_prompt_is_sized_for_the_tier_it_will_settle_at() {
        // Where the prompt bills on two legs, it also SIZES as two legs: the
        // settlement resolves the threshold against the sum of the classes, so a
        // counted prompt at 60% of the threshold can settle at 120% of it. Five
        // percent of guard band cannot cover a factor of two, so the reservation
        // has to assume the same worst case the rate already assumes.
        let p = ModelPrice::new("openai", "m", 1_000_000, 5_000_000)
            .with_long_context(tier(200_000, 2_000, 1_500));

        let doubled = p.reserve_micros(120_000, 1_000, profile(false, true));
        let disjoint = p.reserve_micros(120_000, 1_000, profile(false, false));

        // The disjoint profile is nowhere near the band at 120k and reserves 1x.
        assert_eq!(disjoint, 120_000 + 5_000);
        // The double-billing profile sizes at 240k, over the threshold, so both
        // legs carry their multiple. The rate is doubled by the profile too.
        assert_eq!(doubled, 120_000 * 2 * 2 + 5_000 * 3 / 2);
        // And that reservation covers the settlement it was sized for.
        let settled = p.cost_micros(Usage::with_cache(120_000, 1_000, 120_000, 0));
        assert!(
            settled <= doubled,
            "settle {settled} exceeded reserve {doubled}"
        );
    }

    #[test]
    fn per_model_tier_falls_back_field_by_field() {
        let deployment = tier(200_000, 2_000, 1_500);
        let base = || ModelPrice::new("p", "m", 1_000_000, 1_000_000);

        // Nothing set on the row: inherit the deployment tier whole.
        let inherited = base().with_long_context_row(None, None, None, deployment);
        assert_eq!(inherited.long_context, deployment);

        // Per FIELD, not all-or-nothing, so a model can take just the threshold
        // and inherit the multiples, or the reverse.
        let partial = base().with_long_context_row(Some(500_000), None, None, deployment);
        assert_eq!(partial.long_context.threshold_tokens, 500_000);
        assert_eq!(partial.long_context.multiple_permille, 2_000);
        assert_eq!(partial.long_context.output_multiple_permille, 1_500);

        let only_output = base().with_long_context_row(None, None, Some(1_250), deployment);
        assert_eq!(only_output.long_context.threshold_tokens, 200_000);
        assert_eq!(only_output.long_context.output_multiple_permille, 1_250);

        // An explicit zero threshold on the row disables tiering for THIS model
        // while other models keep the deployment default, which is the whole
        // reason the columns exist.
        let disabled = base().with_long_context_row(Some(0), None, None, deployment);
        assert!(!disabled.long_context.applies_to(10_000_000));
        assert_eq!(disabled.cost_micros(Usage::new(1_000_000, 0)), 1_000_000);

        // A negative or nonsense stored value floors rather than wrapping into a
        // huge multiple.
        let bad = base().with_long_context_row(Some(-1), Some(-5), Some(-5), deployment);
        assert_eq!(bad.long_context.threshold_tokens, 0);
        assert_eq!(bad.long_context.multiple_permille, 1_000);

        // Inherited multiples must be distinguishable from row-set ones, or a
        // model deliberately disabled with `--long-context-threshold 0` looks
        // identical to a misconfiguration and gets warned about on every reload.
        assert!(!disabled.long_context_uplift_from_row, "inherited");
        let explicit = base().with_long_context_row(Some(0), Some(2_000), None, deployment);
        assert!(explicit.long_context_uplift_from_row, "set on the row");

        // An explicit 1.0x is a deliberate no-op, not a stranded uplift: there
        // is nothing for the inert-tier warning to be about. Without this, a
        // disable written as `--long-context-threshold 0
        // --long-context-input-permille 1000` would warn every reload about an
        // output multiple that came from the DEPLOYMENT, not the row.
        let no_op = base().with_long_context_row(Some(0), Some(1_000), None, deployment);
        assert!(!no_op.long_context_uplift_from_row, "1.0x strands nothing");
    }

    #[test]
    fn a_deployment_uplift_with_no_threshold_is_stranded() {
        // Refused at boot. It re-rates nothing, and because the under-charge
        // warning needs a threshold to test against it logs nothing either, so
        // the operator sees a 2x configured and gets no signal at all.
        assert!(tier(0, 2_000, 1_000).has_stranded_uplift());
        assert!(tier(0, 1_000, 1_500).has_stranded_uplift());

        // A threshold with no uplift is the documented opt-in posture: warn
        // about long requests, do not re-rate them.
        assert!(!tier(200_000, 1_000, 1_000).has_stranded_uplift());
        // Nothing configured at all is fine.
        assert!(!tier(0, 1_000, 1_000).has_stranded_uplift());
        // A working tier is obviously fine.
        assert!(!tier(200_000, 2_000, 1_500).has_stranded_uplift());

        // The same shape on a RESOLVED per-model tier is a model deliberately
        // disabled, inheriting the deployment multiples. It must not be refused,
        // which is why this check is not part of `validate()`.
        let deployment = tier(200_000, 2_000, 1_500);
        let disabled = ModelPrice::new("p", "m", 1_000_000, 1_000_000).with_long_context_row(
            Some(0),
            None,
            None,
            deployment,
        );
        assert!(disabled.long_context.has_stranded_uplift());
        assert!(disabled.long_context.validate().is_ok());
    }

    #[test]
    fn an_unverified_upstream_sizes_its_prompt_upward() {
        // On an unverified upstream the prompt is billed on BOTH legs, because
        // we do not know whether the cached count sits inside the prompt or
        // beside it. The threshold must resolve the same way, upward.
        //
        // Taking the larger class instead would under-size a genuinely disjoint
        // report (150k fresh + 100k cached IS a 250k prompt) and skip a tier the
        // provider applied: an under-charge on exactly the requests this feature
        // exists to catch. An earlier version billed conservatively and sized
        // optimistically, which is the worst of both.
        let overlapping = Usage::with_cache(150_000, 100, 100_000, 0).with_overlapping_classes();
        assert_eq!(overlapping.total_prompt_tokens(), 250_000);
        assert_eq!(
            overlapping.threshold_prompt_tokens(),
            250_000,
            "sizing must not be more optimistic than billing"
        );

        let p = ModelPrice::new("openai", "m", 1_000_000, 1_000_000)
            .with_long_context(tier(200_000, 2_000, 2_000));
        let disjoint = Usage::with_cache(150_000, 100, 100_000, 0);
        assert_eq!(
            p.cost_micros(disjoint),
            p.cost_micros(overlapping),
            "the overlap flag must not change what a request costs"
        );
    }

    #[test]
    fn a_long_context_multiple_below_one_is_rejected() {
        // Would make a large request cheaper than a small one, when providers
        // charge more for it.
        assert!(tier(200_000, 500, 1_000).validate().is_err());
        // The output multiple is bounded on both sides too, or re-rating output
        // downward would under-charge exactly the requests being re-rated.
        assert!(tier(200_000, 2_000, 500).validate().is_err());
        assert!(LongContextTier::default().validate().is_ok());
        // A units mistake (2_000_000 meaning 2x) would saturate the money path
        // to i64::MAX, which no budget counter can come back from.
        assert!(tier(200_000, 2_000_000, 1_000).validate().is_err());
        assert!(tier(200_000, 2_000, 2_000_000).validate().is_err());
        // Threshold 0 disables the feature, but the multiples are STILL checked:
        // a per-model row that sets a threshold inherits these, and that
        // inherited value never passes a database CHECK, so boot is the only
        // place it can be caught.
        assert!(tier(0, 1_000, 1_000).validate().is_ok());
        assert!(tier(0, 2_000_000, 1_000).validate().is_err());

        // The default is a no-op uplift with a threshold to notice at, so a
        // large prompt is flagged as possibly under-charged rather than being
        // silently re-rated on a model that may not tier at all.
        let d = LongContextTier::default();
        assert!(!d.applies_to(500_000), "default must not change any cost");
        assert!(
            d.is_unpriced_long_context(500_000),
            "but it must be visible"
        );
        assert!(!d.is_unpriced_long_context(1_000));
    }

    #[test]
    fn unpriced_cache_classes_are_never_free() {
        // The whole reason the rate columns are nullable. A model priced before
        // cache rates existed must not start metering cache tokens at zero.
        let p = ModelPrice::new("anthropic", "m", 3_000_000, 15_000_000);
        assert!(p.cache_rates_are_fallback);
        assert_eq!(p.cache_read_per_1m_micros, 3_000_000); // 1.0x input
        assert_eq!(p.cache_write_per_1m_micros, 6_000_000); // 2.0x input
        // 1M cache-read tokens cost something, not nothing.
        assert!(p.cost_micros(Usage::with_cache(0, 0, 1_000_000, 0)) > 0);
    }

    #[test]
    fn operator_rates_replace_the_fallback_per_class() {
        let fb = CacheRateFallback::default();
        // Read priced, write left unpriced: only the write falls back.
        let p = ModelPrice::new("anthropic", "m", 3_000_000, 15_000_000).with_cache_rates(
            Some(300_000),
            None,
            fb,
        );
        assert_eq!(p.cache_read_per_1m_micros, 300_000);
        assert_eq!(p.cache_write_per_1m_micros, 6_000_000);
        assert!(p.cache_rates_are_fallback);
        // Both priced: no fallback in play.
        let p = p.with_cache_rates(Some(300_000), Some(3_750_000), fb);
        assert!(!p.cache_rates_are_fallback);
        assert_eq!(p.cache_write_per_1m_micros, 3_750_000);
    }

    #[test]
    fn cost_rounds_once_across_all_four_legs() {
        // Each leg is half a micro. Rounding legs independently would give 4;
        // rounding the sum (2.0 micros) gives 2.
        let p = ModelPrice::new("p", "m", 500_000, 500_000).with_cache_rates(
            Some(500_000),
            Some(500_000),
            CacheRateFallback::default(),
        );
        assert_eq!(p.cost_micros(Usage::with_cache(1, 1, 1, 1)), 2);
    }

    #[test]
    fn reservation_covers_the_write_rate_only_where_writes_can_be_reported() {
        // Unpriced write resolves to 2x input. Reserving at that rate on an
        // adapter that can never report a write would double every reservation
        // in the deployment and start refusing traffic that used to fit.
        let p = ModelPrice::new("p", "m", 3_000_000, 15_000_000);
        assert_eq!(p.reserve_prompt_rate(profile(false, false)), 3_000_000);
        assert_eq!(p.reserve_prompt_rate(profile(true, false)), 6_000_000);
    }

    /// Build a tier without spelling the struct out at every call site.
    fn tier(threshold: u64, input_permille: u32, output_permille: u32) -> LongContextTier {
        LongContextTier {
            threshold_tokens: threshold,
            multiple_permille: input_permille,
            output_multiple_permille: output_permille,
        }
    }

    /// Build a reserve profile without spelling the struct out at every call.
    fn profile(can_cache_write: bool, may_double_bill_prompt: bool) -> PromptReserveProfile {
        PromptReserveProfile {
            can_cache_write,
            may_double_bill_prompt,
        }
    }

    #[test]
    fn reservation_covers_a_prompt_billed_on_two_legs_at_once() {
        // On an upstream whose cache convention is unverified, the parser bills
        // the whole prompt AND the cached count, because that is the only
        // reading that cannot under-charge. The reservation therefore has to
        // cover the SUM of the two rates, not the larger of them: reserving the
        // larger would leave every cache hit settling above what was admitted.
        let p = ModelPrice::new("openai", "m", 1_000_000, 1_000_000);
        assert_eq!(p.reserve_prompt_rate(profile(false, false)), 1_000_000);
        assert_eq!(p.reserve_prompt_rate(profile(false, true)), 2_000_000);

        // Worst case end to end: a 1000-token prompt reported as entirely
        // cached, so the parser bills 1000 fresh plus 1000 cache-read.
        let reserved = p.reserve_micros(1_000, 0, profile(false, true));
        let settled = p.cost_micros(Usage::with_cache(1_000, 0, 1_000, 0));
        assert!(
            settled <= reserved,
            "settle {settled} exceeded reserve {reserved}"
        );
    }

    #[test]
    fn reservation_covers_a_read_rate_set_above_the_input_rate() {
        // Only a >= 0 check guards the read rate in the database, so a
        // fat-fingered rate above the input rate must still be reserved for,
        // otherwise settle exceeds reserve and the exact hard cap breaks.
        let p = ModelPrice::new("p", "m", 1_000_000, 1_000_000).with_cache_rates(
            Some(9_000_000),
            None,
            CacheRateFallback::default(),
        );
        assert_eq!(p.reserve_prompt_rate(profile(false, false)), 9_000_000);
        // A whole prompt served from cache still settles within its reservation.
        let reserved = p.reserve_micros(1_000, 0, profile(false, false));
        let settled = p.cost_micros(Usage::with_cache(0, 0, 1_000, 0));
        assert!(
            settled <= reserved,
            "settle {settled} exceeded reserve {reserved}"
        );
    }

    #[test]
    fn fallback_multiples_below_one_are_rejected() {
        assert!(CacheRateFallback::default().validate().is_ok());
        let bad = CacheRateFallback {
            read_permille: 100,
            write_permille: 2_000,
        };
        assert!(bad.validate().unwrap_err().contains("cache_read"));
        // Zero is the fail-open the nullable columns exist to prevent.
        let zero = CacheRateFallback {
            read_permille: 0,
            write_permille: 0,
        };
        assert!(zero.validate().is_err());
    }

    #[test]
    fn fallback_multiple_rounds_up_and_uses_integer_math() {
        // 1.0x of an odd rate is exact; anything inexact must round UP so the
        // fallback stays on the over-charging side.
        assert_eq!(CacheRateFallback::apply(999_999, 1_000), 999_999);
        assert_eq!(CacheRateFallback::apply(999_999, 2_000), 1_999_998);
        // 1001 per-mille of 1 micro is 1.001, which must not round down to 1.
        assert_eq!(CacheRateFallback::apply(1, 1_001), 2);
        // Saturates rather than wrapping.
        assert_eq!(CacheRateFallback::apply(i64::MAX, 2_000), i64::MAX);
    }

    #[test]
    fn total_prompt_tokens_restores_the_pre_split_series() {
        let u = Usage::with_cache(5, 22, 200_000, 1_000);
        assert_eq!(u.total_prompt_tokens(), 201_005);
    }

    #[test]
    fn implausible_usage_is_detected_on_either_leg() {
        // Real requests, however large, stay well under the ceiling.
        assert!(!Usage::new(2_000_000, 100_000).is_implausible());
        assert!(
            !Usage::new(MAX_PLAUSIBLE_TOKENS_PER_LEG, MAX_PLAUSIBLE_TOKENS_PER_LEG)
                .is_implausible()
        );
        // Either leg alone is enough to condemn the response.
        assert!(Usage::new(MAX_PLAUSIBLE_TOKENS_PER_LEG + 1, 0).is_implausible());
        assert!(Usage::new(0, MAX_PLAUSIBLE_TOKENS_PER_LEG + 1).is_implausible());
        assert!(Usage::new(u64::MAX, u64::MAX).is_implausible());
    }

    #[test]
    fn implausible_usage_is_what_would_have_saturated_the_money_path() {
        // Demonstrates why the guard exists: costing this saturates to i64::MAX,
        // which the budget counter can never come back down from.
        let price = ModelPrice::new("p", "m", 3_000_000, 15_000_000);
        let bad = Usage::new(u64::MAX, u64::MAX);
        assert!(bad.is_implausible());
        assert_eq!(price.cost_micros(bad), i64::MAX);
    }

    #[test]
    fn format_micros_examples() {
        assert_eq!(format_micros(0), "0.000000");
        assert_eq!(format_micros(1_250_000), "1.250000");
        assert_eq!(format_micros(10_500), "0.010500");
        assert_eq!(format_micros(-500_000), "-0.500000");
    }
}
