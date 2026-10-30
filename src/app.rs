// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! HTTP application: router construction, shared state, and graceful shutdown.
//!
//! Production `serve` builds the shared [`GatewayCore`] from the Postgres and
//! Redis/Valkey backends and the configured providers, then serves the same
//! request flow the demo uses.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;
use redis::aio::ConnectionManager;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::apikey::KeyHasher;
use crate::auth_gate::Authed;
use crate::backends::{
    PgKeyStore, PgSpendLedger, PgUsageSink, RedisBudgetBackend, SpendLedger, budget_spent,
    load_budgets, load_prices,
};
use crate::budget::{
    Budget, PROVIDER_ANTHROPIC, PROVIDER_MOCK, PROVIDER_OPENAI, PROVIDER_VERTEX, RequestCtx, Scope,
};
use crate::config::Config;
use crate::gateway::{
    GatewayCore, KeyStore, Metrics, ReserveError, Settlement, UsageSink, outcome_response,
};
use crate::pricing::{ModelPrice, PriceBook, Usage, format_micros};
use crate::provider::{MockProvider, Provider, ProviderError};
use crate::providers::{CacheSemantics, OpenAiProvider, stream_requested};
use crate::routes::health;

/// Cap the SSE line-reassembly buffer so an upstream that never emits a newline
/// cannot grow it without bound.
const MAX_SSE_LINE_BUFFER: usize = 1024 * 1024;
/// Idle timeout. Each chunk's upstream read and its send to the client share one
/// window of this length: the stream ends if no upstream chunk arrives within
/// it, or if the client has not accepted the chunk when it closes. After the
/// provider's finish (OpenAI streams only), a client that disconnects or misses
/// the window is drained instead.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard cap on total stream duration.
const STREAM_MAX_DURATION: Duration = Duration::from_secs(15 * 60);
/// How long the relay keeps reading after a client stops, once the provider has
/// signalled its finish, to receive the final usage and the end marker. Short,
/// because nothing billable is expected after the finish; the drain stops if
/// more output arrives without a new finish.
const STREAM_DRAIN_GRACE: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub redis: ConnectionManager,
    /// The live gateway core. Swapped atomically by the reload task so budget and
    /// price changes take effect without a restart; requests read a snapshot.
    pub core: Arc<ArcSwap<GatewayCore>>,
    /// Current budget config, swapped in lockstep with the core, for the
    /// read-only console endpoints.
    pub budgets: Arc<ArcSwap<Vec<Budget>>>,
    /// Typed handle to the OpenAI upstream adapter for the streaming path (the
    /// buffered path uses it via the provider map). None when disabled. Upstream
    /// config is static, so this is built once and not hot-reloaded.
    pub openai: Option<Arc<OpenAiProvider>>,
    /// Typed handle to the Anthropic adapter, for the same reason: the streaming
    /// path needs `forward_stream` and the stream meter, which the `dyn Provider`
    /// map cannot expose.
    pub anthropic: Option<Arc<crate::providers::AnthropicProvider>>,
}

/// The immutable parts of a [`GatewayCore`], reused every time the core is rebuilt
/// on config reload. Only budgets and prices change at runtime.
#[derive(Clone)]
struct CoreParts {
    hasher: KeyHasher,
    dummy_hash: String,
    keys: Arc<dyn KeyStore>,
    usage: Arc<dyn UsageSink>,
    providers: HashMap<String, Arc<dyn Provider>>,
    admission_exact: bool,
    redis: ConnectionManager,
    /// So the budget backend can rebuild a counter Valkey has lost.
    ledger: Arc<dyn SpendLedger>,
    /// Held HERE, not in the core, because the core is rebuilt every reload
    /// tick and counters that reset every fifteen seconds are worse than none.
    metrics: Arc<Metrics>,
}

impl CoreParts {
    fn build(&self, budgets: Vec<Budget>, prices: PriceBook) -> GatewayCore {
        GatewayCore {
            hasher: self.hasher.clone(),
            dummy_hash: self.dummy_hash.clone(),
            keys: self.keys.clone(),
            budgets: Arc::new(RedisBudgetBackend::new(
                self.redis.clone(),
                budgets,
                self.ledger.clone(),
            )),
            usage: self.usage.clone(),
            prices: Arc::new(prices),
            providers: self.providers.clone(),
            admission_exact: self.admission_exact,
            metrics: self.metrics.clone(),
        }
    }
}

/// One problem with one budget. `counted` findings raise the
/// `tollgate_unmatchable_budgets` gauge because the operator can clear them; the
/// Vertex-route note cannot be cleared by any budget, so it is logged only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BudgetFinding {
    budget: String,
    reason: String,
    counted: bool,
}

/// Find every budget that cannot match some or all of the requests it appears to
/// cover, so it is reported rather than silently not applying. A budget can have
/// more than one finding.
///
/// Cannot match anything:
/// - an `api_key` id that is not a UUID (only possible for a row written outside
///   the CLI before migration 0011 added its constraint);
/// - a `provider` this deployment has not enabled;
/// - a `model` whose provider is not enabled, or which has no current price
///   (an unpriced model is refused before budgets are consulted).
///
/// Matches only part of the traffic it appears to cover:
/// - a priced `model` that is also priced under a second name differing by a
///   `/` prefix (such as `x` and `google/x`), when that name has no budget of its
///   own for the same period that is at least as hard: requests sent under the
///   other name are not covered;
/// - a `vertex` provider or model budget when the OpenAI-compatible route also
///   reaches Vertex: those requests are scoped as `openai`. Logged, not counted.
///
/// It does not refuse to boot: a budget for a provider that is switched off for
/// now is legitimate configuration. It makes the gap visible. Aliases that do not
/// differ by a `/` prefix (dated model ids, say) are not detected.
fn unmatchable_budgets(
    budgets: &[Budget],
    prices: &PriceBook,
    is_enabled: impl Fn(&str) -> bool,
    openai_reaches_vertex: bool,
) -> Vec<BudgetFinding> {
    let mut out = Vec::new();
    let mut add = |budget: &str, reason: String, counted: bool| {
        out.push(BudgetFinding {
            budget: budget.to_owned(),
            reason,
            counted,
        });
    };
    let vertex_route_note = || {
        "requests to /v1/chat/completions also reach Vertex, are scoped as `openai`, and are \
         not covered by this budget"
            .to_owned()
    };
    for b in budgets {
        let at = format!("{} ({})", b.scope, b.period.as_str());
        match &b.scope {
            Scope::Global => {}
            Scope::ApiKey(id) => {
                if crate::budget::canonical_key_id(id).is_none() {
                    add(
                        &at,
                        "the key id is not a UUID, so it matches no key".to_owned(),
                        true,
                    );
                }
            }
            Scope::Provider(p) => {
                if !is_enabled(p) {
                    add(
                        &at,
                        format!("provider {p:?} is not enabled in this deployment"),
                        true,
                    );
                } else if p == PROVIDER_VERTEX && openai_reaches_vertex {
                    add(&at, vertex_route_note(), false);
                }
            }
            Scope::Model(pm) => {
                let (p, m) = pm.split_once(':').unwrap_or((pm.as_str(), ""));
                if !is_enabled(p) {
                    add(
                        &at,
                        format!("provider {p:?} is not enabled in this deployment"),
                        true,
                    );
                    continue;
                }
                if prices.lookup(p, m).is_none() {
                    let reason =
                        format!("{p}/{m} has no current price, and unpriced models are refused");
                    add(&at, reason, true);
                    continue;
                }
                if p == PROVIDER_VERTEX && openai_reaches_vertex {
                    add(&at, vertex_route_note(), false);
                }
                let tail = |s: &str| s.rsplit_once('/').map(|(_, t)| t.to_owned());
                // Covered only by a budget for the same period that is at least as
                // hard: a soft cap on the alias does not stop its traffic.
                let has_own_budget = |alias: &str| {
                    let scope = Scope::Model(format!("{p}:{alias}"));
                    budgets.iter().any(|o| {
                        o.scope == scope && o.period == b.period && (o.hard_stop || !b.hard_stop)
                    })
                };
                let mut aliases: Vec<&str> = prices
                    .models_for(p)
                    .filter(|other| {
                        *other != m
                            && (tail(other).as_deref() == Some(m)
                                || tail(m).as_deref() == Some(*other))
                            && !has_own_budget(other)
                    })
                    .collect();
                aliases.sort_unstable();
                for alias in aliases {
                    let reason = format!(
                        "{p}/{alias} is also priced and has no budget at least as hard for \
                         this period, so requests sent as {alias:?} are not covered"
                    );
                    add(&at, reason, true);
                }
            }
        }
    }
    out.sort();
    out
}

/// Log the findings when they differ from the last report, and set the gauge to
/// the number of distinct budgets with a counted finding every time. Returns
/// whether anything was logged. Comparing with the last report keeps a standing
/// problem from logging every reload cycle.
fn report_unmatchable(
    last: &mut Vec<BudgetFinding>,
    now: Vec<BudgetFinding>,
    metrics: &Metrics,
) -> bool {
    let budgets: std::collections::BTreeSet<&str> = now
        .iter()
        .filter(|f| f.counted)
        .map(|f| f.budget.as_str())
        .collect();
    metrics.unmatchable_budgets.store(
        u64::try_from(budgets.len()).unwrap_or(u64::MAX),
        std::sync::atomic::Ordering::Relaxed,
    );
    if now == *last {
        return false;
    }
    for f in &now {
        tracing::warn!(
            budget = %f.budget,
            reason = %f.reason,
            "a configured budget cannot match some or all of its requests"
        );
    }
    if now.is_empty() && !last.is_empty() {
        tracing::info!("every configured budget can now match its requests");
    }
    *last = now;
    true
}

/// Refuse to serve against a database that is behind this binary.
///
/// Migrations are applied by an explicit `admin migrate` (outside the dev-only
/// `auto_migrate` boot option), so a deploy can easily start a new binary
/// against an old schema. That failure is silent and
/// dangerous rather than loud: the usage sink is best-effort and logs-and-drops
/// on error, so every ledger row would vanish while Valkey counters kept
/// enforcing and everything looked healthy. A later cache flush then rebuilds
/// those counters from an under-counted ledger and permits overspend.
///
/// Fail closed, like every other unknown in this system.
async fn ensure_schema_current(pool: &PgPool) -> Result<()> {
    // A brand new database has no migrations table at all, which is simply
    // "nothing applied" rather than an error worth surfacing differently.
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    let missing: Vec<String> = crate::db::MIGRATOR
        .iter()
        .filter(|m| !applied.contains(&m.version))
        .map(|m| format!("{} ({})", m.version, m.description))
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "database schema is behind this binary; {} migration(s) not applied: {}. \
             Run `tollgate admin migrate` before starting. Refusing to serve: the usage \
             ledger would silently fail to record spend against an older schema.",
            missing.len(),
            missing.join(", ")
        );
    }
    Ok(())
}

/// Check the Vertex project and location before either is built into a URL.
///
/// Both are spliced into every Vertex request URL, and the location into the
/// hostname (see `providers::vertex_host`), on requests that carry Tollgate's
/// Google access token. A value containing `.`, `:`, `/`, `@`, `?` or `#` in the
/// location, or `/`, `@`, `?` or `#` in the project, could move that request,
/// token included, to another host or path, so anything outside the character
/// sets Google uses is refused at boot.
///
/// Locations are lowercase letters, digits and inner hyphens (`us-central1`,
/// `europe-west4`, `eu`, `global`). Projects are either a project number (all
/// digits) or a project ID: lowercase letters, digits and hyphens starting with
/// a letter, plus `.` and `:` for legacy domain-scoped IDs
/// (`example.com:my-project`). Neither form can start with `.`, so neither can
/// be a `.` or `..` path segment.
fn validate_vertex_target(project: &str, location: &str) -> Result<()> {
    if project.is_empty() || location.is_empty() {
        anyhow::bail!(
            "Vertex (TOLLGATE_PROVIDERS__VERTEX__ENABLED, or the OpenAI endpoint with \
             upstream=vertex) requires TOLLGATE_PROVIDERS__VERTEX__PROJECT and __LOCATION"
        );
    }
    let location_ok = location
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !location.starts_with('-')
        && !location.ends_with('-');
    if !location_ok {
        anyhow::bail!(
            "TOLLGATE_PROVIDERS__VERTEX__LOCATION must be a Google Cloud location such as \
             us-central1 or global (lowercase letters, digits and hyphens), got {location:?}"
        );
    }
    let project_number = project.bytes().all(|b| b.is_ascii_digit());
    let project_id = project.starts_with(|c: char| c.is_ascii_lowercase())
        && project.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.' | b':')
        });
    if !(project_number || project_id) {
        anyhow::bail!(
            "TOLLGATE_PROVIDERS__VERTEX__PROJECT must be a Google Cloud project number \
             (all digits) or project ID (lowercase letters, digits and hyphens, starting \
             with a letter; domain-scoped IDs may also contain . and :), got {project:?}"
        );
    }
    Ok(())
}

