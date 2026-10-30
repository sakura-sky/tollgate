// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Budget resolution and enforcement.
//!
//! A request is checked against every budget whose scope applies to it -
//! `global` (always), the request's `api_key`, its `provider`, and its
//! `provider:model`. A hard-stop budget rejects the request when the reservation
//! would push its period counter over the limit.
//!
//! ## Reserve then settle (why two steps)
//!
//! Cost is dominated by *output* tokens, which aren't known until the provider
//! responds. Checking only an input estimate would let one long completion
//! overshoot a nearly-empty budget. So the gateway **reserves** the maximum a
//! request could cost *before* forwarding, and after metering **settles** the
//! reservation to the actual cost (releasing the difference). A request that
//! can't reserve is rejected before it ever reaches the provider.
//!
//! This in-memory implementation is the demo backend and the behavioural
//! oracle; production runs the identical check-and-increment as a single Redis
//! Lua script. A `global` budget is the recommended backstop, not an enforced
//! one: nothing refuses to start without it, and its absence is only logged by
//! the reload task. What IS enforced is that a request matching no budget at
//! all is denied. A request that
//! matches no budget is rejected, never allowed (no fail-open by omission).

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Datelike, Utc};

/// Budget period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Daily,
    Weekly,
    Monthly,
}

impl Period {
    /// Deterministic period bucket key for `now` (e.g. monthly → `2026-08`).
    #[must_use]
    pub fn bucket(self, now: DateTime<Utc>) -> String {
        match self {
            Period::Daily => now.format("%Y-%m-%d").to_string(),
            Period::Weekly => {
                let iso = now.iso_week();
                format!("{}-W{:02}", iso.year(), iso.week())
            }
            Period::Monthly => now.format("%Y-%m").to_string(),
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Period::Daily => "daily",
            Period::Weekly => "weekly",
            Period::Monthly => "monthly",
        }
    }
}

/// Every provider id this build can register, and so every id a `provider:`
/// or `model:` budget scope may name. The router registers and scopes requests
/// under these constants, and `admin budget set` validates against
/// [`PROVIDER_IDS`]. Add any new provider to that list.
pub const PROVIDER_ANTHROPIC: &str = "anthropic";
pub const PROVIDER_MOCK: &str = "mock";
pub const PROVIDER_OPENAI: &str = "openai";
pub const PROVIDER_VERTEX: &str = "vertex";
pub const PROVIDER_IDS: [&str; 4] = [
    PROVIDER_ANTHROPIC,
    PROVIDER_MOCK,
    PROVIDER_OPENAI,
    PROVIDER_VERTEX,
];

/// The canonical text of an API key id: the lowercase, hyphenated UUID that
/// [`RequestCtx::key_id`] carries, which is what an `api_key` scope is compared
/// against. Accepts, after trimming surrounding whitespace, every form
/// `uuid::Uuid::parse_str` accepts and a `urn:uuid:` prefix in any case. `None`
/// when the text is not a UUID at all.
///
/// A per-key budget stored in any other form never matches its key, which
/// silently leaves the key under only the wider budgets: a fail-open. Every
/// place an id enters or is read back from the budget table goes through this.
#[must_use]
pub fn canonical_key_id(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let bare = match raw.get(..9) {
        Some(p) if p.eq_ignore_ascii_case("urn:uuid:") => &raw[9..],
        _ => raw,
    };
    uuid::Uuid::parse_str(bare).ok().map(|u| u.to_string())
}

