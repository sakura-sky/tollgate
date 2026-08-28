// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Production storage backends: Postgres (keys, usage, config) and Redis/Valkey
//! (budget counters). These implement the same [`crate::gateway`] traits the
//! in-memory demo backends do, so the request flow is identical.
//!
//! Redis and Valkey are wire-compatible; the `redis` crate talks to either. Use
//! whichever your platform offers (Valkey is typically cheaper on GCP).

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use redis::aio::ConnectionManager;
use sqlx::{PgPool, Row};

use crate::budget::{Budget, BudgetDenied, Period, RequestCtx, Reservation, Scope, resolve};
use crate::gateway::{BudgetBackend, KeyRecord, KeyStore, ReserveError, UsageEvent, UsageSink};
use crate::pricing::{ModelPrice, PriceBook};

/// Counter keys expire this many seconds after creation, so stale period
/// buckets do not accumulate. ~40 days covers a monthly period with margin.
const COUNTER_TTL_SECS: i64 = 40 * 24 * 60 * 60;

// ---------------------------------------------------------------------------
// Postgres: key store
// ---------------------------------------------------------------------------

/// Looks up API keys in Postgres by prefix (active keys only).
pub struct PgKeyStore {
    pool: PgPool,
}

impl PgKeyStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl KeyStore for PgKeyStore {
    async fn lookup(&self, prefix: &str) -> Option<KeyRecord> {
        // A DB error must be logged, not silently collapsed into "key not found"
        // (which would mask an outage as a wave of 401s).
        let row = match sqlx::query(
            "SELECT id, key_hash FROM api_keys WHERE prefix = $1 AND revoked_at IS NULL",
        )
        .bind(prefix)
        .fetch_optional(&self.pool)
        .await
        {
            Ok(row) => row?,
            Err(e) => {
                tracing::error!(error = %e, "api key lookup failed");
                return None;
            }
        };
        Some(KeyRecord {
            id: row.get::<uuid::Uuid, _>("id").to_string(),
            key_hash: row.get::<String, _>("key_hash"),
        })
    }
}

// ---------------------------------------------------------------------------
// Postgres: usage sink (the spend ledger)
// ---------------------------------------------------------------------------

/// Appends usage events to `usage_events`. Best-effort: a ledger write failure
/// is logged, not surfaced to the client (the budget counter is authoritative
/// on the hot path and is reconciled from this table).
pub struct PgUsageSink {
    pool: PgPool,
}

