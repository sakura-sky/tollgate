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
use serde_json::json;
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
        /// `--hard-stop false` makes this a soft cap: spend is still counted
        /// against it and a breach is logged at WARN, but no request is
        /// refused. Defaults to true.
        ///
        /// Takes a value rather than being a bare flag. A bare `bool` here is a
        /// presence flag, so `--hard-stop false` was an error and absence gave
        /// true: soft caps existed in the schema and in the enforcement code and
        /// could not be created.
        #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
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
        /// Prompt tokens above which THIS model re-rates the whole request.
        /// Omit to carry forward. 0 disables tiering for this model and drops
        /// any multiples it carried; use --clear-long-context instead to send it
        /// back to the deployment default.
        #[arg(long)]
        long_context_threshold: Option<u64>,
        /// Multiple applied to prompt rates above the threshold, per-mille
        /// (2000 = 2.0x). Omit to carry forward.
        #[arg(long)]
        long_context_input_permille: Option<u32>,
        /// Multiple applied to the output rate above the threshold, per-mille
        /// (1500 = 1.5x). Separate from the input multiple because real tiers
        /// move the two by different amounts. Omit to carry forward.
        #[arg(long)]
        long_context_output_permille: Option<u32>,
        /// Unset this model's tier, falling back to the deployment default.
        #[arg(
            long,
            conflicts_with_all = [
                "long_context_threshold",
                "long_context_input_permille",
                "long_context_output_permille"
            ]
        )]
        clear_long_context: bool,
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

/// The long-context flags as given on one `admin price set`.
#[derive(Clone, Copy)]
struct LongContextFlags {
    threshold: Option<u64>,
    input_permille: Option<u32>,
    output_permille: Option<u32>,
    clear: bool,
}

/// Decide what long-context columns a re-price writes, given the flags and what
/// the superseded row held.
///
/// Values carry forward unless explicitly set or cleared, for the same reason
/// the cache rates do: the common operation is bumping a base rate, and silently
/// dropping a model's tier there would under-charge every long request on it.
///
/// An explicit threshold of 0 is the documented per-model disable, and drops any
/// CARRIED multiples with it. Without that, a model that had ever been given a
/// multiple could not be disabled at all: the carried multiple would resolve
/// against the zero threshold and be refused as inert, and
/// `--clear-long-context` conflicts with the threshold flag, so no single
/// command is left. Multiples passed EXPLICITLY alongside a zero threshold are
/// still refused downstream, because that pair is a contradiction rather than a
/// leftover.
///
/// Extracted from the write path so it can be tested without a database.
fn carry_long_context(
    flags: LongContextFlags,
    carried: (Option<i64>, Option<i32>, Option<i32>),
) -> (Option<i64>, Option<i32>, Option<i32>) {
    if flags.clear {
        return (None, None, None);
    }
    let (carried_threshold, carried_in, carried_out) = carried;
    let disabling = flags.threshold == Some(0);
    (
        flags
            .threshold
            .map(|v| i64::try_from(v).unwrap_or(i64::MAX))
            .or(carried_threshold),
        flags
            .input_permille
            .map(|v| i32::try_from(v).unwrap_or(i32::MAX))
            .or(if disabling { None } else { carried_in }),
        flags
            .output_permille
            .map(|v| i32::try_from(v).unwrap_or(i32::MAX))
            .or(if disabling { None } else { carried_out }),
    )
}