/// Collapse budgets that share a scope and period into one, keeping the lower
/// limit and a hard stop if either was one, so two rows for the same key never
/// both reserve against one counter. Returns the merged list and a description
/// of each merge for the caller to log.
///
/// The schema refuses such duplicates once migration 0011 has run, but a row
/// written outside the CLI, or before the migration, must still enforce once.
#[must_use]
pub fn merge_duplicate_budgets(budgets: Vec<Budget>) -> (Vec<Budget>, Vec<String>) {
    let mut out: Vec<Budget> = Vec::with_capacity(budgets.len());
    let mut merged = Vec::new();
    for b in budgets {
        if let Some(existing) = out
            .iter_mut()
            .find(|e| e.scope == b.scope && e.period == b.period)
        {
            merged.push(format!(
                "{} ({}): limits {} and {}; keeping {}",
                b.scope,
                b.period.as_str(),
                existing.limit_micros,
                b.limit_micros,
                existing.limit_micros.min(b.limit_micros)
            ));
            existing.limit_micros = existing.limit_micros.min(b.limit_micros);
            existing.hard_stop = existing.hard_stop || b.hard_stop;
        } else {
            out.push(b);
        }
    }
    (out, merged)
}

/// What a budget applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The whole deployment. The recommended backstop, not an enforced one.
    Global,
    /// One API key, by id.
    ApiKey(String),
    /// One provider, e.g. `vertex`.
    Provider(String),
    /// One `provider:model`.
    Model(String),
}

impl Scope {
    /// Whether this scope applies to the given request.
    #[must_use]
    pub fn applies_to(&self, ctx: &RequestCtx<'_>) -> bool {
        match self {
            Scope::Global => true,
            Scope::ApiKey(id) => id == ctx.key_id,
            Scope::Provider(p) => p == ctx.provider,
            Scope::Model(pm) => *pm == format!("{}:{}", ctx.provider, ctx.model),
        }
    }

    /// Stable string form of this scope, used in counter keys.
    #[must_use]
    pub fn key_string(&self) -> String {
        match self {
            Scope::Global => "global".to_owned(),
            Scope::ApiKey(id) => format!("api_key:{id}"),
            Scope::Provider(p) => format!("provider:{p}"),
            Scope::Model(pm) => format!("model:{pm}"),
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key_string())
    }
}

/// A configured spend ceiling.
#[derive(Debug, Clone)]
pub struct Budget {
    pub scope: Scope,
    pub period: Period,
    pub limit_micros: i64,
    /// true → breach rejects the request; false → breach only tracks (soft).
    pub hard_stop: bool,
}

/// The request being priced against budgets.
#[derive(Debug, Clone, Copy)]
pub struct RequestCtx<'a> {
    pub key_id: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
}

/// A held reservation: the counters that were incremented and by how much.
///
/// The entries are the period-counter keys and reserved amounts, so any budget
/// backend (in-memory or Redis/Valkey) can settle the same reservation.
#[derive(Debug, Clone)]
pub struct Reservation {
    entries: Vec<(String, i64)>,
    /// The seed generation each counter carried when this reservation was
    /// taken, positionally matching `entries`. Empty for backends that cannot
    /// lose a counter, such as the in-memory one.
    ///
    /// A counter rebuilt from the ledger is not the counter this reservation
    /// was taken against: the rebuilt figure already accounts for everything the
    /// ledger knows, and this reservation is not in it. Settling against the new
    /// counter would subtract a reservation it never held, which for a typical
    /// request is a negative delta, so the counter would end up UNDER the truth
    /// by the whole reserved amount and stay there until the next restart.
    /// Carrying the generation lets settle recognise that and decline.
    generations: Vec<i64>,
}

impl Reservation {
    /// Build a reservation from counter-key/amount pairs (used by backends).
    #[must_use]
    pub fn from_entries(entries: Vec<(String, i64)>) -> Self {
        Self {
            entries,
            generations: Vec::new(),
        }
    }

    /// Attach the seed generations observed at reserve time.
    #[must_use]
    pub fn with_generations(mut self, generations: Vec<i64>) -> Self {
        self.generations = generations;
        self
    }

    /// The (counter key, reserved micros) pairs this reservation holds.
    #[must_use]
    pub fn entries(&self) -> &[(String, i64)] {
        &self.entries
    }

    /// The seed generations observed at reserve time, or empty.
    #[must_use]
    pub fn generations(&self) -> &[i64] {
        &self.generations
    }
}