impl PgUsageSink {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl UsageSink for PgUsageSink {
    async fn record(&self, event: UsageEvent<'_>) {
        let Ok(api_key_id) = uuid::Uuid::parse_str(event.key_id) else {
            tracing::warn!(
                key_id = event.key_id,
                "usage event with non-UUID key id; skipping"
            );
            return;
        };
        let res = sqlx::query(
            "INSERT INTO usage_events \
             (api_key_id, provider, model, input_tokens, output_tokens, cost_micros, \
              gateway_micros, decision) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(api_key_id)
        .bind(event.provider)
        .bind(event.model)
        .bind(i64::try_from(event.usage.input_tokens).unwrap_or(i64::MAX))
        .bind(i64::try_from(event.usage.output_tokens).unwrap_or(i64::MAX))
        .bind(event.cost_micros)
        .bind(event.overhead_micros)
        .bind(event.decision)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            tracing::error!(error = %e, "failed to write usage event");
        }
    }
}

// ---------------------------------------------------------------------------
// Redis/Valkey: budget backend
// ---------------------------------------------------------------------------

/// Enforces budgets with an atomic Redis/Valkey Lua check-and-increment. Holds
/// its own budget config (loaded from Postgres at startup) and computes the same
/// counter keys the in-memory backend uses.
pub struct RedisBudgetBackend {
    conn: ConnectionManager,
    budgets: Vec<Budget>,
}

impl RedisBudgetBackend {
    #[must_use]
    pub fn new(conn: ConnectionManager, budgets: Vec<Budget>) -> Self {
        Self { conn, budgets }
    }
}

// Reserve: check every hard budget, then increment all applicable counters.
// KEYS = counter keys; ARGV = [reserve, ttl, limits..., hard_flags...].
// Returns {failing_index (0-based) or -1, spent_on_failing}.
const RESERVE_LUA: &str = r"
local reserve = tonumber(ARGV[1])
local ttl = tonumber(ARGV[2])
local n = #KEYS
for i = 1, n do
  if ARGV[2 + n + i] == '1' then
    local cur = tonumber(redis.call('GET', KEYS[i]) or '0')
    local limit = tonumber(ARGV[2 + i])
    if cur + reserve > limit then
      return {i - 1, cur}
    end
  end
end
for i = 1, n do
  redis.call('INCRBY', KEYS[i], reserve)
  if ttl > 0 then redis.call('EXPIRE', KEYS[i], ttl) end
end
return {-1, 0}
";

// Settle: apply (actual - reserved) to every counter, flooring at 0.
const SETTLE_LUA: &str = r"
local delta = tonumber(ARGV[1])
for i = 1, #KEYS do
  local v = redis.call('INCRBY', KEYS[i], delta)
  if v < 0 then redis.call('SET', KEYS[i], 0) end
end
return 1
";

#[async_trait]
impl BudgetBackend for RedisBudgetBackend {
    async fn reserve(
        &self,
        ctx: &RequestCtx<'_>,
        reserve_micros: i64,
    ) -> Result<Reservation, ReserveError> {
        let now = Utc::now();
        let slots = resolve(&self.budgets, ctx, now);
        // Mandatory backstop: no applicable budget means reject, never allow.
        if slots.is_empty() {
            return Err(ReserveError::Denied(BudgetDenied {
                scope: "<none>".to_owned(),
                period: "n/a",
                limit_micros: 0,
                spent_micros: 0,
                cost_micros: reserve_micros,
            }));
        }

        let script = redis::Script::new(RESERVE_LUA);
        let mut invocation = script.prepare_invoke();
        for s in &slots {
            invocation.key(&s.counter_key);
        }
        invocation.arg(reserve_micros).arg(COUNTER_TTL_SECS);
        for s in &slots {
            invocation.arg(s.limit_micros);
        }
        for s in &slots {
            invocation.arg(i32::from(s.hard_stop));
        }

        let mut conn = self.conn.clone();
        // A cache failure is a BACKEND error (503), not a budget denial (429),
        // and its detail is never shown to the client.
        let result: Vec<i64> = invocation
            .invoke_async(&mut conn)
            .await
            .map_err(|e| ReserveError::Backend(e.to_string()))?;

        let failing = result.first().copied().unwrap_or(-1);
        if failing >= 0 {
            let spent = result.get(1).copied().unwrap_or(0);
            let slot = &slots[usize::try_from(failing).unwrap_or(0)];
            return Err(ReserveError::Denied(BudgetDenied {
                scope: slot.scope.clone(),
                period: slot.period.as_str(),
                limit_micros: slot.limit_micros,
                spent_micros: spent,
                cost_micros: reserve_micros,
            }));
        }

        let entries = slots
            .into_iter()
            .map(|s| (s.counter_key, reserve_micros))
            .collect();
        Ok(Reservation::from_entries(entries))
    }

