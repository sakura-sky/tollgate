// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Command-line interface dispatcher.
//!
//! `tollgate serve`           - run the HTTP gateway
//! `tollgate admin migrate`   - apply database migrations
//! `tollgate admin key issue` - issue an API key
//!
//! Tollgate is single-tenant per deployment, so there is no `tenant` subcommand.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sqlx::Row;

use crate::config::Config;

#[derive(Debug, Parser)]
#[command(
    name = "tollgate",
    version,
    about = "Tollgate - AI gateway and spend-control proxy for LLM providers"
)]
pub struct Cli {
    /// Optional path to a TOML config file. If omitted, `./tollgate.toml` is
    /// used when present. Environment variables prefixed `TOLLGATE_` always
    /// override file values.
    #[arg(long, global = true, env = "TOLLGATE_CONFIG")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the HTTP gateway.
    Serve,
    /// Run a self-contained, zero-infra demo (in-memory; no Postgres/Redis/creds).
    Demo,
    /// Administrative subcommands.
    #[command(subcommand)]
    Admin(AdminCommand),
}

#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// Apply pending database migrations.
    Migrate,
    /// API key management.
    #[command(subcommand)]
    Key(KeyCommand),
    /// Budget management.
    #[command(subcommand)]
    Budget(BudgetCommand),
    /// Model price management.
    #[command(subcommand)]
    Price(PriceCommand),
}