/// A rejection: which hard budget would have been exceeded. Amounts are shown as
/// currency decimals (not micros, and no unit symbol) so the message reads
/// cleanly for finance and analytics.
#[derive(Debug, Clone)]
pub struct BudgetDenied {
    pub scope: String,
    pub period: &'static str,
    pub limit_micros: i64,
    pub spent_micros: i64,
    pub cost_micros: i64,
}

impl std::fmt::Display for BudgetDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "budget exceeded: {} ({}) limit {} reached (already spent {}, this request would add {})",
            self.scope,
            self.period,
            crate::pricing::format_micros(self.limit_micros),
            crate::pricing::format_micros(self.spent_micros),
            crate::pricing::format_micros(self.cost_micros),
        )
    }
}

impl std::error::Error for BudgetDenied {}

/// A resolved budget that applies to a request: its period counter key, limit,
/// and whether it hard-stops. Backends (in-memory or Redis) act on these.
#[derive(Debug, Clone)]
pub struct BudgetSlot {
    pub scope: String,
    pub period: Period,
    pub counter_key: String,
    pub limit_micros: i64,
    pub hard_stop: bool,
}

/// The period counter key for a scope, e.g. `tollgate:api_key:abc|2026-08`. The
/// `tollgate:` prefix namespaces the keyspace so a shared Redis/Valkey cannot
/// collide with another workload's keys.
#[must_use]
pub fn counter_key(scope: &Scope, period: Period, now: DateTime<Utc>) -> String {
    format!("tollgate:{}|{}", scope.key_string(), period.bucket(now))
}

/// The budgets that apply to a request, as [`BudgetSlot`]s. Used by backends
/// that keep their own budget config (e.g. loaded from the database).
#[must_use]
pub fn resolve(configs: &[Budget], ctx: &RequestCtx<'_>, now: DateTime<Utc>) -> Vec<BudgetSlot> {
    configs
        .iter()
        .filter(|b| b.scope.applies_to(ctx))
        .map(|b| BudgetSlot {
            scope: b.scope.to_string(),
            period: b.period,
            counter_key: counter_key(&b.scope, b.period, now),
            limit_micros: b.limit_micros,
            hard_stop: b.hard_stop,
        })
        .collect()
}

/// In-memory budget store: configs + period counters.
#[derive(Debug)]
pub struct Budgets {
    configs: Vec<Budget>,
    counters: Mutex<HashMap<String, i64>>,
}

impl Budgets {
    /// Build from budget configs. A `Global` budget SHOULD be present; without
    /// one, [`Budgets::try_reserve`] rejects every request (fail closed).
    #[must_use]
    pub fn new(configs: Vec<Budget>) -> Self {
        Self {
            configs,
            counters: Mutex::new(HashMap::new()),
        }
    }

    fn counter_key(scope: &Scope, period: Period, now: DateTime<Utc>) -> String {
        counter_key(scope, period, now)
    }