    async fn commit(&self, reservation: &Reservation, actual_micros: i64) {
        // Every entry shares the same reserved amount, so the delta is uniform.
        // Settle ALL counters in ONE atomic EVAL (mirrors reserve) so a mid-way
        // failure cannot leave some counters settled and others inflated.
        let entries = reservation.entries();
        let Some((_, reserved)) = entries.first() else {
            return;
        };
        let delta = actual_micros - reserved;
        let script = redis::Script::new(SETTLE_LUA);
        let mut invocation = script.prepare_invoke();
        for (key, _) in entries {
            invocation.key(key);
        }
        invocation.arg(delta);
        let mut conn = self.conn.clone();
        let res: redis::RedisResult<i64> = invocation.invoke_async(&mut conn).await;
        if let Err(e) = res {
            tracing::error!(error = %e, "failed to settle budget counters");
        }
    }
}

// ---------------------------------------------------------------------------
// Loaders: budget config and prices from Postgres
// ---------------------------------------------------------------------------

fn scope_from_row(scope_type: &str, scope_id: Option<String>) -> Option<Scope> {
    match scope_type {
        "global" => Some(Scope::Global),
        "api_key" => scope_id.map(Scope::ApiKey),
        "provider" => scope_id.map(Scope::Provider),
        "model" => scope_id.map(Scope::Model),
        _ => None,
    }
}

fn period_from_str(p: &str) -> Period {
    match p {
        "daily" => Period::Daily,
        "weekly" => Period::Weekly,
        _ => Period::Monthly,
    }
}

/// Load all budget configs from Postgres.
///
/// # Errors
/// Returns any query error.
pub async fn load_budgets(pool: &PgPool) -> Result<Vec<Budget>, sqlx::Error> {
    let rows =
        sqlx::query("SELECT scope_type, scope_id, period, limit_micros, hard_stop FROM budgets ORDER BY scope_type, scope_id, period")
            .fetch_all(pool)
            .await?;
    let mut budgets = Vec::with_capacity(rows.len());
    for row in rows {
        let scope_type: String = row.get("scope_type");
        let scope_id: Option<String> = row.get("scope_id");
        let Some(scope) = scope_from_row(&scope_type, scope_id) else {
            continue;
        };
        budgets.push(Budget {
            scope,
            period: period_from_str(&row.get::<String, _>("period")),
            limit_micros: row.get::<i64, _>("limit_micros"),
            hard_stop: row.get::<bool, _>("hard_stop"),
        });
    }
    Ok(budgets)
}

/// Load the currently-effective prices into a [`PriceBook`].
///
/// # Errors
/// Returns any query error.
pub async fn load_prices(pool: &PgPool) -> Result<PriceBook, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT provider, model, input_per_1m_micros, output_per_1m_micros \
         FROM model_prices WHERE effective_to IS NULL",
    )
    .fetch_all(pool)
    .await?;
    let prices = rows.into_iter().map(|row| {
        ModelPrice::new(
            row.get::<String, _>("provider"),
            row.get::<String, _>("model"),
            row.get::<i64, _>("input_per_1m_micros"),
            row.get::<i64, _>("output_per_1m_micros"),
        )
    });
    Ok(PriceBook::from_prices(prices))
}

/// The `[start, end)` window of `period` containing `now`, in UTC.
fn period_window(period: Period, now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let at_midnight = |d: NaiveDate| Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap());
    match period {
        Period::Daily => {
            let start = at_midnight(now.date_naive());
            (start, start + Duration::days(1))
        }
        Period::Weekly => {
            let back = i64::from(now.weekday().num_days_from_monday());
            let start = at_midnight(now.date_naive() - Duration::days(back));
            (start, start + Duration::days(7))
        }
        Period::Monthly => {
            let d = now.date_naive();
            let first = NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap();
            let (ny, nm) = if d.month() == 12 {
                (d.year() + 1, 1)
            } else {
                (d.year(), d.month() + 1)
            };
            let next = NaiveDate::from_ymd_opt(ny, nm, 1).unwrap();
            (at_midnight(first), at_midnight(next))
        }
    }
}

