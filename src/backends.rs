// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Production storage backends: Postgres (keys, usage, config) and Redis/Valkey
//! (budget counters). These implement the same [`crate::gateway`] traits the
//! in-memory demo backends do, so the request flow is identical.
//!
//! Redis and Valkey are wire-compatible; the `redis` crate talks to either. Use
//! whichever your platform offers (Valkey is typically cheaper on GCP).

use std::sync::Arc;

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
              gateway_micros, decision, cache_read_tokens, cache_write_tokens) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(api_key_id)
        .bind(event.provider)
        .bind(event.model)
        .bind(i64::try_from(event.usage.input_tokens).unwrap_or(i64::MAX))
        .bind(i64::try_from(event.usage.output_tokens).unwrap_or(i64::MAX))
        .bind(event.cost_micros)
        .bind(event.overhead_micros)
        .bind(event.decision)
        .bind(i64::try_from(event.usage.cache_read_tokens).unwrap_or(i64::MAX))
        .bind(i64::try_from(event.usage.cache_write_tokens).unwrap_or(i64::MAX))
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
/// Where a lost budget counter is rebuilt from.
///
/// A seam rather than a `PgPool` so the repair path can be exercised without a
/// database. The enforcement mechanism is the part of this system with the least
/// margin for error, and a repair that only runs against real Postgres is a
/// repair nobody tests.
#[async_trait]
pub trait SpendLedger: Send + Sync {
    /// Spend already recorded against `budget` for the period containing `now`.
    ///
    /// # Errors
    /// Returns a message when the spend cannot be determined. The caller must
    /// then refuse the request: admitting it would admit it unenforced.
    async fn spent_micros(&self, budget: &Budget, now: DateTime<Utc>) -> Result<i64, String>;
}

/// The real one: the Postgres ledger.
pub struct PgSpendLedger {
    pool: PgPool,
}