pub async fn serve(cfg: Config) -> Result<()> {
    // Fail closed at boot on a fallback multiple that would price an unpriced
    // cache class below the model's own input rate. That is the same fail-open
    // the nullable rate columns exist to prevent, just moved into config.
    let cache_fallback = cfg.billing.cache_rate_fallback();
    cache_fallback
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid billing config: {e}"))?;

    let long_context = cfg.billing.long_context_tier();
    long_context
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid billing config: {e}"))?;
    // A deployment-wide multiple against a threshold of zero re-rates nothing
    // and, because the under-charge warning needs a threshold to test against,
    // logs nothing either: the operator sees a 2x in their environment and gets
    // neither the charge nor a signal that it is inert. Refused here rather than
    // warned, matching `admin price set`, which refuses the same pair on a row.
    //
    // Deliberately NOT part of `validate()`. A per-model tier resolves to this
    // same shape whenever a model is disabled with `--long-context-threshold 0`
    // and inherits the deployment multiples, and that is a working
    // configuration, not a mistake.
    if long_context.has_stranded_uplift() {
        anyhow::bail!(
            "invalid billing config: TOLLGATE_BILLING__LONG_CONTEXT_MULTIPLE_PERMILLE or \
             __LONG_CONTEXT_OUTPUT_MULTIPLE_PERMILLE is above 1000, but \
             __LONG_CONTEXT_THRESHOLD_TOKENS is 0, so no request is ever re-rated and \
             none is logged as a possible under-charge either. Set a threshold (e.g. \
             200000), or leave both multiples at 1000 to keep the warning without the \
             re-rating"
        );
    }

    // Fail closed at boot on a missing/weak pepper. Starting with an ephemeral
    // pepper would make every already-issued key fail to verify while the
    // service still reported healthy: a silent, total auth outage.
    if cfg.security.api_key_pepper.len() < 16 {
        anyhow::bail!(
            "TOLLGATE_SECURITY__API_KEY_PEPPER must be a fixed secret of at least 16 bytes \
             (the same value for `serve` and `admin key issue`); refusing to start"
        );
    }
    if cfg.security.api_key_pepper == crate::config::DEV_PLACEHOLDER_PEPPER {
        anyhow::bail!(
            "TOLLGATE_SECURITY__API_KEY_PEPPER is still the .env.example placeholder; \
             set a real secret before starting"
        );
    }

    // Provider, admission and Redis URL checks need no I/O, so they run before
    // the database is touched: a misconfigured process never reaches the schema.
    let vertex_in_use = cfg.providers.vertex.enabled
        || (cfg.providers.openai.enabled && cfg.providers.openai.upstream == "vertex");
    if vertex_in_use {
        validate_vertex_target(
            &cfg.providers.vertex.project,
            &cfg.providers.vertex.location,
        )?;
    }
    if cfg.providers.openai.enabled {
        let oc = &cfg.providers.openai;
        match oc.upstream.as_str() {
            // Project and location were validated above.
            "vertex" => {}
            "custom" => {
                // An empty key would make the adapter fall back to the GCP
                // metadata token and send it to the custom host: refuse.
                if oc.base_url.is_empty() || oc.api_key.is_empty() {
                    anyhow::bail!(
                        "OpenAI endpoint with upstream=custom requires \
                         TOLLGATE_PROVIDERS__OPENAI__BASE_URL and __API_KEY"
                    );
                }
                // Anthropic's OpenAI-compatible endpoint reports NO prompt-cache
                // tokens: its token-details fields are documented as always
                // empty and caching is unsupported there. Pointing the OpenAI
                // adapter at it would silently under-count every cached Claude
                // request, and no tripwire on our side can detect data the
                // upstream never sends. Use /v1/messages instead.
                //
                // A host compare, so it is a lint rather than a guarantee: it
                // cannot see a CNAME, an egress proxy, or a gateway fronting
                // Anthropic. It catches the obvious mistake.
                if let Some(host) = reqwest::Url::parse(&oc.base_url)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| h.trim_end_matches('.').to_lowercase()))
                {
                    if host == "anthropic.com" || host.ends_with(".anthropic.com") {
                        anyhow::bail!(
                            "refusing to use {host} as an OpenAI-compatible upstream: that \
                             endpoint reports no prompt-cache tokens, so every cached request \
                             would be silently under-counted. Enable the anthropic provider \
                             and use /v1/messages instead."
                        );
                    }
                }
            }
            other => anyhow::bail!(
                "TOLLGATE_PROVIDERS__OPENAI__UPSTREAM must be 'vertex' or 'custom' (got {other:?})"
            ),
        }
    }
    let admission_mode = cfg.providers.admission.to_ascii_lowercase();
    if admission_mode != "fast" && admission_mode != "exact" {
        anyhow::bail!(
            "TOLLGATE_PROVIDERS__ADMISSION must be 'fast' or 'exact' (got {:?})",
            cfg.providers.admission
        );
    }
    let redis_client = redis::Client::open(cfg.redis.url.as_str()).context("parsing Redis URL")?;

    // `serve` takes a share of the connections for API-key lookups, so the total
    // stays `max_connections`. Admin commands keep the whole pool.
    let Some((shared_connections, key_connections)) =
        crate::db::split_connections(cfg.database.max_connections)
    else {
        anyhow::bail!(
            "TOLLGATE_DATABASE__MAX_CONNECTIONS must be at least {} for `serve` (got {}): a \
             quarter of the connections, at most 4, is kept for API-key lookups so a flood of \
             them cannot starve the ledger, and each pool needs at least one",
            crate::db::MIN_SERVE_CONNECTIONS,
            cfg.database.max_connections
        );
    };
    let Some(key_timeout_ms) = crate::db::statement_timeout_ms(cfg.database.acquire_timeout) else {
        anyhow::bail!(
            "TOLLGATE_DATABASE__ACQUIRE_TIMEOUT must be between 1ms and 2147483647ms (got {:?}): \
             it is also the server-side statement_timeout of every API-key lookup, and Postgres \
             accepts nothing outside that range (0 would switch the timeout off)",
            cfg.database.acquire_timeout
        );
    };

    // Every check that can fail on config alone goes above this line; the test
    // `config_errors_are_reported_before_postgres_is_touched` pins the order.
    let db = crate::db::build_pool_sized(&cfg.database, shared_connections).await?;
    // Before migrating: it refuses to start if Postgres does not apply the
    // lookups' statement_timeout, and a process that cannot bound its key
    // lookups must exit without touching the schema.
    let key_db = crate::db::build_key_pool(&cfg.database, key_connections, key_timeout_ms).await?;
    // DEV ONLY (the compose stack): apply migrations at boot. It runs after the
    // config checks above, so a mistake in any of those never touches the schema.
    // Production keeps migrations an explicit `admin migrate` step, and the
    // schema check below still guards it.
    if cfg.database.auto_migrate {
        tracing::warn!(
            "TOLLGATE_DATABASE__AUTO_MIGRATE is set: applying migrations at boot (dev only)"
        );
        crate::db::run_migrations(&db).await?;
    }
    ensure_schema_current(&db).await?;
    let redis = ConnectionManager::new(redis_client)
        .await
        .context("connecting to Redis")?;

    // Load budget config and prices from Postgres into the core.
    let budgets = load_budgets(&db).await.context("loading budgets")?;
    let prices = load_prices(&db, cache_fallback, long_context)
        .await
        .context("loading model prices")?;
    tracing::info!(
        budgets = budgets.len(),
        priced_models = prices.len(),
        "loaded budget and price config"
    );

    let hasher = KeyHasher::new(cfg.security.api_key_pepper.clone().into_bytes());
    let dummy_hash = hasher.hash("tollgate-fixed-dummy-secret");

    // Rebuild budget counters from the durable ledger so a cache flush or
    // restart cannot reset budgets to zero.
    let restored = crate::backends::reconcile_counters(&db, &redis, &budgets)
        .await
        .context("reconciling budget counters")?;
    tracing::info!(restored, "reconciled budget counters from ledger");

    // Ledger retention: ensure current/next month partitions exist and drop those
    // older than the window. Run once now (so partitions are present before we
    // serve), then periodically.
    if let Err(e) = crate::backends::run_usage_maintenance(&db, cfg.retention.window).await {
        tracing::warn!(error = %e, "usage ledger maintenance failed at startup");
    }
    spawn_maintenance_task(db.clone(), cfg.retention.window);

    // Do NOT follow redirects: a cross-host redirect would resend the request,
    // including provider credentials (e.g. Anthropic's x-api-key), to another
    // host. Treat any 3xx as an upstream error instead.
    let http = reqwest::Client::builder()
        // The PROVIDER timeout, not the local-route one. A non-streaming LLM
        // sends no headers until generation finishes, so this has to allow for a
        // full large response.
        .timeout(cfg.providers.request_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building HTTP client")?;

    // Streaming client for the OpenAI SSE relay: NO total request timeout (a total
    // deadline would sever long streams mid-body), only a connect timeout.
    // Redirects stay disabled (credential protection). Per-read idle timeout and a
    // max stream duration are enforced by the relay task.
    let stream_http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building streaming HTTP client")?;

    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    if cfg.providers.enable_mock {
        providers.insert(PROVIDER_MOCK.to_owned(), Arc::new(MockProvider));
    }
    let mut anthropic_handle: Option<Arc<crate::providers::AnthropicProvider>> = None;
    if cfg.providers.anthropic.enabled {
        let arc = Arc::new(crate::providers::AnthropicProvider::new(
            http.clone(),
            stream_http.clone(),
            cfg.providers.anthropic.api_key.clone(),
            cfg.providers.anthropic.base_url.clone(),
            cfg.providers.anthropic.version.clone(),
        ));
        providers.insert(
            PROVIDER_ANTHROPIC.to_owned(),
            arc.clone() as Arc<dyn Provider>,
        );
        anthropic_handle = Some(arc);
    }
    if cfg.providers.vertex.enabled {
        providers.insert(
            PROVIDER_VERTEX.to_owned(),
            Arc::new(crate::providers::VertexProvider::new(
                http.clone(),
                cfg.providers.vertex.project.clone(),
                cfg.providers.vertex.location.clone(),
                cfg.providers.vertex.access_token.clone(),
            )),
        );
    }
    let mut openai_handle: Option<Arc<crate::providers::OpenAiProvider>> = None;
    if cfg.providers.openai.enabled {
        let oc = &cfg.providers.openai;
        let p = match oc.upstream.as_str() {
            "vertex" => crate::providers::OpenAiProvider::vertex(
                http.clone(),
                stream_http.clone(),
                &cfg.providers.vertex.project,
                &cfg.providers.vertex.location,
                cfg.providers.vertex.access_token.clone(),
            ),
            "custom" => crate::providers::OpenAiProvider::custom(
                http.clone(),
                stream_http.clone(),
                oc.base_url.clone(),
                oc.api_key.clone(),
            ),
            // Validated before the database is touched; see the pre-flight checks.
            _ => unreachable!("openai upstream validated at boot"),
        };
        let arc = Arc::new(p);
        providers.insert(PROVIDER_OPENAI.to_owned(), arc.clone() as Arc<dyn Provider>);
        openai_handle = Some(arc);
    }
    let admission_exact = admission_mode == "exact";
    tracing::info!(
        providers = providers.len(),
        admission = %cfg.providers.admission,
        "providers registered"
    );

    let parts = CoreParts {
        hasher,
        dummy_hash,
        keys: Arc::new(PgKeyStore::new(key_db)),
        usage: Arc::new(PgUsageSink::new(db.clone())),
        providers,
        admission_exact,
        redis: redis.clone(),
        ledger: Arc::new(PgSpendLedger::new(db.clone())),
        metrics: Arc::new(Metrics::default()),
    };

    // Report, once at boot, every budget that cannot match some or all of its
    // requests.
    let openai_reaches_vertex =
        cfg.providers.openai.enabled && cfg.providers.openai.upstream == "vertex";
    let unmatchable = unmatchable_budgets(
        &budgets,
        &prices,
        |p| parts.providers.contains_key(p),
        openai_reaches_vertex,
    );
    report_unmatchable(&mut Vec::new(), unmatchable.clone(), &parts.metrics);

    let core = Arc::new(ArcSwap::from_pointee(parts.build(budgets.clone(), prices)));
    let budgets_view = Arc::new(ArcSwap::from_pointee(budgets));

    // Periodically reload budgets and prices from Postgres so `admin budget set`
    // and `admin price set` take effect without a restart. Only the config
    // swaps; spend counters are left untouched, so a changed limit applies to the
    // existing counter and a new budget's counter, if it has none yet, is seeded
    // from the spend already recorded this period when it is picked up.
    if !cfg.reload.interval.is_zero() {
        spawn_reload_task(
            db.clone(),
            parts.clone(),
            core.clone(),
            budgets_view.clone(),
            cfg.reload.interval,
            cache_fallback,
            long_context,
            unmatchable,
            openai_reaches_vertex,
        );
        tracing::info!(interval = ?cfg.reload.interval, "config hot-reload enabled");
    }

    let state = AppState {
        db,
        redis,
        core,
        budgets: budgets_view,
        openai: openai_handle,
        anthropic: anthropic_handle,
    };
    let app = router(
        state,
        cfg.http.request_timeout,
        cfg.database.acquire_timeout,
    );

    let listener = TcpListener::bind(cfg.http.bind)
        .await
        .with_context(|| format!("binding {}", cfg.http.bind))?;

    tracing::info!(addr = %cfg.http.bind, "tollgate listening");

    axum::serve(
        listener,
        axum::ServiceExt::<axum::extract::Request>::into_make_service(app),
    )
    .with_graceful_shutdown(shutdown_signal(cfg.http.shutdown_grace))
    .await
    .context("http server error")
}

/// Background task: reload budgets and prices from Postgres on an interval and
/// atomically swap them into the live core. On any load error it logs and keeps
/// the current config rather than dropping enforcement.
#[allow(clippy::too_many_arguments)]
fn spawn_reload_task(
    db: PgPool,
    parts: CoreParts,
    core: Arc<ArcSwap<GatewayCore>>,
    budgets_view: Arc<ArcSwap<Vec<Budget>>>,
    interval: Duration,
    cache_fallback: crate::pricing::CacheRateFallback,
    long_context: crate::pricing::LongContextTier,
    // The set reported at boot, so the first reload does not repeat it.
    mut last_unmatchable: Vec<BudgetFinding>,
    openai_reaches_vertex: bool,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // the first tick fires immediately; skip it
        loop {
            ticker.tick().await;
            let budgets = match load_budgets(&db).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "config reload: budgets query failed; keeping current");
                    continue;
                }
            };
            // A price book loads whole or not at all, and one unusable row keeps
            // the PREVIOUS book rather than swapping in a partial one. Dropping
            // just the bad model would make its requests unpriced, which the
            // gateway refuses anyway, so the partial book buys nothing and costs
            // every other model a silent change of price mid-flight. At boot the
            // same failure is fatal: there is no previous book to keep.
            let prices = match load_prices(&db, cache_fallback, long_context).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "config reload: prices failed to load; keeping current. \
                         Every model is still priced as it was before this cycle"
                    );
                    continue;
                }
            };
            let (n_b, n_p) = (budgets.len(), prices.len());
            // Seed counters for any budget new since the last snapshot so a
            // runtime-added cap enforces against this period's existing spend
            // instead of starting at zero. On failure, skip this cycle rather
            // than swap in an under-enforcing budget.
            if let Err(e) =
                crate::backends::seed_missing_counters(&db, &parts.redis, &budgets).await
            {
                tracing::warn!(error = %e, "config reload: seeding new counters failed; keeping current config");
                continue;
            }
            if n_b == 0 {
                tracing::warn!(
                    "config reload: no budgets configured; all traffic will be denied (fail closed)"
                );
            } else if !budgets.iter().any(|b| matches!(b.scope, Scope::Global)) {
                tracing::warn!(
                    "config reload: no Global budget; the deployment-wide backstop is not enforced"
                );
            }
            let unmatchable = unmatchable_budgets(
                &budgets,
                &prices,
                |p| parts.providers.contains_key(p),
                openai_reaches_vertex,
            );
            report_unmatchable(&mut last_unmatchable, unmatchable, &parts.metrics);
            core.store(Arc::new(parts.build(budgets.clone(), prices)));
            budgets_view.store(Arc::new(budgets));
            tracing::debug!(budgets = n_b, priced_models = n_p, "reloaded config");
        }
    });
}

/// Background task: run usage-ledger partition maintenance (create-ahead + drop
/// old partitions) every few hours.
fn spawn_maintenance_task(db: PgPool, window: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(6 * 60 * 60));
        ticker.tick().await; // the first tick fires immediately; startup already ran once
        loop {
            ticker.tick().await;
            match crate::backends::run_usage_maintenance(&db, window).await {
                Ok(()) => tracing::debug!("usage ledger maintenance ran"),
                Err(e) => tracing::warn!(error = %e, "usage ledger maintenance failed"),
            }
        }
    });
}

/// Build the HTTP service: the routes, wrapped by the authentication gate.
///
/// Outermost first:
///
/// 1. a response conversion, so the service's response type is axum's;
/// 2. a panic guard for the gate itself;
/// 3. the gate ([`crate::auth_gate::require_key`]), outside routing so it also
///    answers unknown paths and wrong methods under `/v1` and `/console/`
///    before the router can (a `Router::layer` gate would leak `Allow` on a
///    405);
/// 4. tracing, inside the gate, so unauthenticated probes are counted on
///    `/metrics` rather than traced one by one;
/// 5. a panic guard around the routes, inside the trace span;
/// 6. the routes.
///
/// `key_lookup_timeout` is `database.acquire_timeout`: the gate's deadline for
/// a key lookup is that plus [`crate::auth_gate::DEADLINE_MARGIN`].
pub fn router(
    state: AppState,
    request_timeout: Duration,
    key_lookup_timeout: Duration,
) -> impl tower::Service<
    axum::extract::Request,
    Response = Response,
    Error = std::convert::Infallible,
    Future: Send + 'static,
> + Clone
+ Send
+ 'static {
    let gate = crate::auth_gate::GateState::new(state.core.clone(), key_lookup_timeout);

    // Local routes: bounded by the HTTP timeout, because nothing else bounds
    // them. They do their own work and should never run long.
    let local = Router::new()
        .route("/healthz", get(health::live))
        .route("/readyz", get(health::ready))
        .route("/metrics", get(metrics))
        .route("/console", get(console))
        .route("/console/budgets", get(console_budgets))
        .route("/console/usage", get(console_usage))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            request_timeout,
        ));

    // Proxy routes: deliberately NOT under that layer.
    //
    // The layer's clock starts at the request head, while the provider client's
    // starts later, after auth, parse and reserve. Applying the same duration to
    // both means the layer always wins, and tower-http cancels the handler
    // future rather than letting the provider call fail on its own terms. That
    // matters because a non-streaming LLM sends no headers until generation
    // finishes, so any response slower than the timeout, which large outputs
    // routinely are, was cancelled mid-flight.
    //
    // A cancelled handler is now caught by the Settlement guard, so it no longer
    // strands a reservation, but it would still turn every slow-but-successful
    // request into a 503 charged at its reservation. These routes carry their
    // own bounds instead: the buffered client's request timeout, and the
    // streaming path's idle and max-duration guards.
    let proxied = Router::new()
        .route("/v1/chat/completions", post(openai_chat))
        // Native Anthropic, at the path the SDK and LiteLLM actually use.
        .route("/v1/messages", post(anthropic_messages))
        // Everything else under /v1/messages/ (count_tokens, batches) would
        // otherwise fall into the catch-all as provider "messages" and write an
        // unpriced ledger row per call. Refuse explicitly.
        .route("/v1/messages/{*rest}", post(anthropic_messages_unsupported))
        .route("/v1/{provider}/{*rest}", post(gateway));

    let routes = local
        .merge(proxied)
        .with_state(state)
        // Applied around the routes, so it sees a panic from anywhere inside
        // them, including from a dependency, and logs it inside the request's
        // trace span. A panic on one request for one key must fail that request,
        // not the process: this gateway is the thing standing between every
        // client and its provider, so a total outage is the most expensive
        // failure it has. The binary is built to unwind rather than abort for
        // exactly this reason.
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(TraceLayer::new_for_http());

    tower::ServiceBuilder::new()
        .map_response(IntoResponse::into_response)
        // The same guard again, for a panic in the gate (outside the routes).
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(axum::middleware::from_fn_with_state(
            gate,
            crate::auth_gate::require_key,
        ))
        .service(routes)
}

/// The response for a request that panicked. The body is fixed text: a panic
/// message can carry anything that was in scope, so it is logged and never
/// returned.
fn panic_response(_panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    tracing::error!("a request panicked; failing that request, not the process");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [("x-tollgate-reason", "backend_error")],
        Json(json!({"error": "internal error"})),
    )
        .into_response()
}

async fn gateway(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path((provider, rest)): Path<(String, String)>,
    body: String,
) -> Response {
    let core = state.core.load_full();
    let outcome = core
        .evaluate_authenticated(&authed.key_id, authed.auth_elapsed, &provider, &rest, &body)
        .await;
    outcome_response(outcome)
}

/// Sub-paths of `/v1/messages` that Tollgate does not meter.
///
/// `count_tokens` is free but unmetered, and `batches` bills asynchronously in a
/// way no per-request ledger row can capture. Without this they fall into the
/// catch-all as provider "messages" and write an unpriced row per call.
///
/// Behind the gate like every `/v1` path, so only an authenticated caller sees
/// this 404. It takes `Authed` only so that, like every gated handler, it fails
/// closed if the gate is ever missing.
async fn anthropic_messages_unsupported(Extension(_authed): Extension<Authed>) -> Response {
    anthropic_error(
        StatusCode::NOT_FOUND,
        "not_found_error",
        "only /v1/messages is proxied; batches and count_tokens are not metered",
    )
}

/// Native Anthropic Messages endpoint, at the path the Anthropic SDK and LiteLLM
/// actually use.
///
/// The pre-existing `/v1/anthropic/messages` route cannot serve a native client:
/// the SDK appends `/v1/messages` to its base URL (so that route becomes
/// `/v1/anthropic/v1/messages` and is refused), the key arrives as `x-api-key`
/// which the gateway did not read, and successes come back wrapped in a
/// `{"response": ..., "tollgate": ...}` envelope the SDK cannot parse. That route
/// and its envelope are left untouched for existing users; this is the one to
/// point a real client at.
///
/// Claude is served natively rather than by translating OpenAI requests, because
/// Anthropic's own OpenAI-compatible endpoint does not report prompt-cache tokens
/// at all: routing through it would silently under-count every cached request.
async fn anthropic_messages(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // `x-api-key` is read as a Tollgate key by the gate, on `/v1/messages` and
    // the paths under it only (see `crate::auth_gate::require_key`).

    // A beta can change the billing rate: the 1M-context beta bills input above
    // 200k tokens at twice standard, which one input rate cannot express. Refuse
    // what we cannot meter rather than forwarding it.
    for v in headers.get_all("anthropic-beta") {
        let Ok(s) = v.to_str() else {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "malformed anthropic-beta header",
            );
        };
        if let Some(unknown) = crate::providers::unknown_anthropic_beta(s) {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!(
                    "anthropic-beta '{unknown}' is not supported: a beta can change the \
                     per-token rate, which a single configured price cannot express"
                ),
            );
        }
    }

    // The service tier is pinned in the adapter's forward, not here, so the
    // legacy /v1/anthropic/messages route gets it too.
    let wants_stream = serde_json::from_str::<Value>(&body)
        .map(|v| stream_requested(&v))
        .unwrap_or(false);
    if wants_stream {
        return anthropic_messages_stream(state, authed, body).await;
    }
    let core = state.core.load_full();
    let outcome = core
        .evaluate_authenticated(
            &authed.key_id,
            authed.auth_elapsed,
            PROVIDER_ANTHROPIC,
            "messages",
            &body,
        )
        .await;
    anthropic_outcome_response(outcome)
}

/// Record a refusal that happened BEFORE the request was forwarded, on a
/// streaming route.
///
/// The streaming routes run their own admission rather than going through
/// [`GatewayCore::evaluate`], and they used to record nothing at all for a
/// refusal. An hour of budget denials, or of a provider's count endpoint being
/// down, read as an hour in which no requests arrived: the ledger is the only
/// place a refused request is visible, since there is no metric for it either.
///
/// Zero cost on every one of these, because nothing was reserved and nothing was
/// forwarded. The decision words are the ones the buffered path already uses for
/// the same situations, so the two agree rather than inventing a third
/// vocabulary for streams.
///
/// A parse failure is deliberately NOT recorded, matching `evaluate`: the
/// request never resolved to a model, and a caller looping on a malformed body
/// would otherwise fill the ledger with rows that describe their bug rather than
/// this deployment's spend.
async fn record_stream_refusal(
    core: &GatewayCore,
    key_id: &str,
    provider: &str,
    model: &str,
    decision: &str,
    started: Instant,
    provider_micros: u128,
) {
    core.record(
        key_id,
        provider,
        model,
        Usage::default(),
        0,
        decision,
        started,
        provider_micros,
    )
    .await;
}

/// An Anthropic-shaped error, so SDK exception classes resolve correctly.
///
/// The BODY carries Anthropic's error vocabulary, because the SDK maps it to an
/// exception class. The `x-tollgate-reason` HEADER carries Tollgate's, the same
/// on every route, because that is what an operator alerts on. They are not the
/// same word: a budget refusal is `permission_error` to the SDK and
/// `budget_exceeded` to the operator.
fn anthropic_error(status: StatusCode, kind: &str, message: &str) -> Response {
    anthropic_error_reason(status, kind, default_reason_for(kind), message)
}

