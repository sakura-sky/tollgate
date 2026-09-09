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
use axum::{Json, Router};
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;
use redis::aio::ConnectionManager;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::apikey::KeyHasher;
use crate::backends::{
    PgKeyStore, PgUsageSink, RedisBudgetBackend, budget_spent, load_budgets, load_prices,
};
use crate::budget::{Budget, RequestCtx, Scope};
use crate::config::Config;
use crate::gateway::{
    GatewayCore, KeyStore, ReserveError, Settlement, UsageSink, outcome_response,
};
use crate::pricing::{ModelPrice, PriceBook, Usage, format_micros};
use crate::provider::{MockProvider, Provider, ProviderError};
use crate::providers::{CacheSemantics, OpenAiProvider, stream_requested, usage_from_sse_data};
use crate::routes::health;

/// Cap the SSE line-reassembly buffer so an upstream that never emits a newline
/// cannot grow it without bound.
const MAX_SSE_LINE_BUFFER: usize = 1024 * 1024;
/// Idle timeout: abort a stream if no upstream chunk arrives within this window.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard cap on total stream duration.
const STREAM_MAX_DURATION: Duration = Duration::from_secs(15 * 60);

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

/// Refuse to serve against a database that is behind this binary.
///
/// Migrations are applied by an explicit `admin migrate`, so a deploy can easily
/// start a new binary against an old schema. That failure is silent and
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

