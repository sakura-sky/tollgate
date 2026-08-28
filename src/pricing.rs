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

/// The USD price for one `(provider, model)`, in micros per 1,000,000 tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub provider: String,
    pub model: String,
    /// USD micros per 1M input tokens.
    pub input_per_1m_micros: i64,
    /// USD micros per 1M output tokens.
    pub output_per_1m_micros: i64,
}

/// Token usage for a single request, normalised across providers by
/// `Provider::parse_usage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    #[must_use]
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
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
        Self {
            provider: provider.into(),
            model: model.into(),
            input_per_1m_micros: input_per_1m_micros.max(0),
            output_per_1m_micros: output_per_1m_micros.max(0),
        }
    }

    /// Cost of `usage` at this price, in USD micros.
    #[must_use]
    pub fn cost_micros(&self, usage: Usage) -> i64 {
        // Sum both legs at full micro-precision, THEN round once. Rounding each
        // leg independently (as an earlier version did) doubles the rounding
        // error and can over-count a request's cost.
        let input = i128::from(usage.input_tokens) * i128::from(self.input_per_1m_micros);
        let output = i128::from(usage.output_tokens) * i128::from(self.output_per_1m_micros);
        // saturating_add: two near-maximal legs can overflow even i128.
        round_to_micros(input.saturating_add(output))
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
    fn format_micros_examples() {
        assert_eq!(format_micros(0), "0.000000");
        assert_eq!(format_micros(1_250_000), "1.250000");
        assert_eq!(format_micros(10_500), "0.010500");
        assert_eq!(format_micros(-500_000), "-0.500000");
    }
}