impl PgSpendLedger {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SpendLedger for PgSpendLedger {
    async fn spent_micros(&self, budget: &Budget, now: DateTime<Utc>) -> Result<i64, String> {
        budget_spent(&self.pool, budget, now)
            .await
            .map_err(|e| e.to_string())
    }
}

pub struct RedisBudgetBackend {
    conn: ConnectionManager,
    budgets: Vec<Budget>,
    /// For repairing a counter that is not there. See [`RESERVE_UNSEEDED`].
    ledger: Arc<dyn SpendLedger>,
    /// Serialises counter repair, so one cache loss produces one round of ledger
    /// queries rather than one per request in flight.
    repair_lock: tokio::sync::Mutex<()>,
}

impl RedisBudgetBackend {
    #[must_use]
    pub fn new(
        conn: ConnectionManager,
        budgets: Vec<Budget>,
        ledger: Arc<dyn SpendLedger>,
    ) -> Self {
        Self {
            conn,
            budgets,
            ledger,
            repair_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Rebuild the named counters from the ledger, creating only those that are
    /// genuinely absent and stamping each creation with a new generation.
    ///
    /// Only the keys the script reported missing, not every applicable budget: a
    /// `SUM` over a global monthly scope is not a query to run for a counter
    /// that is sitting there.
    async fn seed_keys(&self, keys: &[String], now: DateTime<Utc>) -> Result<(), String> {
        let script = redis::Script::new(SEED_LUA);
        for b in &self.budgets {
            let key = crate::budget::counter_key(&b.scope, b.period, now);
            if !keys.iter().any(|k| k == &key) {
                continue;
            }
            let spent = self.ledger.spent_micros(b, now).await?;
            let mut c = self.conn.clone();
            let created: redis::RedisResult<i64> = script
                .key(&key)
                .key(generation_key(&key))
                .arg(spent)
                .arg(COUNTER_TTL_SECS)
                .arg(new_generation())
                .invoke_async(&mut c)
                .await;
            match created {
                // 0 means someone else created it first, which is not an event.
                Ok(0) => {}
                Ok(_) => tracing::info!(
                    key = %key,
                    spent_micros = spent,
                    "budget counter was absent; restored from the ledger"
                ),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "failed to restore budget counter")
                }
            }
        }
        Ok(())
    }
}

/// First element of [`RESERVE_LUA`]'s reply when a counter key does not exist.
///
/// An absent counter is NOT a counter at zero. It is a counter whose history
/// Valkey has lost, or has never been told: a flush, a failover to a cold
/// replica, an eviction, or simply the first request of a new budget period.
/// `INCRBY` cannot tell those apart, and creates the key at the reserve amount,
/// which silently resets the budget and lets a period's worth of spend through
/// until someone restarts the gateway.
///
/// So the script refuses instead, and the caller loads the period's spend from
/// the ledger and tries once more. The ledger is the system of record; the
/// counter is a fast copy of it. Repairing on absence makes a Valkey flush cost
/// one Postgres query per budget rather than an unenforced period.
const RESERVE_UNSEEDED: i64 = -2;

/// The seed-generation stamp beside a counter key.
///
/// Bumped every time a counter is created from the ledger rather than by
/// ordinary traffic. A reservation records the generation it saw, and a
/// settlement declines if the generation has moved: the counter it is holding a
/// reservation against no longer exists, and the one now under that key already
/// accounts for the ledger without this request in it.
///
/// A separate key rather than a field because the counter is manipulated with
/// INCRBY, and it is passed through KEYS rather than derived inside the script
/// so every key the script touches is declared.
fn generation_key(counter_key: &str) -> String {
    format!("{counter_key}:gen")
}

/// A fresh generation for a counter being created.
///
/// Random, not sequential. A sequence looks like it identifies an incarnation
/// and does not: the stamp lives in the same Valkey as the counter and dies with
/// it, so an `INCR` on the rebuilt key starts again at 1. Since the steady-state
/// generation for any counter is also 1, every rebuild after a flush, a failover
/// to a cold replica, or a TTL expiry would hand back the number a stale
/// reservation is holding, and the settle it was supposed to decline would
/// apply. That is the bug the stamp exists to prevent, reintroduced by the
/// stamp.
///
/// 53 bits because the value round-trips through Lua, where numbers are
/// doubles and anything above 2^53 stops being exact. Never 0: that is reserved
/// for a counter created before this existed, so a reservation carrying no stamp
/// still settles.
fn new_generation() -> i64 {
    use rand::RngCore;
    let raw = rand::thread_rng().next_u64() & ((1 << 53) - 1);
    i64::try_from(raw).unwrap_or(1).max(1)
}

// Reserve: check every hard budget, then increment all applicable counters.
//
// KEYS = [counter_1..counter_n, generation_1..generation_n].
// ARGV = [reserve, ttl, n, limits..., hard_flags...].
//
// Replies:
//   {RESERVE_UNSEEDED, absent_index...}  one or more counters do not exist
//   {failing_index (0-based), spent}     a hard budget would be exceeded
//   {-1, 0, gen_1..gen_n, soft_index...} admitted
const RESERVE_LUA: &str = r"
local reserve = tonumber(ARGV[1])
local ttl = tonumber(ARGV[2])
local n = tonumber(ARGV[3])
local missing = {-2}
for i = 1, n do
  if redis.call('EXISTS', KEYS[i]) == 0 then
    missing[#missing + 1] = i - 1
  end
end
if #missing > 1 then
  return missing
end
for i = 1, n do
  if ARGV[3 + n + i] == '1' then
    local cur = tonumber(redis.call('GET', KEYS[i]) or '0')
    local limit = tonumber(ARGV[3 + i])
    if cur + reserve > limit then
      return {i - 1, cur}
    end
  end
end
local out = {-1, 0}
for i = 1, n do
  redis.call('INCRBY', KEYS[i], reserve)
  if ttl > 0 then
    redis.call('EXPIRE', KEYS[i], ttl)
    redis.call('EXPIRE', KEYS[n + i], ttl)
  end
end
for i = 1, n do
  out[#out + 1] = tonumber(redis.call('GET', KEYS[n + i]) or '0')
end
for i = 1, n do
  if ARGV[3 + n + i] ~= '1' then
    local cur = tonumber(redis.call('GET', KEYS[i]) or '0')
    if cur > tonumber(ARGV[3 + i]) then
      out[#out + 1] = i - 1
    end
  end
end
return out
";

// Settle: apply (actual - reserved) to every counter, flooring at 0.
//
// KEYS = [counter_1..counter_n, generation_1..generation_n].
// ARGV = [delta, n, expected_generation...].
//
// Two ways a counter can decline the delta, and they are different failures.
//
// An ABSENT counter has no reservation to settle. INCRBY would resurrect the key
// holding only this one request's delta, and the next reserve would find it
// present, skip the repair, and under-enforce for the rest of the period.
//
// A counter whose GENERATION has moved was rebuilt from the ledger while this
// request was in flight. The rebuilt figure is the ledger's, and this request is
// not in it, so subtracting a reservation it never held would leave it under the
// truth by the whole reserved amount. Reservations are deliberately generous, so
// that delta is negative for almost every request: this is the common case
// during a cache incident, not an exotic one.
//
// Either way the ledger is still appended, and the next rebuild picks the
// request up from there.
const SETTLE_LUA: &str = r"
local delta = tonumber(ARGV[1])
local n = tonumber(ARGV[2])
for i = 1, n do
  if redis.call('EXISTS', KEYS[i]) == 1 then
    local gen = tonumber(redis.call('GET', KEYS[n + i]) or '0')
    if gen == tonumber(ARGV[2 + i]) then
      local v = redis.call('INCRBY', KEYS[i], delta)
      if v < 0 then redis.call('SET', KEYS[i], 0) end
    end
  end
end
return 1
";

// Create one counter from the ledger, but only if it is genuinely absent, and
// stamp the creation so reservations taken against the previous incarnation can
// be told apart. KEYS = [counter, generation]; ARGV = [spent, ttl, generation].
//
// The stamp is supplied by the caller and random. Deriving it here from what
// Valkey holds cannot work: after a total loss there is nothing to derive it
// from. See `new_generation`.
const SEED_LUA: &str = r"
if redis.call('EXISTS', KEYS[1]) == 1 then
  return 0
end
redis.call('SET', KEYS[1], ARGV[1])
redis.call('SET', KEYS[2], ARGV[3])
if tonumber(ARGV[2]) > 0 then
  redis.call('EXPIRE', KEYS[1], ARGV[2])
  redis.call('EXPIRE', KEYS[2], ARGV[2])
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

        let n = slots.len();
        let script = redis::Script::new(RESERVE_LUA);
        let build = || {
            let mut invocation = script.prepare_invoke();
            for s in &slots {
                invocation.key(&s.counter_key);
            }
            for s in &slots {
                invocation.key(generation_key(&s.counter_key));
            }
            invocation
                .arg(reserve_micros)
                .arg(COUNTER_TTL_SECS)
                .arg(n as i64);
            for s in &slots {
                invocation.arg(s.limit_micros);
            }
            for s in &slots {
                invocation.arg(i32::from(s.hard_stop));
            }
            invocation
        };

        let mut conn = self.conn.clone();
        // A cache failure is a BACKEND error (503), not a budget denial (402),
        // and its detail is never shown to the client.
        let mut result: Vec<i64> = build()
            .invoke_async(&mut conn)
            .await
            .map_err(|e| ReserveError::Backend(e.to_string()))?;

        // One or more counters were missing. Rebuild just those from the ledger
        // and run the script once more.
        //
        // Serialised across the whole backend. A cache loss makes every request
        // in flight discover the same absent counters at the same moment, and
        // without this each of them would run its own `SUM` over the period.
        // That turns a Valkey blip into a Postgres incident, and the connection
        // pool would start timing out before any repair landed. Whoever holds
        // the lock repairs; everyone else re-runs the script when they get it
        // and finds the work already done.
        if result.first().copied() == Some(RESERVE_UNSEEDED) {
            let _repairing = self.repair_lock.lock().await;
            result = build()
                .invoke_async(&mut conn)
                .await
                .map_err(|e| ReserveError::Backend(e.to_string()))?;
            if result.first().copied() == Some(RESERVE_UNSEEDED) {
                let absent: Vec<String> = result
                    .iter()
                    .skip(1)
                    .filter_map(|i| usize::try_from(*i).ok())
                    .filter_map(|i| slots.get(i))
                    .map(|s| s.counter_key.clone())
                    .collect();
                self.seed_keys(&absent, now)
                    .await
                    .map_err(|e| ReserveError::Backend(format!("counter repair failed: {e}")))?;
                result = build()
                    .invoke_async(&mut conn)
                    .await
                    .map_err(|e| ReserveError::Backend(e.to_string()))?;
                // Once. A second failure means something is actively removing
                // keys, and admitting the request would be admitting it
                // unenforced.
                if result.first().copied() == Some(RESERVE_UNSEEDED) {
                    return Err(ReserveError::Backend(
                        "budget counter still absent after repair".to_owned(),
                    ));
                }
            }
        }

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

        // Soft budgets are counted but never refused. Without this they were
        // also silent, so `--hard-stop false` produced a cap that did nothing
        // and said nothing, which is worse than having no cap at all: the
        // operator believes they are being watched.
        // The admitted reply is {-1, 0, gen_1..gen_n, soft_index...}.
        let generations: Vec<i64> = result.iter().skip(2).take(n).copied().collect();

        for idx in result.iter().skip(2 + n) {
            let Ok(i) = usize::try_from(*idx) else {
                continue;
            };
            let Some(slot) = slots.get(i) else { continue };
            tracing::warn!(
                scope = %slot.scope,
                period = slot.period.as_str(),
                limit_micros = slot.limit_micros,
                "soft budget exceeded; the request was NOT refused because this budget \
                 was created with `--hard-stop false`"
            );
        }

        let entries = slots
            .into_iter()
            .map(|s| (s.counter_key, reserve_micros))
            .collect();
        Ok(Reservation::from_entries(entries).with_generations(generations))
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
        let n = entries.len();
        // A reservation taken before this field existed, or by a backend that
        // does not stamp, settles against generation 0, which is what an
        // untouched counter reports. Falling back to 0 rather than skipping
        // keeps an upgrade from stranding every reservation in flight across it.
        let generations = reservation.generations();
        let script = redis::Script::new(SETTLE_LUA);
        let mut invocation = script.prepare_invoke();
        for (key, _) in entries {
            invocation.key(key);
        }
        for (key, _) in entries {
            invocation.key(generation_key(key));
        }
        invocation.arg(delta).arg(n as i64);
        for i in 0..n {
            invocation.arg(generations.get(i).copied().unwrap_or(0));
        }
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
pub async fn load_prices(
    pool: &PgPool,
    fallback: crate::pricing::CacheRateFallback,
    long_context: crate::pricing::LongContextTier,
) -> Result<PriceBook, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT provider, model, input_per_1m_micros, output_per_1m_micros, \
                cache_read_per_1m_micros, cache_write_per_1m_micros, \
                long_context_threshold_tokens, long_context_input_permille, \
                long_context_output_permille \
         FROM model_prices WHERE effective_to IS NULL",
    )
    .fetch_all(pool)
    .await?;
    let prices: Vec<ModelPrice> = rows
        .into_iter()
        .map(|row| {
            // NULL cache rates resolve to a conservative multiple of the input
            // rate rather than to zero, so a model priced before cache rates
            // existed can never meter cache tokens as free.
            ModelPrice::new(
                row.get::<String, _>("provider"),
                row.get::<String, _>("model"),
                row.get::<i64, _>("input_per_1m_micros"),
                row.get::<i64, _>("output_per_1m_micros"),
            )
            .with_cache_rates(
                row.get::<Option<i64>, _>("cache_read_per_1m_micros"),
                row.get::<Option<i64>, _>("cache_write_per_1m_micros"),
                fallback,
            )
            // Per-model tier where set, deployment default otherwise, resolved
            // per field so a model can take just the threshold or just the
            // multiples.
            .with_long_context_row(
                row.get::<Option<i64>, _>("long_context_threshold_tokens"),
                row.get::<Option<i32>, _>("long_context_input_permille"),
                row.get::<Option<i32>, _>("long_context_output_permille"),
                long_context,
            )
        })
        .collect();
    // One warning per reload naming the models being over-charged on purpose,
    // so an operator sees it here rather than in a variance review later.
    let unpriced: Vec<String> = prices
        .iter()
        .filter(|p| p.cache_rates_are_fallback)
        .map(|p| format!("{}/{}", p.provider, p.model))
        .collect();
    if !unpriced.is_empty() {
        tracing::warn!(
            models = %unpriced.join(", "),
            "no cache rates configured; charging cache tokens at a conservative \
             multiple of the input rate, which OVER-charges cache reads. Set real \
             rates with `admin price set --cache-read-per-1m/--cache-write-per-1m`"
        );
    }
    // A resolved tier mixes per-model columns with the deployment default, so
    // the combination is never seen by a database CHECK and boot validation only
    // saw the default. Validate what each model actually ended up with.
    for p in &prices {
        if let Err(e) = p.long_context.validate() {
            return Err(sqlx::Error::Protocol(format!(
                "model {}/{} resolves to an invalid long-context tier: {e}",
                p.provider, p.model
            )));
        }
    }
    // Surface tiers that are set but inert: multiples configured against a zero
    // threshold do nothing AND suppress the under-charge warning, so a long
    // request on that model bills flat with no signal at all.
    let inert: Vec<String> = prices
        .iter()
        .filter(|p| {
            // Only when the ROW set an uplift of its own. A model deliberately
            // disabled with `--long-context-threshold 0` inherits the deployment
            // multiples and resolves to exactly this shape, and warning about it
            // on every reload (every 15s by default) would drown the signal this
            // warning exists to carry.
            //
            // The resolved multiples are checked as well as the flag: the flag
            // says the row asked for an uplift, and this says one survived
            // resolution. They agree today, and if a future change to
            // `resolve_row` makes them disagree, the quiet direction is the one
            // that does not invent a warning.
            p.long_context_uplift_from_row
                && p.long_context.threshold_tokens == 0
                && (p.long_context.multiple_permille > 1_000
                    || p.long_context.output_multiple_permille > 1_000)
        })
        .map(|p| format!("{}/{}", p.provider, p.model))
        .collect();
    if !inert.is_empty() {
        tracing::warn!(
            models = %inert.join(", "),
            "long-context multiples are set but the threshold is 0, so they do \
             nothing and no under-charge warning will fire. Set \
             `admin price set --long-context-threshold`"
        );
    }
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

/// Current-period spend for one budget, summed from the durable ledger. Counts
/// every charged row (`cost_micros > 0`), not just `allowed`, so a billed error
/// still counts against the budget. Shared by the reconciler, the reserve path's
/// counter repair, and the read-only admin endpoints, so all three compute spend
/// identically.
///
/// # Errors
/// Returns the Postgres query error. Callers on the request path must refuse the
/// request rather than treating an unknown figure as zero.
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

/// Run usage-ledger partition maintenance: create the current month's partition
/// and the two after it, and drop any partition older than `window` (a zero window keeps
/// everything). Delegates to the `tollgate_usage_maintain` SQL function so the
/// partition logic lives with the schema.
pub async fn run_usage_maintenance(
    pool: &PgPool,
    window: std::time::Duration,
) -> Result<(), sqlx::Error> {
    let secs = i64::try_from(window.as_secs()).unwrap_or(i64::MAX);
    sqlx::query("SELECT tollgate_usage_maintain($1 * interval '1 second')")
        .bind(secs)
        .execute(pool)
        .await?;
    Ok(())
}

/// Seed Redis/Valkey counters for budgets whose current-period counter key does
/// not yet exist, from the durable ledger. Used on hot-reload so a budget added
/// at runtime enforces against spend already incurred this period rather than
/// starting at zero.
///
/// Existing counters are never touched: the write goes through [`SEED_LUA`],
/// which re-checks existence inside the script, so a counter that live traffic
/// created while the ledger was being queried survives. The reserve path repairs
/// a missing counter with the same script, so this is no longer the only writer
/// of an absent key and the old EXISTS-then-SET could have reset a counter
/// created in that window. Going through the script also means this path stamps
/// a generation, which a counter creation must always do.
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
        // The same script the reserve path repairs with, so this door into
        // counter creation stamps a new generation too. Without the stamp, a
        // counter lost and then recreated here would look to a settlement like
        // the counter it reserved against, and that settlement would subtract a
        // reservation the new counter never held.
        //
        // The script re-checks existence, so traffic that created the counter
        // while the ledger was being queried keeps it.
        let res: redis::RedisResult<i64> = redis::Script::new(SEED_LUA)
            .key(&key)
            .key(generation_key(&key))
            .arg(spent)
            .arg(COUNTER_TTL_SECS)
            .arg(new_generation())
            .invoke_async(&mut c2)
            .await;
        match res {
            Ok(_) => seeded += 1,
            Err(e) => {
                tracing::warn!(error = %e, key = %key, "failed to seed new budget counter");
            }
        }
    }
    Ok(seeded)
}

/// Rebuild every budget's current-period counter from the Postgres
/// `usage_events` ledger, at startup.
///
/// The boot half of the durability story. The reserve path repairs a counter it
/// finds missing during a request; this one runs once before traffic and raises
/// every counter to at least what the ledger says, catching spend that happened
/// while this process was down.
///
/// Raising only, never lowering: a best-effort ledger write can be lost after
/// its counter was already incremented, so the counter can legitimately sit
/// ahead of the ledger, and setting it back down would permit overspend for the
/// rest of the period.
///
/// # Errors
/// Returns the Postgres query error. Individual Valkey write failures are logged
/// per budget and do not fail the whole pass.
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
        // Never LOWER a live counter. A best-effort ledger write can be lost after
        // the counter was already incremented, leaving Redis ahead of the ledger;
        // setting the counter down to the ledger sum would then permit overspend
        // for the rest of the period. Reconcile up to max(current, ledger) so the
        // counter is only ever raised.
        //
        // This instance's own traffic has not started, but another instance's
        // has: on Cloud Run several serve at once, so a reserve landing between
        // this GET and its SET is lost. The window is one round trip and the
        // loss is bounded by what fits in it, so this is a known gap rather than
        // a solved problem. The absent-counter branch below does not have it,
        // because that path is a single script.
        let current: Option<i64> = match redis::cmd("GET").arg(&key).query_async(&mut c).await {
            Ok(v) => v,
            Err(e) => {
                // Do NOT default to 0 on a read error: that could lower a live
                // counter and permit overspend. Skip this key and retry next boot.
                tracing::warn!(error = %e, key = %key, "reconcile: counter read failed; skipping");
                continue;
            }
        };
        // An ABSENT counter is a creation, and creations carry a generation
        // stamp so a settlement can tell one incarnation of a counter from the
        // next. Raising an EXISTING counter is not: nothing about it has been
        // forgotten, so reservations against it are still valid.
        if current.is_none() {
            let mut c3 = conn.clone();
            let seeded: redis::RedisResult<i64> = redis::Script::new(SEED_LUA)
                .key(&key)
                .key(generation_key(&key))
                .arg(spent)
                .arg(COUNTER_TTL_SECS)
                .arg(new_generation())
                .invoke_async(&mut c3)
                .await;
            match seeded {
                Ok(_) => restored += 1,
                Err(e) => {
                    tracing::error!(error = %e, key = %key, "failed to reconcile budget counter");
                }
            }
            continue;
        }
        let value = spent.max(current.unwrap_or(0));
        // The generation's TTL is refreshed alongside the counter's. If it
        // expired first, a reservation in flight would settle against a missing
        // stamp, read 0, mismatch, and be declined: a settle silently dropped
        // for a counter that was never lost. A no-op when the stamp is absent.
        let res: redis::RedisResult<()> = redis::pipe()
            .set(&key, value)
            .ignore()
            .expire(&key, COUNTER_TTL_SECS)
            .ignore()
            .expire(generation_key(&key), COUNTER_TTL_SECS)
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

/// Enforcement tests against a REAL Valkey/Redis.
///
/// These exist because the ordinary unit suite uses an in-memory budget backend
/// and therefore never executes RESERVE_LUA or SETTLE_LUA at all. A green
/// `cargo test` proves nothing about the enforcement mechanism, which makes it
/// useless as a gate on a `redis` crate upgrade. These run the real scripts
/// through the real client.
///
/// Ignored by default because they need a server. Run them with:
///
/// ```text
/// docker compose -f compose/docker-compose.yaml up -d valkey
/// TOLLGATE_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test --lib redis_live -- --ignored --test-threads=1
/// ```
///
/// Run the whole file on the CURRENT redis version and again on the candidate,
/// and compare. A behavioural difference is a finding to explain before merge.
#[cfg(test)]
mod redis_live_tests {
    use super::*;
    use crate::budget::{Period, Scope};

