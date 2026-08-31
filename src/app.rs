// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! HTTP application: router construction, shared state, and graceful shutdown.
//!
//! Production `serve` builds the shared [`GatewayCore`] from the Postgres and
//! Redis/Valkey backends and the configured providers, then serves the same
//! request flow the demo uses.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use redis::aio::ConnectionManager;
use serde_json::json;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::apikey::KeyHasher;
use crate::backends::{
    PgKeyStore, PgUsageSink, RedisBudgetBackend, budget_spent, load_budgets, load_prices,
};
use crate::budget::{Budget, Scope};
use crate::config::Config;
use crate::gateway::{GatewayCore, KeyStore, UsageSink, outcome_response};
use crate::pricing::{PriceBook, format_micros};
use crate::provider::{MockProvider, Provider};
use crate::routes::health;

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
}

impl CoreParts {
    fn build(&self, budgets: Vec<Budget>, prices: PriceBook) -> GatewayCore {
        GatewayCore {
            hasher: self.hasher.clone(),
            dummy_hash: self.dummy_hash.clone(),
            keys: self.keys.clone(),
            budgets: Arc::new(RedisBudgetBackend::new(self.redis.clone(), budgets)),
            usage: self.usage.clone(),
            prices: Arc::new(prices),
            providers: self.providers.clone(),
            admission_exact: self.admission_exact,
        }
    }
}

pub async fn serve(cfg: Config) -> Result<()> {
    let db = crate::db::build_pool(&cfg.database).await?;
    let redis = build_redis(&cfg.redis.url).await?;

    // Load budget config and prices from Postgres into the core.
    let budgets = load_budgets(&db).await.context("loading budgets")?;
    let prices = load_prices(&db).await.context("loading model prices")?;
    tracing::info!(
        budgets = budgets.len(),
        priced_models = prices.len(),
        "loaded budget and price config"
    );

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
        .timeout(cfg.http.request_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building HTTP client")?;

    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    if cfg.providers.enable_mock {
        providers.insert("mock".to_owned(), Arc::new(MockProvider));
    }
    if cfg.providers.anthropic.enabled {
        providers.insert(
            "anthropic".to_owned(),
            Arc::new(crate::providers::AnthropicProvider::new(
                http.clone(),
                cfg.providers.anthropic.api_key.clone(),
                cfg.providers.anthropic.base_url.clone(),
                cfg.providers.anthropic.version.clone(),
            )),
        );
    }
    if cfg.providers.vertex.enabled {
        providers.insert(
            "vertex".to_owned(),
            Arc::new(crate::providers::VertexProvider::new(
                http.clone(),
                cfg.providers.vertex.project.clone(),
                cfg.providers.vertex.location.clone(),
                cfg.providers.vertex.access_token.clone(),
            )),
        );
    }
    if cfg.providers.openai.enabled {
        let oc = &cfg.providers.openai;
        let provider: Arc<dyn Provider> = match oc.upstream.as_str() {
            "vertex" => {
                if cfg.providers.vertex.project.is_empty()
                    || cfg.providers.vertex.location.is_empty()
                {
                    anyhow::bail!(
                        "OpenAI endpoint with upstream=vertex requires \
                         TOLLGATE_PROVIDERS__VERTEX__PROJECT and __LOCATION"
                    );
                }
                Arc::new(crate::providers::OpenAiProvider::vertex(
                    http.clone(),
                    &cfg.providers.vertex.project,
                    &cfg.providers.vertex.location,
                    cfg.providers.vertex.access_token.clone(),
                ))
            }
            "custom" => {
                // An empty key would make the adapter fall back to the GCP
                // metadata token and send it to the custom host: refuse.
                if oc.base_url.is_empty() || oc.api_key.is_empty() {
                    anyhow::bail!(
                        "OpenAI endpoint with upstream=custom requires \
                         TOLLGATE_PROVIDERS__OPENAI__BASE_URL and __API_KEY"
                    );
                }
                Arc::new(crate::providers::OpenAiProvider::custom(
                    http.clone(),
                    oc.base_url.clone(),
                    oc.api_key.clone(),
                ))
            }
            other => anyhow::bail!(
                "TOLLGATE_PROVIDERS__OPENAI__UPSTREAM must be 'vertex' or 'custom' (got {other:?})"
            ),
        };
        providers.insert("openai".to_owned(), provider);
    }
    let admission = cfg.providers.admission.to_ascii_lowercase();
    if admission != "fast" && admission != "exact" {
        anyhow::bail!(
            "TOLLGATE_PROVIDERS__ADMISSION must be 'fast' or 'exact' (got {:?})",
            cfg.providers.admission
        );
    }
    let admission_exact = admission == "exact";
    tracing::info!(
        providers = providers.len(),
        admission = %cfg.providers.admission,
        "providers registered"
    );

    let parts = CoreParts {
        hasher,
        dummy_hash,
        keys: Arc::new(PgKeyStore::new(db.clone())),
        usage: Arc::new(PgUsageSink::new(db.clone())),
        providers,
        admission_exact,
        redis: redis.clone(),
    };

    let core = Arc::new(ArcSwap::from_pointee(parts.build(budgets.clone(), prices)));
    let budgets_view = Arc::new(ArcSwap::from_pointee(budgets));

    // Periodically reload budgets and prices from Postgres so `admin budget set`
    // and `admin price set` take effect without a restart. Only the config
    // swaps; spend counters are left untouched, so a changed limit applies to the
    // existing counter and a new budget starts counting when it is picked up.
    if !cfg.reload.interval.is_zero() {
        spawn_reload_task(
            db.clone(),
            parts.clone(),
            core.clone(),
            budgets_view.clone(),
            cfg.reload.interval,
        );
        tracing::info!(interval = ?cfg.reload.interval, "config hot-reload enabled");
    }

    let state = AppState {
        db,
        redis,
        core,
        budgets: budgets_view,
    };
    let app = router(state, cfg.http.request_timeout);

    let listener = TcpListener::bind(cfg.http.bind)
        .await
        .with_context(|| format!("binding {}", cfg.http.bind))?;

    tracing::info!(addr = %cfg.http.bind, "tollgate listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(cfg.http.shutdown_grace))
        .await
        .context("http server error")
}

/// Background task: reload budgets and prices from Postgres on an interval and
/// atomically swap them into the live core. On any load error it logs and keeps
/// the current config rather than dropping enforcement.
fn spawn_reload_task(
    db: PgPool,
    parts: CoreParts,
    core: Arc<ArcSwap<GatewayCore>>,
    budgets_view: Arc<ArcSwap<Vec<Budget>>>,
    interval: Duration,
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
            let prices = match load_prices(&db).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "config reload: prices query failed; keeping current");
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

pub fn router(state: AppState, request_timeout: Duration) -> Router {
    Router::new()
        .route("/healthz", get(health::live))
        .route("/readyz", get(health::ready))
        .route("/metrics", get(metrics))
        .route("/console", get(console))
        .route("/console/budgets", get(console_budgets))
        .route("/console/usage", get(console_usage))
        .route("/v1/chat/completions", post(openai_chat))
        .route("/v1/{provider}/{*rest}", post(gateway))
        .with_state(state)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            request_timeout,
        ))
        .layer(TraceLayer::new_for_http())
}