/// Tollgate's reason for an Anthropic error kind, where the two correspond.
///
/// Only for kinds used one way in this file. Two refusals do NOT correspond and
/// must call [`anthropic_error_reason`] directly: an unpriced model is an
/// `invalid_request_error` to the SDK but `unpriced` to an operator, and a
/// backend failure is an `api_error` but `backend_error`. If you use one of
/// these kinds for a new reason, do not reach for this function.
fn default_reason_for(kind: &str) -> &'static str {
    match kind {
        "authentication_error" => "unauthenticated",
        "permission_error" => "budget_exceeded",
        "api_error" => "upstream_error",
        "invalid_request_error" | "not_found_error" => "bad_request",
        // Anything new. Listed above rather than left to fall through, so that
        // adding a kind is a decision rather than an accident.
        _ => "bad_request",
    }
}

/// As [`anthropic_error`], with the operator-facing reason stated explicitly.
fn anthropic_error_reason(
    status: StatusCode,
    kind: &str,
    reason: &'static str,
    message: &str,
) -> Response {
    (
        status,
        [("x-tollgate-reason", reason)],
        Json(json!({"type": "error", "error": {"type": kind, "message": message}})),
    )
        .into_response()
}

/// Map an [`Outcome`] to a native Anthropic response: the upstream body verbatim
/// on success, Anthropic's error envelope on refusal.
pub(crate) fn anthropic_outcome_response(outcome: crate::gateway::Outcome) -> Response {
    use crate::gateway::Outcome;
    match outcome {
        Outcome::Allowed {
            status,
            body,
            cost_micros,
            overhead_micros,
            ..
        } => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            [
                ("x-tollgate-cost", format_micros(cost_micros)),
                ("x-tollgate-overhead-us", overhead_micros.to_string()),
            ],
            Json(body),
        )
            .into_response(),
        Outcome::Unauthenticated => anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid or missing API key",
        ),
        Outcome::BadRequest(m) => {
            anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", &m)
        }
        Outcome::Unpriced { provider, model } => anthropic_error_reason(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "unpriced",
            &format!("no price configured for {provider}/{model}"),
        ),
        Outcome::BudgetDenied(d) => anthropic_error(
            StatusCode::PAYMENT_REQUIRED,
            "permission_error",
            &format!(
                "budget exhausted: {} of {} spent",
                format_micros(d.spent_micros),
                format_micros(d.limit_micros)
            ),
        ),
        Outcome::BackendError(_) => anthropic_error_reason(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "backend_error",
            "gateway backend unavailable",
        ),
        Outcome::Upstream(_) => {
            anthropic_error(StatusCode::BAD_GATEWAY, "api_error", "upstream error")
        }
    }
}

/// OpenAI-compatible Chat Completions endpoint. Routes to the `openai` provider
/// (which fronts Vertex's OpenAI endpoint or a custom upstream) through the same
/// reserve-then-settle enforcement, and returns the upstream's OpenAI body
/// verbatim so OpenAI clients, LiteLLM, and ADK agents work unchanged.
///
/// Streaming (`stream: true`) is dispatched to [`openai_chat_stream`], which
/// relays the upstream SSE and meters usage as [`OpenAiStreamMeter`] describes.
/// Buffered requests run the shared [`GatewayCore::evaluate`] flow unchanged.
async fn openai_chat(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    body: String,
) -> Response {
    // Peek whether the client asked for streaming. A body that does not parse is
    // treated as non-streaming so the buffered path returns a proper OpenAI error
    // envelope (rather than opening an event-stream for a malformed request).
    let wants_stream = serde_json::from_str::<Value>(&body)
        .map(|v| stream_requested(&v))
        .unwrap_or(false);
    if wants_stream {
        return openai_chat_stream(state, authed, body).await;
    }
    let core = state.core.load_full();
    let outcome = core
        .evaluate_authenticated(
            &authed.key_id,
            authed.auth_elapsed,
            PROVIDER_OPENAI,
            "chat/completions",
            &body,
        )
        .await;
    openai_outcome_response(outcome)
}

/// Streaming path for `/v1/messages`.
///
/// Mirrors the OpenAI streaming path, with two deliberate differences.
///
/// Exact admission is HONOURED here rather than refused. The OpenAI path refuses
/// it because that adapter has no real token counter and would silently fall back
/// to the estimate. Anthropic has `count_tokens`, on the same body that gets
/// forwarded, so refusing would punish exactly the operators who chose strictness.
///
/// Metering uses the Anthropic meter because the protocol differs:
/// `message_start` carries a small placeholder `output_tokens`, completion is
/// signalled by `message_stop` rather than `[DONE]`, and an error event arrives
/// on an otherwise-healthy 200 stream followed by a normal close.
async fn anthropic_messages_stream(state: AppState, authed: Authed, body: String) -> Response {
    // Authentication is part of the overhead, as it was when it ran here.
    let started = crate::gateway::backdate(authed.auth_elapsed);
    let key_id = authed.key_id;
    let core = state.core.load_full();
    let Some(provider) = state.anthropic.clone() else {
        return anthropic_error_reason(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "backend_error",
            "anthropic upstream is not configured",
        );
    };

    let parsed = match provider.parse_streaming("messages", &body) {
        Ok(p) => p,
        Err(ProviderError::BadRequest(m)) => {
            return anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", &m);
        }
        Err(ProviderError::Upstream(_) | ProviderError::MeteringFailed(_)) => {
            return anthropic_error(StatusCode::BAD_GATEWAY, "api_error", "upstream error");
        }
    };

    let Some(price) = core
        .prices
        .lookup(PROVIDER_ANTHROPIC, &parsed.model)
        .cloned()
    else {
        record_stream_refusal(
            &core,
            &key_id,
            PROVIDER_ANTHROPIC,
            &parsed.model,
            "unpriced",
            started,
            0,
        )
        .await;
        return anthropic_error_reason(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "unpriced",
            &format!("no price configured for anthropic/{}", parsed.model),
        );
    };

    // Exact admission counts the SAME body that is forwarded, so reserved and
    // enforced cannot drift.
    //
    // The count is a round trip to the provider, so its duration is PROVIDER
    // time, not gateway overhead. Without subtracting it the console reports
    // ~240ms of "Tollgate overhead" for what is almost entirely Anthropic,
    // against ~4ms on the other paths.
    let mut provider_micros: u128 = 0;
    let input_tokens = if core.admission_exact {
        let c0 = Instant::now();
        let counted = provider
            .count_input_tokens("messages", &body, &parsed)
            .await;
        provider_micros += c0.elapsed().as_micros();
        match counted {
            Ok(n) => n,
            Err(e) => {
                // Upstream, not backend: the count is a round trip to the
                // provider. Matches the buffered path in `GatewayCore::evaluate`,
                // including the split between a provider that REFUSED the count
                // (the caller's body is wrong, 400) and one that could not answer
                // (502).
                // warn, not error: a `BadRequest` here is the caller's malformed
                // body, and a client looping on its own bug must not fill the
                // operator's error budget. The genuinely alarming cases, a
                // rejected credential, are logged at ERROR by `count_refusal`.
                tracing::warn!(error = %e.to_string(), "pre-flight token count failed");
                record_stream_refusal(
                    &core,
                    &key_id,
                    PROVIDER_ANTHROPIC,
                    &parsed.model,
                    "error",
                    started,
                    provider_micros,
                )
                .await;
                return match e {
                    ProviderError::BadRequest(m) => {
                        anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", &m)
                    }
                    _ => anthropic_error(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        "pre-flight token count failed upstream",
                    ),
                };
            }
        }
    } else {
        parsed.estimated_input_tokens
    };

    let reserve_profile = provider.prompt_reserve_profile(&parsed);
    let reserve_micros = price
        .reserve_micros(input_tokens, parsed.max_output_tokens, reserve_profile)
        .max(1);
    // The prompt leg alone, for the one case where the provider tells us it
    // generated nothing. Computed here while the parsed request is still in
    // scope: it is moved into the settlement guard below.
    let prompt_floor_micros = price
        .reserve_micros(input_tokens, 0, reserve_profile)
        .max(1);
    let reservation = {
        let ctx = RequestCtx {
            key_id: &key_id,
            provider: PROVIDER_ANTHROPIC,
            model: &parsed.model,
        };
        match core.budgets.reserve(&ctx, reserve_micros).await {
            Ok(r) => r,
            Err(ReserveError::Denied(d)) => {
                record_stream_refusal(
                    &core,
                    &key_id,
                    PROVIDER_ANTHROPIC,
                    &parsed.model,
                    "rejected_budget",
                    started,
                    provider_micros,
                )
                .await;
                return anthropic_error(
                    StatusCode::PAYMENT_REQUIRED,
                    "permission_error",
                    &format!("budget exceeded: {d}"),
                );
            }
            Err(ReserveError::Backend(m)) => {
                tracing::error!(error = %m, "budget backend error on stream reserve");
                record_stream_refusal(
                    &core,
                    &key_id,
                    PROVIDER_ANTHROPIC,
                    &parsed.model,
                    "error",
                    started,
                    provider_micros,
                )
                .await;
                return anthropic_error_reason(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "api_error",
                    "backend_error",
                    "gateway temporarily unavailable",
                );
            }
        }
    };

    // Admission path only, excluding the count round trip, matching how the
    // buffered path defines overhead.
    let overhead_micros = i64::try_from(
        started
            .elapsed()
            .as_micros()
            .saturating_sub(provider_micros),
    )
    .unwrap_or(i64::MAX);
    let guard = Settlement::new(
        &core,
        reservation,
        reserve_micros,
        key_id,
        PROVIDER_ANTHROPIC.to_owned(),
        parsed.model.clone(),
        overhead_micros,
    );

    let upstream = match provider.forward_stream(&body).await {
        Ok(r) => r,
        Err(ProviderError::MeteringFailed(m)) => {
            // Possibly billed; charge the reservation rather than releasing.
            tracing::error!(detail = %m, "anthropic stream may have been billed but not metered");
            guard
                .settle(reserve_micros, Usage::default(), "estimated")
                .await;
            return anthropic_error(StatusCode::BAD_GATEWAY, "api_error", "upstream error");
        }
        Err(e) => {
            tracing::warn!(detail = %e.to_string(), "anthropic stream upstream error");
            guard.settle(0, Usage::default(), "error").await;
            return anthropic_error(StatusCode::BAD_GATEWAY, "api_error", "upstream error");
        }
    };

    let up_status = upstream.status();
    if !up_status.is_success() {
        guard.settle(0, Usage::default(), "error").await;
        return anthropic_error(up_status, "api_error", "upstream provider error");
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(relay_stream(
        upstream,
        tx,
        guard,
        price,
        reserve_micros,
        prompt_floor_micros,
        StreamMeter::Anthropic {
            line_buf: Vec::new(),
            meter: crate::providers::AnthropicStreamMeter::new(),
        },
        PROVIDER_ANTHROPIC,
        parsed.model,
    ));

    (
        StatusCode::OK,
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
        ],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}

/// Streaming path for `/v1/chat/completions`. Authenticates, prices, and reserves
/// the worst case up front (fast admission), opens the upstream SSE stream, then
/// spawns [`relay_stream`] to pass chunks through to the client while metering
/// usage. Settlement is owned by a [`Settlement`] guard that charges the measured
/// cost once the provider has reported its finish, usage and `[DONE]` (see
/// [`OpenAiStreamMeter`]) and the FULL reservation otherwise: for example a
/// client disconnect or stall before the finish, an upstream error or close
/// before `[DONE]`, a drain that expires or is aborted by more output, an
/// idle/duration timeout, missing, implausible or contradictory usage, or a
/// panic. Usage on OpenAI-protocol streams is assumed cumulative: an upstream
/// that reports per-chunk deltas would be measured low.
async fn openai_chat_stream(state: AppState, authed: Authed, body: String) -> Response {
    // Authentication is part of the overhead, as it was when it ran here.
    let started = crate::gateway::backdate(authed.auth_elapsed);
    let key_id = authed.key_id;
    let core = state.core.load_full();
    let Some(provider) = state.openai.clone() else {
        // The endpoint is registered even when the openai upstream is disabled;
        // without a handle there is nothing to stream to.
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend_error",
            "openai upstream is not configured",
        );
    };

    // Exact admission reserves against a real pre-flight token count, but the
    // OpenAI adapter has no exact counter (it would silently fall back to the fast
    // estimate, under-reserving on token-dense input). Rather than downgrade
    // silently, refuse streaming under exact admission (fail closed). The buffered
    // path stays available; fast admission is the supported streaming mode.
    if core.admission_exact {
        return openai_error(
            StatusCode::NOT_IMPLEMENTED,
            "unsupported",
            "streaming is not supported under exact admission; use fast admission or the \
             non-streaming endpoint",
        );
    }

    let parsed = match provider.parse_streaming("chat/completions", &body) {
        Ok(p) => p,
        Err(ProviderError::BadRequest(m)) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                &format!("invalid request: {m}"),
            );
        }
        // Pre-reservation, so nothing can have been billed on either variant.
        Err(ProviderError::Upstream(_) | ProviderError::MeteringFailed(_)) => {
            return openai_error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "upstream provider error",
            );
        }
    };

    let Some(price) = core.prices.lookup(PROVIDER_OPENAI, &parsed.model).cloned() else {
        record_stream_refusal(
            &core,
            &key_id,
            PROVIDER_OPENAI,
            &parsed.model,
            "unpriced",
            started,
            0,
        )
        .await;
        return openai_error(
            StatusCode::BAD_REQUEST,
            "unpriced",
            &format!("no price configured for openai/{}", parsed.model),
        );
    };

    // Reserve the worst case (estimated input + capped output), floored to 1 micro
    // so nothing meters as zero. build_payload pins the outbound max_tokens to the
    // same cap, so the upstream cannot generate past what we reserved.
    // Uses the same helper as the buffered path in gateway::evaluate, so the two
    // reservations cannot drift apart. The OpenAI adapter cannot report cache
    // writes, so the prompt leg does not carry the write rate here.
    let reserve_micros = price
        .reserve_micros(
            parsed.estimated_input_tokens,
            parsed.max_output_tokens,
            provider.prompt_reserve_profile(&parsed),
        )
        .max(1);
    let reservation = {
        let ctx = RequestCtx {
            key_id: &key_id,
            provider: PROVIDER_OPENAI,
            model: &parsed.model,
        };
        match core.budgets.reserve(&ctx, reserve_micros).await {
            Ok(r) => r,
            Err(ReserveError::Denied(d)) => {
                record_stream_refusal(
                    &core,
                    &key_id,
                    PROVIDER_OPENAI,
                    &parsed.model,
                    "rejected_budget",
                    started,
                    0,
                )
                .await;
                return openai_error(
                    StatusCode::PAYMENT_REQUIRED,
                    "budget_exceeded",
                    &format!("budget exceeded: {d}"),
                );
            }
            Err(ReserveError::Backend(m)) => {
                tracing::error!(error = %m, "budget backend error on stream reserve");
                record_stream_refusal(
                    &core,
                    &key_id,
                    PROVIDER_OPENAI,
                    &parsed.model,
                    "error",
                    started,
                    0,
                )
                .await;
                return openai_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "backend_error",
                    "gateway temporarily unavailable",
                );
            }
        }
    };

    // Overhead = admission path only (everything before the upstream call), matching
    // the buffered path's definition. Build the settlement guard now: from here on,
    // every exit settles the reservation exactly once (Drop covers panic/cancel).
    let overhead_micros = i64::try_from(started.elapsed().as_micros()).unwrap_or(i64::MAX);
    let guard = Settlement::new(
        &core,
        reservation,
        reserve_micros,
        key_id,
        PROVIDER_OPENAI.to_owned(),
        parsed.model.clone(),
        overhead_micros,
    );

    // Open the upstream stream. A connection failure means the provider never
    // billed, so settle to zero (parity with the buffered forward-error path).
    let upstream = match provider.forward_stream(&body).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(detail = %e.to_string(), "openai stream upstream error");
            guard.settle(0, Usage::default(), "error").await;
            return openai_error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "upstream provider error",
            );
        }
    };

    // A non-2xx upstream is an error page, not an SSE stream: do not relay it.
    // Charge zero (not billed) and pass the status through with a generic body so
    // no upstream detail (GCP project/region/host) leaks to the client.
    // reqwest and axum both re-export `http::StatusCode`, so the upstream status
    // passes straight through (generic body only: no upstream detail leaks).
    let up_status = upstream.status();
    if !up_status.is_success() {
        guard.settle(0, Usage::default(), "error").await;
        return openai_error(up_status, "upstream_error", "upstream provider error");
    }

    // Relay: bounded channel gives natural backpressure (a slow client throttles
    // upstream reads). The spawned task owns the guard and settles at its single
    // exit point.
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    // The meter costs each usage reading to keep the dearest, so it needs the
    // price; the relay keeps its own copy for the settlement.
    let meter = StreamMeter::OpenAi(OpenAiStreamMeter::new(
        provider.cache_semantics(),
        price.clone(),
    ));
    tokio::spawn(relay_stream(
        upstream,
        tx,
        guard,
        price,
        reserve_micros,
        // Unused on this path: the OpenAI meter has no pre-generation signal, so
        // there is no prompt-leg discount.
        reserve_micros,
        meter,
        PROVIDER_OPENAI,
        parsed.model,
    ));

    (
        StatusCode::OK,
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
        ],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}

/// The relay's time limits. Passed in rather than read from the constants, so
/// tests can drive every exit without waiting on a real clock.
#[derive(Debug, Clone, Copy)]
struct RelayLimits {
    idle: Duration,
    max: Duration,
    drain: Duration,
}

const RELAY_LIMITS: RelayLimits = RelayLimits {
    idle: STREAM_IDLE_TIMEOUT,
    max: STREAM_MAX_DURATION,
    drain: STREAM_DRAIN_GRACE,
};

/// Why the relay stopped reading the upstream. Logged with every settlement, so
/// an operator can see why a stream was charged as estimated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamEnd {
    /// The upstream closed the connection.
    Eof,
    /// The provider's own end marker arrived (`[DONE]`, or `message_stop`).
    Done,
    /// The client disconnected.
    ClientGone,
    /// The client did not accept a chunk before the idle or max-duration
    /// deadline.
    SlowClient,
    /// The upstream went quiet for the idle timeout, or the stream hit its
    /// maximum duration.
    IdleOrMax,
    /// The upstream connection failed mid-body.
    UpstreamError,
    /// The client stopped reading after the provider's finish, and the end
    /// marker did not arrive within the drain grace period or before the
    /// stream's maximum duration.
    DrainExpired,
    /// More output arrived while draining, so the finish was not final and the
    /// drain stopped at once rather than keep an unread continuation running.
    DrainAborted,
}

impl StreamEnd {
    fn as_str(self) -> &'static str {
        match self {
            Self::Eof => "eof",
            Self::Done => "done",
            Self::ClientGone => "client_gone",
            Self::SlowClient => "slow_client",
            Self::IdleOrMax => "idle_or_max",
            Self::UpstreamError => "upstream_error",
            Self::DrainExpired => "drain_expired",
            Self::DrainAborted => "drain_aborted",
        }
    }
}

/// Relay upstream SSE chunks to the client while metering usage, then settle the
/// [`Settlement`] guard exactly once: the measured cost when the meter says the
/// provider reported its final usage and finished, otherwise the FULL
/// reservation (never under-charge). Enforces an idle timeout, a max total
/// duration, and a bounded line-reassembly buffer so a hostile or hung upstream
/// cannot grief the gateway.
#[allow(clippy::too_many_arguments)]
async fn relay_stream(
    upstream: reqwest::Response,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    guard: Settlement,
    price: ModelPrice,
    reserve_micros: i64,
    // Cost of the reserved PROMPT leg alone, for the one case where a provider
    // tells us it generated nothing. Computed by the caller, which is where the
    // input token count is known.
    prompt_floor_micros: i64,
    mut meter: StreamMeter,
    provider: &'static str,
    model: String,
) {
    let (end, drained) = relay_body(upstream.bytes_stream(), &tx, &mut meter, RELAY_LIMITS).await;
    if meter.is_clean() && !meter.usage_seen() {
        tracing::warn!("stream ended cleanly without any usage; charging reservation");
    }
    let (actual, usage, decision) =
        choose_settlement(&meter, &price, reserve_micros, prompt_floor_micros);
    log_stream_settlement(provider, &model, end, drained, &meter, decision);
    guard.settle(actual, usage, decision).await;
}