    fn url() -> Option<String> {
        std::env::var("TOLLGATE_TEST_REDIS_URL").ok()
    }

    /// A ledger that has recorded nothing.
    ///
    /// These tests delete their counters to start clean, and a deleted counter
    /// now sends the reserve path to the ledger to rebuild it. With this, it
    /// rebuilds to zero, which is what a fresh test period should be, and the
    /// tests keep measuring the Lua rather than Postgres.
    struct ZeroLedger;

    #[async_trait]
    impl SpendLedger for ZeroLedger {
        async fn spent_micros(&self, _b: &Budget, _now: DateTime<Utc>) -> Result<i64, String> {
            Ok(0)
        }
    }

    async fn backend(budgets: Vec<Budget>) -> (RedisBudgetBackend, ConnectionManager) {
        let url = url().expect("TOLLGATE_TEST_REDIS_URL must be set for these tests");
        let client = redis::Client::open(url).expect("redis url parses");
        let conn = ConnectionManager::new(client)
            .await
            .expect("valkey reachable");
        (
            RedisBudgetBackend::new(conn.clone(), budgets, Arc::new(ZeroLedger)),
            conn,
        )
    }

    fn global(limit: i64, hard: bool) -> Budget {
        Budget {
            scope: Scope::Global,
            period: Period::Monthly,
            limit_micros: limit,
            hard_stop: hard,
        }
    }