async fn gateway(
    State(state): State<AppState>,
    Path((provider, rest)): Path<(String, String)>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let core = state.core.load_full();
    let outcome = core.evaluate(&provider, &rest, &headers, &body).await;
    outcome_response(outcome)
}

/// OpenAI-compatible Chat Completions endpoint. Routes to the `openai` provider
/// (which fronts Vertex's OpenAI endpoint or a custom upstream) through the same
/// reserve-then-settle enforcement, and returns the upstream's OpenAI body
/// verbatim so OpenAI clients, LiteLLM, and ADK agents work unchanged.
async fn openai_chat(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    let core = state.core.load_full();
    let outcome = core
        .evaluate("openai", "chat/completions", &headers, &body)
        .await;
    openai_outcome_response(outcome)
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
fn openai_outcome_response(outcome: crate::gateway::Outcome) -> Response {
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

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "the console requires a valid API key"})),
    )
        .into_response()
}

/// Read-only budgets view: current-period spend and limit per configured budget,
/// summed from the durable ledger. Authenticated by any valid key: Tollgate is
/// single-tenant per deployment, so any key in the deployment may observe its
/// budgets and usage. Per-tenant scoping is an Enterprise-edition concern.
async fn console_budgets(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let core = state.core.load_full();
    if core.authenticate(&headers).await.is_none() {
        return unauthorized();
    }
    let budgets = state.budgets.load_full();
    let now = Utc::now();
    let mut out = Vec::with_capacity(budgets.len());
    for b in budgets.iter() {
        let spent = match budget_spent(&state.db, b, now).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "console: budget spend query failed");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"error": "budget read failed"})),
                )
                    .into_response();
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
    input_tokens: i64,
    output_tokens: i64,
    cost_micros: i64,
    gateway_micros: i64,
    decision: String,
}

/// Read-only usage view: the 100 most recent ledger rows, oldest-first so the
/// console can number them chronologically. Authenticated by any valid key (see
/// `console_budgets`). Cost is a plain decimal string, never raw micros.
async fn console_usage(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let core = state.core.load_full();
    if core.authenticate(&headers).await.is_none() {
        return unauthorized();
    }
    let rows = sqlx::query_as::<_, UsageRow>(
        "SELECT provider, model, input_tokens, output_tokens, cost_micros, gateway_micros, \
                decision \
         FROM (SELECT provider, model, input_tokens, output_tokens, cost_micros, gateway_micros, \
                      decision, started_at \
               FROM usage_events ORDER BY started_at DESC LIMIT 100) recent \
         ORDER BY started_at ASC",
    )
    .fetch_all(&state.db)
    .await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "console: usage query failed");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "usage read failed"})),
            )
                .into_response();
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
                "cost": format_micros(r.cost_micros),
                "overhead_us": r.gateway_micros,
                "decision": r.decision,
            })
        })
        .collect();
    let total: i64 = rows.iter().map(|r| r.cost_micros).sum();
    Json(json!({
        "events": events,
        "total_cost": format_micros(total),
        "count": rows.len(),
    }))
    .into_response()
}

/// Minimal Prometheus endpoint. Per-request counters are added with a metrics
/// layer in a later change; the ledger in Postgres is the system of record.
async fn metrics() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        "# HELP tollgate_up 1 if the gateway is serving.\n\
         # TYPE tollgate_up gauge\n\
         tollgate_up 1\n"
            .to_owned(),
    )
}

async fn build_redis(url: &str) -> Result<ConnectionManager> {
    let client = redis::Client::open(url).context("parsing Redis URL")?;
    ConnectionManager::new(client)
        .await
        .context("connecting to Redis")
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
