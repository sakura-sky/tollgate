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
    /// Prompt tokens above which the multiple applies. Zero disables it.
    pub threshold_tokens: u64,
    /// Applied to every prompt-side rate once the threshold is crossed.
    pub multiple_permille: u32,
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
        }
    }
}

impl LongContextTier {
    /// Whether an uplift applies to this prompt. False when no multiple is
    /// configured, even above the threshold.
    #[must_use]
    pub fn applies_to(&self, prompt_tokens: u64) -> bool {
        self.threshold_tokens > 0
            && self.multiple_permille > 1_000
            && prompt_tokens > self.threshold_tokens
    }

    /// Whether this prompt is large enough that the provider may be tiering it
    /// while Tollgate is not. Used to log the gap rather than silently accept it.
    #[must_use]
    pub fn is_unpriced_long_context(&self, prompt_tokens: u64) -> bool {
        self.threshold_tokens > 0
            && self.multiple_permille <= 1_000
            && prompt_tokens > self.threshold_tokens
    }

    /// Reject a multiple that would make a large request CHEAPER, which would
    /// invert the whole point.
    ///
    /// # Errors
    /// Returns a message when the multiple is below 1.0x.
    pub fn validate(&self) -> Result<(), String> {
        if self.threshold_tokens == 0 {
            return Ok(());
        }
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

/// Divide a micros-scaled numerator (`Σ tokens × price_per_1M`) by one million,
/// rounding half-up to the nearest micro.
///
/// The `i128` intermediate cannot overflow for any `u64` token count times `i64`
/// price. As a last resort the result saturates into `i64` rather than wrapping,
/// but callers MUST reject implausible token counts upstream (see the metering
/// layer) so saturation never actually occurs on the money path.
#[must_use]
fn round_to_micros(numerator: i128) -> i64 {
    let rounded = numerator.saturating_add(TOKENS_PER_MILLION / 2) / TOKENS_PER_MILLION;
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

    /// The uplift owed on a prompt that crosses the long-context threshold.
    ///
    /// The multiple applies to the WHOLE prompt, not just the excess, because
    /// that is how providers bill it: crossing the threshold re-rates the entire
    /// request rather than the tokens beyond it.
    fn long_context_uplift(&self, usage: Usage) -> i64 {
        if !self.long_context.applies_to(usage.total_prompt_tokens()) {
            return 0;
        }
        let prompt_only = Usage::with_cache(
            usage.input_tokens,
            0,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
        );
        let prompt_base = self.cost_micros_untiered(prompt_only);
        let uplift = i128::from(prompt_base)
            * i128::from(self.long_context.multiple_permille.saturating_sub(1_000))
            / 1_000;
        i64::try_from(uplift).unwrap_or(i64::MAX)
    }

    /// Cost of `usage` at this price, in micros, INCLUDING long-context
    /// re-rating.
    #[must_use]
    pub fn cost_micros(&self, usage: Usage) -> i64 {
        let base = self.cost_micros_untiered(usage);
        base.saturating_add(self.long_context_uplift(usage))
    }

    /// Cost at the flat per-class rates, before any long-context re-rating.
    #[must_use]
    fn cost_micros_untiered(&self, usage: Usage) -> i64 {
        // Sum ALL FOUR legs at full micro-precision, THEN round once. Rounding
        // each leg independently multiplies the rounding error by the number of
        // legs and can over-count a request's cost.
        let input = i128::from(usage.input_tokens) * i128::from(self.input_per_1m_micros);
        let output = i128::from(usage.output_tokens) * i128::from(self.output_per_1m_micros);
        let read = i128::from(usage.cache_read_tokens) * i128::from(self.cache_read_per_1m_micros);
        let write =
            i128::from(usage.cache_write_tokens) * i128::from(self.cache_write_per_1m_micros);
        // saturating_add: near-maximal legs can overflow even i128.
        round_to_micros(
            input
                .saturating_add(output)
                .saturating_add(read)
                .saturating_add(write),
        )
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
        let mut rate = i128::from(self.reserve_prompt_rate(profile));
        // A prompt that will be re-rated for length must be RESERVED at the
        // re-rated price too, or settle exceeds reserve on exactly the largest
        // requests and walks a hard cap.
        if self.long_context.applies_to(prompt_tokens) {
            rate = rate * i128::from(self.long_context.multiple_permille) / 1_000;
        }
        let prompt = i128::from(prompt_tokens) * rate;
        let output = i128::from(max_output_tokens) * i128::from(self.output_per_1m_micros);
        round_to_micros(prompt.saturating_add(output))
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
        let on = off.clone().with_long_context(LongContextTier {
            threshold_tokens: 200_000,
            multiple_permille: 2_000,
        });

        // Under the threshold: identical either way.
        let small = Usage::new(100_000, 1_000);
        assert_eq!(on.cost_micros(small), off.cost_micros(small));

        // Over it: the PROMPT leg doubles, output does not.
        let big = Usage::new(300_000, 1_000);
        assert_eq!(
            on.cost_micros(big),
            off.cost_micros(big) + 300_000,
            "prompt leg should double, output unchanged"
        );

        // Cache classes count toward the threshold and are re-rated with it: a
        // 300k prompt served from cache is still a 300k prompt to the provider.
        let cached = Usage::with_cache(0, 1_000, 300_000, 0);
        assert!(on.cost_micros(cached) > off.cost_micros(cached));

        // The RESERVATION is re-rated too, or settle would exceed reserve on
        // exactly the largest requests and walk a hard cap.
        let prof = profile(false, false);
        assert!(on.reserve_micros(300_000, 1_000, prof) > off.reserve_micros(300_000, 1_000, prof));
        let reserved = on.reserve_micros(300_000, 1_000, prof);
        let settled = on.cost_micros(Usage::new(300_000, 1_000));
        assert!(
            settled <= reserved,
            "settle {settled} exceeded reserve {reserved}"
        );
    }

    #[test]
    fn a_long_context_multiple_below_one_is_rejected() {
        // Would make a large request cheaper than a small one, when providers
        // charge more for it.
        let bad = LongContextTier {
            threshold_tokens: 200_000,
            multiple_permille: 500,
        };
        assert!(bad.validate().is_err());
        assert!(LongContextTier::default().validate().is_ok());
        // A units mistake (2_000_000 meaning 2x) would saturate the money path.
        let absurd = LongContextTier {
            threshold_tokens: 200_000,
            multiple_permille: 2_000_000,
        };
        assert!(absurd.validate().is_err());
        // Threshold 0 disables the feature, so the multiple is not checked.
        let off = LongContextTier {
            threshold_tokens: 0,
            multiple_permille: 0,
        };
        assert!(off.validate().is_ok());

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
