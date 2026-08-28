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
//! Lua script. A mandatory `global` budget is the backstop: a request that
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

/// What a budget applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The whole deployment (mandatory backstop).
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
}

impl Reservation {
    /// Build a reservation from counter-key/amount pairs (used by backends).
    #[must_use]
    pub fn from_entries(entries: Vec<(String, i64)>) -> Self {
        Self { entries }
    }

    /// The (counter key, reserved micros) pairs this reservation holds.
    #[must_use]
    pub fn entries(&self) -> &[(String, i64)] {
        &self.entries
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
    /// budget applies at all (the mandatory-backstop rule).
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
            entries.push((key, reserve_micros));
        }
        Ok(Reservation { entries })
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
}
