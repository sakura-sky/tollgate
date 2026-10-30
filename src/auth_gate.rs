// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Authentication before anything else, on every gated path.
//!
//! One middleware wraps the whole router (outside routing, so it also sees
//! unknown paths and wrong methods) and authenticates every request under
//! `/v1` and `/console/` before any route, extractor or handler runs. Without a
//! valid key, what a caller observes depends only on the path's surface and on
//! whether the key store answered: never on which providers are enabled, the
//! admission mode, prices, budgets, the method, the sub-path or the body.
//!
//! - **401** per surface when the key is missing, malformed or wrong.
//! - **503 `backend_error`** per surface when the key store cannot answer, or
//!   does not answer within the key-lookup deadline. Never a 401: SDKs do not
//!   retry a 401, and an operator reads a wave of them as a credential problem
//!   rather than a database one.
//!
//! `/healthz`, `/readyz`, `/metrics` and the `/console` page are public by
//! design and pass straight through.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::body::{Body, HttpBody};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::gateway::{AuthError, GatewayCore, KEY_HEADER, Outcome, outcome_response};

const REASON_HEADER: &str = "x-tollgate-reason";

/// How long after the key store's own timeout the gate gives up. The store's
/// typed error (`pool_timed_out`, a `statement_timeout` cancel) normally
/// arrives first and is the one logged; this margin keeps the gate's generic
/// `deadline` for a store that does not answer at all.
pub const DEADLINE_MARGIN: Duration = Duration::from_millis(250);

/// How often a store failure is logged. Every failure is counted on
/// `tollgate_requests_total{decision="key_store_error"}`; the log line is
/// rate-limited because unauthenticated callers can trigger it.
const STORE_ERROR_LOG_WINDOW: Duration = Duration::from_secs(60);

/// Which response family a gated path belongs to. It decides the shape of the
/// 401 and 503, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Surface {
    /// `/v1/messages` and everything under it: Anthropic's error envelope, and
    /// the only surface where `x-api-key` is read as a Tollgate key.
    Anthropic,
    /// `/v1/chat/completions`: OpenAI's error envelope.
    OpenAi,
    /// Every other path under `/v1`: the original enveloped API.
    Legacy,
    /// Everything under `/console/` (not the `/console` page itself).
    Console,
}

/// Classify a request path, or `None` for a public path.
///
/// Uses the raw path, the same string the router matches, so classification
/// and routing cannot disagree. `/v1/messagesx` is `Legacy` and `/v1x` is
/// public (no route matches it, so it is a 404 like any unknown path).
#[must_use]
pub fn surface_for(path: &str) -> Option<Surface> {
    if path == "/v1/messages" || path.starts_with("/v1/messages/") {
        Some(Surface::Anthropic)
    } else if path == "/v1/chat/completions" {
        Some(Surface::OpenAi)
    } else if path == "/v1" || path.starts_with("/v1/") {
        Some(Surface::Legacy)
    } else if path.starts_with("/console/") {
        Some(Surface::Console)
    } else {
        None
    }
}

/// The authenticated caller, inserted into the request by the gate. Gated
/// handlers take it as an extractor, so if the gate were ever removed they
/// would fail closed (axum answers a missing extension with a 500) rather than
/// run unauthenticated.
#[derive(Debug, Clone)]
pub struct Authed {
    pub key_id: String,
    /// Time spent authenticating, added to the handler's overhead figures so
    /// they still include authentication without including the body upload
    /// that happens between the gate and the handler.
    pub auth_elapsed: Duration,
}

/// Logs at most one line per window, carrying the count of the ones it
/// skipped. Lock-free; the counts are approximate under contention, and the
/// last window's skipped count is only reported by the next failure. The
/// `key_store_error` metric is the authoritative count.
#[derive(Debug)]
pub struct ErrorLogLimiter {
    origin: Instant,
    window_ms: u64,
    next_at_ms: AtomicU64,
    suppressed: AtomicU64,
}