    /// Reserve `reserve_micros` against all applicable budgets, atomically. If
    /// any hard budget would be exceeded, nothing is incremented and the
    /// offending budget is returned.
    ///
    /// # Errors
    /// Returns [`BudgetDenied`] if a hard budget would be exceeded, or if no
    /// budget applies at all: unbudgeted means refused, never allowed.
    pub fn try_reserve(
        &self,
        ctx: &RequestCtx<'_>,
        now: DateTime<Utc>,
        reserve_micros: i64,
    ) -> Result<Reservation, BudgetDenied> {
        let applicable: Vec<&Budget> = self
            .configs
            .iter()
            .filter(|b| b.scope.applies_to(ctx))
            .collect();

        // C2: a request that matches no budget is rejected, not allowed.
        if applicable.is_empty() {
            return Err(BudgetDenied {
                scope: "<none>".to_owned(),
                period: "n/a",
                limit_micros: 0,
                spent_micros: 0,
                cost_micros: reserve_micros,
            });
        }

        let mut counters = self.counters.lock().unwrap_or_else(|e| e.into_inner());

        // First pass: check every hard budget. Commit nothing yet.
        for b in &applicable {
            if !b.hard_stop {
                continue;
            }
            let key = Self::counter_key(&b.scope, b.period, now);
            let spent = counters.get(&key).copied().unwrap_or(0);
            if spent.saturating_add(reserve_micros) > b.limit_micros {
                return Err(BudgetDenied {
                    scope: b.scope.to_string(),
                    period: b.period.as_str(),
                    limit_micros: b.limit_micros,
                    spent_micros: spent,
                    cost_micros: reserve_micros,
                });
            }
        }

        // Second pass: all hard checks passed - increment every applicable
        // counter (hard and soft) and record the reservation.
        let mut entries = Vec::with_capacity(applicable.len());
        for b in &applicable {
            let key = Self::counter_key(&b.scope, b.period, now);
            let counter = counters.entry(key.clone()).or_insert(0);
            // saturating: a soft budget is incremented every request and never
            // limit-checked, so its counter could otherwise overflow.
            *counter = counter.saturating_add(reserve_micros);
            // Counted but never refused. Say so, or a soft cap is a cap that
            // does nothing AND reports nothing.
            if !b.hard_stop && *counter > b.limit_micros {
                tracing::warn!(
                    scope = %b.scope,
                    period = b.period.as_str(),
                    limit_micros = b.limit_micros,
                    spent_micros = *counter,
                    "soft budget exceeded; the request was NOT refused because this budget \
                     was created with `--hard-stop false`"
                );
            }
            entries.push((key, reserve_micros));
        }
        // No generations: this backend keeps its counters in process memory, so
        // a counter cannot vanish and be rebuilt underneath a live reservation.
        Ok(Reservation::from_entries(entries))
    }

    /// Settle a reservation to the actual cost, releasing (or claiming) the
    /// difference on every counter it touched. Counters never go below zero.
    pub fn settle(&self, reservation: &Reservation, actual_micros: i64) {
        let mut counters = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        for (key, reserved) in &reservation.entries {
            let delta = actual_micros - reserved;
            let entry = counters.entry(key.clone()).or_insert(0);
            *entry = entry.saturating_add(delta).max(0);
        }
    }

