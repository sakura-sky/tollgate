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
use crate::gateway::{GatewayCore, outcome_response};
use crate::pricing::format_micros;
use crate::provider::{MockProvider, Provider};
use crate::routes::health;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub redis: ConnectionManager,
    pub core: Arc<GatewayCore>,
    /// Budget config (loaded at startup) for the read-only console endpoints.
    pub budgets: Arc<Vec<Budget>>,
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
    let hasher = KeyHasher::new(cfg.security.api_key_pepper.clone().into_bytes());
    let dummy_hash = hasher.hash("tollgate-fixed-dummy-secret");

    // Rebuild budget counters from the durable ledger so a cache flush or
    // restart cannot reset budgets to zero.
    let restored = crate::backends::reconcile_counters(&db, &redis, &budgets)
        .await
        .context("reconciling budget counters")?;
    tracing::info!(restored, "reconciled budget counters from ledger");

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

    let core = Arc::new(GatewayCore {
        hasher,
        dummy_hash,
        keys: Arc::new(PgKeyStore::new(db.clone())),
        budgets: Arc::new(RedisBudgetBackend::new(redis.clone(), budgets.clone())),
        usage: Arc::new(PgUsageSink::new(db.clone())),
        prices: Arc::new(prices),
        providers,
        admission_exact,
    });

    let state = AppState {
        db,
        redis,
        core,
        budgets: Arc::new(budgets),
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

pub fn router(state: AppState, request_timeout: Duration) -> Router {
    Router::new()
        .route("/healthz", get(health::live))
        .route("/readyz", get(health::ready))
        .route("/metrics", get(metrics))
        .route("/console", get(console))
        .route("/admin/budgets", get(admin_budgets))
        .route("/admin/usage", get(admin_usage))
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
    let outcome = state.core.evaluate(&provider, &rest, &headers, &body).await;
    outcome_response(outcome)
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
        Json(json!({"error": "admin endpoints require a valid API key"})),
    )
        .into_response()
}

/// Read-only budgets view: current-period spend and limit per configured budget,
/// summed from the durable ledger. Key-authenticated.
async fn admin_budgets(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if state.core.authenticate(&headers).await.is_none() {
        return unauthorized();
    }
    let now = Utc::now();
    let mut out = Vec::with_capacity(state.budgets.len());
    for b in state.budgets.iter() {
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
    decision: String,
}

/// Read-only usage view: the 100 most recent ledger rows, oldest-first so the
/// console can number them chronologically. Key-authenticated. Cost is a plain
/// decimal string, never raw micros.
async fn admin_usage(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if state.core.authenticate(&headers).await.is_none() {
        return unauthorized();
    }
    let rows = sqlx::query_as::<_, UsageRow>(
        "SELECT provider, model, input_tokens, output_tokens, cost_micros, decision \
         FROM (SELECT provider, model, input_tokens, output_tokens, cost_micros, decision, \
                      started_at \
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
