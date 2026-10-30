// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Postgres pool builder and migration runner.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::postgres::{PgConnectOptions, PgConnection, PgPoolOptions};
use sqlx::{ConnectOptions, Connection, PgPool};

use crate::config::DatabaseConfig;

/// Embedded migrations from the `migrations/` directory.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// The smallest `database.max_connections` `serve` accepts: enough for one
/// key-lookup connection and three shared ones.
pub const MIN_SERVE_CONNECTIONS: u32 = 4;

/// The largest `statement_timeout` Postgres accepts, in milliseconds.
const MAX_STATEMENT_TIMEOUT_MS: u64 = i32::MAX as u64;

/// How `serve` divides `database.max_connections` between the shared pool and
/// the key-lookup pool, as `(shared, key)`. The key pool gets a quarter, at
/// most 4; the total is unchanged. `None` below [`MIN_SERVE_CONNECTIONS`].
#[must_use]
pub fn split_connections(max_connections: u32) -> Option<(u32, u32)> {
    if max_connections < MIN_SERVE_CONNECTIONS {
        return None;
    }
    let key = (max_connections / 4).min(4);
    Some((max_connections - key, key))
}

/// `database.acquire_timeout` in whole milliseconds, for the key pool's
/// `statement_timeout`, or `None` when it is outside what Postgres accepts.
/// Zero would disable the timeout, and anything above `i32::MAX` ms is refused
/// by the server at connect time.
#[must_use]
pub fn statement_timeout_ms(acquire_timeout: Duration) -> Option<u64> {
    let ms = u64::try_from(acquire_timeout.as_millis()).ok()?;
    (1..=MAX_STATEMENT_TIMEOUT_MS).contains(&ms).then_some(ms)
}

/// Whether the `setting` column of `pg_settings` for `statement_timeout` (raw
/// milliseconds, unlike `SHOW`, which prints `5s`) equals `want_ms`.
#[must_use]
pub fn statement_timeout_matches(setting: &str, want_ms: u64) -> bool {
    setting.trim().parse::<u64>().ok() == Some(want_ms)
}

/// The shared pool, sized at `database.max_connections`. Used by every admin
/// command; `serve` uses [`build_pool_sized`] with its share instead.
pub async fn build_pool(cfg: &DatabaseConfig) -> Result<PgPool> {
    build_pool_sized(cfg, cfg.max_connections).await
}

/// The shared pool with an explicit size.
pub async fn build_pool_sized(cfg: &DatabaseConfig, max_connections: u32) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(cfg.acquire_timeout)
        .connect(&cfg.url)
        .await
        .with_context(|| format!("connecting to Postgres at {}", redact(&cfg.url)))
}

/// The key-lookup pool `serve` uses for API-key lookups only.
///
/// Apart from the shared pool so a flood of lookups (well-formed junk keys cost
/// one each) cannot take the connections the ledger, reload and maintenance
/// need. Every connection carries `statement_timeout` = `acquire_timeout`, set
/// as a startup option, so a stalled lookup is ended by the server: dropping a
/// sqlx query future does not cancel the query.
///
/// The setting is checked twice. Once here, on a plain connection, so that a
/// connection pooler dropping startup options refuses the boot with a clear
/// message (before migrations run). And on every connection the pool opens
/// later, so one that comes up without it is discarded rather than used.
///
/// Statement and slow-acquire logging are off: the lookup's bound value is a
/// key prefix, and a flood of slow acquires during an outage would otherwise
/// write a log line per request. Store failures are logged once per window by
/// the HTTP gate instead.
pub async fn build_key_pool(cfg: &DatabaseConfig, size: u32, timeout_ms: u64) -> Result<PgPool> {
    let options = PgConnectOptions::from_str(&cfg.url)
        .with_context(|| format!("parsing the Postgres URL {}", redact(&cfg.url)))?
        .options([("statement_timeout", format!("{timeout_ms}ms"))])
        .disable_statement_logging();

    // Bounded like the pool's own connects: a plain connection has no timeout.
    let connecting = || {
        format!(
            "connecting to Postgres at {} (key-lookup pool)",
            redact(&cfg.url)
        )
    };
    let mut probe = tokio::time::timeout(cfg.acquire_timeout, options.connect())
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {:?}", cfg.acquire_timeout))
        .with_context(connecting)?
        .with_context(connecting)?;
    let setting = tokio::time::timeout(cfg.acquire_timeout, statement_timeout_setting(&mut probe))
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {:?}", cfg.acquire_timeout))
        .context("reading statement_timeout on the key-lookup pool")?
        .context("reading statement_timeout on the key-lookup pool")?;
    let _ = tokio::time::timeout(cfg.acquire_timeout, probe.close()).await;
    if !statement_timeout_matches(&setting, timeout_ms) {
        anyhow::bail!(
            "{}; refusing to start",
            statement_timeout_mismatch(timeout_ms, &setting)
        );
    }

    PgPoolOptions::new()
        .max_connections(size)
        .acquire_timeout(cfg.acquire_timeout)
        .acquire_slow_level(log::LevelFilter::Off)
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                let setting = statement_timeout_setting(conn).await?;
                if statement_timeout_matches(&setting, timeout_ms) {
                    Ok(())
                } else {
                    Err(sqlx::Error::Configuration(
                        statement_timeout_mismatch(timeout_ms, &setting).into(),
                    ))
                }
            })
        })
        .connect_with(options)
        .await
        .with_context(|| {
            format!(
                "connecting to Postgres at {} (key-lookup pool)",
                redact(&cfg.url)
            )
        })
}