    /// Current spend on a scope's period counter (for inspection / the demo).
    #[must_use]
    pub fn spent(&self, scope: &Scope, period: Period, now: DateTime<Utc>) -> i64 {
        let key = Self::counter_key(scope, period, now);
        self.counters
            .lock()
            .expect("budget lock")
            .get(&key)
            .copied()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn ctx<'a>() -> RequestCtx<'a> {
        RequestCtx {
            key_id: "key1",
            provider: "mock",
            model: "demo",
        }
    }

    #[test]
    fn reserve_within_limit_succeeds_and_settles() {
        let budgets = Budgets::new(vec![Budget {
            scope: Scope::Global,
            period: Period::Monthly,
            limit_micros: 1_000,
            hard_stop: true,
        }]);
        let r = budgets.try_reserve(&ctx(), now(), 800).expect("under cap");
        // Reserved 800; settle to actual 300 → counter should be 300.
        budgets.settle(&r, 300);
        assert_eq!(budgets.spent(&Scope::Global, Period::Monthly, now()), 300);
    }

    #[test]
    fn hard_budget_blocks_over_cap_and_leaves_counter_untouched() {
        let budgets = Budgets::new(vec![Budget {
            scope: Scope::Global,
            period: Period::Monthly,
            limit_micros: 1_000,
            hard_stop: true,
        }]);
        budgets.settle(&budgets.try_reserve(&ctx(), now(), 900).unwrap(), 900);
        // 900 spent; a 200 reservation would exceed 1000 → denied, counter stays 900.
        let denied = budgets.try_reserve(&ctx(), now(), 200).unwrap_err();
        assert_eq!(denied.limit_micros, 1_000);
        assert_eq!(budgets.spent(&Scope::Global, Period::Monthly, now()), 900);
    }

    #[test]
    fn per_key_cap_enforced_alongside_global() {
        let budgets = Budgets::new(vec![
            Budget {
                scope: Scope::Global,
                period: Period::Monthly,
                limit_micros: 1_000_000,
                hard_stop: true,
            },
            Budget {
                scope: Scope::ApiKey("key1".to_owned()),
                period: Period::Monthly,
                limit_micros: 500,
                hard_stop: true,
            },
        ]);
        // Global has room, but the per-key cap (500) blocks a 600 reservation.
        let denied = budgets.try_reserve(&ctx(), now(), 600).unwrap_err();
        assert_eq!(denied.scope, "api_key:key1");
    }

    #[test]
    fn no_matching_budget_is_rejected() {
        // No global backstop, and no budget matches this key → fail closed.
        let budgets = Budgets::new(vec![Budget {
            scope: Scope::ApiKey("other".to_owned()),
            period: Period::Monthly,
            limit_micros: 1_000,
            hard_stop: true,
        }]);
        assert!(budgets.try_reserve(&ctx(), now(), 1).is_err());
    }

    /// Every form `parse_str` accepts, plus a `urn:uuid:` prefix in any case and
    /// surrounding whitespace, canonicalises to the id the request carries, so
    /// the budget applies. The
    /// request side is a hard-coded literal, not `Uuid::to_string()`, so this
    /// cannot pass merely because both sides went through the same function.
    #[test]
    fn every_accepted_key_id_form_canonicalises_and_applies() {
        const KEY: &str = "a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8";
        let ctx = RequestCtx {
            key_id: KEY,
            provider: "vertex",
            model: "m",
        };
        for form in [
            "a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8",
            "A1A2A3A4-B1B2-C1C2-D1D2-D3D4D5D6D7D8",
            "a1a2a3a4b1b2c1c2d1d2d3d4d5d6d7d8",
            "{a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8}",
            "urn:uuid:a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8",
            "URN:UUID:A1A2A3A4-B1B2-C1C2-D1D2-D3D4D5D6D7D8",
            " a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8 ",
        ] {
            let canon = canonical_key_id(form).unwrap_or_else(|| panic!("{form:?} rejected"));
            assert_eq!(canon, KEY, "{form:?}");
            assert!(Scope::ApiKey(canon).applies_to(&ctx), "{form:?}");
        }
        // Control: the raw non-canonical text, unconverted, does not apply.
        // This is the fail-open the canonical form closes.
        assert!(!Scope::ApiKey("A1A2A3A4B1B2C1C2D1D2D3D4D5D6D7D8".to_owned()).applies_to(&ctx));
        for bad in [
            "",
            "not-a-uuid",
            "a1a2a3a4",
            "urn:uuid:",
            "g1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8",
        ] {
            assert_eq!(canonical_key_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn provider_ids_are_the_four_this_build_registers() {
        assert_eq!(PROVIDER_IDS, ["anthropic", "mock", "openai", "vertex"]);
    }

    #[test]
    fn duplicate_budgets_merge_to_the_stricter_one() {
        let key = Scope::ApiKey("a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8".to_owned());
        let b = |scope: Scope, period: Period, limit: i64, hard_stop: bool| Budget {
            scope,
            period,
            limit_micros: limit,
            hard_stop,
        };
        let (out, merged) = merge_duplicate_budgets(vec![
            b(key.clone(), Period::Monthly, 500, false),
            b(key.clone(), Period::Monthly, 300, true),
            // Same key, different period: not a duplicate.
            b(key.clone(), Period::Daily, 100, true),
            b(Scope::Global, Period::Monthly, 1_000, true),
        ]);
        assert_eq!(out.len(), 3);
        assert_eq!(merged.len(), 1, "{merged:?}");
        let monthly = out
            .iter()
            .find(|x| x.scope == key && x.period == Period::Monthly)
            .unwrap();
        assert_eq!(monthly.limit_micros, 300);
        assert!(monthly.hard_stop, "a hard stop on either row must survive");
        let (_, none) = merge_duplicate_budgets(vec![b(Scope::Global, Period::Monthly, 1, true)]);
        assert!(none.is_empty());
    }
}