/// Rebuild every budget's current-period counter in Redis/Valkey from the
/// Postgres `usage_events` ledger. Run at startup so a cache flush, restart, or
/// failover to a cold replica cannot reset budgets to zero (which would let
/// every key overspend). Postgres is the durable source of truth; Redis is the
/// hot-path cache.
///
/// # Errors
/// Returns a Postgres query error. Redis write failures are logged per budget.
/// Current-period spend for one budget, summed from the durable ledger. Counts
/// every charged row (`cost_micros > 0`), not just `allowed`, so a billed error
/// still counts against the budget. Shared by the reconciler and the read-only
/// admin endpoints so both compute spend identically.
pub async fn budget_spent(
    pool: &PgPool,
    budget: &Budget,
    now: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    let (start, end) = period_window(budget.period, now);
    let base = "SELECT COALESCE(SUM(cost_micros), 0)::bigint FROM usage_events \
                WHERE cost_micros > 0 AND started_at >= $1 AND started_at < $2";
    let spent = match &budget.scope {
        Scope::Global => {
            sqlx::query_scalar::<_, i64>(base)
                .bind(start)
                .bind(end)
                .fetch_one(pool)
                .await?
        }
        Scope::ApiKey(id) => {
            let Ok(uid) = uuid::Uuid::parse_str(id) else {
                return Ok(0);
            };
            sqlx::query_scalar::<_, i64>(&format!("{base} AND api_key_id = $3"))
                .bind(start)
                .bind(end)
                .bind(uid)
                .fetch_one(pool)
                .await?
        }
        Scope::Provider(p) => {
            sqlx::query_scalar::<_, i64>(&format!("{base} AND provider = $3"))
                .bind(start)
                .bind(end)
                .bind(p)
                .fetch_one(pool)
                .await?
        }
        Scope::Model(pm) => {
            let Some((prov, model)) = pm.split_once(':') else {
                return Ok(0);
            };
            sqlx::query_scalar::<_, i64>(&format!("{base} AND provider = $3 AND model = $4"))
                .bind(start)
                .bind(end)
                .bind(prov)
                .bind(model)
                .fetch_one(pool)
                .await?
        }
    };
    Ok(spent)
}

/// Seed Redis/Valkey counters for budgets whose current-period counter key does
/// not yet exist, from the durable ledger. Used on hot-reload so a budget added
/// at runtime enforces against spend already incurred this period rather than
/// starting at zero. Existing counters are never touched (no reset, no race with
/// live traffic, which only ever writes keys that already exist).
pub async fn seed_missing_counters(
    pool: &PgPool,
    conn: &ConnectionManager,
    budgets: &[Budget],
) -> Result<usize, sqlx::Error> {
    let now = Utc::now();
    let mut seeded = 0usize;
    for b in budgets {
        let key = crate::budget::counter_key(&b.scope, b.period, now);
        let mut c = conn.clone();
        // On any Redis error, assume the key exists (skip): never overwrite a
        // live counter, which could reset spend and permit overspend.
        let exists: bool = redis::cmd("EXISTS")
            .arg(&key)
            .query_async(&mut c)
            .await
            .unwrap_or(true);
        if exists {
            continue;
        }
        let spent = budget_spent(pool, b, now).await?;
        let mut c2 = conn.clone();
        let res: redis::RedisResult<()> = redis::pipe()
            .set(&key, spent)
            .ignore()
            .expire(&key, COUNTER_TTL_SECS)
            .ignore()
            .query_async(&mut c2)
            .await;
        match res {
            Ok(()) => seeded += 1,
            Err(e) => {
                tracing::warn!(error = %e, key = %key, "failed to seed new budget counter");
            }
        }
    }
    Ok(seeded)
}

pub async fn reconcile_counters(
    pool: &PgPool,
    conn: &ConnectionManager,
    budgets: &[Budget],
) -> Result<usize, sqlx::Error> {
    let now = Utc::now();
    let mut restored = 0usize;
    for b in budgets {
        let spent = budget_spent(pool, b, now).await?;

        let key = crate::budget::counter_key(&b.scope, b.period, now);
        let mut c = conn.clone();
        let res: redis::RedisResult<()> = redis::pipe()
            .set(&key, spent)
            .ignore()
            .expire(&key, COUNTER_TTL_SECS)
            .ignore()
            .query_async(&mut c)
            .await;
        if let Err(e) = res {
            tracing::error!(error = %e, key = %key, "failed to reconcile budget counter");
        } else {
            restored += 1;
        }
    }
    Ok(restored)
}