#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Issue a new API key (printed once).
    Issue {
        #[arg(long)]
        label: String,
    },
    /// Revoke a key by its prefix or full value.
    Revoke {
        #[arg(long)]
        key: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum BudgetCommand {
    /// Set (replace) a budget for a scope and period.
    Set {
        /// `global` | `api_key:<uuid>` | `provider:<name>` | `model:<provider:model>`
        #[arg(long)]
        scope: String,
        #[arg(long, default_value = "monthly")]
        period: String,
        /// Spend ceiling in the deployment's currency (e.g. dollars).
        #[arg(long)]
        limit: f64,
        /// If false, breach only notifies (soft cap) rather than rejecting.
        #[arg(long, default_value_t = true)]
        hard_stop: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum PriceCommand {
    /// Set (supersede) the current price for a provider/model, per 1M tokens.
    Set {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
        /// Input price per 1,000,000 tokens (currency units).
        #[arg(long)]
        input_per_1m: f64,
        /// Output price per 1,000,000 tokens (currency units).
        #[arg(long)]
        output_per_1m: f64,
        /// Cache-READ price per 1,000,000 tokens. Omit to CARRY FORWARD the
        /// current value; the common case is bumping a base rate without meaning
        /// to touch cache rates. Use --clear-cache-read to unset it.
        #[arg(long)]
        cache_read_per_1m: Option<f64>,
        /// Cache-WRITE price per 1,000,000 tokens. Omit to carry forward.
        #[arg(long)]
        cache_write_per_1m: Option<f64>,
        /// Unset the cache-read price, restoring the conservative fallback.
        #[arg(long, conflicts_with = "cache_read_per_1m")]
        clear_cache_read: bool,
        /// Unset the cache-write price, restoring the conservative fallback.
        #[arg(long, conflicts_with = "cache_write_per_1m")]
        clear_cache_write: bool,
    },
}

/// What to do with one cache rate on a re-price.
enum RateChange {
    /// Keep whatever the superseded row had. The default, because the common
    /// operation is bumping a base rate, and silently resetting a cache rate
    /// there would swap a real rate for the fallback: a large, invisible
    /// over-charge on exactly the cache-heavy workloads that configured it.
    Carry,
    Set(i64),
    Clear,
}

impl RateChange {
    fn resolve(value: Option<f64>, clear: bool, carried: Option<i64>) -> Result<Option<i64>> {
        Ok(match Self::from_flags(value, clear)? {
            Self::Carry => carried,
            Self::Set(v) => Some(v),
            Self::Clear => None,
        })
    }

    fn from_flags(value: Option<f64>, clear: bool) -> Result<Self> {
        match (value, clear) {
            (Some(v), _) => Ok(Self::Set(to_micros(v)?)),
            (None, true) => Ok(Self::Clear),
            (None, false) => Ok(Self::Carry),
        }
    }
}

pub async fn dispatch(cli: Cli) -> Result<()> {
    let cfg = Config::load(cli.config.clone())?;
    let _telemetry = crate::telemetry::init(&cfg.telemetry)?;

    match cli.command {
        Command::Serve => crate::app::serve(cfg).await,
        Command::Demo => crate::demo::serve(cfg).await,
        Command::Admin(AdminCommand::Migrate) => admin_migrate(&cfg).await,
        Command::Admin(AdminCommand::Key(KeyCommand::Issue { label })) => {
            admin_key_issue(&cfg, &label).await
        }
        Command::Admin(AdminCommand::Key(KeyCommand::Revoke { key })) => {
            admin_key_revoke(&cfg, &key).await
        }
        Command::Admin(AdminCommand::Budget(BudgetCommand::Set {
            scope,
            period,
            limit,
            hard_stop,
        })) => admin_budget_set(&cfg, &scope, &period, limit, hard_stop).await,
        Command::Admin(AdminCommand::Price(PriceCommand::Set {
            provider,
            model,
            input_per_1m,
            output_per_1m,
            cache_read_per_1m,
            cache_write_per_1m,
            clear_cache_read,
            clear_cache_write,
        })) => {
            admin_price_set(
                &cfg,
                &provider,
                &model,
                input_per_1m,
                output_per_1m,
                cache_read_per_1m,
                cache_write_per_1m,
                clear_cache_read,
                clear_cache_write,
            )
            .await
        }
    }
}

async fn admin_migrate(cfg: &Config) -> Result<()> {
    tracing::info!("running database migrations");
    let pool = crate::db::build_pool(&cfg.database).await?;
    crate::db::run_migrations(&pool).await?;
    tracing::info!("migrations complete");
    Ok(())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_micros(amount: f64) -> Result<i64> {
    if !amount.is_finite() || amount < 0.0 {
        bail!("amount must be a finite, non-negative number (got {amount})");
    }
    Ok((amount * 1_000_000.0).round() as i64)
}

async fn admin_key_revoke(cfg: &Config, key: &str) -> Result<()> {
    // Accept either the full key or just its prefix.
    let prefix = crate::apikey::parse(key)
        .map(|(p, _)| p)
        .unwrap_or_else(|_| key.to_owned());
    let pool = crate::db::build_pool(&cfg.database).await?;
    let n = sqlx::query(
        "UPDATE api_keys SET revoked_at = NOW() WHERE prefix = $1 AND revoked_at IS NULL",
    )
    .bind(&prefix)
    .execute(&pool)
    .await
    .context("revoking api key")?
    .rows_affected();
    if n == 0 {
        println!("no active key with prefix {prefix}");
    } else {
        println!("revoked key {prefix}");
    }
    Ok(())
}

async fn admin_key_issue(cfg: &Config, label: &str) -> Result<()> {
    if cfg.security.api_key_pepper.len() < 16 {
        bail!(
            "set TOLLGATE_SECURITY__API_KEY_PEPPER to a fixed secret of at least 16 bytes \
             before issuing keys (the same value must be configured for `serve`)"
        );
    }
    if cfg.security.api_key_pepper == crate::config::DEV_PLACEHOLDER_PEPPER {
        bail!(
            "TOLLGATE_SECURITY__API_KEY_PEPPER is still the .env.example placeholder; \
             set a real secret before issuing keys"
        );
    }
    let hasher = crate::apikey::KeyHasher::new(cfg.security.api_key_pepper.clone().into_bytes());
    let key = hasher.generate();
    let pool = crate::db::build_pool(&cfg.database).await?;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO api_keys (key_hash, prefix, label) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(&key.key_hash)
    .bind(&key.prefix)
    .bind(label)
    .fetch_one(&pool)
    .await
    .context("inserting api key")?;
    println!("API key issued (store it now; it is not recoverable):");
    println!("  key: {}", key.plaintext);
    println!("  id:  {id}");
    println!("\nSet a per-key budget with:");
    println!("  tollgate admin budget set --scope api_key:{id} --period monthly --limit <amount>");
    Ok(())
}

fn parse_scope(scope: &str) -> Result<(&'static str, Option<String>)> {
    if scope == "global" {
        return Ok(("global", None));
    }
    let (kind, id) = scope
        .split_once(':')
        .context("scope must be `global` or `<kind>:<id>`")?;
    match kind {
        "api_key" => Ok(("api_key", Some(id.to_owned()))),
        "provider" => Ok(("provider", Some(id.to_owned()))),
        "model" => Ok(("model", Some(id.to_owned()))),
        other => bail!("unknown scope kind: {other}"),
    }
}

async fn admin_budget_set(
    cfg: &Config,
    scope: &str,
    period: &str,
    limit: f64,
    hard_stop: bool,
) -> Result<()> {
    let (scope_type, scope_id) = parse_scope(scope)?;
    let limit_micros = to_micros(limit)?;
    if !matches!(period, "daily" | "weekly" | "monthly") {
        bail!("period must be daily, weekly, or monthly (got {period})");
    }
    let pool = crate::db::build_pool(&cfg.database).await?;

    // A per-key budget must reference a real key, or it silently never matches
    // and the key runs under the looser global cap (a fail-open).
    if scope_type == "api_key" {
        let id = scope_id.as_deref().unwrap_or_default();
        let uid = uuid::Uuid::parse_str(id).context("api_key scope id must be a UUID")?;
        let exists: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT id FROM api_keys WHERE id = $1")
                .bind(uid)
                .fetch_optional(&pool)
                .await
                .context("checking api key exists")?;
        if exists.is_none() {
            bail!("no api key with id {id}");
        }
    }

    // Replace atomically: delete any existing budget for this scope+period, then
    // insert, in one transaction so a failure can't leave the scope uncapped.
    let mut tx = pool.begin().await.context("begin transaction")?;
    sqlx::query(
        "DELETE FROM budgets WHERE scope_type = $1 \
         AND (scope_id IS NOT DISTINCT FROM $2) AND period = $3",
    )
    .bind(scope_type)
    .bind(&scope_id)
    .bind(period)
    .execute(&mut *tx)
    .await
    .context("clearing existing budget")?;
    sqlx::query(
        "INSERT INTO budgets (scope_type, scope_id, period, limit_micros, hard_stop) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(scope_type)
    .bind(&scope_id)
    .bind(period)
    .bind(limit_micros)
    .bind(hard_stop)
    .execute(&mut *tx)
    .await
    .context("inserting budget")?;
    tx.commit().await.context("commit transaction")?;

    println!(
        "budget set: scope={scope} period={period} limit={} hard_stop={hard_stop}",
        crate::pricing::format_micros(limit_micros)
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn admin_price_set(
    cfg: &Config,
    provider: &str,
    model: &str,
    input_per_1m: f64,
    output_per_1m: f64,
    cache_read_per_1m: Option<f64>,
    cache_write_per_1m: Option<f64>,
    clear_cache_read: bool,
    clear_cache_write: bool,
) -> Result<()> {
    let input = to_micros(input_per_1m)?;
    let output = to_micros(output_per_1m)?;
    let pool = crate::db::build_pool(&cfg.database).await?;
    // Close the current row and insert the new one atomically.
    let mut tx = pool.begin().await.context("begin transaction")?;
    // Close and READ the superseded row in ONE statement. A separate SELECT then
    // UPDATE loses updates under READ COMMITTED: two concurrent re-prices would
    // both read the same old row, and the second would close the first's
    // brand-new row and carry values from the stale one, silently discarding it.
    // The partial unique index does not catch that, because the second
    // transaction closed the first's row before inserting.
    let carried = sqlx::query(
        "UPDATE model_prices SET effective_to = NOW() \
         WHERE provider = $1 AND model = $2 AND effective_to IS NULL \
         RETURNING cache_read_per_1m_micros, cache_write_per_1m_micros",
    )
    .bind(provider)
    .bind(model)
    .fetch_optional(&mut *tx)
    .await
    .context("closing current price")?;
    // No row means this is the first price for the model: nothing to carry.
    let (carried_read, carried_write) = carried.map_or((None, None), |row| {
        (
            row.get::<Option<i64>, _>("cache_read_per_1m_micros"),
            row.get::<Option<i64>, _>("cache_write_per_1m_micros"),
        )
    });
    let cache_read = RateChange::resolve(cache_read_per_1m, clear_cache_read, carried_read)?;
    let cache_write = RateChange::resolve(cache_write_per_1m, clear_cache_write, carried_write)?;
    sqlx::query(
        "INSERT INTO model_prices \
         (provider, model, input_per_1m_micros, output_per_1m_micros, \
          cache_read_per_1m_micros, cache_write_per_1m_micros, source) \
         VALUES ($1, $2, $3, $4, $5, $6, 'admin price set')",
    )
    .bind(provider)
    .bind(model)
    .bind(input)
    .bind(output)
    .bind(cache_read)
    .bind(cache_write)
    .execute(&mut *tx)
    .await
    .context("inserting price")?;
    tx.commit().await.context("commit transaction")?;
    let show = |v: Option<i64>| {
        v.map_or_else(
            || "unset (conservative fallback applies)".to_owned(),
            crate::pricing::format_micros,
        )
    };
    println!(
        "price set: {provider}/{model} per 1M tokens\n  input       {}\n  output      {}\n  cache read  {}\n  cache write {}",
        crate::pricing::format_micros(input),
        crate::pricing::format_micros(output),
        show(cache_read),
        show(cache_write),
    );
    if cache_read.is_none() || cache_write.is_none() {
        println!(
            "note: an unset cache rate is charged at a conservative multiple of the \
             input rate, which OVER-charges cache reads (often by around 10x)."
        );
    }
    Ok(())
}
