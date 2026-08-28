// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Zero-infra demo mode: a self-contained Tollgate that needs no Postgres,
//! Redis, or cloud credentials.
//!
//! `tollgate demo` boots the shared [`GatewayCore`] with in-memory backends and
//! the built-in mock provider, preloads one API key and two budgets, and serves
//! on loopback. It demonstrates the whole value loop (authenticate, price by
//! tokens, reserve, forward, meter, settle, and hard-stop before overspend) end
//! to end. Production wires the same core to Postgres + Redis/Valkey.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde_json::json;
use tokio::net::TcpListener;

use crate::apikey::KeyHasher;
use crate::budget::{Budget, Budgets, Period, Scope};
use crate::config::Config;
use crate::gateway::{GatewayCore, KEY_HEADER, KeyRecord, MemKeyStore, MemUsageSink, Outcome};
use crate::pricing::{ModelPrice, PriceBook, format_micros};
use crate::provider::{MockProvider, Provider};

/// In-memory demo state: the shared core plus handles for inspection endpoints.
pub struct DemoState {
    core: GatewayCore,
    budgets: Arc<Budgets>,
    usage: Arc<MemUsageSink>,
    unauthenticated: AtomicU64,
    demo_key_id: String,
    per_key_limit_micros: i64,
    global_limit_micros: i64,
    // Demo only: the preloaded plaintext key, used to seed the console so it
    // connects on load. The demo binds loopback and already prints this key.
    demo_plaintext_key: String,
}

struct Preloaded {
    state: Arc<DemoState>,
    plaintext_key: String,
}

fn preload() -> Preloaded {
    let hasher = KeyHasher::random();
    let generated = hasher.generate();
    let demo_key_id = uuid::Uuid::new_v4().to_string();

    let mut keys = MemKeyStore::new();
    keys.insert(
        generated.prefix.clone(),
        KeyRecord {
            id: demo_key_id.clone(),
            key_hash: generated.key_hash.clone(),
        },
    );

    // Mock model priced like a mid-tier model: 3.00 / 15.00 per 1M in/out.
    let prices = Arc::new(PriceBook::from_prices(vec![ModelPrice::new(
        "mock", "demo", 3_000_000, 15_000_000,
    )]));

    let per_key_limit_micros = 50_000; // 0.05, about 3 default requests
    let global_limit_micros = 1_000_000; // 1.00 backstop
    let budgets = Arc::new(Budgets::new(vec![
        Budget {
            scope: Scope::Global,
            period: Period::Monthly,
            limit_micros: global_limit_micros,
            hard_stop: true,
        },
        Budget {
            scope: Scope::ApiKey(demo_key_id.clone()),
            period: Period::Monthly,
            limit_micros: per_key_limit_micros,
            hard_stop: true,
        },
    ]));
    let usage = Arc::new(MemUsageSink::new());

    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    providers.insert("mock".to_owned(), Arc::new(MockProvider));

    let core = GatewayCore {
        dummy_hash: hasher.hash("tollgate-fixed-dummy-secret"),
        hasher,
        keys: Arc::new(keys),
        budgets: budgets.clone(),
        usage: usage.clone(),
        prices,
        providers,
        admission_exact: false,
    };

    Preloaded {
        state: Arc::new(DemoState {
            core,
            budgets,
            usage,
            unauthenticated: AtomicU64::new(0),
            demo_key_id,
            per_key_limit_micros,
            global_limit_micros,
            demo_plaintext_key: generated.plaintext.clone(),
        }),
        plaintext_key: generated.plaintext,
    }
}