/// Check that a long-context tier about to be written will actually do
/// something, and is in range. Returns the tier that will result.
///
/// Extracted from the write path so it can be tested: the branching lives in a
/// function that needs a database otherwise, which is how the first version of
/// this check shipped with the wrong resolution rule and nothing to catch it.
///
/// The threshold resolves the SAME way the price loader resolves it: this
/// model's column if set, else the deployment default. Treating an absent column
/// as zero refuses a perfectly good configuration, since inheriting the
/// deployment threshold is exactly what the per-field fallback is for.
///
/// # Errors
/// Refuses multiples that no threshold will ever apply to (they would do nothing
/// AND suppress the under-charge warning, so a long request bills flat with no
/// signal at all), and any value out of range.
fn resolve_long_context_intent(
    threshold: Option<i64>,
    input_permille: Option<i32>,
    output_permille: Option<i32>,
    deployment_threshold: u64,
) -> Result<crate::pricing::LongContextTier> {
    let effective_threshold =
        threshold.map_or(deployment_threshold, |v| u64::try_from(v).unwrap_or(0));
    let sets_multiple =
        input_permille.is_some_and(|v| v > 1_000) || output_permille.is_some_and(|v| v > 1_000);
    if sets_multiple && effective_threshold == 0 {
        bail!(
            "long-context multiples were given but no threshold applies to this model: \
             neither --long-context-threshold nor the deployment default \
             (TOLLGATE_BILLING__LONG_CONTEXT_THRESHOLD_TOKENS) is set. They would do \
             nothing AND suppress the under-charge warning, so a long request would \
             bill flat with no signal. Pass --long-context-threshold (e.g. 200000)."
        );
    }
    let tier = crate::pricing::LongContextTier {
        threshold_tokens: effective_threshold,
        multiple_permille: u32::try_from(input_permille.unwrap_or(1_000)).unwrap_or(1_000),
        output_multiple_permille: u32::try_from(output_permille.unwrap_or(1_000)).unwrap_or(1_000),
    };
    // Checked here so an out-of-range value is a clear message rather than a raw
    // constraint violation from Postgres.
    tier.validate().map_err(|e| anyhow::anyhow!(e))?;
    Ok(tier)
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
            long_context_threshold,
            long_context_input_permille,
            long_context_output_permille,
            clear_long_context,
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
                long_context_threshold,
                long_context_input_permille,
                long_context_output_permille,
                clear_long_context,
            )
            .await
        }
    }
}

/// Who the process believes ran this command.
///
/// SELF-ASSERTED, and the audit trail says so. The CLI authenticates nobody: it
/// holds the database URL and the pepper, so anyone who can run it is already
/// trusted by everything downstream. This records the OS user and host so a
/// change can be correlated with a shell history or a bastion log, not so it can
/// be relied on against someone who edited their own environment.
///
/// The control that actually binds is who can reach the database and read the
/// secrets, which is the role split in docs/OPERATIONS.md.
fn cli_principal() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned());
    // HOSTNAME is a bash SHELL variable, not an exported one, so it is absent
    // from the environment of anything a human runs from a terminal, which is
    // most uses of this CLI. Containers do export it, so relying on it alone
    // reads correctly in production and records `@unknown` on the laptop where
    // the change was actually made. Fall back to the kernel's answer.
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    format!("cli:{user}@{host}")
}