    fn key_budget(id: &str, limit: i64, hard: bool) -> Budget {
        Budget {
            scope: Scope::ApiKey(id.to_owned()),
            period: Period::Monthly,
            limit_micros: limit,
            hard_stop: hard,
        }
    }

    fn ctx(key_id: &str) -> RequestCtx<'_> {
        RequestCtx {
            key_id,
            provider: "mock",
            model: "m",
        }
    }

    /// Wipe only this test's keys so a shared server is not clobbered.
    async fn reset(conn: &mut ConnectionManager, pattern: &str) {
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg(pattern)
            .query_async(conn)
            .await
            .unwrap_or_default();
        for k in keys {
            let _: i64 = redis::cmd("DEL")
                .arg(&k)
                .query_async(conn)
                .await
                .unwrap_or(0);
        }
    }

    async fn counter(conn: &mut ConnectionManager, key: &str) -> i64 {
        redis::cmd("GET")
            .arg(key)
            .query_async::<Option<i64>>(conn)
            .await
            .expect("GET works")
            .unwrap_or(0)
    }

    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn reserve_increments_every_applicable_counter() {
        let id = "live-a";
        let (be, mut conn) =
            backend(vec![global(1_000_000, true), key_budget(id, 500_000, true)]).await;
        reset(&mut conn, "*live-a*").await;
        reset(&mut conn, "tollgate:*global*").await;

        let res = be.reserve(&ctx(id), 1_000).await.expect("within budget");
        for (key, reserved) in res.entries() {
            assert_eq!(*reserved, 1_000);
            assert_eq!(counter(&mut conn, key).await, 1_000, "counter {key}");
            // The generation stamp must expire WITH its counter. If SEED_LUA
            // ever stopped setting that TTL, a stamp could outlive its counter
            // by up to the full window and start declining valid settles.
            let gen_ttl: i64 = redis::cmd("TTL")
                .arg(generation_key(key))
                .query_async(&mut conn)
                .await
                .expect("TTL works");
            assert!(gen_ttl > 0, "generation stamp for {key} has no expiry");
            // TTL must be set, or a counter outlives its period forever.
            let ttl: i64 = redis::cmd("TTL")
                .arg(key)
                .query_async(&mut conn)
                .await
                .unwrap();
            assert!(ttl > 0, "counter {key} has no TTL");
        }
    }