async fn gateway(
    State(state): State<Arc<DemoState>>,
    Path((provider, rest)): Path<(String, String)>,
    headers: HeaderMap,
    body: String,
) -> axum::response::Response {
    match state.core.evaluate(&provider, &rest, &headers, &body).await {
        Outcome::Allowed {
            status,
            body,
            cost_micros,
            overhead_micros,
            key_id,
        } => {
            let key_spent = state
                .budgets
                .spent(&Scope::ApiKey(key_id), Period::Monthly, Utc::now());
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
                [("x-tollgate-overhead-us", overhead_micros.to_string())],
                Json(json!({
                    "response": body,
                    "tollgate": {
                        "cost": format_micros(cost_micros),
                        "overhead_us": overhead_micros,
                        "key_budget_spent": format_micros(key_spent),
                        "key_budget_limit": format_micros(state.per_key_limit_micros),
                        "key_budget_remaining": format_micros(state.per_key_limit_micros - key_spent),
                    }
                })),
            )
                .into_response()
        }
        Outcome::Unauthenticated => {
            state.unauthenticated.fetch_add(1, Ordering::Relaxed);
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid or missing API key", "header": KEY_HEADER})),
            )
                .into_response()
        }
        Outcome::BadRequest(m) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid request body: {m}")})),
        )
            .into_response(),
        Outcome::Unpriced { provider, model } => (
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({
                "error": "no price configured for this provider/model - request refused (fail closed)",
                "provider": provider,
                "model": model,
            })),
        )
            .into_response(),
        Outcome::BudgetDenied(d) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": "budget exceeded - request refused before reaching the provider",
                "detail": d.to_string(),
                "scope": d.scope,
                "limit": format_micros(d.limit_micros),
                "already_spent": format_micros(d.spent_micros),
                "this_request_would_add": format_micros(d.cost_micros),
            })),
        )
            .into_response(),
        Outcome::BackendError(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "gateway temporarily unavailable"})),
        )
            .into_response(),
        Outcome::Upstream(m) => {
            tracing::warn!(detail = %m, "upstream provider error");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": "upstream provider error"})),
            )
                .into_response()
        }
    }
}

async fn admin_usage(
    State(state): State<Arc<DemoState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if state.core.authenticate(&headers).await.is_none() {
        state.unauthenticated.fetch_add(1, Ordering::Relaxed);
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "the console requires a valid API key"})),
        )
            .into_response();
    }
    let events = state.usage.snapshot();
    let total: i64 = events.iter().map(|e| e.cost_micros).sum();
    Json(json!({
        "events": events,
        "total_cost": format_micros(total),
        "count": events.len(),
    }))
    .into_response()
}

async fn admin_budgets(
    State(state): State<Arc<DemoState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if state.core.authenticate(&headers).await.is_none() {
        state.unauthenticated.fetch_add(1, Ordering::Relaxed);
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "the console requires a valid API key"})),
        )
            .into_response();
    }
    let now = Utc::now();
    let global = state.budgets.spent(&Scope::Global, Period::Monthly, now);
    let key = state.budgets.spent(
        &Scope::ApiKey(state.demo_key_id.clone()),
        Period::Monthly,
        now,
    );
    let budget = |label: &str, spent: i64, limit: i64| {
        json!({
            "label": label,
            "period": "monthly",
            "spent": format_micros(spent),
            "limit": format_micros(limit),
            "remaining": format_micros(limit - spent),
            "hard_stop": true,
        })
    };
    Json(json!({
        "budgets": [
            budget("Per-key (demo)", key, state.per_key_limit_micros),
            budget("Global", global, state.global_limit_micros),
        ]
    }))
    .into_response()
}

/// The read-only web console. The demo seeds the preloaded key so the page
/// connects on load; the page itself calls the key-authenticated JSON endpoints.
async fn console(State(state): State<Arc<DemoState>>) -> impl IntoResponse {
    let boot = crate::console::demo_boot(&state.demo_plaintext_key);
    Html(crate::console::render(&boot))
}

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok", "mode": "demo"}))
}

async fn ready() -> impl IntoResponse {
    Json(json!({"status": "ready", "mode": "demo"}))
}