/// Append one row to the audit trail, in the SAME transaction as the change it
/// describes.
///
/// Not best effort. The first version logged a failure and carried on, on the
/// reasoning that the change had already committed so failing the command would
/// mislead the operator. That reasoning only holds outside a transaction, and it
/// leaned on someone alerting on a log line emitted to the stderr of a CLI run
/// on a laptop, which reaches no alerting pipeline anyone has. Inside the
/// transaction the objection disappears: if the audit insert fails, the change
/// genuinely did not apply, the error names `audit_log`, and "no privileged
/// change without a trail row" is an invariant rather than a hope.
///
/// Takes the transaction rather than the pool so the caller cannot accidentally
/// get the old behaviour back by passing something else.
///
/// `metadata` must never carry a secret. Key plaintext, the pepper, and provider
/// credentials are not written here, and there is no path that would put them in
/// scope: `admin key issue` passes the key's id and label, never the key.
async fn record_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    action: &str,
    resource: &str,
    metadata: serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_log (principal, action, resource, metadata) VALUES ($1, $2, $3, $4)",
    )
    .bind(cli_principal())
    .bind(action)
    .bind(resource)
    .bind(metadata)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("writing the audit row for {action}; the change was rolled back"))?;
    Ok(())
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
    let mut tx = pool.begin().await.context("begin transaction")?;
    let n = sqlx::query(
        "UPDATE api_keys SET revoked_at = NOW() WHERE prefix = $1 AND revoked_at IS NULL",
    )
    .bind(&prefix)
    .execute(&mut *tx)
    .await
    .context("revoking api key")?
    .rows_affected();
    if n > 0 {
        // Only on an actual revocation. Recording a no-op would fill the trail
        // with rows that describe nothing having happened.
        record_audit(&mut tx, "key.revoke", &prefix, json!({})).await?;
    }
    tx.commit().await.context("commit transaction")?;
    // AFTER the commit, and this is the direction that matters most. Printing
    // "revoked" and then failing to commit tells someone a compromised key is
    // dead while it is still live, and the error on stderr is easy to miss under
    // a success line on stdout.
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
    let mut tx = pool.begin().await.context("begin transaction")?;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO api_keys (key_hash, prefix, label) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(&key.key_hash)
    .bind(&key.prefix)
    .bind(label)
    .fetch_one(&mut *tx)
    .await
    .context("inserting api key")?;
    // The id and the label, never the key. The trail records that a key was
    // issued and which one, so a later revocation can be tied to it; anyone who
    // could read the plaintext from here could read it from the terminal it was
    // just printed to.
    record_audit(
        &mut tx,
        "key.issue",
        &id.to_string(),
        json!({ "label": label, "prefix": key.prefix }),
    )
    .await?;
    tx.commit().await.context("commit transaction")?;
    // Printed only after the commit. A key shown to an operator that was then
    // rolled back is a key they will store and never be able to use.
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
    // A budget change is the most consequential thing this CLI does: it is the
    // number that decides what may be spent, and nothing else in the system
    // records who moved it. In the same transaction, so the change and its trail
    // row commit together or not at all.
    record_audit(
        &mut tx,
        "budget.set",
        scope,
        json!({
            "period": period,
            "limit_micros": limit_micros,
            "hard_stop": hard_stop,
        }),
    )
    .await?;
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
    long_context_threshold: Option<u64>,
    long_context_input_permille: Option<u32>,
    long_context_output_permille: Option<u32>,
    clear_long_context: bool,
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
         RETURNING cache_read_per_1m_micros, cache_write_per_1m_micros, \
                   long_context_threshold_tokens, long_context_input_permille, \
                   long_context_output_permille",
    )
    .bind(provider)
    .bind(model)
    .fetch_optional(&mut *tx)
    .await
    .context("closing current price")?;
    // No row means this is the first price for the model: nothing to carry.
    #[allow(clippy::type_complexity)]
    let (carried_read, carried_write, carried_thr, carried_in, carried_out): (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i32>,
        Option<i32>,
    ) = carried.map_or((None, None, None, None, None), |row| {
        (
            row.get("cache_read_per_1m_micros"),
            row.get("cache_write_per_1m_micros"),
            row.get("long_context_threshold_tokens"),
            row.get("long_context_input_permille"),
            row.get("long_context_output_permille"),
        )
    });
    let cache_read = RateChange::resolve(cache_read_per_1m, clear_cache_read, carried_read)?;
    let cache_write = RateChange::resolve(cache_write_per_1m, clear_cache_write, carried_write)?;
    let (lc_threshold, lc_input, lc_output) = carry_long_context(
        LongContextFlags {
            threshold: long_context_threshold,
            input_permille: long_context_input_permille,
            output_permille: long_context_output_permille,
            clear: clear_long_context,
        },
        (carried_thr, carried_in, carried_out),
    );
    // Refuse a tier that would be inert. Multiples against a zero or absent
    // threshold do nothing AND suppress the under-charge warning, so a long
    // request bills flat with no signal, which is the one direction this proxy
    // may never err in silently.
    //
    // Resolve the threshold the SAME way the price loader will: this model's
    // column if set, otherwise the deployment default. Treating an absent column
    // as zero here would refuse a perfectly good configuration, because
    // inheriting the deployment threshold is exactly what the per-field fallback
    // is for.
    resolve_long_context_intent(
        lc_threshold,
        lc_input,
        lc_output,
        cfg.billing.long_context_tier().threshold_tokens,
    )?;
    sqlx::query(
        "INSERT INTO model_prices \
         (provider, model, input_per_1m_micros, output_per_1m_micros, \
          cache_read_per_1m_micros, cache_write_per_1m_micros, \
          long_context_threshold_tokens, long_context_input_permille, \
          long_context_output_permille, source) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'admin price set')",
    )
    .bind(provider)
    .bind(model)
    .bind(input)
    .bind(output)
    .bind(cache_read)
    .bind(cache_write)
    .bind(lc_threshold)
    .bind(lc_input)
    .bind(lc_output)
    .execute(&mut *tx)
    .await
    .context("inserting price")?;

    // A price change silently re-values every future request on that model, and
    // the `model_prices` table keeps the old row (superseded by `effective_to`)
    // without recording who replaced it or when they decided to. In the same
    // transaction as the re-price.
    record_audit(
        &mut tx,
        "price.set",
        &format!("{provider}/{model}"),
        json!({
            "input_per_1m_micros": input,
            "output_per_1m_micros": output,
            "cache_read_per_1m_micros": cache_read,
            "cache_write_per_1m_micros": cache_write,
            "long_context_threshold_tokens": lc_threshold,
            "long_context_input_permille": lc_input,
            "long_context_output_permille": lc_output,
        }),
    )
    .await?;
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
    // Print the tier this model will actually run with, resolved the way the
    // price loader resolves it. The columns alone do not say: a NULL inherits
    // the deployment default, and an explicit threshold of 0 drops multiples the
    // row was carrying. An operator who cannot see the result of a re-price
    // cannot tell a tier that applies from one that silently does nothing, which
    // is the failure this whole feature exists to make visible.
    let deployment = cfg.billing.long_context_tier();
    let resolved =
        crate::pricing::LongContextTier::resolve_row(lc_threshold, lc_input, lc_output, deployment);
    // Marked per FIELD, because they resolve per field: setting only the input
    // multiple leaves the output one inherited, and a single marker covering
    // both would say the inherited one came from this row.
    let from = |set: bool| if set { "" } else { "*" };
    if resolved.threshold_tokens == 0 {
        println!("  long context  no tier: this model is never re-rated for length");
    } else if resolved.multiple_permille > 1_000 || resolved.output_multiple_permille > 1_000 {
        println!(
            "  long context  above {}{} prompt tokens: input x{}{}, output x{}{}",
            resolved.threshold_tokens,
            from(lc_threshold.is_some()),
            format_permille(resolved.multiple_permille),
            from(lc_input.is_some()),
            format_permille(resolved.output_multiple_permille),
            from(lc_output.is_some()),
        );
    } else {
        println!(
            "  long context  above {}{} prompt tokens: NOT re-rated (both multiples \
             are 1.0x), and a prompt that large is logged as a possible under-charge",
            resolved.threshold_tokens,
            from(lc_threshold.is_some()),
        );
    }
    // This command resolves against ITS OWN environment. Run somewhere the
    // gateway's TOLLGATE_BILLING__* settings are not set and every inherited
    // field above is a guess about a different process, so say which values were
    // used rather than letting the line read as a promise.
    if lc_threshold.is_none() || lc_input.is_none() || lc_output.is_none() {
        println!(
            "                * inherited from this shell's deployment default \
             (threshold {}, input x{}, output x{}); it must match the environment \
             `tollgate serve` runs with",
            deployment.threshold_tokens,
            format_permille(deployment.multiple_permille),
            format_permille(deployment.output_multiple_permille),
        );
    }
    Ok(())
}

