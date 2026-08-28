// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! `/healthz` - liveness. Always returns 200 if the process is running.
//! `/readyz`  - readiness. Returns 200 only when DB and Redis are reachable.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;
use sqlx::PgPool;

use crate::app::AppState;

#[derive(Serialize)]
pub struct HealthBody {
    pub status: &'static str,
    pub version: &'static str,
}

pub async fn live() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(HealthBody {
            status: "ok",
            version: env!("CARGO_PKG_VERSION"),
        }),
    )
}

#[derive(Serialize)]
pub struct ReadyBody {
    pub status: &'static str,
    pub checks: ReadyChecks,
}

#[derive(Serialize)]
pub struct ReadyChecks {
    pub postgres: CheckResult,
    pub redis: CheckResult,
}

#[derive(Serialize)]
#[serde(tag = "status")]
#[serde(rename_all = "lowercase")]
pub enum CheckResult {
    Ok,
    // No error detail is serialized: sqlx/redis errors can embed the private DB
    // host or IP, and /readyz may be exposed publicly. The cause is logged
    // server-side instead.
    Fail,
}

pub async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let postgres = check_postgres(&state.db).await;
    let redis = check_redis(&state.redis).await;

    let all_ok = matches!(postgres, CheckResult::Ok) && matches!(redis, CheckResult::Ok);
    let status_code = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status_code,
        Json(ReadyBody {
            status: if all_ok { "ready" } else { "not_ready" },
            checks: ReadyChecks { postgres, redis },
        }),
    )
}

async fn check_postgres(pool: &PgPool) -> CheckResult {
    // `SELECT 1` returns INT4 in Postgres; bind it to i32 to match.
    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(pool)
        .await
    {
        Ok(_) => CheckResult::Ok,
        Err(err) => {
            tracing::warn!(error = %err, "readiness: postgres check failed");
            CheckResult::Fail
        }
    }
}

async fn check_redis(client: &redis::aio::ConnectionManager) -> CheckResult {
    let mut conn = client.clone();
    match redis::cmd("PING").query_async::<String>(&mut conn).await {
        Ok(_) => CheckResult::Ok,
        Err(err) => {
            tracing::warn!(error = %err, "readiness: redis check failed");
            CheckResult::Fail
        }
    }
}