    /// A ledger holding one fixed figure, to stand for spend already recorded.
    struct FixedLedger(i64);

    #[async_trait]
    impl SpendLedger for FixedLedger {
        async fn spent_micros(&self, _b: &Budget, _now: DateTime<Utc>) -> Result<i64, String> {
            Ok(self.0)
        }
    }

    /// A ledger that cannot answer.
    struct BrokenLedger;

    #[async_trait]
    impl SpendLedger for BrokenLedger {
        async fn spent_micros(&self, _b: &Budget, _now: DateTime<Utc>) -> Result<i64, String> {
            Err("ledger unavailable".to_owned())
        }
    }

    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn a_flushed_counter_is_rebuilt_from_the_ledger_not_reset_to_zero() {
        // The failure this prevents: Valkey loses a counter (FLUSHALL, a
        // failover to a cold replica, an eviction), INCRBY recreates it at the
        // reserve amount, and the budget silently starts the period again. A
        // month of spend goes through unenforced with nothing logged.
        let id = "live-flush";
        let budgets = vec![key_budget(id, 100_000, true)];
        let url = url().expect("TOLLGATE_TEST_REDIS_URL must be set for these tests");
        let client = redis::Client::open(url).expect("redis url parses");
        let mut conn = ConnectionManager::new(client)
            .await
            .expect("valkey reachable");
        reset(&mut conn, "*live-flush*").await;

        // The ledger says this budget has already spent 99,000 of its 100,000
        // this period. Valkey knows nothing: its counter is gone.
        let be = RedisBudgetBackend::new(conn.clone(), budgets, Arc::new(FixedLedger(99_000)));

        // A 5,000 reserve must be DENIED, because 99,000 + 5,000 is over. Before
        // the repair this was admitted: the counter did not exist, so it read as
        // zero and 5,000 fitted easily.
        let err = be
            .reserve(&ctx(id), 5_000)
            .await
            .expect_err("must be denied against the ledger's spend");
        match err {
            ReserveError::Denied(d) => {
                assert_eq!(d.spent_micros, 99_000, "denial must cite the rebuilt spend");
            }
            other => panic!("expected Denied, got {other:?}"),
        }

        // And the counter is now present at the ledger's figure, so the next
        // request does not pay for the repair again.
        let key =
            crate::budget::counter_key(&Scope::ApiKey(id.to_owned()), Period::Monthly, Utc::now());
        assert_eq!(counter(&mut conn, &key).await, 99_000);

        // A reserve that DOES fit is admitted and increments from the rebuilt
        // figure, not from zero.
        be.reserve(&ctx(id), 500).await.expect("500 still fits");
        assert_eq!(counter(&mut conn, &key).await, 99_500);
    }

    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn a_counter_that_cannot_be_rebuilt_refuses_the_request() {
        // Fail closed. If the counter is gone AND the ledger cannot say what was
        // spent, there is no number to enforce against, and admitting the
        // request would admit it unenforced. A 503 is the honest answer.
        let id = "live-noledger";
        let budgets = vec![key_budget(id, 100_000, true)];
        let url = url().expect("TOLLGATE_TEST_REDIS_URL must be set for these tests");
        let client = redis::Client::open(url).expect("redis url parses");
        let mut conn = ConnectionManager::new(client)
            .await
            .expect("valkey reachable");
        reset(&mut conn, "*live-noledger*").await;

        let be = RedisBudgetBackend::new(conn.clone(), budgets, Arc::new(BrokenLedger));
        let err = be.reserve(&ctx(id), 1).await.expect_err("must refuse");
        match err {
            ReserveError::Backend(m) => assert!(
                m.contains("counter repair failed"),
                "unexpected message: {m}"
            ),
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn a_settle_from_before_a_rebuild_does_not_drag_the_counter_down() {
        // The hole the absent-key check alone left open. Skipping a settle only
        // while the counter is MISSING is not enough: once another request has
        // rebuilt it, the key is present again, and a settle still in flight
        // from before the loss subtracts a reservation the new counter never
        // held. Reserves are deliberately generous, so that delta is negative
        // for almost every request, and the counter ends up under the truth by
        // the whole reserved amount until the next restart.
        let id = "live-gen";
        let budgets = vec![key_budget(id, 10_000_000, true)];
        let url = url().expect("TOLLGATE_TEST_REDIS_URL must be set for these tests");
        let client = redis::Client::open(url).expect("redis url parses");
        let mut conn = ConnectionManager::new(client)
            .await
            .expect("valkey reachable");
        reset(&mut conn, "*live-gen*").await;

        // The ledger says 40,000 has been spent this period.
        let be = RedisBudgetBackend::new(conn.clone(), budgets, Arc::new(FixedLedger(40_000)));
        let key =
            crate::budget::counter_key(&Scope::ApiKey(id.to_owned()), Period::Monthly, Utc::now());

        // Request A reserves 30,000 against the rebuilt counter: 40k + 30k.
        let a = be.reserve(&ctx(id), 30_000).await.expect("A fits");
        assert_eq!(counter(&mut conn, &key).await, 70_000);

        // The cache loses the counter while A is still in flight. BOTH keys go:
        // a flush, an eviction, or a failover to a cold replica does not
        // politely leave the generation stamp behind, and an earlier version of
        // this test deleted only the counter, which is why it passed against a
        // stamp that could not actually tell two incarnations apart.
        let _: i64 = redis::cmd("DEL")
            .arg(&key)
            .arg(generation_key(&key))
            .query_async(&mut conn)
            .await
            .expect("DEL works");

        // Request B arrives, finds it absent, and rebuilds it from the ledger.
        let b = be.reserve(&ctx(id), 5_000).await.expect("B fits");
        assert_eq!(
            counter(&mut conn, &key).await,
            45_000,
            "rebuilt to the ledger figure plus B's reservation"
        );

        // A now settles, cheaply: delta is 1,000 - 30,000 = -29,000. The
        // counter it reserved against is gone, so this must do nothing.
        be.commit(&a, 1_000).await;
        assert_eq!(
            counter(&mut conn, &key).await,
            45_000,
            "a settle from before the rebuild must not touch the new counter"
        );

        // B, which reserved against the CURRENT counter, settles normally.
        be.commit(&b, 2_000).await;
        assert_eq!(counter(&mut conn, &key).await, 42_000);
    }

    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn settling_against_a_vanished_counter_does_not_resurrect_it() {
        // If the counter disappears between reserve and settle, the delta has
        // nothing to apply to. INCRBY would recreate the key holding just this
        // one request's delta, and the next reserve would find it present, skip
        // the repair, and under-enforce for the rest of the period.
        let id = "live-vanish";
        let budgets = vec![key_budget(id, 100_000, true)];
        let (be, mut conn) = backend(budgets).await;
        reset(&mut conn, "*live-vanish*").await;

        let res = be.reserve(&ctx(id), 10_000).await.expect("fits");
        for (key, _) in res.entries() {
            let _: i64 = redis::cmd("DEL")
                .arg(key)
                .query_async(&mut conn)
                .await
                .expect("DEL works");
        }
        be.commit(&res, 1_000).await;

        for (key, _) in res.entries() {
            let exists: bool = redis::cmd("EXISTS")
                .arg(key)
                .query_async(&mut conn)
                .await
                .expect("EXISTS works");
            assert!(!exists, "settle must not recreate {key}");
        }
    }

    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn denial_is_atomic_and_touches_nothing() {
        // The second (per-key) budget is the one that cannot fit. RESERVE_LUA
        // must check ALL budgets before incrementing ANY, or a denied request
        // leaves the global counter inflated and slowly starves the deployment.
        let id = "live-b";
        let (be, mut conn) =
            backend(vec![global(1_000_000, true), key_budget(id, 100, true)]).await;
        reset(&mut conn, "*live-b*").await;
        reset(&mut conn, "tollgate:*global*").await;

        let before: Vec<i64> = {
            let probe = be.reserve(&ctx(id), 1).await.expect("tiny reserve fits");
            be.commit(&probe, 1).await;
            let mut v = Vec::new();
            for (key, _) in probe.entries() {
                v.push(counter(&mut conn, key).await);
            }
            v
        };

        let err = be
            .reserve(&ctx(id), 10_000)
            .await
            .expect_err("must be denied");
        match err {
            ReserveError::Denied(_) => {}
            other => panic!("expected Denied, got {other:?}"),
        }

        let probe = be
            .reserve(&ctx(id), 1)
            .await
            .expect("tiny reserve still fits");
        for (i, (key, _)) in probe.entries().iter().enumerate() {
            let now = counter(&mut conn, key).await;
            assert_eq!(
                now,
                before[i] + 1,
                "denied reserve must not have moved {key}"
            );
        }
    }

    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn settle_applies_the_delta_exactly_once() {
        let id = "live-c";
        let (be, mut conn) = backend(vec![
            global(10_000_000, true),
            key_budget(id, 10_000_000, true),
        ])
        .await;
        reset(&mut conn, "*live-c*").await;
        reset(&mut conn, "tollgate:*global*").await;

        let res = be.reserve(&ctx(id), 100).await.expect("fits");
        be.commit(&res, 40).await;
        for (key, _) in res.entries() {
            assert_eq!(
                counter(&mut conn, key).await,
                40,
                "counter {key} after settle"
            );
        }
    }

    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn settle_floors_at_zero_and_never_goes_negative() {
        // A counter knocked DOWN mid-flight, by a manual correction or a
        // partial restore, must not be driven negative by the settle delta, or
        // the next period starts with free budget.
        //
        // The counter is lowered rather than deleted, and its generation stamp
        // is left alone. Deleting it would make this test pass without the floor
        // existing at all: settle now skips an absent counter, and the `counter`
        // helper reads a missing key as 0, so the assertion would hold whatever
        // the floor did. Pinning a guard requires reaching the guard.
        let id = "live-d";
        let (be, mut conn) = backend(vec![
            global(10_000_000, true),
            key_budget(id, 10_000_000, true),
        ])
        .await;
        reset(&mut conn, "*live-d*").await;
        reset(&mut conn, "tollgate:*global*").await;

        let res = be.reserve(&ctx(id), 1_000).await.expect("fits");
        for (key, _) in res.entries() {
            let _: () = redis::cmd("SET")
                .arg(key)
                .arg(100)
                .arg("KEEPTTL")
                .query_async(&mut conn)
                .await
                .expect("SET works");
        }
        // Settling at zero cost applies a delta of -1000 to a counter holding
        // 100. Without the floor it lands at -900.
        be.commit(&res, 0).await;
        for (key, _) in res.entries() {
            assert_eq!(counter(&mut conn, key).await, 0, "counter {key} floored");
        }
    }

    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn concurrent_reserves_admit_exactly_the_budgeted_number() {
        // THE atomicity test. 50 concurrent reserves of R against a hard cap of
        // 10R must admit exactly 10. Any interleaving that reads a stale counter
        // admits more, which is overspend. This is what proves EVAL atomicity
        // still holds end to end through whatever client version is in the build.
        let id = "live-e";
        const R: i64 = 1_000;
        const ALLOWED: i64 = 10;
        let (be, mut conn) = backend(vec![
            global(100_000_000, true),
            key_budget(id, R * ALLOWED, true),
        ])
        .await;
        reset(&mut conn, "*live-e*").await;
        reset(&mut conn, "tollgate:*global*").await;

        let be = std::sync::Arc::new(be);
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..50 {
            let be = be.clone();
            set.spawn(async move { be.reserve(&ctx("live-e"), R).await.is_ok() });
        }
        let mut admitted = 0;
        while let Some(r) = set.join_next().await {
            if r.expect("task did not panic") {
                admitted += 1;
            }
        }
        assert_eq!(
            admitted, ALLOWED,
            "exactly {ALLOWED} reserves may be admitted"
        );
    }

    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn script_survives_a_script_flush() {
        // Script::invoke_async uses EVALSHA and must fall back to EVAL on
        // NOSCRIPT. If that fallback ever regresses, every request starts
        // failing the moment the server's script cache is cleared, which
        // happens on failover and restart.
        let id = "live-f";
        let (be, mut conn) = backend(vec![
            global(10_000_000, true),
            key_budget(id, 10_000_000, true),
        ])
        .await;
        reset(&mut conn, "*live-f*").await;
        reset(&mut conn, "tollgate:*global*").await;

        be.reserve(&ctx(id), 10).await.expect("first reserve");
        let _: String = redis::cmd("SCRIPT")
            .arg("FLUSH")
            .query_async(&mut conn)
            .await
            .expect("script flush");
        be.reserve(&ctx(id), 10)
            .await
            .expect("reserve must survive a script cache flush");
    }

    /// Settle is NOT idempotent: SETTLE_LUA applies `INCRBY delta` with a
    /// negative delta, so running it twice for one reservation lowers the
    /// counter BELOW true spend and hands out free budget. Nothing in the
    /// script or the schema prevents a replay; the only thing that does is the
    /// client never sending the command twice.
    ///
    /// This test is the one that actually gates a client upgrade. It kills the
    /// connection underneath an in-flight settle and asserts the counter is
    /// either fully settled or not settled at all, never settled twice.
    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn settle_is_never_applied_twice_across_a_connection_kill() {
        let id = "live-g";
        let (be, mut conn) = backend(vec![
            global(10_000_000, true),
            key_budget(id, 10_000_000, true),
        ])
        .await;
        reset(&mut conn, "*live-g*").await;
        reset(&mut conn, "tollgate:*global*").await;

        // How many rounds lost the settle entirely, reported so it is visible
        // that the reconnect path was really exercised rather than the test
        // passing because nothing ever happened.
        let mut lost = 0;
        for round in 0..25 {
            let res = be.reserve(&ctx(id), 1_000).await.expect("fits");
            let keys: Vec<String> = res.entries().iter().map(|(k, _)| k.clone()).collect();
            let before: Vec<i64> = {
                let mut v = Vec::new();
                for k in &keys {
                    v.push(counter(&mut conn, k).await);
                }
                v
            };

            // Kill every client connection FIRST, then settle. This forces the
            // manager down its reconnect path deterministically on every round.
            //
            // Racing the kill against an in-flight settle does not work: tokio's
            // sleep granularity is around a millisecond while a local settle
            // completes in microseconds, so the kill always lands after the
            // command has already completed and the test passes without ever
            // exercising anything. Killing first is both deterministic and the
            // case the client's retry behaviour actually governs.
            let mut killer = conn.clone();
            let _: redis::RedisResult<i64> = redis::cmd("CLIENT")
                .arg("KILL")
                .arg("TYPE")
                .arg("normal")
                .query_async(&mut killer)
                .await;
            be.commit(&res, 400).await;

            // Reserved 1000, settling to 400 means a delta of -600. The counter
            // must have moved by 0 (settle lost) or exactly -600 (settle
            // applied). -1200 means it was applied twice.
            for (i, k) in keys.iter().enumerate() {
                let now = counter(&mut conn, k).await;
                let moved = now - before[i];
                assert!(
                    moved == 0 || moved == -600,
                    "round {round}: counter {k} moved by {moved}; expected 0 or -600. \
                     -1200 means the settle was REPLAYED, which lowers counters below \
                     true spend and permits overspend"
                );
                if i == 0 && moved == 0 {
                    lost += 1;
                }
            }
        }
        // Reported, not asserted. Every round forced a reconnect by construction,
        // so the replay path was exercised regardless of the split. A settle
        // LOST after a kill is safe (counters stay inflated, spend is
        // over-counted, never under). A settle applied TWICE is the failure the
        // per-round assertion above catches.
        eprintln!(
            "settle-after-kill: {lost}/25 rounds lost the settle, {}/25 applied it once",
            25 - lost
        );
    }

    #[tokio::test]
    #[ignore = "needs a live Valkey; see module docs"]
    async fn reserve_is_never_applied_twice_across_a_connection_kill() {
        // Mirror of the settle case. A replayed reserve inflates counters, which
        // is the safe direction but still wrong, and it indicates the client
        // replays commands, which would make the settle case unsafe too.
        let id = "live-h";
        let (be, mut conn) = backend(vec![
            global(100_000_000, true),
            key_budget(id, 100_000_000, true),
        ])
        .await;
        reset(&mut conn, "*live-h*").await;
        reset(&mut conn, "tollgate:*global*").await;

        for round in 0..25 {
            let mut killer = conn.clone();
            let probe_key = {
                let r = be.reserve(&ctx(id), 1).await.expect("probe");
                be.commit(&r, 1).await;
                r.entries()[0].0.clone()
            };
            let before = counter(&mut conn, &probe_key).await;

            // Kill first, then reserve, for the same reason as the settle case.
            let _: redis::RedisResult<i64> = redis::cmd("CLIENT")
                .arg("KILL")
                .arg("TYPE")
                .arg("normal")
                .query_async(&mut killer)
                .await;
            if let Ok(r) = be.reserve(&ctx(id), 1_000).await {
                be.commit(&r, 1_000).await;
            }
            let moved = counter(&mut conn, &probe_key).await - before;
            assert!(
                moved == 0 || moved == 1_000,
                "round {round}: counter moved by {moved}; 2000 means the reserve was REPLAYED"
            );
        }
    }
}