pub async fn serve(cfg: Config) -> Result<()> {
    let db = crate::db::build_pool(&cfg.database).await?;
    ensure_schema_current(&db).await?;
    let redis = build_redis(&cfg.redis.url).await?;

    // Fail closed at boot on a fallback multiple that would price an unpriced
    // cache class below the model's own input rate. That is the same fail-open
    // the nullable rate columns exist to prevent, just moved into config.
    let cache_fallback = cfg.billing.cache_rate_fallback();
    cache_fallback
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid billing config: {e}"))?;

    // Load budget config and prices from Postgres into the core.
    let budgets = load_budgets(&db).await.context("loading budgets")?;
    let prices = load_prices(&db, cache_fallback)
        .await
        .context("loading model prices")?;
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
        providers.insert("mock".to_owned(), Arc::new(MockProvider));
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
        providers.insert("anthropic".to_owned(), arc.clone() as Arc<dyn Provider>);
        anthropic_handle = Some(arc);
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
    let mut openai_handle: Option<Arc<crate::providers::OpenAiProvider>> = None;
    if cfg.providers.openai.enabled {
        let oc = &cfg.providers.openai;
        let p = match oc.upstream.as_str() {
            "vertex" => {
                if cfg.providers.vertex.project.is_empty()
                    || cfg.providers.vertex.location.is_empty()
                {
                    anyhow::bail!(
                        "OpenAI endpoint with upstream=vertex requires \
                         TOLLGATE_PROVIDERS__VERTEX__PROJECT and __LOCATION"
                    );
                }
                crate::providers::OpenAiProvider::vertex(
                    http.clone(),
                    stream_http.clone(),
                    &cfg.providers.vertex.project,
                    &cfg.providers.vertex.location,
                    cfg.providers.vertex.access_token.clone(),
                )
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
                crate::providers::OpenAiProvider::custom(
                    http.clone(),
                    stream_http.clone(),
                    oc.base_url.clone(),
                    oc.api_key.clone(),
                )
            }
            other => anyhow::bail!(
                "TOLLGATE_PROVIDERS__OPENAI__UPSTREAM must be 'vertex' or 'custom' (got {other:?})"
            ),
        };
        let arc = Arc::new(p);
        providers.insert("openai".to_owned(), arc.clone() as Arc<dyn Provider>);
        openai_handle = Some(arc);
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
            cache_fallback,
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
    cache_fallback: crate::pricing::CacheRateFallback,
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
            let prices = match load_prices(&db, cache_fallback).await {
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

    local
        .merge(proxied)
        .with_state(state)
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

/// Sub-paths of `/v1/messages` that Tollgate does not meter.
///
/// `count_tokens` is free but unmetered, and `batches` bills asynchronously in a
/// way no per-request ledger row can capture. Without this they fall into the
/// catch-all as provider "messages" and write an unpriced row per call.
async fn anthropic_messages_unsupported() -> Response {
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
    headers: HeaderMap,
    body: String,
) -> Response {
    // Accept `x-api-key` as a Tollgate key on THIS ROUTE ONLY, by rewriting it
    // into the header the gateway already reads. Widening `presented_key`
    // instead would quietly accept it on /console/* and every other route.
    let mut headers = headers;
    if !headers.contains_key(crate::gateway::KEY_HEADER) {
        if let Some(v) = headers.get("x-api-key").cloned() {
            headers.insert(crate::gateway::KEY_HEADER, v);
        }
    }

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
        return anthropic_messages_stream(state, headers, body).await;
    }
    let core = state.core.load_full();
    let outcome = core
        .evaluate("anthropic", "messages", &headers, &body)
        .await;
    anthropic_outcome_response(outcome)
}

/// An Anthropic-shaped error, so SDK exception classes resolve correctly.
fn anthropic_error(status: StatusCode, kind: &str, message: &str) -> Response {
    (
        status,
        [("x-tollgate-reason", kind.to_owned())],
        Json(json!({"type": "error", "error": {"type": kind, "message": message}})),
    )
        .into_response()
}

/// Map an [`Outcome`] to a native Anthropic response: the upstream body verbatim
/// on success, Anthropic's error envelope on refusal.
fn anthropic_outcome_response(outcome: crate::gateway::Outcome) -> Response {
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
        Outcome::Unpriced { provider, model } => anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
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
        Outcome::BackendError(_) => anthropic_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
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
/// relays the upstream SSE and meters usage from the terminal chunk. Buffered
/// requests run the shared [`GatewayCore::evaluate`] flow unchanged.
async fn openai_chat(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    // Peek whether the client asked for streaming. A body that does not parse is
    // treated as non-streaming so the buffered path returns a proper OpenAI error
    // envelope (rather than opening an event-stream for a malformed request).
    let wants_stream = serde_json::from_str::<Value>(&body)
        .map(|v| stream_requested(&v))
        .unwrap_or(false);
    if wants_stream {
        return openai_chat_stream(state, headers, body).await;
    }
    let core = state.core.load_full();
    let outcome = core
        .evaluate("openai", "chat/completions", &headers, &body)
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
/// Metering uses the Anthropic meter, not the OpenAI one, because Anthropic's
/// usage is not terminal: `message_start` carries a placeholder `output_tokens:
/// 1`, and an error event arrives on an otherwise-healthy 200 stream followed by
/// a normal close. Treating transport EOF as success would settle a failed
/// stream on one output token.
async fn anthropic_messages_stream(state: AppState, headers: HeaderMap, body: String) -> Response {
    let started = Instant::now();
    let core = state.core.load_full();
    let Some(provider) = state.anthropic.clone() else {
        return anthropic_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "anthropic upstream is not configured",
        );
    };

    let Some(key_id) = core.authenticate(&headers).await else {
        return anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid or missing API key",
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

    let Some(price) = core.prices.lookup("anthropic", &parsed.model).cloned() else {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
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
                tracing::error!(error = %e.to_string(), "exact token count failed");
                return anthropic_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "api_error",
                    "gateway temporarily unavailable",
                );
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
            provider: "anthropic",
            model: &parsed.model,
        };
        match core.budgets.reserve(&ctx, reserve_micros).await {
            Ok(r) => r,
            Err(ReserveError::Denied(d)) => {
                return anthropic_error(
                    StatusCode::PAYMENT_REQUIRED,
                    "permission_error",
                    &format!("budget exceeded: {d}"),
                );
            }
            Err(ReserveError::Backend(m)) => {
                tracing::error!(error = %m, "budget backend error on stream reserve");
                return anthropic_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "api_error",
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
        "anthropic".to_owned(),
        parsed.model,
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
/// usage from the terminal chunk. Settlement is owned by a [`Settlement`]
/// guard that charges the observed cost on a clean finish and the FULL reservation
/// on any abnormal end (client/upstream disconnect, idle/duration timeout, panic),
/// so a stream is never under-charged.
async fn openai_chat_stream(state: AppState, headers: HeaderMap, body: String) -> Response {
    let started = Instant::now();
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

    let Some(key_id) = core.authenticate(&headers).await else {
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "invalid or missing API key",
        );
    };

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

    let Some(price) = core.prices.lookup("openai", &parsed.model).cloned() else {
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
            provider: "openai",
            model: &parsed.model,
        };
        match core.budgets.reserve(&ctx, reserve_micros).await {
            Ok(r) => r,
            Err(ReserveError::Denied(d)) => {
                return openai_error(
                    StatusCode::PAYMENT_REQUIRED,
                    "budget_exceeded",
                    &format!("budget exceeded: {d}"),
                );
            }
            Err(ReserveError::Backend(m)) => {
                tracing::error!(error = %m, "budget backend error on stream reserve");
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
        "openai".to_owned(),
        parsed.model,
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
    tokio::spawn(relay_stream(
        upstream,
        tx,
        guard,
        price,
        reserve_micros,
        // Unused on this path: the OpenAI meter has no pre-generation signal, so
        // every abnormal end charges the full reservation.
        reserve_micros,
        StreamMeter::OpenAi {
            line_buf: Vec::new(),
            // The SAME semantics the buffered path uses for this upstream. If
            // these ever diverge, a caller picks the cheaper metering by
            // setting stream:true.
            semantics: provider.cache_semantics(),
            seen: None,
        },
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

/// Relay upstream SSE chunks to the client while metering usage. Runs to a single
/// exit point that settles the [`Settlement`] guard: observed cost on a
/// clean finish with a terminal usage chunk, otherwise the FULL reservation (never
/// under-charge). Enforces an idle timeout, a max total duration, and a bounded
/// line-reassembly buffer so a hostile or hung upstream cannot grief the gateway.
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
) {
    let mut stream = upstream.bytes_stream();
    let mut transport_eof = false;
    let deadline = tokio::time::Instant::now() + STREAM_MAX_DURATION;

    loop {
        let wait_until = (tokio::time::Instant::now() + STREAM_IDLE_TIMEOUT).min(deadline);
        match tokio::time::timeout_at(wait_until, stream.next()).await {
            // Idle timeout or max-duration hit: abnormal end.
            Err(_) => {
                tracing::warn!("stream aborted: idle timeout or max duration exceeded");
                break;
            }
            // Upstream finished normally.
            Ok(None) => {
                transport_eof = true;
                break;
            }
            // Upstream stream error: abnormal end.
            Ok(Some(Err(e))) => {
                tracing::warn!(detail = %e.to_string(), "stream upstream error mid-body");
                break;
            }
            Ok(Some(Ok(chunk))) => {
                meter.observe(&chunk);
                // Nothing billable follows Anthropic's message_stop, so exit
                // there rather than holding the upstream connection and the
                // reservation open until the idle timeout fires.
                let finished = meter.finished();
                // Relay downstream, bounded by the SAME idle/duration deadline as
                // the upstream read: a client that stops reading fills the bounded
                // channel and would otherwise park this task (and hold the
                // reservation and upstream connection) indefinitely. On send error
                // (client gone) or timeout, end abnormally and charge the full
                // reservation.
                match tokio::time::timeout_at(wait_until, tx.send(Ok(chunk))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        tracing::debug!("stream client disconnected");
                        break;
                    }
                    Err(_) => {
                        tracing::warn!(
                            "stream aborted: client too slow to drain (idle/max duration)"
                        );
                        break;
                    }
                }
                if finished {
                    transport_eof = true;
                    break;
                }
            }
        }
    }

    let clean = meter.is_clean(transport_eof);
    let seen_usage = meter.usage();
    if clean && seen_usage.is_none() {
        tracing::warn!("stream ended cleanly without any usage; charging reservation");
    }
    // Anthropic delivers `overloaded_error` as an event on an otherwise-200
    // stream, BEFORE message_start, so nothing was generated and nothing billed.
    // Charging the full worst-case reservation there is punitive, and SDK
    // retries multiply it through an overload window, so charge the reserved
    // PROMPT leg instead.
    //
    // Strictly Anthropic-only, and that restriction is load-bearing. On the
    // OpenAI path `started` can only become true when the TERMINAL usage chunk
    // arrives, so a client that reads an entire response and then disconnects
    // before that chunk looks identical to one that never started. Applying
    // this branch there would hand out a full generation for the price of the
    // prompt, on demand and repeatably. Every other abnormal end charges the
    // full reservation.
    let (actual, usage, decision) = match &meter {
        StreamMeter::Anthropic { .. } if !clean && !meter.started() => {
            tracing::warn!(
                "anthropic stream failed before generating anything; charging the prompt leg"
            );
            (
                prompt_floor_micros.min(reserve_micros).max(1),
                Usage::default(),
                "estimated",
            )
        }
        _ => stream_settlement(clean, seen_usage, &price, reserve_micros),
    };
    guard.settle(actual, usage, decision).await;
}

/// Per-provider stream metering, so the relay stays protocol-agnostic.
///
/// The two protocols disagree about what "finished" means, and getting that
/// wrong is a silent under-charge. OpenAI's usage arrives in a terminal chunk,
/// so transport EOF is a fair proxy for completion. Anthropic's does not:
/// `message_start` carries a placeholder `output_tokens: 1`, an error event
/// arrives on an otherwise-healthy 200 stream and is followed by a normal close,
/// and nothing billable follows `message_stop`.
pub enum StreamMeter {
    OpenAi {
        line_buf: Vec<u8>,
        semantics: CacheSemantics,
        seen: Option<Usage>,
    },
    Anthropic {
        line_buf: Vec<u8>,
        meter: crate::providers::AnthropicStreamMeter,
    },
}

impl StreamMeter {
    fn observe(&mut self, chunk: &[u8]) {
        match self {
            Self::OpenAi {
                line_buf,
                semantics,
                seen,
            } => {
                if let Some(u) = scan_sse_for_usage(line_buf, chunk, *semantics) {
                    *seen = Some(u);
                }
            }
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

    /// Whether the stream genuinely completed. `transport_eof` is only a proxy,
    /// and only a sound one for OpenAI.
    fn is_clean(&self, transport_eof: bool) -> bool {
        match self {
            Self::OpenAi { .. } => transport_eof,
            Self::Anthropic { meter, .. } => meter.is_clean(),
        }
    }

    /// Whether the relay can stop reading now. Only Anthropic has an explicit
    /// terminal event; OpenAI is finished when the transport says so.
    fn finished(&self) -> bool {
        match self {
            Self::OpenAi { .. } => false,
            Self::Anthropic { meter, .. } => meter.is_clean(),
        }
    }

    fn started(&self) -> bool {
        match self {
            Self::OpenAi { seen, .. } => seen.is_some(),
            Self::Anthropic { meter, .. } => meter.started(),
        }
    }

    fn usage(&self) -> Option<Usage> {
        match self {
            Self::OpenAi { seen, .. } => *seen,
            Self::Anthropic { meter, .. } => meter.usage(),
        }
    }
}

/// Append a chunk to the SSE line-reassembly buffer and return the usage from the
/// most recent complete `data:` line that carried one (usage can arrive split
/// across chunk boundaries). Complete lines are drained; a partial line longer
/// than [`MAX_SSE_LINE_BUFFER`] is dropped so a newline-starved upstream cannot
/// grow the buffer without bound (worst case: the usage chunk is missed and the
/// caller settles the full reservation).
///
/// `semantics` must be the upstream's, matching what the buffered path uses.
fn scan_sse_for_usage(
    line_buf: &mut Vec<u8>,
    chunk: &[u8],
    semantics: CacheSemantics,
) -> Option<Usage> {
    line_buf.extend_from_slice(chunk);
    let mut found = None;
    while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = line_buf.drain(..=pos).collect();
        if let Ok(s) = std::str::from_utf8(&line) {
            if let Some(u) = usage_from_sse_data(s.trim_end(), semantics) {
                found = Some(u);
            }
        }
    }
    if line_buf.len() > MAX_SSE_LINE_BUFFER {
        line_buf.clear();
    }
    found
}

/// Decide how to settle a finished stream. A clean finish with a metered terminal
/// chunk charges the observed cost (floored to 1 micro). A clean finish with no
/// usage chunk, or ANY abnormal end (disconnect, upstream error, idle/duration
/// timeout), charges the FULL reservation so a stream is never under-charged.
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
        // this; the terminal usage chunk is the same untrusted input, and it
        // reaches this function through a client-selected code path, so a caller
        // could otherwise pick the unguarded one by setting stream:true.
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
        (true, Some(usage)) => (price.cost_micros(usage).max(1), usage, "allowed"),
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
async fn console_usage(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let core = state.core.load_full();
    if core.authenticate(&headers).await.is_none() {
        return unauthorized();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_sse_detects_usage_split_across_chunk_boundaries() {
        let mut buf = Vec::new();
        // The terminal usage line arrives split across two upstream chunks with no
        // newline in the first: no usage yet, buffer holds the partial.
        assert!(
            scan_sse_for_usage(
                &mut buf,
                b"data: {\"choices\":[],\"usage\":{\"prompt_tokens",
                CacheSemantics::Inclusive
            )
            .is_none()
        );
        let u = scan_sse_for_usage(
            &mut buf,
            b"\":5,\"completion_tokens\":2}}\n",
            CacheSemantics::Inclusive,
        )
        .unwrap();
        assert_eq!(u.input_tokens, 5);
        assert_eq!(u.output_tokens, 2);
        assert!(buf.is_empty(), "completed line should be drained");
    }

    #[test]
    fn scan_sse_ignores_deltas_done_and_bounds_the_buffer() {
        let mut buf = Vec::new();
        let sem = CacheSemantics::Inclusive;
        assert!(
            scan_sse_for_usage(&mut buf, b"data: {\"choices\":[{\"delta\":{}}]}\n", sem).is_none()
        );
        assert!(scan_sse_for_usage(&mut buf, b"data: [DONE]\n", sem).is_none());
        assert!(buf.is_empty());
        // A single line larger than the cap with no newline is dropped, so a
        // newline-starved upstream cannot grow the buffer without bound.
        let big = vec![b'x'; MAX_SSE_LINE_BUFFER + 10];
        assert!(scan_sse_for_usage(&mut buf, &big, sem).is_none());
        assert!(buf.is_empty(), "oversized partial line must be dropped");
    }

    #[test]
    fn stream_settlement_never_undercharges() {
        // $1 per million tokens on both legs => 1 micro/token.
        let price = ModelPrice::new("openai", "m", 1_000_000, 1_000_000);
        let reserve = 999;

        // Clean finish with usage: charge the observed cost (15 tokens => 15 micros).
        let (actual, _u, decision) =
            stream_settlement(true, Some(Usage::new(10, 5)), &price, reserve);
        assert_eq!(actual, 15);
        assert_eq!(decision, "allowed");

        // Clean finish, no usage chunk: charge the full reservation, not zero,
        // and label it estimated because nothing was measured.
        let (actual, _u, decision) = stream_settlement(true, None, &price, reserve);
        assert_eq!(actual, reserve);
        assert_eq!(decision, "estimated");

        // Abnormal end (even with partial usage seen): charge the full reservation.
        let (actual, _u, decision) =
            stream_settlement(false, Some(Usage::new(10, 5)), &price, reserve);
        assert_eq!(actual, reserve);
        assert_eq!(decision, "estimated");
    }

    #[test]
    fn openai_stream_cut_before_the_usage_chunk_still_charges_the_reservation() {
        // A client can read an entire response and disconnect before the
        // terminal usage chunk. On the OpenAI path that is indistinguishable
        // from a stream that never started, because `started` only becomes true
        // when that chunk arrives.
        //
        // An earlier version applied Anthropic's "nothing was generated, charge
        // the prompt leg" rule to both providers, which handed out a full
        // generation for a token of budget, on demand and repeatably. Every
        // abnormal end on this path charges the full reservation.
        let price = ModelPrice::new("openai", "m", 1_000_000, 1_000_000);
        let reserve = 999;
        let (actual, _u, decision) = stream_settlement(false, None, &price, reserve);
        assert_eq!(
            actual, reserve,
            "a stream cut before its usage chunk must not be cheap"
        );
        assert_eq!(decision, "estimated");
    }

    #[test]
    fn stream_settlement_rejects_an_implausible_terminal_chunk() {
        // The buffered path guards this in gateway::evaluate. The streaming path
        // is selected by the CLIENT (stream:true), so leaving it unguarded would
        // let a caller pick the code path where a hostile terminal chunk
        // saturates the cost to i64::MAX and pins a budget counter that nothing
        // can lower. Charge the reservation and record an error instead.
        let price = ModelPrice::new("openai", "m", 1_000_000, 1_000_000);
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
}