impl ErrorLogLimiter {
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            origin: Instant::now(),
            window_ms: u64::try_from(window.as_millis()).unwrap_or(u64::MAX),
            next_at_ms: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
        }
    }

    /// `Some(skipped)` if the caller should log now, with the number of lines
    /// skipped since the last one; `None` if this one is skipped.
    pub fn admit(&self, now: Instant) -> Option<u64> {
        let now_ms = u64::try_from(now.saturating_duration_since(self.origin).as_millis())
            .unwrap_or(u64::MAX);
        let next = self.next_at_ms.load(Ordering::Relaxed);
        if now_ms >= next
            && self
                .next_at_ms
                .compare_exchange(
                    next,
                    now_ms.saturating_add(self.window_ms),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            Some(self.suppressed.swap(0, Ordering::Relaxed))
        } else {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// What the gate needs: the live core (for the key store, hasher and metrics),
/// the key-lookup deadline and the log limiter.
#[derive(Clone)]
pub struct GateState {
    pub core: Arc<ArcSwap<GatewayCore>>,
    pub deadline: Duration,
    pub store_errors: Arc<ErrorLogLimiter>,
}

impl GateState {
    /// `acquire_timeout` is `database.acquire_timeout`, which also bounds the
    /// key pool's acquire and its `statement_timeout`.
    #[must_use]
    pub fn new(core: Arc<ArcSwap<GatewayCore>>, acquire_timeout: Duration) -> Self {
        Self {
            core,
            deadline: acquire_timeout.saturating_add(DEADLINE_MARGIN),
            store_errors: Arc::new(ErrorLogLimiter::new(STORE_ERROR_LOG_WINDOW)),
        }
    }

    fn log_store_error(&self, kind: &str, sqlstate: Option<&str>) {
        if let Some(skipped) = self.store_errors.admit(Instant::now()) {
            // Fixed fields only: nothing from the request reaches this line.
            tracing::error!(
                kind,
                sqlstate = sqlstate.unwrap_or(""),
                skipped_since_last_line = skipped,
                "api key lookup failed; answering 503 until the key store recovers"
            );
        }
    }
}

/// The gate. See the module docs.
pub async fn require_key(State(gate): State<GateState>, mut req: Request, next: Next) -> Response {
    let Some(surface) = surface_for(req.uri().path()) else {
        return next.run(req).await;
    };
    let started = Instant::now();

    // Anthropic's SDK sends its key as `x-api-key`. Read it as a Tollgate key on
    // this surface only, by copying it into the header the gateway reads, and
    // only when that header is absent. Widening `presented_key` instead would
    // accept it on every route.
    if surface == Surface::Anthropic && !req.headers().contains_key(KEY_HEADER) {
        if let Some(v) = req.headers().get("x-api-key").cloned() {
            req.headers_mut().insert(KEY_HEADER, v);
        }
    }

    let core = gate.core.load_full();
    let result = tokio::time::timeout(gate.deadline, core.authenticate(req.headers())).await;
    match result {
        Ok(Ok(key_id)) => {
            let is_head = req.method() == Method::HEAD;
            req.extensions_mut().insert(Authed {
                key_id,
                auth_elapsed: started.elapsed(),
            });
            let response = next.run(req).await;
            if surface == Surface::Console {
                label_console_refusal(response, is_head)
            } else {
                response
            }
        }
        // Counted inside `authenticate`.
        Ok(Err(AuthError::Unauthenticated)) => unauthenticated(surface),
        Ok(Err(AuthError::StoreUnavailable(e))) => {
            gate.log_store_error(e.kind, e.code.as_deref());
            store_unavailable(surface)
        }
        // The dropped future could not count itself, so count it here.
        Err(_elapsed) => {
            core.metrics.observe_key_store_error();
            gate.log_store_error("deadline", None);
            store_unavailable(surface)
        }
    }
}

/// The 401 for a surface. Byte-identical to what each route returned when it
/// authenticated for itself, except that the console now carries the reason
/// header like every other refusal.
#[must_use]
pub fn unauthenticated(surface: Surface) -> Response {
    match surface {
        Surface::Anthropic => crate::app::anthropic_outcome_response(Outcome::Unauthenticated),
        Surface::OpenAi => crate::app::openai_outcome_response(Outcome::Unauthenticated),
        Surface::Legacy => outcome_response(Outcome::Unauthenticated),
        Surface::Console => crate::app::console_unauthorized(),
    }
}

/// The 503 for a surface when the key store cannot answer.
#[must_use]
pub fn store_unavailable(surface: Surface) -> Response {
    // The detail is never shown to the caller; the gate logs it.
    let backend = || Outcome::BackendError(String::new());
    match surface {
        Surface::Anthropic => crate::app::anthropic_outcome_response(backend()),
        Surface::OpenAi => crate::app::openai_outcome_response(backend()),
        Surface::Legacy => outcome_response(backend()),
        Surface::Console => crate::app::console_unavailable(),
    }
}

/// Give every console refusal an `x-tollgate-reason`, including the ones axum
/// and tower-http generate (404, 405, the request-timeout 503), which carry
/// none. Never reads or buffers a body.
///
/// The request timeout's 503 has an empty body and, because axum's route future
/// sets it, `content-length: 0`. That body is replaced with the console's JSON,
/// so the stale `content-length` is removed: hyper trusts the header, and a
/// mismatch would corrupt a kept-alive connection. A HEAD response keeps its
/// empty body and only gains the header.
fn label_console_refusal(mut response: Response, is_head: bool) -> Response {
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error())
        || response.headers().contains_key(REASON_HEADER)
    {
        return response;
    }
    let reason = if status.is_server_error() {
        "backend_error"
    } else {
        "bad_request"
    };
    if status == StatusCode::SERVICE_UNAVAILABLE
        && !is_head
        && response.body().size_hint().exact() == Some(0)
    {
        let headers = response.headers_mut();
        headers.remove(header::CONTENT_LENGTH);
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        *response.body_mut() = Body::from(crate::app::CONSOLE_UNAVAILABLE_BODY);
    }
    response
        .headers_mut()
        .insert(REASON_HEADER, HeaderValue::from_static(reason));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_is_classified_and_the_edges_are_pinned() {
        let cases = [
            ("/v1/messages", Some(Surface::Anthropic)),
            ("/v1/messages/count_tokens", Some(Surface::Anthropic)),
            ("/v1/messages/batches/x", Some(Surface::Anthropic)),
            ("/v1/messages/", Some(Surface::Anthropic)),
            ("/v1/messagesx", Some(Surface::Legacy)),
            ("/v1/chat/completions", Some(Surface::OpenAi)),
            ("/v1/chat/completions/", Some(Surface::Legacy)),
            ("/v1/anthropic/messages", Some(Surface::Legacy)),
            ("/v1/mock/generate", Some(Surface::Legacy)),
            ("/v1/models", Some(Surface::Legacy)),
            ("/v1", Some(Surface::Legacy)),
            ("/v1/", Some(Surface::Legacy)),
            ("/v1/%FF/x", Some(Surface::Legacy)),
            ("/console/budgets", Some(Surface::Console)),
            ("/console/usage", Some(Surface::Console)),
            ("/console/nosuch", Some(Surface::Console)),
            ("/console", None),
            ("/consolex", None),
            ("/v1x", None),
            ("/V1/messages", None),
            ("/healthz", None),
            ("/readyz", None),
            ("/metrics", None),
            ("/", None),
            ("", None),
        ];
        for (path, want) in cases {
            assert_eq!(surface_for(path), want, "{path:?}");
        }
    }

    #[test]
    fn the_log_limiter_admits_one_line_per_window_and_counts_the_rest() {
        let limiter = ErrorLogLimiter::new(Duration::from_secs(60));
        let t0 = limiter.origin;
        assert_eq!(limiter.admit(t0), Some(0));
        assert_eq!(limiter.admit(t0 + Duration::from_secs(1)), None);
        assert_eq!(limiter.admit(t0 + Duration::from_secs(59)), None);
        // The window has passed: log, reporting the two skipped.
        assert_eq!(limiter.admit(t0 + Duration::from_secs(60)), Some(2));
        assert_eq!(limiter.admit(t0 + Duration::from_secs(61)), None);
        assert_eq!(limiter.admit(t0 + Duration::from_secs(200)), Some(1));
        // A clock reading before the origin is treated as the origin.
        let early = ErrorLogLimiter::new(Duration::from_secs(60));
        let before = early.origin.checked_sub(Duration::from_secs(5));
        if let Some(before) = before {
            assert_eq!(early.admit(before), Some(0));
        }
    }

    #[test]
    fn console_refusals_gain_a_reason_and_the_timeout_body_is_replaced_safely() {
        // axum's own 404: empty body, gains `bad_request` only.
        let r = label_console_refusal(
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CONTENT_LENGTH, "0")
                .body(Body::empty())
                .unwrap(),
            false,
        );
        assert_eq!(r.headers()[REASON_HEADER], "bad_request");
        assert_eq!(r.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(r.body().size_hint().exact(), Some(0));

        // The request timeout's 503: replaced, with no stale content-length.
        let r = label_console_refusal(
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header(header::CONTENT_LENGTH, "0")
                .body(Body::empty())
                .unwrap(),
            false,
        );
        assert_eq!(r.headers()[REASON_HEADER], "backend_error");
        assert!(r.headers().get(header::CONTENT_LENGTH).is_none());
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            r.body().size_hint().exact(),
            Some(crate::app::CONSOLE_UNAVAILABLE_BODY.len() as u64)
        );

        // The same on HEAD: header only, body left empty.
        let r = label_console_refusal(
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header(header::CONTENT_LENGTH, "0")
                .body(Body::empty())
                .unwrap(),
            true,
        );
        assert_eq!(r.headers()[REASON_HEADER], "backend_error");
        assert_eq!(r.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(r.body().size_hint().exact(), Some(0));

        // A refusal that already has a reason is left alone, body included.
        let r = label_console_refusal(crate::app::console_unavailable(), false);
        assert_eq!(r.headers()[REASON_HEADER], "backend_error");

        // A success is left alone.
        let r = label_console_refusal(
            Response::builder()
                .status(StatusCode::OK)
                .body(Body::empty())
                .unwrap(),
            false,
        );
        assert!(r.headers().get(REASON_HEADER).is_none());
    }
}