/// The relay loop, separated from settlement so tests can drive it with a
/// scripted upstream. Returns why it stopped, and whether it was draining.
///
/// Every chunk is metered BEFORE it is relayed: what the provider sent is what
/// it bills, whether or not the client is still there to receive it.
///
/// The drain exists because the client chooses when to disconnect. Once the
/// provider has signalled its finish, nothing billable is left to generate, but
/// the final usage and the end marker may still be in flight. A client that
/// stops reading at that point would otherwise turn a fully measured stream
/// into a reservation charge. So after the finish, a failed or timed-out send
/// stops forwarding but keeps reading the upstream, for at most
/// [`RelayLimits::drain`], until the end marker. Before the finish it stops at
/// once, as it always has: draining mid-generation would make the provider bill
/// an answer nobody reads.
async fn relay_body<S, E>(
    mut stream: S,
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    meter: &mut StreamMeter,
    limits: RelayLimits,
) -> (StreamEnd, bool)
where
    S: futures_util::stream::Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let deadline = tokio::time::Instant::now() + limits.max;
    let mut drain_until: Option<tokio::time::Instant> = None;
    loop {
        let mut wait_until = (tokio::time::Instant::now() + limits.idle).min(deadline);
        if let Some(d) = drain_until {
            wait_until = wait_until.min(d);
        }
        let draining = drain_until.is_some();
        match tokio::time::timeout_at(wait_until, stream.next()).await {
            // Idle timeout, max duration, or the drain grace ran out.
            Err(_) => {
                if draining {
                    tracing::warn!(
                        "stream drain ended before the provider's end marker; \
                         charging the reservation"
                    );
                    return (StreamEnd::DrainExpired, true);
                }
                tracing::warn!("stream aborted: idle timeout or max duration exceeded");
                return (StreamEnd::IdleOrMax, false);
            }
            Ok(None) => {
                meter.finish_input();
                return (StreamEnd::Eof, draining);
            }
            Ok(Some(Err(e))) => {
                tracing::warn!(detail = %e, "stream upstream error mid-body");
                return (StreamEnd::UpstreamError, draining);
            }
            Ok(Some(Ok(chunk))) => {
                meter.observe(&chunk);
                let finished = meter.finished();
                if draining {
                    if finished {
                        return (StreamEnd::Done, true);
                    }
                    if !meter.drain_eligible() {
                        tracing::warn!(
                            "stream produced more output after its finish while draining; \
                             charging the reservation"
                        );
                        return (StreamEnd::DrainAborted, true);
                    }
                    continue;
                }
                // Relay downstream, bounded by the SAME idle/duration deadline as
                // the upstream read: a client that stops reading fills the bounded
                // channel and would otherwise park this task (and hold the
                // reservation and upstream connection) indefinitely.
                let cut = match tokio::time::timeout_at(wait_until, tx.send(Ok(chunk))).await {
                    Ok(Ok(())) => None,
                    Ok(Err(_)) => {
                        tracing::debug!("stream client disconnected");
                        Some(StreamEnd::ClientGone)
                    }
                    Err(_) => {
                        tracing::warn!("stream client too slow to drain (idle/max duration)");
                        Some(StreamEnd::SlowClient)
                    }
                };
                // Nothing billable follows the end marker, so exit there rather
                // than hold the upstream connection and the reservation open
                // until the idle timeout fires.
                if finished {
                    return (StreamEnd::Done, false);
                }
                if let Some(cut) = cut {
                    if meter.drain_eligible() {
                        tracing::info!(
                            cause = cut.as_str(),
                            "client stopped reading after the provider's finish; \
                             reading on to the end marker"
                        );
                        drain_until =
                            Some((tokio::time::Instant::now() + limits.drain).min(deadline));
                        continue;
                    }
                    return (cut, false);
                }
            }
        }
    }
}

/// One line per settled stream: why it ended, what the meter saw, and how it was
/// charged. A measured stream that ended normally logs at debug; anything else
/// at info, because those are the rows an operator reconciling against an
/// invoice will ask about.
fn log_stream_settlement(
    provider: &str,
    model: &str,
    end: StreamEnd,
    drained: bool,
    meter: &StreamMeter,
    decision: &str,
) {
    let finish_seen = meter.finish_seen();
    let usage_seen = meter.usage_seen();
    let done_seen = meter.is_clean();
    if decision == "allowed" && end == StreamEnd::Done && !drained {
        tracing::debug!(
            provider,
            model,
            end = end.as_str(),
            drained,
            finish_seen,
            usage_seen,
            done_seen,
            decision,
            "stream settled"
        );
    } else {
        tracing::info!(
            provider,
            model,
            end = end.as_str(),
            drained,
            finish_seen,
            usage_seen,
            done_seen,
            decision,
            "stream settled"
        );
    }
}

/// Per-provider stream metering, so the relay stays protocol-agnostic.
///
/// The two protocols disagree about what "finished" means, and getting that
/// wrong is a silent under-charge. Anthropic's `message_start` carries a small
/// placeholder `output_tokens`, an error event arrives on an otherwise-healthy
/// 200 stream and is followed by a normal close, and nothing billable follows
/// `message_stop`. The OpenAI protocol's rules are on [`OpenAiStreamMeter`].
pub enum StreamMeter {
    OpenAi(OpenAiStreamMeter),
    Anthropic {
        line_buf: Vec<u8>,
        meter: crate::providers::AnthropicStreamMeter,
    },
}

impl StreamMeter {
    fn observe(&mut self, chunk: &[u8]) {
        match self {
            Self::OpenAi(m) => m.observe(chunk),
            Self::Anthropic { line_buf, meter } => {
                line_buf.extend_from_slice(chunk);
                while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = line_buf.drain(..=pos).collect();
                    if let Ok(s) = std::str::from_utf8(&line) {
                        meter.observe_line(s.trim_end());
                    }
                }
                if line_buf.len() > MAX_SSE_LINE_BUFFER {
                    line_buf.clear();
                }
            }
        }
    }

    /// The upstream closed. Lets the OpenAI meter parse a final line that had
    /// no trailing newline. The Anthropic path is unchanged.
    fn finish_input(&mut self) {
        if let Self::OpenAi(m) = self {
            m.flush();
        }
    }

    /// Whether the provider's own end marker arrived. Transport EOF is not
    /// evidence of completion on either protocol: a proxy can close cleanly
    /// mid-generation.
    fn is_clean(&self) -> bool {
        match self {
            Self::OpenAi(m) => m.done_seen,
            Self::Anthropic { meter, .. } => meter.is_clean(),
        }
    }

    /// Whether the relay can stop reading now.
    fn finished(&self) -> bool {
        self.is_clean()
    }

    /// Whether a client that stops reading now should be drained rather than
    /// cut: the provider has signalled its finish and the end marker is still
    /// to come. OpenAI only; the Anthropic path is unchanged.
    fn drain_eligible(&self) -> bool {
        match self {
            Self::OpenAi(m) => m.finish_seen && !m.done_seen,
            Self::Anthropic { .. } => false,
        }
    }

    fn finish_seen(&self) -> bool {
        match self {
            Self::OpenAi(m) => m.finish_seen,
            Self::Anthropic { meter, .. } => meter.is_clean(),
        }
    }

    fn usage_seen(&self) -> bool {
        match self {
            Self::OpenAi(m) => m.best.is_some(),
            Self::Anthropic { meter, .. } => meter.usage().is_some(),
        }
    }

    // No `started()` here on purpose. It existed to feed the prompt-leg
    // discount, and having it at this level is what made that discount
    // applicable to OpenAI, where a stream cut before its finish carries either
    // no usage or only a partial count, so the gateway cannot tell how much the
    // provider generated and will bill. `choose_settlement` reaches
    // the Anthropic meter directly, so the discount cannot be spread to a
    // provider that has no pre-generation signal.
}

/// Meter for an OpenAI-protocol stream.
///
/// A stream is charged its measured usage only when the provider has said it is
/// finished and reported usage for the finished generation:
///
/// - a non-empty `finish_reason`, with no output after it;
/// - a usage reading in or after that finish chunk;
/// - `data: [DONE]`.
///
/// Anything less charges the reservation. Transport EOF is not enough: a proxy
/// or load balancer can close cleanly mid-generation, and on an upstream that
/// reports cumulative usage on every chunk, the last reading before that close
/// is a partial count.
///
/// In live captures of Vertex's OpenAI-compatible endpoint (one run per
/// shape), usage arrived once, on the same line as `finish_reason`, then
/// `[DONE]`. OpenAI's own convention sends the usage on a separate
/// `choices: []` chunk after the finish. Both satisfy the rule. Usage is assumed
/// to be cumulative: an upstream that reported per-chunk deltas would be
/// measured low, or charged the reservation where a delta is smaller than the
/// one before.
///
/// Every reading is kept whole and never merged field by field: `input_tokens`
/// is derived (prompt minus cached), so mixing fields from different readings
/// can cost more than any reading the provider sent. The settled reading is the
/// one with the highest cost, later readings winning ties, so a disagreement
/// between readings settles on the dearer side. A fall in the total prompt or
/// in the output between readings is itself a contradiction and charges the
/// reservation, as does any untrustworthy reading.
pub struct OpenAiStreamMeter {
    line_buf: Vec<u8>,
    /// Set after a line overflowed the buffer, so the rest of that line is
    /// discarded up to its newline instead of being parsed as a line of its own.
    discarding: bool,
    /// The SAME semantics the buffered path uses for this upstream. If these
    /// ever diverge, a caller picks the cheaper metering by setting stream:true.
    semantics: CacheSemantics,
    price: ModelPrice,
    finish_seen: bool,
    /// A usage reading arrived since the current finish was set. Cleared with
    /// the finish, so a finish followed by more output and a second finish
    /// without usage never reuses the first finish's reading.
    reading_since_finish: bool,
    done_seen: bool,
    best: Option<(Usage, i64)>,
    last: Option<Usage>,
    untrustworthy: bool,
}

impl OpenAiStreamMeter {
    #[must_use]
    pub fn new(semantics: CacheSemantics, price: ModelPrice) -> Self {
        Self {
            line_buf: Vec::new(),
            discarding: false,
            semantics,
            price,
            finish_seen: false,
            reading_since_finish: false,
            done_seen: false,
            best: None,
            last: None,
            untrustworthy: false,
        }
    }

    fn observe(&mut self, chunk: &[u8]) {
        if self.done_seen {
            // Nothing after the end marker counts.
            return;
        }
        self.line_buf.extend_from_slice(chunk);
        while let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.line_buf.drain(..=pos).collect();
            if std::mem::take(&mut self.discarding) {
                continue;
            }
            self.observe_line_bytes(&line);
            if self.done_seen {
                self.line_buf.clear();
                return;
            }
        }
        if self.line_buf.len() > MAX_SSE_LINE_BUFFER {
            self.line_buf.clear();
            self.discarding = true;
        }
    }

    /// Parse a final line the upstream closed without terminating.
    fn flush(&mut self) {
        let rest = std::mem::take(&mut self.line_buf);
        if !self.done_seen && !std::mem::take(&mut self.discarding) && !rest.is_empty() {
            self.observe_line_bytes(&rest);
        }
    }

    fn observe_line_bytes(&mut self, line: &[u8]) {
        if let Ok(s) = std::str::from_utf8(line) {
            self.observe_line(s.trim_end());
        }
    }

    fn observe_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data == "[DONE]" {
            self.done_seen = true;
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return;
        };
        // Order matters within a line. Vertex sends its last text, the finish
        // and the usage on ONE line, so: clear on output, then set the finish,
        // then record the usage against it.
        if let Some(choices) = v.get("choices").and_then(Value::as_array) {
            if choices.iter().any(choice_carries_output) {
                self.finish_seen = false;
                self.reading_since_finish = false;
            }
            if choices.iter().any(|c| {
                c.get("finish_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|r| !r.is_empty())
            }) {
                self.finish_seen = true;
            }
        }
        if let Some(u) = crate::providers::usage_from_sse_value(&v, self.semantics) {
            self.record(u);
        }
    }

    fn record(&mut self, u: Usage) {
        if u.is_untrustworthy() {
            self.untrustworthy = true;
        }
        if let Some(prev) = self.last {
            if u.total_prompt_tokens() < prev.total_prompt_tokens()
                || u.output_tokens < prev.output_tokens
            {
                tracing::warn!(
                    prev_prompt = prev.total_prompt_tokens(),
                    prompt = u.total_prompt_tokens(),
                    prev_output = prev.output_tokens,
                    output = u.output_tokens,
                    "stream usage DECREASED between readings; usage is self-contradictory"
                );
                self.untrustworthy = true;
            }
        }
        self.last = Some(u);
        let cost = self.price.cost_micros(u);
        if self.best.is_none_or(|(_, c)| cost >= c) {
            self.best = Some((u, cost));
        }
        if self.finish_seen {
            self.reading_since_finish = true;
        }
    }

    fn settlement(&self, reserve_micros: i64) -> (i64, Usage, &'static str) {
        let Some((usage, cost)) = self.best else {
            return (reserve_micros, Usage::default(), "estimated");
        };
        // An implausible or contradictory count is a metering FAILURE, not an
        // expensive stream. Costing it can saturate to i64::MAX, which the
        // budget counter has no path back down from, and the ledger row it
        // writes overflows SUM(cost_micros) on every later startup reconcile.
        if self.untrustworthy {
            tracing::error!(
                input_tokens = usage.input_tokens,
                output_tokens = usage.output_tokens,
                cache_read_tokens = usage.cache_read_tokens,
                cache_write_tokens = usage.cache_write_tokens,
                "stream usage is implausible or self-contradictory; charging the reservation"
            );
            return (reserve_micros, Usage::default(), "estimated");
        }
        if self.finish_seen && self.reading_since_finish && self.done_seen {
            warn_if_unpriced_long_context(&self.price, usage);
            return (cost.max(1), usage, "allowed");
        }
        // Charged the reservation, so `estimated`; the tokens seen still go to
        // the ledger so the row shows what was observed.
        (reserve_micros, usage, "estimated")
    }
}

/// Whether a choice carries generated output, so a finish seen before it was
/// not final. A deny-list on purpose: missing a continuation is the
/// under-charge direction, so every field except the bookkeeping ones counts,
/// unless it is empty. On `delta`, every key except `role` counts.
fn choice_carries_output(choice: &Value) -> bool {
    fn has_value(v: &Value) -> bool {
        match v {
            Value::Null => false,
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
            Value::Bool(_) | Value::Number(_) => true,
        }
    }
    let Some(obj) = choice.as_object() else {
        return false;
    };
    obj.iter().any(|(k, v)| match k.as_str() {
        "index" | "finish_reason" | "logprobs" => false,
        "delta" => v
            .as_object()
            .is_some_and(|d| d.iter().any(|(k, v)| k != "role" && has_value(v))),
        _ => has_value(v),
    })
}

/// Same visibility as the buffered path. Streams are the bulk of real traffic,
/// so warning only in `evaluate` would leave the gap invisible for most
/// requests, which defeats the point of a default that reports the gap rather
/// than guessing at it.
fn warn_if_unpriced_long_context(price: &ModelPrice, usage: Usage) {
    if price
        .long_context
        .is_unpriced_long_context(usage.threshold_prompt_tokens())
    {
        tracing::warn!(
            provider = %price.provider,
            model = %price.model,
            prompt_tokens = usage.threshold_prompt_tokens(),
            classes_overlap = usage.classes_overlap,
            "streamed prompt exceeds the long-context threshold and no tier is \
             configured for this model; if it re-rates long requests, this is an \
             UNDER-charge. Set it with `admin price set --long-context-threshold \
             --long-context-input-permille --long-context-output-permille`"
        );
    }
}

/// Pick the settlement for a finished relay.
///
/// Extracted as a pure function so the discount path is testable. It previously
/// lived inline in `relay_stream`, which meant the branch that BYPASSES
/// `stream_settlement` had no coverage at all: the regression test for it
/// exercised `stream_settlement` and would have passed against the bug.
fn choose_settlement(
    meter: &StreamMeter,
    price: &ModelPrice,
    reserve_micros: i64,
    prompt_floor_micros: i64,
) -> (i64, Usage, &'static str) {
    match meter {
        StreamMeter::OpenAi(m) => m.settlement(reserve_micros),
        StreamMeter::Anthropic { meter: m, .. } => {
            let clean = m.is_clean();
            // Anthropic delivers `overloaded_error` as an event on an
            // otherwise-200 stream, before message_start, so nothing was
            // generated and nothing billed. Charging the full worst-case
            // reservation there is punitive, and SDK retries multiply it
            // through an overload window, so charge the prompt leg.
            //
            // Three conditions, all load-bearing:
            //
            // - Anthropic only. The OpenAI meter has no start signal, and a
            //   stream cut before its finish carries either no usage or only a
            //   partial count, so the gateway cannot tell how much the provider
            //   generated and will bill.
            // - Nothing started.
            // - The provider EXPLICITLY errored. Without this, any 200 that is
            //   not an SSE stream at all (an upstream ignoring `stream`, a proxy
            //   rewriting the response) is relayed to the client in full and
            //   then discounted, because it contains neither message_start nor
            //   message_stop.
            if !clean && !m.started() && m.errored() {
                tracing::warn!(
                    "anthropic stream reported an error before generating anything; \
                     charging the prompt leg"
                );
                return (
                    prompt_floor_micros.min(reserve_micros).max(1),
                    Usage::default(),
                    "estimated",
                );
            }
            stream_settlement(clean, m.usage(), price, reserve_micros)
        }
    }
}

/// Settle an Anthropic stream. A clean finish with metered usage charges the
/// observed cost (floored to 1 micro). A clean finish with no usage, or ANY
/// abnormal end (disconnect, upstream error, idle/duration timeout), charges the
/// FULL reservation so a stream is never under-charged.
fn stream_settlement(
    clean: bool,
    seen: Option<Usage>,
    price: &ModelPrice,
    reserve_micros: i64,
) -> (i64, Usage, &'static str) {
    match (clean, seen) {
        // An implausible count is a metering FAILURE, not an expensive stream.
        // Costing it saturates to i64::MAX, which the budget counter has no path
        // back down from, and the ledger row it writes overflows SUM(cost_micros)
        // on every later startup reconcile. The buffered path already guards
        // this; the stream's usage events are the same untrusted input, and they reach
        // this function through a client-selected code path, so a caller could
        // otherwise pick the unguarded one by setting stream:true.
        (_, Some(usage)) if usage.is_untrustworthy() => {
            tracing::error!(
                input_tokens = usage.input_tokens,
                output_tokens = usage.output_tokens,
                cache_read_tokens = usage.cache_read_tokens,
                cache_write_tokens = usage.cache_write_tokens,
                suspect = usage.suspect,
                "stream usage is implausible or self-contradictory; charging the reservation"
            );
            (reserve_micros, Usage::default(), "estimated")
        }
        // Measured: the only branch that costs what the provider reported.
        (true, Some(usage)) => {
            warn_if_unpriced_long_context(price, usage);
            (price.cost_micros(usage).max(1), usage, "allowed")
        }
        // The remaining branches all charge the RESERVATION, so they are
        // `estimated`, not `allowed` or `error`. Streams are the bulk of real
        // traffic, and mislabelling them here would make the console's
        // measured-versus-estimated split wrong for most of a period, which is
        // exactly the figure an operator reconciles against a provider invoice.
        (true, None) => (reserve_micros, Usage::default(), "estimated"),
        (false, usage) => (reserve_micros, usage.unwrap_or_default(), "estimated"),
    }
}