/// `statement_timeout` as `pg_settings` reports it: raw milliseconds.
async fn statement_timeout_setting(conn: &mut PgConnection) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT setting FROM pg_settings WHERE name = 'statement_timeout'")
        .fetch_one(conn)
        .await
}

fn statement_timeout_mismatch(want_ms: u64, setting: &str) -> String {
    format!(
        "the key-lookup pool asked Postgres for statement_timeout = {want_ms}ms but the \
         connection reports {setting:?}ms. Something between Tollgate and Postgres (often a \
         connection pooler in transaction mode) is dropping startup options, which would leave \
         key lookups without a server-side timeout. Connect Tollgate to Postgres directly, or \
         configure the pooler to pass startup options through"
    )
}

pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    MIGRATOR
        .run(pool)
        .await
        .context("running database migrations")
}

/// Strip credentials from a Postgres URL for logging.
fn redact(url: &str) -> String {
    if let Some(at) = url.rfind('@') {
        if let Some(scheme_end) = url.find("://") {
            return format!("{}://***@{}", &url[..scheme_end], &url[at + 1..]);
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_split_without_changing_the_total() {
        for max in 0..MIN_SERVE_CONNECTIONS {
            assert_eq!(split_connections(max), None, "{max}");
        }
        for (max, want) in [
            (4, (3, 1)),
            (7, (6, 1)),
            (8, (6, 2)),
            (10, (8, 2)),
            (12, (9, 3)),
            (16, (12, 4)),
            (100, (96, 4)),
            (u32::MAX, (u32::MAX - 4, 4)),
        ] {
            let got = split_connections(max).expect("splits");
            assert_eq!(got, want, "{max}");
            assert_eq!(got.0 + got.1, max);
            assert!(got.0 >= 3 && got.1 >= 1);
        }
    }

    #[test]
    fn statement_timeout_is_whole_milliseconds_within_postgres_range() {
        assert_eq!(statement_timeout_ms(Duration::ZERO), None);
        assert_eq!(statement_timeout_ms(Duration::from_micros(999)), None);
        assert_eq!(statement_timeout_ms(Duration::from_millis(1)), Some(1));
        assert_eq!(statement_timeout_ms(Duration::from_micros(1_900)), Some(1));
        assert_eq!(statement_timeout_ms(Duration::from_secs(5)), Some(5_000));
        assert_eq!(
            statement_timeout_ms(Duration::from_millis(2_147_483_647)),
            Some(2_147_483_647)
        );
        assert_eq!(
            statement_timeout_ms(Duration::from_millis(2_147_483_648)),
            None
        );
        assert_eq!(statement_timeout_ms(Duration::MAX), None);
    }

    #[test]
    fn statement_timeout_check_compares_milliseconds_not_text() {
        assert!(statement_timeout_matches("5000", 5_000));
        assert!(statement_timeout_matches(" 5000 ", 5_000));
        assert!(!statement_timeout_matches("0", 5_000));
        assert!(!statement_timeout_matches("4999", 5_000));
        // What `SHOW` would print for 5000 ms; `pg_settings.setting` never does.
        assert!(!statement_timeout_matches("5s", 5_000));
        assert!(!statement_timeout_matches("", 5_000));
        assert!(!statement_timeout_matches("abc", 5_000));
    }
}