/// Prometheus text-format metrics for scraping (SRE monitoring). Production
/// exposes the same series backed by Redis/Postgres counters.
async fn metrics(State(state): State<Arc<DemoState>>) -> impl IntoResponse {
    let events = state.usage.snapshot();
    let (mut allowed, mut rejected, mut errors, mut unpriced, mut cost) =
        (0i64, 0i64, 0i64, 0i64, 0i64);
    for e in &events {
        match e.decision.as_str() {
            "allowed" => {
                allowed += 1;
                cost += e.cost_micros;
            }
            "rejected_budget" => rejected += 1,
            "error" => errors += 1,
            "unpriced" => unpriced += 1,
            _ => {}
        }
    }
    let unauthenticated = state.unauthenticated.load(Ordering::Relaxed);
    let now = Utc::now();
    let global_spent = state.budgets.spent(&Scope::Global, Period::Monthly, now);
    let key_spent = state.budgets.spent(
        &Scope::ApiKey(state.demo_key_id.clone()),
        Period::Monthly,
        now,
    );

    let body = format!(
        "# HELP tollgate_up 1 if the gateway is serving.\n\
         # TYPE tollgate_up gauge\n\
         tollgate_up 1\n\
         # HELP tollgate_requests_total Proxied requests by decision.\n\
         # TYPE tollgate_requests_total counter\n\
         tollgate_requests_total{{decision=\"allowed\"}} {allowed}\n\
         tollgate_requests_total{{decision=\"rejected_budget\"}} {rejected}\n\
         tollgate_requests_total{{decision=\"error\"}} {errors}\n\
         tollgate_requests_total{{decision=\"unpriced\"}} {unpriced}\n\
         tollgate_requests_total{{decision=\"unauthenticated\"}} {unauthenticated}\n\
         # HELP tollgate_cost_micros_total Total metered cost (currency micros).\n\
         # TYPE tollgate_cost_micros_total counter\n\
         tollgate_cost_micros_total {cost}\n\
         # HELP tollgate_budget_spent_micros Current-period spend by scope.\n\
         # TYPE tollgate_budget_spent_micros gauge\n\
         tollgate_budget_spent_micros{{scope=\"global\"}} {global_spent}\n\
         tollgate_budget_spent_micros{{scope=\"api_key\"}} {key_spent}\n\
         # HELP tollgate_budget_limit_micros Configured budget limit by scope.\n\
         # TYPE tollgate_budget_limit_micros gauge\n\
         tollgate_budget_limit_micros{{scope=\"global\"}} {global_limit}\n\
         tollgate_budget_limit_micros{{scope=\"api_key\"}} {key_limit}\n",
        global_limit = state.global_limit_micros,
        key_limit = state.per_key_limit_micros,
    );

    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

fn router(state: Arc<DemoState>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/metrics", get(metrics))
        .route("/console", get(console))
        .route("/console/usage", get(admin_usage))
        .route("/console/budgets", get(admin_budgets))
        .route("/v1/{provider}/{*rest}", post(gateway))
        .with_state(state)
}

/// Boot the demo gateway: preload state, print instructions, serve.
pub async fn serve(cfg: Config) -> Result<()> {
    let Preloaded {
        state,
        plaintext_key,
    } = preload();

    // Demo binds LOOPBACK only: it preloads a printed key and must never be
    // exposed on all interfaces. Only the configured port is honoured.
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), cfg.http.bind.port());
    let per_key = format_micros(state.per_key_limit_micros);
    let global = format_micros(state.global_limit_micros);
    let port = bind.port();

    println!("\nTollgate demo (in-memory, no Postgres/Redis/creds)");
    println!("Listening on http://{bind}  (loopback only)\n");
    println!("Open the live console (budgets, spend, hard-stop):");
    println!("    http://localhost:{port}/console\n");
    println!("Demo API key (send as header  {KEY_HEADER}: <key>):");
    println!("    {plaintext_key}\n");
    println!("Budgets (monthly, hard-stop):");
    println!("    global : {global}");
    println!("    per-key: {per_key}   (about 3 default requests before hard-stop)\n");
    println!("Send a request (repeat it; the 4th is rejected with HTTP 429):");
    println!(
        "    curl -s localhost:{port}/v1/mock/generate \\\n        -H 'x-tollgate-key: {plaintext_key}' \\\n        -H 'content-type: application/json' \\\n        -d '{{\"model\":\"demo\",\"prompt\":\"hello tollgate\",\"max_output_tokens\":1000}}'\n"
    );
    println!("Inspect (the console endpoints require the key):");
    println!("    curl -s -H 'x-tollgate-key: {plaintext_key}' localhost:{port}/console/budgets");
    println!("    curl -s -H 'x-tollgate-key: {plaintext_key}' localhost:{port}/console/usage");
    println!("Monitor (for your SRE stack):");
    println!("    curl -s localhost:{port}/healthz     # liveness ping");
    println!("    curl -s localhost:{port}/readyz      # readiness");
    println!("    curl -s localhost:{port}/metrics     # Prometheus metrics\n");

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(addr = %bind, "tollgate demo listening");
    axum::serve(listener, router(state))
        .await
        .context("demo server error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn admin_and_gateway_require_a_valid_key() {
        let Preloaded { state, .. } = preload();
        let app = router(state);

        // Admin endpoint without a key -> 401.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/console/budgets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Gateway without a key -> 401.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/mock/generate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