fn openai_error(status: StatusCode, reason: &'static str, message: &str) -> Response {
    (
        status,
        [("x-tollgate-reason", reason)],
        Json(json!({"error": {"message": message, "type": reason}})),
    )
        .into_response()
}

/// Map a gateway [`Outcome`] to an OpenAI-shaped HTTP response. On success the
/// upstream body is returned verbatim; refusals use OpenAI's `{"error": {...}}`
/// envelope with an `x-tollgate-reason` header.
pub(crate) fn openai_outcome_response(outcome: crate::gateway::Outcome) -> Response {
    use crate::gateway::Outcome;
    match outcome {
        Outcome::Allowed {
            status,
            body,
            cost_micros,
            overhead_micros,
            ..
        } => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            [
                ("x-tollgate-cost", format_micros(cost_micros)),
                ("x-tollgate-overhead-us", overhead_micros.to_string()),
            ],
            Json(body),
        )
            .into_response(),
        Outcome::Unauthenticated => openai_error(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "invalid or missing API key",
        ),
        Outcome::BadRequest(m) => openai_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            &format!("invalid request: {m}"),
        ),
        Outcome::Unpriced { provider, model } => openai_error(
            StatusCode::BAD_REQUEST,
            "unpriced",
            &format!("no price configured for {provider}/{model}"),
        ),
        Outcome::BudgetDenied(d) => openai_error(
            StatusCode::PAYMENT_REQUIRED,
            "budget_exceeded",
            &format!("budget exceeded: {d}"),
        ),
        Outcome::BackendError(_) => openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend_error",
            "gateway temporarily unavailable",
        ),
        Outcome::Upstream(m) => {
            tracing::warn!(detail = %m, "openai upstream error");
            openai_error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "upstream provider error",
            )
        }
    }
}

/// Serve the read-only web console. Production injects no key (unlike the demo);
/// the viewer supplies their own, which the page sends to the endpoints below.
async fn console() -> impl IntoResponse {
    Html(crate::console::render(""))
}

fn budget_label(b: &Budget) -> String {
    match &b.scope {
        Scope::Global => "Global".to_owned(),
        Scope::ApiKey(id) => format!("Per-key {}", id.get(..8).unwrap_or(id.as_str())),
        Scope::Provider(p) => format!("Provider: {p}"),
        Scope::Model(pm) => format!("Model: {pm}"),
    }
}

/// The console's 401. Every console refusal carries `x-tollgate-reason`, like
/// every other route's.
pub(crate) fn console_unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [("x-tollgate-reason", "unauthenticated")],
        Json(json!({"error": "the console requires a valid API key"})),
    )
        .into_response()
}

/// The body of the console's 503 when the gateway cannot answer: the key store,
/// or the request timeout. Shared with the gate, which puts it on the request
/// timeout's otherwise empty 503.
pub(crate) const CONSOLE_UNAVAILABLE_BODY: &str = r#"{"error":"gateway temporarily unavailable"}"#;

/// The console's 503 when the gateway cannot answer.
pub(crate) fn console_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            ("x-tollgate-reason", "backend_error"),
            ("content-type", "application/json"),
        ],
        CONSOLE_UNAVAILABLE_BODY,
    )
        .into_response()
}

/// A console ledger read failed. Kept apart from [`console_unavailable`] so the
/// body still says which read.
fn console_read_failed(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [("x-tollgate-reason", "backend_error")],
        Json(json!({ "error": message })),
    )
        .into_response()
}

/// Read-only budgets view: current-period spend and limit per configured budget,
/// summed from the durable ledger. Authenticated by any valid key, in the gate:
/// Tollgate is single-tenant per deployment, so any key in the deployment may
/// observe its budgets and usage. Per-tenant scoping is an Enterprise-edition
/// concern.
async fn console_budgets(
    State(state): State<AppState>,
    Extension(_authed): Extension<Authed>,
) -> Response {
    let budgets = state.budgets.load_full();
    let now = Utc::now();
    let mut out = Vec::with_capacity(budgets.len());
    for b in budgets.iter() {
        let spent = match budget_spent(&state.db, b, now).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "console: budget spend query failed");
                return console_read_failed("budget read failed");
            }
        };
        out.push(json!({
            "label": budget_label(b),
            "period": b.period.as_str(),
            "spent": format_micros(spent),
            "limit": format_micros(b.limit_micros),
            "remaining": format_micros(b.limit_micros - spent),
            "hard_stop": b.hard_stop,
        }));
    }
    Json(json!({ "budgets": out })).into_response()
}

#[derive(sqlx::FromRow)]
struct UsageRow {
    provider: String,
    model: String,
    /// FRESH prompt tokens only. Cached tokens are carried separately, so this
    /// is not the whole prompt: see `total_prompt_tokens` in the JSON payload.
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_micros: i64,
    gateway_micros: i64,
    decision: String,
}

/// Read-only usage view: the 100 most recent ledger rows, oldest-first so the
/// console can number them chronologically. Authenticated by any valid key (see
/// `console_budgets`). Cost is a plain decimal string, never raw micros.
async fn console_usage(
    State(state): State<AppState>,
    Extension(_authed): Extension<Authed>,
) -> Response {
    let rows = sqlx::query_as::<_, UsageRow>(
        "SELECT provider, model, input_tokens, output_tokens, cache_read_tokens, \
                cache_write_tokens, cost_micros, gateway_micros, decision \
         FROM (SELECT provider, model, input_tokens, output_tokens, cache_read_tokens, \
                      cache_write_tokens, cost_micros, gateway_micros, decision, started_at \
               FROM usage_events ORDER BY started_at DESC LIMIT 100) recent \
         ORDER BY started_at ASC",
    )
    .fetch_all(&state.db)
    .await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "console: usage query failed");
            return console_read_failed("usage read failed");
        }
    };
    let events: Vec<_> = rows
        .iter()
        .map(|r| {
            json!({
                "provider": r.provider,
                "model": r.model,
                "input_tokens": r.input_tokens,
                "output_tokens": r.output_tokens,
                "cache_read_tokens": r.cache_read_tokens,
                "cache_write_tokens": r.cache_write_tokens,
                // input_tokens counts FRESH prompt tokens only, so on a cached
                // request it can be a tiny number sitting next to a large cost.
                // This is the figure to divide cost by.
                "total_prompt_tokens": r.input_tokens
                    + r.cache_read_tokens
                    + r.cache_write_tokens,
                "cost": format_micros(r.cost_micros),
                "overhead_us": r.gateway_micros,
                "decision": r.decision,
            })
        })
        .collect();
    let total: i64 = rows.iter().map(|r| r.cost_micros).sum();
    // Split measured from assumed. Rows charged at their reservation because
    // usage could not be trusted are recorded as `estimated`, and an operator
    // reconciling against a provider invoice needs to know how much of a period
    // that represents rather than discovering it in a variance review.
    let estimated: i64 = rows
        .iter()
        .filter(|r| r.decision == "estimated")
        .map(|r| r.cost_micros)
        .sum();
    Json(json!({
        "events": events,
        "total_cost": format_micros(total),
        "measured_cost": format_micros(total - estimated),
        "estimated_cost": format_micros(estimated),
        "count": rows.len(),
    }))
    .into_response()
}

/// Prometheus exposition.
///
/// Everything here is read from process memory. A scrape deliberately does not
/// touch Postgres or Valkey: putting the observability path on the same
/// dependencies as the money path means an outage takes away the instrument you
/// would use to see it. The consequence is that budget SPEND is not exported,
/// because the authoritative figures live in those two stores. `/console/usage`
/// and `/console/budgets` serve that, and they are honest about querying.
///
/// Counters are process-lifetime and reset on restart, which is what Prometheus
/// expects. `tollgate_budget_limit_micros` is a gauge read from the live config,
/// so it follows a hot-reloaded budget change without a restart.
async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    use std::fmt::Write as _;

    let core = state.core.load_full();
    let m = &core.metrics;
    let load = |c: &std::sync::atomic::AtomicU64| c.load(std::sync::atomic::Ordering::Relaxed);

    let mut body = String::with_capacity(1024);
    body.push_str(
        "# HELP tollgate_up 1 if the gateway is serving.\n\
         # TYPE tollgate_up gauge\n\
         tollgate_up 1\n\
         # HELP tollgate_requests_total Terminal request decisions since start.\n\
         # TYPE tollgate_requests_total counter\n",
    );
    for (decision, value) in [
        ("allowed", load(&m.allowed)),
        ("estimated", load(&m.estimated)),
        ("rejected_budget", load(&m.rejected_budget)),
        ("unpriced", load(&m.unpriced)),
        ("error", load(&m.errors)),
        ("unauthenticated", load(&m.unauthenticated)),
        ("key_store_error", load(&m.key_store_error)),
    ] {
        let _ = writeln!(
            body,
            "tollgate_requests_total{{decision=\"{decision}\"}} {value}"
        );
    }
    let _ = write!(
        body,
        "# HELP tollgate_unmatchable_budgets Configured budgets that cannot match some or all of \
         their requests and that a budget change could fix (see the startup and reload \
         warnings for which and why).\n\
         # TYPE tollgate_unmatchable_budgets gauge\n\
         tollgate_unmatchable_budgets {}\n",
        load(&m.unmatchable_budgets),
    );
    let _ = write!(
        body,
        "# HELP tollgate_cost_micros_total Settled cost since start, in currency micros.\n\
         # TYPE tollgate_cost_micros_total counter\n\
         tollgate_cost_micros_total {}\n\
         # HELP tollgate_budget_limit_micros Configured limit per budget.\n\
         # TYPE tollgate_budget_limit_micros gauge\n",
        load(&m.cost_micros),
    );
    for b in state.budgets.load_full().iter() {
        // Scope and period as labels, so a dashboard can show one budget's
        // ceiling next to the spend the console reports for it.
        let _ = writeln!(
            body,
            "tollgate_budget_limit_micros{{scope=\"{}\",period=\"{}\",hard_stop=\"{}\"}} {}",
            b.scope,
            b.period.as_str(),
            b.hard_stop,
            b.limit_micros,
        );
    }

    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