/// Render a per-mille multiple as a decimal, for display only.
fn format_permille(permille: u32) -> String {
    let whole = permille / 1_000;
    let frac = permille % 1_000;
    if frac == 0 {
        format!("{whole}")
    } else {
        format!("{whole}.{frac:03}")
            .trim_end_matches('0')
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The combinations that decide whether a tier is written, is inert, or is
    /// refused. Walked explicitly because the first version of this check used
    /// the wrong resolution rule and there was nothing to catch it: the logic
    /// sat inside a function that needs a database.
    #[test]
    fn long_context_intent_resolves_the_threshold_like_the_loader_does() {
        // Multiples set explicitly alongside a threshold: allowed.
        let t = resolve_long_context_intent(Some(200_000), Some(2_000), Some(1_500), 0).unwrap();
        assert_eq!(t.threshold_tokens, 200_000);
        assert_eq!(t.multiple_permille, 2_000);
        assert_eq!(t.output_multiple_permille, 1_500);

        // Multiples with NO row threshold, but the deployment has one: allowed,
        // inheriting it. Treating an absent column as zero here would refuse a
        // configuration that works, which is what an earlier version did.
        let t = resolve_long_context_intent(None, Some(2_000), None, 200_000).unwrap();
        assert_eq!(t.threshold_tokens, 200_000);

        // Multiples with no threshold anywhere: REFUSED. They would do nothing
        // and would also suppress the under-charge warning, so a long request
        // bills flat with no signal at all.
        assert!(resolve_long_context_intent(None, Some(2_000), None, 0).is_err());

        // An explicit zero threshold beats a deployment default, and refuses.
        assert!(resolve_long_context_intent(Some(0), Some(2_000), None, 200_000).is_err());

        // No multiples at all is always fine: that is just a normal re-price.
        assert!(resolve_long_context_intent(None, None, None, 0).is_ok());
        assert!(resolve_long_context_intent(Some(0), None, None, 0).is_ok());

        // A no-op multiple is not "setting" one, so it is not refused.
        assert!(resolve_long_context_intent(None, Some(1_000), Some(1_000), 0).is_ok());
    }

    #[test]
    fn long_context_intent_rejects_out_of_range_multiples() {
        // Caught here rather than as a raw Postgres constraint violation.
        assert!(resolve_long_context_intent(Some(200_000), Some(500), None, 0).is_err());
        assert!(resolve_long_context_intent(Some(200_000), Some(2_000_000), None, 0).is_err());
        assert!(resolve_long_context_intent(Some(200_000), None, Some(500), 0).is_err());
        assert!(resolve_long_context_intent(Some(200_000), None, Some(2_000_000), 0).is_err());
    }

    #[test]
    fn the_audit_principal_is_shaped_so_it_cannot_be_mistaken_for_an_identity() {
        // The `cli:` prefix is load-bearing. A bare username in an audit trail
        // reads like an authenticated identity, and this one is taken from the
        // environment by a process that authenticates nobody. The prefix says
        // which it is, at the point someone is reading the row.
        let p = cli_principal();
        assert!(p.starts_with("cli:"), "got {p}");
        assert!(p.contains('@'), "principal should carry user and host: {p}");
        // Never empty, whatever the environment is missing: a blank principal
        // would violate the NOT NULL and fail the insert, turning a missing
        // environment variable into a lost audit row.
        assert!(p.len() > "cli:@".len(), "got {p}");
    }

    #[test]
    fn permille_renders_as_the_multiple_an_operator_typed() {
        assert_eq!(format_permille(1_000), "1");
        assert_eq!(format_permille(1_500), "1.5");
        assert_eq!(format_permille(2_000), "2");
        assert_eq!(format_permille(1_250), "1.25");
        assert_eq!(format_permille(1_001), "1.001");
    }

    fn flags(
        threshold: Option<u64>,
        input_permille: Option<u32>,
        output_permille: Option<u32>,
        clear: bool,
    ) -> LongContextFlags {
        LongContextFlags {
            threshold,
            input_permille,
            output_permille,
            clear,
        }
    }

    #[test]
    fn disabling_one_model_drops_the_multiples_it_carried() {
        // The per-model disable the docs promise:
        //   admin price set --long-context-threshold 0
        // on a model that already carries multiples. Those multiples cannot
        // apply to a zero threshold, so carrying them forward would produce a
        // combination the inertness check refuses, and since
        // --clear-long-context conflicts with the threshold flag there would be
        // no single command that disables the model at all.
        let carried = (Some(200_000_i64), Some(2_000_i32), Some(1_500_i32));
        let (thr, inp, out) = carry_long_context(flags(Some(0), None, None, false), carried);
        assert_eq!(thr, Some(0), "the explicit zero is written, not inherited");
        assert_eq!(inp, None);
        assert_eq!(out, None);
        // And the result is one the write path accepts.
        let tier = resolve_long_context_intent(thr, inp, out, 200_000).unwrap();
        assert_eq!(tier.threshold_tokens, 0);
        assert!(!tier.applies_to(u64::MAX), "nothing re-rates on this model");
    }

    #[test]
    fn a_normal_reprice_still_carries_the_tier() {
        // Only an explicit zero drops the multiples. A plain rate bump, or a
        // threshold change, keeps them: dropping a tier silently there would
        // under-charge every long request on the model.
        let carried = (Some(200_000_i64), Some(2_000_i32), Some(1_500_i32));
        assert_eq!(
            carry_long_context(flags(None, None, None, false), carried),
            carried
        );
        assert_eq!(
            carry_long_context(flags(Some(400_000), None, None, false), carried),
            (Some(400_000), Some(2_000), Some(1_500))
        );
        // An explicit multiple alongside the zero threshold is a contradiction,
        // not a leftover, so it survives to be refused downstream.
        let (thr, inp, _) = carry_long_context(flags(Some(0), Some(2_000), None, false), carried);
        assert_eq!((thr, inp), (Some(0), Some(2_000)));
        assert!(resolve_long_context_intent(thr, inp, None, 200_000).is_err());
        // --clear-long-context still wipes the row and inherits the deployment.
        assert_eq!(
            carry_long_context(flags(None, None, None, true), carried),
            (None, None, None)
        );
    }
}