async fn shutdown_signal(grace: Duration) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("ctrl-c received, shutting down"),
        _ = terminate => tracing::info!("SIGTERM received, shutting down"),
    }

    // Allow in-flight requests a window to drain.
    tokio::time::sleep(grace).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_target_accepts_real_locations_project_ids_and_numbers() {
        for (project, location) in [
            ("my-project-123", "us-central1"),
            ("my-project", "global"),
            ("my-project", "europe-west4"),
            ("my-project", "eu"),
            ("123456789012", "us-central1"),
            ("example.com:my-project", "asia-southeast1"),
        ] {
            assert!(
                validate_vertex_target(project, location).is_ok(),
                "{project:?} {location:?}"
            );
        }
    }

    /// The location becomes part of the hostname and both values part of the
    /// path of a request carrying Tollgate's Google token, so anything that
    /// could move it elsewhere is refused.
    #[test]
    fn vertex_target_refuses_values_that_could_redirect_the_request() {
        for location in [
            "",
            "US-CENTRAL1",
            "us-central1.evil.example",
            "evil.example/x",
            "x@evil.example",
            "us-central1?",
            "us-central1#",
            "-us",
            "us-",
        ] {
            assert!(
                validate_vertex_target("my-project", location).is_err(),
                "location {location:?}"
            );
        }
        for project in [
            "",
            "p/x",
            "p?x",
            "p#x",
            "p@x",
            "1project",
            "12/34",
            "My-Project",
        ] {
            assert!(
                validate_vertex_target(project, "us-central1").is_err(),
                "project {project:?}"
            );
        }
    }

    /// `serve` must report a config mistake before it touches Postgres. With
    /// `TOLLGATE_DATABASE__AUTO_MIGRATE` on, a check that runs after the pool is
    /// built would let a misconfigured process migrate the schema and then exit.
    ///
    /// The database URL points at a port nothing listens on, so if any of these
    /// checks moved below `build_pool`, the error would be the connection
    /// failure instead. The control case proves that: with valid config, the
    /// connection failure is exactly what comes back.
    #[tokio::test]
    async fn config_errors_are_reported_before_postgres_is_touched() {
        fn base() -> Config {
            let mut cfg = Config::default();
            cfg.security.api_key_pepper = "test-pepper-not-the-dev-placeholder".to_owned();
            cfg.database.url = "postgres://tollgate:tollgate@127.0.0.1:1/tollgate".to_owned();
            cfg.database.acquire_timeout = Duration::from_secs(2);
            // The configuration this ordering exists to protect.
            cfg.database.auto_migrate = true;
            cfg
        }
        async fn boot_error(cfg: Config) -> String {
            let err = serve(cfg)
                .await
                .expect_err("serve must not start without a database");
            format!("{err:#}")
        }

        let msg = boot_error(base()).await;
        assert!(msg.contains("connecting to Postgres"), "control: {msg}");

        type Case = (&'static str, &'static str, fn(&mut Config));
        let cases: [Case; 18] = [
            ("short pepper", "at least 16 bytes", |c| {
                c.security.api_key_pepper = "short".to_owned();
            }),
            ("placeholder pepper", ".env.example placeholder", |c| {
                c.security.api_key_pepper = crate::config::DEV_PLACEHOLDER_PEPPER.to_owned();
            }),
            (
                "cache fallback below 1x",
                "cache_read_fallback_permille is 999",
                |c| {
                    c.billing.cache_read_fallback_permille = 999;
                },
            ),
            (
                "long-context multiple below 1x",
                "long_context_multiple_permille is 999",
                |c| {
                    c.billing.long_context_multiple_permille = 999;
                },
            ),
            (
                "stranded long-context uplift",
                "__LONG_CONTEXT_THRESHOLD_TOKENS is 0",
                |c| {
                    c.billing.long_context_threshold_tokens = 0;
                    c.billing.long_context_multiple_permille = 2_000;
                },
            ),
            (
                "unknown openai upstream",
                "UPSTREAM must be 'vertex' or 'custom'",
                |c| {
                    c.providers.openai.enabled = true;
                    c.providers.openai.upstream = "bogus".to_owned();
                },
            ),
            (
                "vertex upstream without project",
                "requires TOLLGATE_PROVIDERS__VERTEX__PROJECT",
                |c| {
                    c.providers.openai.enabled = true;
                    c.providers.openai.upstream = "vertex".to_owned();
                    c.providers.vertex.project = String::new();
                },
            ),
            (
                "native vertex without project",
                "requires TOLLGATE_PROVIDERS__VERTEX__PROJECT",
                |c| {
                    c.providers.vertex.enabled = true;
                    c.providers.vertex.project = String::new();
                },
            ),
            (
                "vertex location that would change the host",
                "TOLLGATE_PROVIDERS__VERTEX__LOCATION must be",
                |c| {
                    c.providers.vertex.enabled = true;
                    c.providers.vertex.project = "my-project".to_owned();
                    c.providers.vertex.location = "evil.example/x#".to_owned();
                },
            ),
            (
                "vertex project with a path separator",
                "TOLLGATE_PROVIDERS__VERTEX__PROJECT must be",
                |c| {
                    c.providers.vertex.enabled = true;
                    c.providers.vertex.project = "p/../x".to_owned();
                },
            ),
            (
                "custom upstream without key",
                "requires TOLLGATE_PROVIDERS__OPENAI__BASE_URL",
                |c| {
                    c.providers.openai.enabled = true;
                    c.providers.openai.upstream = "custom".to_owned();
                    c.providers.openai.base_url = "https://upstream.example".to_owned();
                    c.providers.openai.api_key = String::new();
                },
            ),
            (
                "custom upstream at anthropic.com",
                "refusing to use api.anthropic.com",
                |c| {
                    c.providers.openai.enabled = true;
                    c.providers.openai.upstream = "custom".to_owned();
                    c.providers.openai.base_url = "https://api.anthropic.com/v1".to_owned();
                    c.providers.openai.api_key = "k".to_owned();
                },
            ),
            (
                "unknown admission mode",
                "ADMISSION must be 'fast' or 'exact'",
                |c| {
                    c.providers.admission = "slow".to_owned();
                },
            ),
            ("non-redis URL scheme", "parsing Redis URL", |c| {
                c.redis.url = "http://127.0.0.1:6379".to_owned();
            }),
            (
                "too few connections to split",
                "MAX_CONNECTIONS must be at least 4",
                |c| {
                    c.database.max_connections = 3;
                },
            ),
            (
                "zero acquire timeout",
                "ACQUIRE_TIMEOUT must be between 1ms and 2147483647ms",
                |c| {
                    c.database.acquire_timeout = Duration::ZERO;
                },
            ),
            (
                "sub-millisecond acquire timeout",
                "ACQUIRE_TIMEOUT must be between 1ms and 2147483647ms",
                |c| {
                    c.database.acquire_timeout = Duration::from_micros(500);
                },
            ),
            (
                "acquire timeout beyond statement_timeout's range",
                "ACQUIRE_TIMEOUT must be between 1ms and 2147483647ms",
                |c| {
                    c.database.acquire_timeout = Duration::from_millis(2_147_483_648);
                },
            ),
        ];

        for (label, want, break_it) in cases {
            let mut cfg = base();
            break_it(&mut cfg);
            let msg = boot_error(cfg).await;
            assert!(msg.contains(want), "{label}: expected {want:?}, got {msg}");
            assert!(
                !msg.contains("connecting to Postgres"),
                "{label}: reached Postgres before the config check: {msg}"
            );
        }
    }

    /// `x-tollgate-reason` is what an operator alerts on, so it must mean the
    /// same thing on every route. It did not: the Anthropic path emitted the
    /// SDK's error kind, so a budget refusal on `/v1/messages` reported
    /// `permission_error` while the same refusal on `/v1/chat/completions`
    /// reported `budget_exceeded`. An alert keyed on the latter silently missed
    /// every Claude refusal.
    ///
    /// The body keeps Anthropic's vocabulary, because the SDK maps it to an
    /// exception class. Only the header is shared.
    #[tokio::test]
    async fn the_refusal_reason_header_is_the_same_word_on_every_route() {
        use crate::budget::BudgetDenied;

        let denied = || {
            crate::gateway::Outcome::BudgetDenied(BudgetDenied {
                scope: "api_key:x".to_owned(),
                period: "monthly",
                limit_micros: 1_000,
                spent_micros: 1_000,
                cost_micros: 1,
            })
        };
        let unpriced = || crate::gateway::Outcome::Unpriced {
            provider: "anthropic".to_owned(),
            model: "m".to_owned(),
        };
        let reason = |r: &Response| {
            r.headers()
                .get("x-tollgate-reason")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<missing>")
                .to_owned()
        };

        // Every refusal, on every one of the three response shapes: the native
        // Anthropic envelope, the OpenAI envelope, and the legacy enveloped
        // route. A route that sets the header on some refusals and not others is
        // worse than one that never sets it, because the alert built on it looks
        // like it covers the deployment.
        /// One refusal: a label, the reason every route must report for it, and
        /// a way to make a fresh `Outcome` (the response mappers consume it, so
        /// each of the three routes needs its own).
        type Case = (&'static str, &'static str, fn() -> crate::gateway::Outcome);

        let cases: Vec<Case> = vec![
            ("unauthenticated", "unauthenticated", || {
                crate::gateway::Outcome::Unauthenticated
            }),
            ("bad request", "bad_request", || {
                crate::gateway::Outcome::BadRequest("nope".to_owned())
            }),
            ("unpriced", "unpriced", unpriced),
            ("budget", "budget_exceeded", denied),
            ("backend", "backend_error", || {
                crate::gateway::Outcome::BackendError("valkey down".to_owned())
            }),
            ("upstream", "upstream_error", || {
                crate::gateway::Outcome::Upstream("502 from provider".to_owned())
            }),
        ];

        for (label, want, make) in cases {
            for (route, got) in [
                ("/v1/messages", reason(&anthropic_outcome_response(make()))),
                (
                    "/v1/chat/completions",
                    reason(&openai_outcome_response(make())),
                ),
                (
                    "/v1/{provider}/{path}",
                    reason(&crate::gateway::outcome_response(make())),
                ),
            ] {
                assert_eq!(got, want, "{label} refusal on {route}");
            }
        }

        // The BODY must not follow the header. An SDK resolves its exception
        // class from `error.type`, so unifying that too would be a breaking
        // change dressed up as consistency.
        let body = axum::body::to_bytes(anthropic_outcome_response(denied()).into_body(), 65_536)
            .await
            .expect("body reads");
        let v: Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(
            v["error"]["type"], "permission_error",
            "the Anthropic body must keep Anthropic's vocabulary"
        );
    }

    #[test]
    fn stream_settlement_never_undercharges() {
        // $1 per million tokens on both legs => 1 micro/token.
        let price = ModelPrice::new("anthropic", "m", 1_000_000, 1_000_000);
        let reserve = 999;

        // Clean finish with usage: charge the observed cost (15 tokens => 15 micros).
        let (actual, _u, decision) =
            stream_settlement(true, Some(Usage::new(10, 5)), &price, reserve);
        assert_eq!(actual, 15);
        assert_eq!(decision, "allowed");

        // Clean finish, no usage: charge the full reservation, not zero, and
        // label it estimated because nothing was measured.
        let (actual, _u, decision) = stream_settlement(true, None, &price, reserve);
        assert_eq!(actual, reserve);
        assert_eq!(decision, "estimated");

        // Abnormal end (even with partial usage seen): charge the full reservation.
        let (actual, _u, decision) =
            stream_settlement(false, Some(Usage::new(10, 5)), &price, reserve);
        assert_eq!(actual, reserve);
        assert_eq!(decision, "estimated");
    }

    /// Drive the relay's settlement CHOICE, not just `stream_settlement`.
    ///
    /// The bypass bug lived in the arm that skips `stream_settlement` entirely,
    /// so a test calling `stream_settlement` directly passed against it. These
    /// go through `choose_settlement` with a real meter.
    fn settle_via_meter(meter: &StreamMeter) -> (i64, &'static str) {
        let price = ModelPrice::new("anthropic", "m", 1_000_000, 1_000_000);
        let (actual, _u, decision) = choose_settlement(meter, &price, 999, 100);
        (actual, decision)
    }

    fn anthropic_meter(lines: &[&str]) -> StreamMeter {
        let mut m = crate::providers::AnthropicStreamMeter::new();
        for l in lines {
            m.observe_line(l);
        }
        StreamMeter::Anthropic {
            line_buf: Vec::new(),
            meter: m,
        }
    }

    #[test]
    fn only_an_explicit_provider_error_earns_the_prompt_leg_discount() {
        // The intended case: the provider said it failed before generating.
        let errored =
            anthropic_meter(&[r#"data: {"type":"error","error":{"type":"overloaded_error"}}"#]);
        assert_eq!(settle_via_meter(&errored), (100, "estimated"));

        // A 200 that is not an SSE stream at all: an upstream that ignored
        // `stream`, or a proxy that rewrote the response. It has no
        // message_start and no message_stop, exactly like the errored case, but
        // the client was handed the whole body. Discounting it would give away
        // the output leg on every request.
        let not_a_stream =
            anthropic_meter(&[r#"data: {"id":"msg_x","content":[{"text":"hello"}]}"#]);
        assert_eq!(settle_via_meter(&not_a_stream), (999, "estimated"));

        // Nothing at all: connection died before a single complete line.
        let silent = anthropic_meter(&[]);
        assert_eq!(settle_via_meter(&silent), (999, "estimated"));

        // Generation started, then failed: the provider did work, charge in full.
        let started_then_died = anthropic_meter(&[
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":50,"output_tokens":1}}}"#,
        ]);
        assert_eq!(settle_via_meter(&started_then_died), (999, "estimated"));
    }

    #[test]
    fn the_prompt_leg_discount_never_applies_to_openai() {
        // The OpenAI meter has no start signal, and a stream cut before its
        // finish carries either no usage or only a partial count, so the
        // gateway cannot tell how much the provider generated and will bill.
        // Applying the discount there handed out a whole generation for a
        // token of budget.
        let openai = StreamMeter::OpenAi(openai_meter());
        assert_eq!(settle_via_meter(&openai), (999, "estimated"));
    }

    #[test]
    fn stream_settlement_rejects_an_implausible_terminal_chunk() {
        // The buffered path guards this in gateway::evaluate. The streaming path
        // is selected by the CLIENT (stream:true), so leaving it unguarded would
        // let a caller pick the code path where a hostile terminal chunk
        // saturates the cost to i64::MAX and pins a budget counter that nothing
        // can lower. Charge the reservation and record an error instead.
        let price = ModelPrice::new("anthropic", "m", 1_000_000, 1_000_000);
        let reserve = 999;
        let absurd = Usage::new(1, u64::MAX);
        assert!(absurd.is_implausible());

        let (actual, usage, decision) = stream_settlement(true, Some(absurd), &price, reserve);
        assert_eq!(actual, reserve);
        assert_eq!(decision, "estimated");
        // The ledger must not carry the absurd counts either: a row holding
        // i64::MAX makes SUM(cost_micros) overflow on later reconciliation.
        assert_eq!(usage, Usage::default());

        // A clean stream just under the ceiling still settles normally.
        let ok = Usage::new(1, crate::pricing::MAX_PLAUSIBLE_TOKENS_PER_LEG);
        assert!(!ok.is_implausible());
        let (_actual, _u, decision) = stream_settlement(true, Some(ok), &price, reserve);
        assert_eq!(decision, "allowed");
    }

    // ---- OpenAI-protocol stream metering ----------------------------------
    //
    // Line shapes follow live captures of Vertex's OpenAI-compatible endpoint
    // (one run per shape): usage arrived once, on the same line as
    // `finish_reason`, then `[DONE]`. OpenAI's own convention (usage on a separate `choices: []`
    // chunk) is covered too.

    const RESERVE: i64 = 999;
    const CONTENT: &str = r#"data: {"choices":[{"delta":{"content":"hi","role":"assistant"},"finish_reason":null,"index":0,"logprobs":null}],"usage":null}"#;
    const DONE: &str = "data: [DONE]";

    /// $1 per million on both legs, so cost in micros equals tokens.
    fn openai_meter() -> OpenAiStreamMeter {
        OpenAiStreamMeter::new(
            CacheSemantics::Inclusive,
            ModelPrice::new("openai", "m", 1_000_000, 1_000_000),
        )
    }

    /// Vertex's finish line: empty content, a thought signature, the finish,
    /// and the usage, all on one line.
    fn finish_with_usage(prompt: u64, completion: u64, total: u64) -> String {
        format!(
            r#"data: {{"choices":[{{"delta":{{"content":"","extra_content":{{"google":{{"thought_signature":"AY89"}}}},"role":"assistant"}},"finish_reason":"stop","index":0,"logprobs":null}}],"usage":{{"completion_tokens":{completion},"prompt_tokens":{prompt},"total_tokens":{total}}}}}"#
        )
    }

    fn usage_only(prompt: u64, completion: u64) -> String {
        format!(
            r#"data: {{"choices":[],"usage":{{"prompt_tokens":{prompt},"completion_tokens":{completion}}}}}"#
        )
    }

    const FINISH_NO_USAGE: &str = r#"data: {"choices":[{"delta":{"content":""},"finish_reason":"stop","index":0}],"usage":null}"#;

    fn feed(m: &mut OpenAiStreamMeter, lines: &[&str]) {
        for l in lines {
            m.observe(format!("{l}\n").as_bytes());
        }
    }

    fn settle(lines: &[&str]) -> (i64, Usage, &'static str) {
        let mut m = openai_meter();
        feed(&mut m, lines);
        m.settlement(RESERVE)
    }

    #[test]
    fn vertex_stream_with_finish_usage_and_done_is_measured() {
        let fin = finish_with_usage(9, 509, 518);
        let (actual, usage, decision) = settle(&[CONTENT, CONTENT, &fin, DONE]);
        assert_eq!((actual, decision), (518, "allowed"));
        assert_eq!((usage.input_tokens, usage.output_tokens), (9, 509));
    }

    #[test]
    fn vertex_thinking_tokens_are_billed_as_output() {
        // Live capture, reasoning_effort=high: completion_tokens excludes the
        // 1000 thinking tokens, total_tokens includes them.
        let fin = r#"data: {"choices":[{"delta":{"content":""},"finish_reason":"stop","index":0}],"usage":{"completion_tokens":272,"completion_tokens_details":{"reasoning_tokens":1000},"prompt_tokens":13,"total_tokens":1285}}"#;
        let (actual, usage, decision) = settle(&[CONTENT, fin, DONE]);
        assert_eq!((actual, decision), (1285, "allowed"));
        assert_eq!(usage.output_tokens, 1272);
    }

    #[test]
    fn openai_convention_usage_after_finish_is_measured() {
        let u = usage_only(10, 5);
        assert_eq!(
            settle(&[CONTENT, FINISH_NO_USAGE, &u, DONE]).2,
            "allowed",
            "usage on a separate chunk after the finish"
        );
    }

    #[test]
    fn a_stream_without_done_charges_the_reservation() {
        // The usage arrived, but the end marker did not: an upstream-side cut
        // or a proxy close can fall between a partial reading and a final one.
        let fin = finish_with_usage(9, 509, 518);
        let (actual, usage, decision) = settle(&[CONTENT, &fin]);
        assert_eq!((actual, decision), (RESERVE, "estimated"));
        // What was observed still reaches the ledger.
        assert_eq!(usage.output_tokens, 509);
    }

    #[test]
    fn usage_seen_only_before_the_finish_is_not_final() {
        // Cumulative usage on every chunk, then a finish with none.
        let early = r#"data: {"choices":[{"delta":{"content":"hi"},"finish_reason":null,"index":0}],"usage":{"prompt_tokens":9,"completion_tokens":3}}"#;
        assert_eq!(
            settle(&[early, FINISH_NO_USAGE, DONE]).0,
            RESERVE,
            "a reading from before the finish may be partial"
        );
    }

    #[test]
    fn output_after_a_finish_clears_it() {
        let fin = finish_with_usage(9, 100, 109);
        assert_eq!(settle(&[CONTENT, &fin, CONTENT, DONE]).0, RESERVE);
        // A second finish without usage must not reuse the first finish's
        // partial reading.
        assert_eq!(
            settle(&[CONTENT, &fin, CONTENT, FINISH_NO_USAGE, DONE]).0,
            RESERVE
        );
    }

    #[test]
    fn the_dearest_reading_wins_and_a_falling_one_is_a_contradiction() {
        // Rising: the later, dearer reading is charged.
        let fin = finish_with_usage(9, 100, 109);
        let more = usage_only(9, 200);
        assert_eq!(settle(&[&fin, &more, DONE]), {
            let mut m = openai_meter();
            feed(&mut m, &[&fin, &more]);
            (209, m.best.unwrap().0, "allowed")
        });
        // Falling output between readings contradicts itself: reservation.
        let early = r#"data: {"choices":[{"delta":{"content":"hi"},"finish_reason":null,"index":0}],"usage":{"prompt_tokens":9,"completion_tokens":900}}"#;
        let fin_low = finish_with_usage(9, 300, 309);
        let (actual, usage, decision) = settle(&[early, &fin_low, DONE]);
        assert_eq!((actual, decision), (RESERVE, "estimated"));
        assert_eq!(usage, Usage::default());
    }

    #[test]
    fn moving_prompt_tokens_into_the_cache_is_not_a_contradiction() {
        // Inclusive semantics: the same 1000-token prompt, first with nothing
        // cached, then with 800 cached. The total prompt does not fall, so it is
        // not flagged; the dearer whole reading settles.
        let r1 = r#"data: {"choices":[{"delta":{"content":""},"finish_reason":"stop","index":0}],"usage":{"prompt_tokens":1000,"completion_tokens":10}}"#;
        let r2 = r#"data: {"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":800}}}"#;
        // Cache reads at a tenth of the input rate, so the two readings cost
        // different amounts and the EARLIER one is dearer: last-wins would
        // under-charge here.
        let price = ModelPrice::new("openai", "m", 1_000_000, 1_000_000).with_cache_rates(
            Some(100_000),
            None,
            crate::pricing::CacheRateFallback::default(),
        );
        let mut m = OpenAiStreamMeter::new(CacheSemantics::Inclusive, price.clone());
        feed(&mut m, &[r1, r2, DONE]);
        let c1 = price.cost_micros(Usage::with_cache(1000, 10, 0, 0));
        let c2 = price.cost_micros(Usage::with_cache(200, 10, 800, 0));
        assert!(c1 > c2);
        // A reserve above both readings, so the test does not depend on whether
        // settlement may exceed the reservation.
        let (actual, _u, decision) = m.settlement(1_000_000);
        assert_eq!(decision, "allowed");
        assert_eq!(actual, c1);
    }

    #[test]
    fn one_untrustworthy_reading_charges_the_reservation() {
        // Audio tokens have no rate, so that reading is suspect. A later clean
        // reading must not wash the flag out.
        let audio = r#"data: {"choices":[{"delta":{"content":""},"finish_reason":"stop","index":0}],"usage":{"prompt_tokens":10,"completion_tokens":5,"completion_tokens_details":{"audio_tokens":600}}}"#;
        let clean = usage_only(10, 5);
        assert_eq!(settle(&[audio, &clean, DONE]).0, RESERVE);
        // Implausible counts: reservation, and a zeroed ledger row.
        let absurd = finish_with_usage(1, u64::MAX / 2, u64::MAX / 2);
        let (actual, usage, _) = settle(&[&absurd, DONE]);
        assert_eq!((actual, usage), (RESERVE, Usage::default()));
    }

    #[test]
    fn an_empty_finish_reason_is_not_a_finish() {
        // Some upstreams send "" on every chunk.
        let line = r#"data: {"choices":[{"delta":{"content":"a"},"finish_reason":"","index":0}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
        assert_eq!(settle(&[line, line, DONE]).0, RESERVE);
    }

    #[test]
    fn which_deltas_count_as_output() {
        let carries = |delta: &str| {
            let v: Value = serde_json::from_str(&format!(
                r#"{{"index":0,"finish_reason":null,"logprobs":null,"delta":{delta}}}"#
            ))
            .unwrap();
            choice_carries_output(&v)
        };
        for empty in [
            r#"{"role":"assistant"}"#,
            r#"{"content":""}"#,
            r#"{"content":null}"#,
            r#"{"tool_calls":[]}"#,
            r#"{}"#,
        ] {
            assert!(!carries(empty), "{empty} must not clear a finish");
        }
        for output in [
            r#"{"content":"x"}"#,
            r#"{"tool_calls":[{"index":0,"function":{"arguments":""}}]}"#,
            r#"{"reasoning_content":"thinking"}"#,
            r#"{"extra_content":{"google":{"thought_signature":"s"}}}"#,
            r#"{"refusal":"no"}"#,
        ] {
            assert!(carries(output), "{output} must clear a finish");
        }
        // Output outside `delta` counts too.
        let v: Value = serde_json::from_str(r#"{"index":0,"text":"x"}"#).unwrap();
        assert!(choice_carries_output(&v));
    }

    #[test]
    fn line_boundaries_do_not_change_the_result() {
        let fin = finish_with_usage(9, 509, 518);
        let whole = format!("{CONTENT}\r\n{fin}\r\n{DONE}\r\n");
        // Every split point, including inside `[DONE]` and the usage, with CRLF.
        for cut in 1..whole.len() {
            let mut m = openai_meter();
            m.observe(&whole.as_bytes()[..cut]);
            m.observe(&whole.as_bytes()[cut..]);
            assert_eq!(m.settlement(RESERVE).2, "allowed", "split at {cut}");
        }
    }

    #[test]
    fn a_final_line_without_a_newline_is_read_at_eof() {
        let fin = finish_with_usage(9, 509, 518);
        let mut m = openai_meter();
        m.observe(format!("{fin}\n{DONE}").as_bytes());
        assert_eq!(m.settlement(RESERVE).2, "estimated", "not yet terminated");
        m.flush();
        assert_eq!(m.settlement(RESERVE).2, "allowed");
    }

    #[test]
    fn an_oversized_line_is_discarded_up_to_its_newline() {
        let fin = finish_with_usage(9, 509, 518);
        let mut m = openai_meter();
        m.observe(&vec![b'x'; MAX_SSE_LINE_BUFFER + 10]);
        // The tail of the oversized line must not be parsed as a line of its own,
        // even though here it is a complete, valid usage line.
        m.observe(format!("{fin}\n").as_bytes());
        assert!(m.best.is_none(), "an overflow tail was parsed");
        feed(&mut m, &[&fin, DONE]);
        assert_eq!(m.settlement(RESERVE).2, "allowed");
    }

    #[test]
    fn nothing_after_done_counts() {
        let late = usage_only(9, 5);
        assert_eq!(settle(&[CONTENT, FINISH_NO_USAGE, DONE, &late]).0, RESERVE);
    }

    // ---- The relay loop, driven by a scripted upstream ----------------------

    fn upstream(
        lines: &[&str],
        hold_open: bool,
    ) -> futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>> {
        let items: Vec<Result<Bytes, std::io::Error>> = lines
            .iter()
            .map(|l| {
                if *l == "ERR" {
                    Err(std::io::Error::other("upstream reset"))
                } else {
                    Ok(Bytes::from(format!("{l}\n")))
                }
            })
            .collect();
        let s = futures_util::stream::iter(items);
        if hold_open {
            s.chain(futures_util::stream::pending()).boxed()
        } else {
            s.boxed()
        }
    }

    const TEST_LIMITS: RelayLimits = RelayLimits {
        idle: Duration::from_millis(100),
        max: Duration::from_secs(5),
        drain: Duration::from_millis(100),
    };

    /// Run the relay against `lines`. `client` is how the receiving side
    /// behaves. Returns the end, whether it drained, and the settlement.
    async fn relay(
        lines: &[&str],
        hold_open: bool,
        client: Client,
    ) -> (StreamEnd, bool, (i64, Usage, &'static str)) {
        let mut meter = StreamMeter::OpenAi(openai_meter());
        let (tx, rx) = mpsc::channel(match client {
            Client::Reads => 64,
            Client::Gone | Client::Stalls => 1,
        });
        let _rx = match client {
            Client::Gone => {
                drop(rx);
                None
            }
            Client::Reads | Client::Stalls => Some(rx),
        };
        let (end, drained) =
            relay_body(upstream(lines, hold_open), &tx, &mut meter, TEST_LIMITS).await;
        let price = ModelPrice::new("openai", "m", 1_000_000, 1_000_000);
        (
            end,
            drained,
            choose_settlement(&meter, &price, RESERVE, RESERVE),
        )
    }

    #[derive(Clone, Copy)]
    enum Client {
        Reads,
        Gone,
        /// Holds the receiver but never reads: the first chunk fills the
        /// channel and every later send times out.
        Stalls,
    }

    #[tokio::test]
    async fn a_client_that_stalls_at_the_finish_is_drained_and_measured() {
        // The client never reads: the first chunk fills the one-slot channel,
        // the send of the finish line times out, and the drain receives [DONE].
        let fin = finish_with_usage(9, 509, 518);
        let (end, drained, (actual, _, decision)) =
            relay(&[CONTENT, &fin, DONE], false, Client::Stalls).await;
        assert_eq!((end, drained), (StreamEnd::Done, true));
        assert_eq!((actual, decision), (518, "allowed"));
    }

    #[tokio::test]
    async fn a_client_gone_when_the_finish_is_forwarded_is_drained_and_measured() {
        // Vertex shape: finish and usage on one line. The client is already gone
        // when that line is forwarded, so the send fails after the finish.
        let fin = finish_with_usage(9, 509, 518);
        let (end, drained, (actual, _, decision)) = relay(&[&fin, DONE], false, Client::Gone).await;
        assert_eq!((end, drained), (StreamEnd::Done, true));
        assert_eq!((actual, decision), (518, "allowed"));

        // OpenAI shape: the usage is on its own chunk after the finish, and is
        // only received because the relay drains.
        let usage = usage_only(9, 509);
        let (end, drained, (actual, _, decision)) =
            relay(&[FINISH_NO_USAGE, &usage, DONE], false, Client::Gone).await;
        assert_eq!((end, drained), (StreamEnd::Done, true));
        assert_eq!((actual, decision), (518, "allowed"));
    }

    #[tokio::test]
    async fn a_failed_send_of_the_end_marker_still_measures() {
        // The finish and [DONE] arrive in one upstream chunk and the client is
        // gone: the meter has already seen [DONE] when the send fails.
        let fin = finish_with_usage(9, 509, 518);
        let both = format!("{fin}\n{DONE}");
        let (end, drained, (actual, _, decision)) = relay(&[&both], false, Client::Gone).await;
        assert_eq!((end, drained), (StreamEnd::Done, false));
        assert_eq!((actual, decision), (518, "allowed"));
    }

    #[tokio::test]
    async fn a_drain_that_never_sees_done_charges_the_reservation() {
        let fin = finish_with_usage(9, 509, 518);
        let (end, drained, (actual, _, decision)) =
            relay(&[CONTENT, &fin], true, Client::Stalls).await;
        assert_eq!((end, drained), (StreamEnd::DrainExpired, true));
        assert_eq!((actual, decision), (RESERVE, "estimated"));
    }

    #[tokio::test]
    async fn output_during_a_drain_stops_it_at_once() {
        let fin = finish_with_usage(9, 509, 518);
        let (end, _, (actual, _, _)) =
            relay(&[CONTENT, &fin, CONTENT, DONE], false, Client::Stalls).await;
        assert_eq!(end, StreamEnd::DrainAborted);
        assert_eq!(actual, RESERVE);
    }

    #[tokio::test]
    async fn a_client_gone_before_the_finish_is_not_drained() {
        let fin = finish_with_usage(9, 509, 518);
        let (end, drained, (actual, _, _)) =
            relay(&[CONTENT, &fin, DONE], false, Client::Gone).await;
        assert_eq!((end, drained), (StreamEnd::ClientGone, false));
        assert_eq!(actual, RESERVE);
    }

    #[tokio::test]
    async fn an_upstream_error_before_done_charges_the_reservation() {
        let fin = finish_with_usage(9, 509, 518);
        let (end, _, (actual, _, _)) = relay(&[CONTENT, &fin, "ERR"], false, Client::Reads).await;
        assert_eq!(end, StreamEnd::UpstreamError);
        assert_eq!(actual, RESERVE);
    }

    #[tokio::test]
    async fn the_relay_stops_at_done_even_if_the_upstream_holds_the_socket() {
        let fin = finish_with_usage(9, 509, 518);
        // `Done`, not `IdleOrMax`: the relay stopped at [DONE] rather than
        // waiting out the idle timeout on the held-open socket.
        let (end, _, (_, _, decision)) = relay(&[CONTENT, &fin, DONE], true, Client::Reads).await;
        assert_eq!(end, StreamEnd::Done);
        assert_eq!(decision, "allowed");
    }

    #[tokio::test]
    async fn a_clean_close_mid_generation_is_not_measured() {
        // Cumulative usage on every chunk, then the connection closes cleanly
        // with no finish and no [DONE]. The old rule treated EOF as completion
        // and charged the partial count.
        let early = r#"data: {"choices":[{"delta":{"content":"hi"},"finish_reason":null,"index":0}],"usage":{"prompt_tokens":9,"completion_tokens":3}}"#;
        let (end, _, (actual, _, decision)) = relay(&[early, early], false, Client::Reads).await;
        assert_eq!(end, StreamEnd::Eof);
        assert_eq!((actual, decision), (RESERVE, "estimated"));
    }

    // ---- Budgets that can never match ------------------------------------

    fn budget(scope: Scope) -> Budget {
        Budget {
            scope,
            period: crate::budget::Period::Monthly,
            limit_micros: 1_000_000,
            hard_stop: true,
        }
    }

    #[test]
    fn unmatchable_budgets_names_each_dead_or_partial_budget() {
        let prices = PriceBook::from_prices([
            ModelPrice::new("openai", "gemini-x", 1, 1),
            ModelPrice::new("openai", "google/gemini-x", 1, 1),
            ModelPrice::new("anthropic", "claude-y", 1, 1),
            ModelPrice::new("vertex", "gemini-z", 1, 1),
        ]);
        let enabled = |p: &str| matches!(p, "openai" | "anthropic" | "vertex");
        let budgets = [
            budget(Scope::Global),
            budget(Scope::ApiKey(
                "a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8".to_owned(),
            )),
            budget(Scope::ApiKey("not-a-uuid".to_owned())),
            budget(Scope::Provider("openai".to_owned())),
            budget(Scope::Provider("mock".to_owned())),
            budget(Scope::Model("anthropic:claude-y".to_owned())),
            budget(Scope::Model("anthropic:claude-typo".to_owned())),
            budget(Scope::Model("openai:gemini-x".to_owned())),
        ];
        let out = unmatchable_budgets(&budgets, &prices, enabled, false);
        let has = |budget: &str, reason: &str| {
            out.iter()
                .any(|f| f.budget.starts_with(budget) && f.reason.contains(reason))
        };
        assert!(has("api_key:not-a-uuid", "not a UUID"), "{out:?}");
        assert!(has("provider:mock", "not enabled"), "{out:?}");
        assert!(
            has("model:anthropic:claude-typo", "no current price"),
            "{out:?}"
        );
        assert!(
            has("model:openai:gemini-x", "google/gemini-x is also priced"),
            "{out:?}"
        );
        assert_eq!(
            out.len(),
            4,
            "three dead budgets and one alias note: {out:?}"
        );

        // Budgeting the alias as well, for the same period, clears the note.
        let mut both = budgets.to_vec();
        both.push(budget(Scope::Model("openai:google/gemini-x".to_owned())));
        let out = unmatchable_budgets(&both, &prices, enabled, false);
        assert!(
            !out.iter().any(|f| f.reason.contains("also priced")),
            "{out:?}"
        );

        // A soft cap on the alias does not cover a hard-capped model.
        let mut soft = budgets.to_vec();
        let mut alias = budget(Scope::Model("openai:google/gemini-x".to_owned()));
        alias.hard_stop = false;
        soft.push(alias);
        let out = unmatchable_budgets(&soft, &prices, enabled, false);
        assert!(
            out.iter().any(|f| f.reason.contains("also priced")),
            "{out:?}"
        );

        // A Vertex budget is partial when the OpenAI-compatible route reaches
        // Vertex too, and fine when it does not. No budget can clear that, so it
        // is logged but not counted on the gauge.
        let vertex = [
            budget(Scope::Provider("vertex".to_owned())),
            budget(Scope::Model("vertex:gemini-z".to_owned())),
        ];
        let notes = unmatchable_budgets(&vertex, &prices, enabled, true);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes.iter().all(|f| !f.counted), "{notes:?}");
        assert!(unmatchable_budgets(&vertex, &prices, enabled, false).is_empty());

        // A valid configuration reports nothing.
        let ok = unmatchable_budgets(
            &[
                budget(Scope::Global),
                budget(Scope::Model("anthropic:claude-y".to_owned())),
            ],
            &prices,
            enabled,
            false,
        );
        assert!(ok.is_empty(), "{ok:?}");
    }

    #[test]
    fn unmatchable_report_logs_on_change_and_counts_budgets_not_lines() {
        use std::sync::atomic::Ordering;
        let m = Metrics::default();
        let mut last = Vec::new();
        let finding = |budget: &str, reason: &str, counted: bool| BudgetFinding {
            budget: budget.to_owned(),
            reason: reason.to_owned(),
            counted,
        };
        // Two findings for one budget count as one budget, and an uncounted
        // finding does not count at all.
        let dead = vec![
            finding("model:openai:x (monthly)", "alias a", true),
            finding("model:openai:x (monthly)", "alias b", true),
            finding("provider:vertex (monthly)", "route", false),
        ];
        assert!(
            report_unmatchable(&mut last, dead.clone(), &m),
            "first report logs"
        );
        assert_eq!(m.unmatchable_budgets.load(Ordering::Relaxed), 1);
        assert!(
            !report_unmatchable(&mut last, dead, &m),
            "unchanged set stays quiet"
        );
        assert_eq!(m.unmatchable_budgets.load(Ordering::Relaxed), 1);
        assert!(
            report_unmatchable(&mut last, Vec::new(), &m),
            "the fix is reported"
        );
        assert_eq!(m.unmatchable_budgets.load(Ordering::Relaxed), 0);
    }
}

/// The authentication gate, driven through the real service stack: in-memory
/// keys, lazy Postgres and Valkey handles pointing at a closed port, and
/// providers pointing at the same. Nothing here needs a database, so it all
/// runs in CI.
#[cfg(test)]
mod gate_tests {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::http::{HeaderValue, Method, header};
    use tower::ServiceExt;
    use tower::util::BoxCloneService;

    use super::*;
    use crate::auth_gate::{Surface, store_unavailable, unauthenticated};
    use crate::budget::{Budgets, Period};
    use crate::gateway::{KeyRecord, KeyStoreError, MemKeyStore, MemUsageSink};

    type Svc = BoxCloneService<axum::extract::Request, Response, Infallible>;
    /// A response reduced to what a caller can compare: status, headers, body.
    type Observed = (StatusCode, Vec<(String, String)>, Vec<u8>);
    /// A credential to present: label, header, lookups expected on the
    /// Anthropic surface, lookups expected elsewhere.
    type Cred = (&'static str, Option<(&'static str, String)>, u64, u64);
    /// Headers to present, and whether the caller should be authenticated.
    type Precedence = (&'static str, Vec<(&'static str, String)>, bool);

    const CLOSED_PG: &str = "postgres://tollgate:tollgate@127.0.0.1:1/tollgate";
    const CLOSED_HTTP: &str = "http://127.0.0.1:1";

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum StoreMode {
        Answer,
        Fail,
        Hang,
    }

    /// A key store that counts lookups and can fail or hang on demand.
    struct TestKeys {
        inner: MemKeyStore,
        mode: StoreMode,
        lookups: AtomicU64,
    }

    #[async_trait::async_trait]
    impl KeyStore for TestKeys {
        async fn lookup(&self, prefix: &str) -> Result<Option<KeyRecord>, KeyStoreError> {
            self.lookups.fetch_add(1, Ordering::Relaxed);
            match self.mode {
                StoreMode::Answer => self.inner.lookup(prefix).await,
                StoreMode::Fail => Err(KeyStoreError::new("test_failure")),
                StoreMode::Hang => std::future::pending().await,
            }
        }
    }

    /// Everything about a deployment that an unauthenticated caller must not be
    /// able to observe.
    #[derive(Clone, Copy, Debug)]
    struct Deployment {
        openai: bool,
        anthropic: bool,
        exact: bool,
        priced: bool,
        budgets: bool,
    }

    fn deployments() -> Vec<Deployment> {
        (0u8..32)
            .map(|b| Deployment {
                openai: b & 1 != 0,
                anthropic: b & 2 != 0,
                exact: b & 4 != 0,
                priced: b & 8 != 0,
                budgets: b & 16 != 0,
            })
            .collect()
    }

    struct Harness {
        svc: Svc,
        keys: Arc<TestKeys>,
        metrics: Arc<Metrics>,
        usage: Arc<MemUsageSink>,
        /// A valid key.
        key: String,
        /// The valid key's prefix with a different, well-formed secret.
        wrong_secret: String,
    }

    impl Harness {
        async fn send(&self, req: axum::extract::Request) -> Response {
            self.svc.clone().oneshot(req).await.expect("infallible")
        }
        fn lookups(&self) -> u64 {
            self.keys.lookups.load(Ordering::Relaxed)
        }
        fn unauthenticated(&self) -> u64 {
            self.metrics.unauthenticated.load(Ordering::Relaxed)
        }
        fn key_store_errors(&self) -> u64 {
            self.metrics.key_store_error.load(Ordering::Relaxed)
        }
    }

    fn lazy_pool(acquire_timeout: Duration) -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(acquire_timeout)
            .connect_lazy(CLOSED_PG)
            .expect("a lazy pool parses its URL only")
    }

    struct Options {
        deployment: Deployment,
        mode: StoreMode,
        request_timeout: Duration,
        key_lookup_timeout: Duration,
        /// Use the real Postgres key store over a closed port instead of
        /// `TestKeys`.
        pg_keys: Option<Duration>,
        db_acquire_timeout: Duration,
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                deployment: Deployment {
                    openai: true,
                    anthropic: true,
                    exact: false,
                    priced: true,
                    budgets: true,
                },
                mode: StoreMode::Answer,
                request_timeout: Duration::from_secs(30),
                key_lookup_timeout: Duration::from_secs(5),
                pg_keys: None,
                db_acquire_timeout: Duration::from_millis(300),
            }
        }
    }

    fn harness(opts: Options) -> Harness {
        let d = opts.deployment;
        let hasher = KeyHasher::random();
        let generated = hasher.generate();
        let other = hasher.generate();
        let (prefix, _) = crate::apikey::parse_strict(&generated.plaintext).unwrap();
        let (_, other_secret) = crate::apikey::parse_strict(&other.plaintext).unwrap();
        let wrong_secret = format!("{prefix}_{other_secret}");

        let mut mem = MemKeyStore::new();
        mem.insert(
            generated.prefix.clone(),
            KeyRecord {
                id: "8f0c6a4e-0d7e-4c2b-9b1a-2f6d3c5e7a91".to_owned(),
                key_hash: generated.key_hash.clone(),
            },
        );
        let keys = Arc::new(TestKeys {
            inner: mem,
            mode: opts.mode,
            lookups: AtomicU64::new(0),
        });
        let key_store: Arc<dyn KeyStore> = match opts.pg_keys {
            Some(t) => Arc::new(PgKeyStore::new(lazy_pool(t))),
            None => keys.clone(),
        };

        let http = reqwest::Client::new();
        let openai = d.openai.then(|| {
            Arc::new(OpenAiProvider::custom(
                http.clone(),
                http.clone(),
                CLOSED_HTTP.to_owned(),
                "test-key".to_owned(),
            ))
        });
        let anthropic = d.anthropic.then(|| {
            Arc::new(crate::providers::AnthropicProvider::new(
                http.clone(),
                http.clone(),
                "test-key".to_owned(),
                CLOSED_HTTP.to_owned(),
                "2023-06-01".to_owned(),
            ))
        });
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert(PROVIDER_MOCK.to_owned(), Arc::new(MockProvider));
        if let Some(p) = &openai {
            providers.insert(PROVIDER_OPENAI.to_owned(), p.clone() as Arc<dyn Provider>);
        }
        if let Some(p) = &anthropic {
            providers.insert(
                PROVIDER_ANTHROPIC.to_owned(),
                p.clone() as Arc<dyn Provider>,
            );
        }
        let prices = if d.priced {
            PriceBook::from_prices(vec![
                ModelPrice::new(PROVIDER_MOCK, "demo", 3_000_000, 15_000_000),
                ModelPrice::new(PROVIDER_ANTHROPIC, "m", 3_000_000, 15_000_000),
                ModelPrice::new(PROVIDER_OPENAI, "m", 3_000_000, 15_000_000),
            ])
        } else {
            PriceBook::from_prices(Vec::new())
        };
        let budget_list = if d.budgets {
            vec![Budget {
                scope: Scope::Global,
                period: Period::Monthly,
                limit_micros: 1_000_000,
                hard_stop: true,
            }]
        } else {
            Vec::new()
        };
        let metrics = Arc::new(Metrics::default());
        let usage = Arc::new(MemUsageSink::new());
        let core = GatewayCore {
            dummy_hash: hasher.hash("dummy"),
            hasher,
            keys: key_store,
            budgets: Arc::new(Budgets::new(budget_list.clone())),
            usage: usage.clone(),
            prices: Arc::new(prices),
            providers,
            admission_exact: d.exact,
            metrics: metrics.clone(),
        };
        let redis = ConnectionManager::new_lazy_with_config(
            redis::Client::open("redis://127.0.0.1:1").expect("parses"),
            redis::aio::ConnectionManagerConfig::new(),
        )
        .expect("a lazy connection manager does not connect");
        let state = AppState {
            db: lazy_pool(opts.db_acquire_timeout),
            redis,
            core: Arc::new(ArcSwap::from_pointee(core)),
            budgets: Arc::new(ArcSwap::from_pointee(budget_list)),
            openai,
            anthropic,
        };
        let svc =
            BoxCloneService::new(router(state, opts.request_timeout, opts.key_lookup_timeout));
        Harness {
            svc,
            keys,
            metrics,
            usage,
            key: generated.plaintext,
            wrong_secret,
        }
    }

    fn request(method: &Method, path: &str, body: Body) -> axum::extract::Request {
        axum::extract::Request::builder()
            .method(method.clone())
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .expect("request builds")
    }

    fn with(
        mut req: axum::extract::Request,
        name: &'static str,
        value: &str,
    ) -> axum::extract::Request {
        req.headers_mut()
            .append(name, HeaderValue::from_str(value).expect("header value"));
        req
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("body reads");
        serde_json::from_slice(&bytes).expect("json body")
    }

    /// A well-formed key nobody issued.
    fn unknown_key() -> String {
        KeyHasher::random().generate().plaintext
    }

    fn surface_of(path: &str) -> Surface {
        crate::auth_gate::surface_for(path).expect("a gated path")
    }

    const GATED_PATHS: [&str; 13] = [
        "/v1/chat/completions",
        "/v1/messages",
        "/v1/messages/count_tokens",
        "/v1/messages/batches",
        "/v1/anthropic/messages",
        "/v1/nosuch/x",
        "/v1/models",
        "/v1",
        "/v1/",
        "/v1/%FF/x",
        "/console/budgets",
        "/console/usage",
        "/console/nosuch",
    ];

    fn golden_401(surface: Surface) -> Value {
        match surface {
            Surface::Anthropic => json!({"type": "error", "error": {
                "type": "authentication_error", "message": "invalid or missing API key"}}),
            Surface::OpenAi => json!({"error": {
                "message": "invalid or missing API key", "type": "unauthenticated"}}),
            Surface::Legacy => {
                json!({"error": "invalid or missing API key", "header": "x-tollgate-key"})
            }
            Surface::Console => json!({"error": "the console requires a valid API key"}),
        }
    }

    fn golden_503(surface: Surface) -> Value {
        match surface {
            Surface::Anthropic => json!({"type": "error", "error": {
                "type": "api_error", "message": "gateway backend unavailable"}}),
            Surface::OpenAi => json!({"error": {
                "message": "gateway temporarily unavailable", "type": "backend_error"}}),
            Surface::Legacy | Surface::Console => {
                json!({"error": "gateway temporarily unavailable"})
            }
        }
    }

    /// The only headers a gate refusal may carry. `content-length` is allowed
    /// but not required: whether it is set before hyper writes the response is
    /// not something a caller can use to tell deployments apart, because it is
    /// the same for every request here.
    fn assert_refusal_headers(resp: &Response, reason: &str, ctx: &str) {
        for name in resp.headers().keys() {
            assert!(
                [header::CONTENT_TYPE, header::CONTENT_LENGTH].contains(name)
                    || name == "x-tollgate-reason",
                "{ctx}: unexpected header {name}"
            );
        }
        assert_eq!(resp.headers()["x-tollgate-reason"], reason, "{ctx}");
        assert!(
            resp.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json"),
            "{ctx}"
        );
    }

    /// Every gated path, method, credential and body, in every deployment,
    /// gets the same 401 for its surface, with no lookup unless a well-formed
    /// key was presented, one count on `unauthenticated` and no ledger row.
    #[tokio::test]
    async fn unauthenticated_callers_see_one_401_per_surface_in_every_deployment() {
        let methods = [
            Method::POST,
            Method::GET,
            Method::HEAD,
            Method::OPTIONS,
            Method::PUT,
        ];
        let three_mib = "x".repeat(3 * 1024 * 1024);
        let mut seen: HashMap<(Surface, bool), Observed> = HashMap::new();

        for d in deployments() {
            let h = harness(Options {
                deployment: d,
                ..Options::default()
            });
            let creds: Vec<Cred> = vec![
                ("none", None, 0, 0),
                (
                    "bearer junk",
                    Some(("authorization", "Bearer nope".to_owned())),
                    0,
                    0,
                ),
                (
                    "malformed tollgate key",
                    Some(("x-tollgate-key", "tgk_a_b".to_owned())),
                    0,
                    0,
                ),
                ("unknown key", Some(("x-tollgate-key", unknown_key())), 1, 1),
                (
                    "unknown bearer",
                    Some(("authorization", format!("Bearer {}", unknown_key()))),
                    1,
                    1,
                ),
                (
                    "wrong secret",
                    Some(("x-tollgate-key", h.wrong_secret.clone())),
                    1,
                    1,
                ),
                (
                    "unknown x-api-key",
                    Some(("x-api-key", unknown_key())),
                    1,
                    0,
                ),
                (
                    "malformed x-api-key",
                    Some(("x-api-key", "sk-ant-nope".to_owned())),
                    0,
                    0,
                ),
            ];
            let mut cases: Vec<(String, axum::extract::Request, u64)> = Vec::new();
            for path in GATED_PATHS {
                let anthropic = surface_of(path) == Surface::Anthropic;
                for m in &methods {
                    for (label, cred, on_anthropic, elsewhere) in &creds {
                        let mut req =
                            request(m, path, Body::from(r#"{"stream":true,"model":"m"}"#));
                        if let Some(&(name, ref value)) = cred.as_ref() {
                            req = with(req, name, value);
                        }
                        let lookups = if anthropic { *on_anthropic } else { *elsewhere };
                        cases.push((format!("{m} {path} {label}"), req, lookups));
                    }
                }
                // Bodies and betas, with and without a well-formed key.
                let bodies: [(&str, Body); 5] = [
                    ("stream body", Body::from(r#"{"stream":true,"model":"m"}"#)),
                    ("empty object", Body::from("{}")),
                    ("not json", Body::from("not json")),
                    ("invalid utf-8", Body::from(vec![0xff_u8, 0xfe, 0xfd])),
                    ("3 MiB", Body::from(three_mib.clone())),
                ];
                for (body_label, body) in bodies {
                    let bytes = axum::body::to_bytes(body, 4 << 20).await.unwrap();
                    for beta in [None, Some("bogus"), Some("\u{fffd}")] {
                        for keyed in [false, true] {
                            let mut req = request(&Method::POST, path, Body::from(bytes.clone()));
                            if let Some(b) = beta {
                                req.headers_mut().insert(
                                    "anthropic-beta",
                                    HeaderValue::from_bytes(b.as_bytes()).unwrap(),
                                );
                            }
                            if keyed {
                                req = with(req, "x-tollgate-key", &unknown_key());
                            }
                            cases.push((
                                format!("POST {path} {body_label} beta={beta:?} keyed={keyed}"),
                                req,
                                u64::from(keyed),
                            ));
                        }
                    }
                }
            }

            for (ctx, req, want_lookups) in cases {
                let path = req.uri().path().to_owned();
                let surface = surface_of(&path);
                let (l0, u0) = (h.lookups(), h.unauthenticated());
                let resp = h.send(req).await;
                let ctx = format!("{d:?} {ctx}");
                assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{ctx}");
                assert!(resp.headers().get(header::ALLOW).is_none(), "{ctx}");
                assert_refusal_headers(&resp, "unauthenticated", &ctx);
                assert_eq!(h.lookups() - l0, want_lookups, "{ctx}: lookups");
                assert_eq!(h.unauthenticated() - u0, 1, "{ctx}: unauthenticated count");
                let status = resp.status();
                let headers: Vec<(String, String)> = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
                    .collect();
                let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
                    .await
                    .unwrap();
                let v: Value = serde_json::from_slice(&body).expect("json");
                assert_eq!(v, golden_401(surface), "{ctx}");
                // Byte-identical across every request on the surface, in every
                // deployment. Keyed on whether a lookup ran only to keep the
                // map small; the expected value is the same either way.
                let entry = seen
                    .entry((surface, want_lookups > 0))
                    .or_insert_with(|| (status, headers.clone(), body.to_vec()));
                assert_eq!(*entry, (status, headers, body.to_vec()), "{ctx}: differs");
            }
            assert_eq!(h.key_store_errors(), 0, "{d:?}");
            assert!(h.usage.snapshot().is_empty(), "{d:?}: ledger rows");
        }
    }

    /// A store that fails, or does not answer, is a 503 `backend_error` on every
    /// surface and in every deployment, counted once as `key_store_error` and
    /// never as `unauthenticated`.
    ///
    /// The hanging store runs in real time (the crate does not enable tokio's
    /// `test-util`), so it covers the two extreme deployments only; the failing
    /// store covers all of them, and the gate's response does not depend on
    /// which of the two happened.
    #[tokio::test]
    async fn a_key_store_that_fails_or_hangs_is_a_503_never_a_401() {
        let lookup_timeout = Duration::from_millis(100);
        let deadline = lookup_timeout + crate::auth_gate::DEADLINE_MARGIN;
        let all = deployments();
        let extremes = vec![all[0], all[all.len() - 1]];
        for (mode, ds) in [(StoreMode::Fail, all.clone()), (StoreMode::Hang, extremes)] {
            for d in ds {
                let h = harness(Options {
                    deployment: d,
                    mode,
                    key_lookup_timeout: lookup_timeout,
                    ..Options::default()
                });
                for path in [
                    "/v1/messages",
                    "/v1/chat/completions",
                    "/v1/mock/generate",
                    "/console/budgets",
                ] {
                    let surface = surface_of(path);
                    let (k0, u0) = (h.key_store_errors(), h.unauthenticated());
                    let started = Instant::now();
                    let resp = h
                        .send(with(
                            request(&Method::POST, path, Body::from("{}")),
                            "x-tollgate-key",
                            &h.key,
                        ))
                        .await;
                    let ctx = format!("{mode:?} {d:?} {path}");
                    if mode == StoreMode::Hang {
                        // The gate's deadline: the lookup timeout plus the margin.
                        let took = started.elapsed();
                        assert!(
                            (deadline..deadline + Duration::from_secs(5)).contains(&took),
                            "{ctx}: took {took:?}"
                        );
                    }
                    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{ctx}");
                    assert_refusal_headers(&resp, "backend_error", &ctx);
                    assert_eq!(body_json(resp).await, golden_503(surface), "{ctx}");
                    assert_eq!(h.key_store_errors() - k0, 1, "{ctx}");
                    assert_eq!(h.unauthenticated() - u0, 0, "{ctx}");
                }
                // A malformed key never reaches the store, so it stays a 401.
                let resp = h
                    .send(with(
                        request(&Method::POST, "/v1/messages", Body::from("{}")),
                        "x-tollgate-key",
                        "tgk_a_b",
                    ))
                    .await;
                assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{mode:?} {d:?}");
                assert!(h.usage.snapshot().is_empty());
            }
        }
    }

    /// The real Postgres key store against a closed port: a 503 and one
    /// `key_store_error`, never the 401 that once hid a database outage.
    #[tokio::test]
    async fn the_postgres_key_store_failing_is_a_503() {
        let h = harness(Options {
            pg_keys: Some(Duration::from_millis(300)),
            key_lookup_timeout: Duration::from_millis(300),
            ..Options::default()
        });
        let resp = h
            .send(with(
                request(&Method::POST, "/v1/chat/completions", Body::from("{}")),
                "authorization",
                &format!("Bearer {}", h.key),
            ))
            .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers()["x-tollgate-reason"], "backend_error");
        assert_eq!(h.key_store_errors(), 1);
        assert_eq!(h.unauthenticated(), 0);

        // And it is on /metrics, which needs no key.
        let resp = h
            .send(request(&Method::GET, "/metrics", Body::empty()))
            .await;
        let text = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            text.contains("tollgate_requests_total{decision=\"key_store_error\"} 1\n"),
            "{text}"
        );
    }

    /// The checks that used to run before authentication still run, for an
    /// authenticated caller, and each request makes exactly one lookup: no
    /// handler authenticates again.
    #[tokio::test]
    async fn authenticated_callers_still_get_every_check_that_moved() {
        let three_mib = "x".repeat(3 * 1024 * 1024);
        let base = Options::default().deployment;
        let exact = || Options {
            deployment: Deployment {
                exact: true,
                ..base
            },
            ..Options::default()
        };
        let no_anthropic = || Options {
            deployment: Deployment {
                anthropic: false,
                ..base
            },
            ..Options::default()
        };

        let stream = r#"{"stream":true,"model":"m","max_tokens":10,"messages":[]}"#;
        type Check = (
            &'static str,
            Options,
            Method,
            &'static str,
            Body,
            Option<(&'static str, &'static str)>,
            StatusCode,
            Option<&'static str>,
        );
        let checks: Vec<Check> = vec![
            (
                "count_tokens",
                Options::default(),
                Method::POST,
                "/v1/messages/count_tokens",
                Body::from("{}"),
                None,
                StatusCode::NOT_FOUND,
                Some("bad_request"),
            ),
            (
                "wrong method",
                Options::default(),
                Method::GET,
                "/v1/messages",
                Body::empty(),
                None,
                StatusCode::METHOD_NOT_ALLOWED,
                None,
            ),
            (
                "unknown path",
                Options::default(),
                Method::GET,
                "/v1/models",
                Body::empty(),
                None,
                StatusCode::NOT_FOUND,
                None,
            ),
            (
                "anthropic not enabled",
                no_anthropic(),
                Method::POST,
                "/v1/messages",
                Body::from(stream),
                None,
                StatusCode::SERVICE_UNAVAILABLE,
                Some("backend_error"),
            ),
            (
                "exact admission stream",
                exact(),
                Method::POST,
                "/v1/chat/completions",
                Body::from(stream),
                None,
                StatusCode::NOT_IMPLEMENTED,
                Some("unsupported"),
            ),
            (
                "bogus beta",
                Options::default(),
                Method::POST,
                "/v1/messages",
                Body::from("{}"),
                Some(("anthropic-beta", "bogus")),
                StatusCode::BAD_REQUEST,
                Some("bad_request"),
            ),
            (
                "3 MiB body",
                Options::default(),
                Method::POST,
                "/v1/chat/completions",
                Body::from(three_mib),
                None,
                StatusCode::PAYLOAD_TOO_LARGE,
                None,
            ),
            (
                "invalid utf-8 body",
                Options::default(),
                Method::POST,
                "/v1/chat/completions",
                Body::from(vec![0xff_u8, 0xfe]),
                None,
                StatusCode::BAD_REQUEST,
                None,
            ),
            (
                "undecodable path",
                Options::default(),
                Method::POST,
                "/v1/%FF/x",
                Body::from("{}"),
                None,
                StatusCode::BAD_REQUEST,
                None,
            ),
            (
                "console unknown path",
                Options::default(),
                Method::GET,
                "/console/nosuch",
                Body::empty(),
                None,
                StatusCode::NOT_FOUND,
                Some("bad_request"),
            ),
            (
                "console wrong method",
                Options::default(),
                Method::POST,
                "/console/budgets",
                Body::empty(),
                None,
                StatusCode::METHOD_NOT_ALLOWED,
                Some("bad_request"),
            ),
        ];
        for (label, opts, method, path, body, extra, want, reason) in checks {
            let h = harness(opts);
            let mut req = with(request(&method, path, body), "x-tollgate-key", &h.key);
            if let Some((name, value)) = extra {
                req = with(req, name, value);
            }
            let resp = h.send(req).await;
            assert_eq!(resp.status(), want, "{label}");
            assert_eq!(
                resp.headers()
                    .get("x-tollgate-reason")
                    .map(|v| v.to_str().unwrap()),
                reason,
                "{label}"
            );
            if want == StatusCode::METHOD_NOT_ALLOWED {
                assert!(resp.headers().get(header::ALLOW).is_some(), "{label}");
            }
            assert_eq!(h.lookups(), 1, "{label}: lookups");
            assert_eq!(h.unauthenticated(), 0, "{label}");
        }
    }

    /// Which header wins, pinned. `x-api-key` is a Tollgate key on the
    /// Anthropic surface only, and only when `x-tollgate-key` is absent.
    #[tokio::test]
    async fn key_header_precedence_is_pinned() {
        let h = harness(Options::default());
        let junk = unknown_key();
        let bearer = |k: &str| format!("Bearer {k}");
        // (path, headers, authenticated?)
        let cases: Vec<Precedence> = vec![
            (
                "/v1/messages/count_tokens",
                vec![("x-api-key", h.key.clone())],
                true,
            ),
            (
                "/v1/messages/count_tokens",
                vec![
                    ("x-tollgate-key", h.key.clone()),
                    ("x-api-key", junk.clone()),
                ],
                true,
            ),
            (
                "/v1/messages/count_tokens",
                vec![
                    ("x-tollgate-key", junk.clone()),
                    ("x-api-key", h.key.clone()),
                ],
                false,
            ),
            (
                "/v1/messages/count_tokens",
                vec![
                    ("x-api-key", h.key.clone()),
                    ("authorization", bearer(&junk)),
                ],
                true,
            ),
            (
                "/v1/messages/count_tokens",
                vec![
                    ("x-api-key", junk.clone()),
                    ("authorization", bearer(&h.key)),
                ],
                false,
            ),
            // An empty x-tollgate-key blocks the rewrite but falls through to
            // Bearer.
            (
                "/v1/messages/count_tokens",
                vec![
                    ("x-tollgate-key", String::new()),
                    ("x-api-key", h.key.clone()),
                ],
                false,
            ),
            (
                "/v1/messages/count_tokens",
                vec![
                    ("x-tollgate-key", String::new()),
                    ("authorization", bearer(&h.key)),
                ],
                true,
            ),
            // Elsewhere x-api-key is ignored.
            ("/v1/models", vec![("x-api-key", h.key.clone())], false),
            (
                "/v1/chat/completions",
                vec![("x-api-key", h.key.clone())],
                false,
            ),
            ("/console/nosuch", vec![("x-api-key", h.key.clone())], false),
            ("/v1/models", vec![("authorization", bearer(&h.key))], true),
            (
                "/v1/models",
                vec![
                    ("x-tollgate-key", h.key.clone()),
                    ("authorization", bearer(&junk)),
                ],
                true,
            ),
        ];
        for (path, headers, authed) in cases {
            let mut req = request(&Method::GET, path, Body::empty());
            if path == "/v1/messages/count_tokens" {
                *req.method_mut() = Method::POST;
            }
            for &(name, ref value) in &headers {
                req = with(req, name, value);
            }
            let status = h.send(req).await.status();
            let ctx = format!("{path} {headers:?}");
            if authed {
                assert_ne!(status, StatusCode::UNAUTHORIZED, "{ctx}");
            } else {
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{ctx}");
            }
        }
    }

    /// Every console refusal carries `x-tollgate-reason`, including the request
    /// timeout's 503, whose empty body is replaced without leaving a stale
    /// `content-length`.
    #[tokio::test]
    async fn every_console_refusal_carries_a_reason() {
        // The request timeout fires while the ledger read waits for a
        // connection that never comes.
        let h = harness(Options {
            request_timeout: Duration::from_millis(100),
            db_acquire_timeout: Duration::from_secs(5),
            ..Options::default()
        });
        let resp = h
            .send(with(
                request(&Method::GET, "/console/budgets", Body::empty()),
                "x-tollgate-key",
                &h.key,
            ))
            .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers()["x-tollgate-reason"], "backend_error");
        let length = resp
            .headers()
            .get(header::CONTENT_LENGTH)
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        if let Some(length) = length {
            assert_eq!(length, bytes.len().to_string(), "stale content-length");
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"error": "gateway temporarily unavailable"})
        );

        // The ledger read itself failing: its own message, and the header.
        let h = harness(Options {
            request_timeout: Duration::from_secs(30),
            db_acquire_timeout: Duration::from_millis(200),
            ..Options::default()
        });
        for (path, message) in [
            ("/console/budgets", "budget read failed"),
            ("/console/usage", "usage read failed"),
        ] {
            let resp = h
                .send(with(
                    request(&Method::GET, path, Body::empty()),
                    "x-tollgate-key",
                    &h.key,
                ))
                .await;
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert_eq!(
                resp.headers()["x-tollgate-reason"],
                "backend_error",
                "{path}"
            );
            assert_eq!(body_json(resp).await, json!({ "error": message }), "{path}");
        }
    }

    /// The gate's own refusals use the shared vocabulary on every surface.
    #[test]
    fn gate_refusals_use_the_shared_reason_words() {
        for surface in [
            Surface::Anthropic,
            Surface::OpenAi,
            Surface::Legacy,
            Surface::Console,
        ] {
            assert_eq!(
                unauthenticated(surface).headers()["x-tollgate-reason"],
                "unauthenticated",
                "{surface:?}"
            );
            assert_eq!(
                unauthenticated(surface).status(),
                StatusCode::UNAUTHORIZED,
                "{surface:?}"
            );
            assert_eq!(
                store_unavailable(surface).headers()["x-tollgate-reason"],
                "backend_error",
                "{surface:?}"
            );
            assert_eq!(
                store_unavailable(surface).status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{surface:?}"
            );
        }
    }
}
