// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! The shared gateway core: one request flow, storage-agnostic.
//!
//! [`GatewayCore::evaluate`] runs the whole hot path (authenticate, price by
//! tokens, reserve, forward, meter, settle) against three storage traits:
//! [`KeyStore`], [`BudgetBackend`], and [`UsageSink`]. The demo wires in-memory
//! backends; production wires Postgres + Redis/Valkey. The flow is identical, so
//! the logic covered by the demo's tests is exactly what production runs.
//!
//! ## Overspend safety (adapter contract)
//!
//! The core reserves `cost(estimated_input + max_output)` BEFORE forwarding and
//! settles the ACTUAL cost afterwards. This only prevents overspend if adapters
//! honour two invariants: `estimated_input_tokens` must not under-count, and the
//! upstream must not exceed `max_output_tokens`. Real adapters MUST enforce both
//! (see the provider tasks); the mock satisfies them by construction.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};

use crate::pricing::format_micros;

use crate::apikey::{self, KeyHasher};
use crate::budget::{BudgetDenied, Budgets, Period, RequestCtx, Reservation, Scope};
use crate::pricing::{PriceBook, Usage};
use crate::provider::{Provider, ProviderError};

/// The header carrying the API key.
pub const KEY_HEADER: &str = "x-tollgate-key";

/// The stored form of an API key: its id and the hash to verify against.
#[derive(Debug, Clone)]
pub struct KeyRecord {
    pub id: String,
    pub key_hash: String,
}

/// Looks up API keys by their public prefix.
#[async_trait]
pub trait KeyStore: Send + Sync {
    async fn lookup(&self, prefix: &str) -> Option<KeyRecord>;
}

/// Why a reservation failed: a real budget denial, or a backend (infra) error.
/// These map to different HTTP statuses (402 vs 503) and the backend error text
/// is never shown to the client.
#[derive(Debug)]
pub enum ReserveError {
    Denied(BudgetDenied),
    Backend(String),
}

impl From<BudgetDenied> for ReserveError {
    fn from(d: BudgetDenied) -> Self {
        ReserveError::Denied(d)
    }
}

/// Reserves and settles spend against budgets. `reserve` returns the offending
/// budget when a hard cap would be exceeded; `commit` releases or claims the
/// difference between the reserved and actual cost.
#[async_trait]
pub trait BudgetBackend: Send + Sync {
    async fn reserve(
        &self,
        ctx: &RequestCtx<'_>,
        reserve_micros: i64,
    ) -> Result<Reservation, ReserveError>;
    async fn commit(&self, reservation: &Reservation, actual_micros: i64);
}

/// One metered request, handed to the [`UsageSink`].
pub struct UsageEvent<'a> {
    pub key_id: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub usage: Usage,
    pub cost_micros: i64,
    pub decision: &'a str,
    /// Gateway overhead in microseconds: our admission path only, excluding the
    /// upstream provider call.
    pub overhead_micros: i64,
}

/// Records usage events (the spend ledger).
#[async_trait]
pub trait UsageSink: Send + Sync {
    async fn record(&self, event: UsageEvent<'_>);
}

/// The result of evaluating a request; the HTTP layer maps this to a response.
pub enum Outcome {
    Allowed {
        status: u16,
        body: Value,
        cost_micros: i64,
        overhead_micros: i64,
        key_id: String,
    },
    Unauthenticated,
    BadRequest(String),
    Unpriced {
        provider: String,
        model: String,
    },
    BudgetDenied(BudgetDenied),
    /// A backend (database or cache) failure. Maps to 503; the internal detail
    /// is logged, never returned to the client.
    BackendError(String),
    Upstream(String),
}

/// Map an [`Outcome`] to a standard HTTP response. Both the demo and production
/// use this so the API surface is identical.
#[must_use]
pub fn outcome_response(outcome: Outcome) -> Response {
    match outcome {
        Outcome::Allowed {
            status,
            body,
            cost_micros,
            overhead_micros,
            ..
        } => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            [("x-tollgate-overhead-us", overhead_micros.to_string())],
            Json(json!({
                "response": body,
                "tollgate": {
                    "cost": format_micros(cost_micros),
                    "overhead_us": overhead_micros,
                }
            })),
        )
            .into_response(),
        Outcome::Unauthenticated => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid or missing API key", "header": KEY_HEADER})),
        )
            .into_response(),
        Outcome::BadRequest(m) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid request body: {m}")})),
        )
            .into_response(),
        // Unknown/unpriced provider or model: the caller targeted something we
        // can't route or price. 400, not 402 (which we reserve for budget).
        Outcome::Unpriced { provider, model } => (
            StatusCode::BAD_REQUEST,
            [("x-tollgate-reason", "unpriced")],
            Json(json!({
                "error": "no price configured for this provider/model - request refused (fail closed)",
                "provider": provider,
                "model": model,
            })),
        )
            .into_response(),
        // Budget exhausted: 402 Payment Required. It is the "out of budget,
        // retrying will not help" signal, and unlike 429 it is not auto-retried
        // by provider SDKs (429 is reserved for real rate limiting).
        Outcome::BudgetDenied(d) => (
            StatusCode::PAYMENT_REQUIRED,
            [("x-tollgate-reason", "budget_exceeded")],
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
            // Log the detail server-side; do not echo it (it can contain the
            // upstream URL: GCP project/region/model or internal host names).
            tracing::warn!(detail = %m, "upstream provider error");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": "upstream provider error"})),
            )
                .into_response()
        }
    }
}

/// The storage-agnostic gateway.
pub struct GatewayCore {
    pub hasher: KeyHasher,
    pub dummy_hash: String,
    pub keys: Arc<dyn KeyStore>,
    pub budgets: Arc<dyn BudgetBackend>,
    pub usage: Arc<dyn UsageSink>,
    pub prices: Arc<PriceBook>,
    pub providers: HashMap<String, Arc<dyn Provider>>,
    /// When true (`exact` admission), reserve against the provider's exact
    /// pre-flight token count; otherwise use the fast parse-time estimate.
    pub admission_exact: bool,
}

/// Extract the presented Tollgate key from either the `x-tollgate-key` header or
/// an `Authorization: Bearer <key>` header (what OpenAI-style clients, including
/// ADK/LiteLLM, send by default).
fn presented_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get(KEY_HEADER).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_owned());
        }
    }
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let bearer = auth
        .strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))?
        .trim();
    (!bearer.is_empty()).then(|| bearer.to_owned())
}

impl GatewayCore {
    /// Resolve the caller's key id from the request headers, or `None`.
    pub async fn authenticate(&self, headers: &HeaderMap) -> Option<String> {
        let presented = presented_key(headers)?;
        let (prefix, secret) = apikey::parse(&presented).ok()?;
        match self.keys.lookup(&prefix).await {
            Some(k) if self.hasher.verify(&secret, &k.key_hash) => Some(k.id),
            Some(_) => None,
            None => {
                // One verify against a fixed dummy hash, matching the hit path.
                let _ = self.hasher.verify(&secret, &self.dummy_hash);
                None
            }
        }
    }

    /// Run the full request flow. Records a usage event for every terminal
    /// decision except unauthenticated (which never resolves to a key).
    pub async fn evaluate(
        &self,
        provider_id: &str,
        rest_path: &str,
        headers: &HeaderMap,
        body: &str,
    ) -> Outcome {
        // Measure the gateway's own overhead: total time in this method minus the
        // time awaiting the provider (pre-flight token count and the forward).
        let started = std::time::Instant::now();
        let mut provider_micros: u128 = 0;
        let Some(key_id) = self.authenticate(headers).await else {
            return Outcome::Unauthenticated;
        };
        let Some(provider) = self.providers.get(provider_id) else {
            self.record(
                &key_id,
                provider_id,
                "",
                Usage::default(),
                0,
                "unpriced",
                started,
                provider_micros,
            )
            .await;
            return Outcome::Unpriced {
                provider: provider_id.to_owned(),
                model: String::new(),
            };
        };
        let parsed = match provider.parse_request(rest_path, body) {
            Ok(p) => p,
            Err(ProviderError::BadRequest(m)) => return Outcome::BadRequest(m),
            // Nothing is reserved or forwarded yet, so neither of these can have
            // been billed. MeteringFailed from a parser would be an adapter bug;
            // it is handled rather than ignored so the match stays exhaustive if
            // a future parser starts returning it.
            Err(ProviderError::Upstream(m) | ProviderError::MeteringFailed(m)) => {
                return Outcome::Upstream(m);
            }
        };
        // In fast admission, refuse requests that reference external media: their
        // token cost is not bounded by body size, so the reservation would be far
        // too low. Such requests must use exact admission (which counts them).
        if !self.admission_exact && crate::providers::references_external_media(body) {
            return Outcome::BadRequest(
                "requests referencing external media (fileUri/file_id/url) require exact admission"
                    .to_owned(),
            );
        }
        // Price the model. Unpriced means fail closed, never free.
        let Some(price) = self.prices.lookup(provider_id, &parsed.model).cloned() else {
            self.record(
                &key_id,
                provider_id,
                &parsed.model,
                Usage::default(),
                0,
                "unpriced",
                started,
                provider_micros,
            )
            .await;
            return Outcome::Unpriced {
                provider: provider_id.to_owned(),
                model: parsed.model,
            };
        };
        // Input tokens for the reservation: exact pre-flight count if configured,
        // else the fast parse-time (over-)estimate. Either way we settle to the
        // provider's exact reported usage after the response.
        let input_tokens = if self.admission_exact {
            // Fail closed: if the exact count fails, do NOT fall back to the weak
            // estimate (that would reopen the under-reservation hole). The count is
            // a provider round trip, so its time is not our overhead.
            let c0 = std::time::Instant::now();
            let counted = provider.count_input_tokens(rest_path, body, &parsed).await;
            provider_micros += c0.elapsed().as_micros();
            match counted {
                Ok(n) => n,
                Err(e) => {
                    tracing::error!(error = %e.to_string(), "exact token count failed");
                    return Outcome::BackendError(e.to_string());
                }
            }
        } else {
            parsed.estimated_input_tokens
        };
        // Reserve the worst case, floored to 1 micro so nothing meters as zero.
        // The prompt leg is reserved at the most expensive rate that could apply
        // to it, since we cannot know before forwarding how the prompt will split
        // across fresh, cache-read and cache-write tokens.
        let reserve_profile = provider.prompt_reserve_profile(&parsed);
        let reserve = price
            .reserve_micros(input_tokens, parsed.max_output_tokens, reserve_profile)
            .max(1);
        let ctx = RequestCtx {
            key_id: &key_id,
            provider: provider_id,
            model: &parsed.model,
        };
        let reservation = match self.budgets.reserve(&ctx, reserve).await {
            Ok(r) => r,
            Err(ReserveError::Denied(denied)) => {
                self.record(
                    &key_id,
                    provider_id,
                    &parsed.model,
                    Usage::default(),
                    0,
                    "rejected_budget",
                    started,
                    provider_micros,
                )
                .await;
                return Outcome::BudgetDenied(denied);
            }
            Err(ReserveError::Backend(msg)) => {
                tracing::error!(error = %msg, "budget backend error on reserve");
                self.record(
                    &key_id,
                    provider_id,
                    &parsed.model,
                    Usage::default(),
                    0,
                    "error",
                    started,
                    provider_micros,
                )
                .await;
                return Outcome::BackendError(msg);
            }
        };
        // Arm the settle guard IMMEDIATELY after reserving, before anything that
        // can be cancelled. axum drops this future when the HTTP timeout fires or
        // the client disconnects, and a buffered LLM call routinely outruns that
        // timeout, so without the guard the reservation below would be stranded
        // in every counter until the period rolled over, with no ledger row.
        // Nothing between here and `settle` may return early without going
        // through the guard.
        let guard = Settlement::new(
            self,
            reservation,
            reserve,
            key_id.clone(),
            provider_id.to_owned(),
            parsed.model.clone(),
            i64::try_from(
                started
                    .elapsed()
                    .as_micros()
                    .saturating_sub(provider_micros),
            )
            .unwrap_or(i64::MAX),
        );
        // Forward. On error, release the whole reservation. The forward is the
        // upstream call, so its duration is excluded from our overhead.
        let f0 = std::time::Instant::now();
        let forwarded = provider.forward(rest_path, body, &parsed).await;
        provider_micros += f0.elapsed().as_micros();
        let overhead_now = |provider_micros: u128| {
            i64::try_from(
                started
                    .elapsed()
                    .as_micros()
                    .saturating_sub(provider_micros),
            )
            .unwrap_or(i64::MAX)
        };
        let resp = match forwarded {
            Ok(r) => r,
            // Never reached the provider: connect, DNS, TLS, or a failure
            // building the call. Nothing was billed, so release it all.
            Err(e @ ProviderError::Upstream(_) | e @ ProviderError::BadRequest(_)) => {
                let msg = e.to_string();
                guard
                    .settle_with_overhead(
                        0,
                        Usage::default(),
                        "error",
                        overhead_now(provider_micros),
                    )
                    .await;
                return Outcome::Upstream(msg);
            }
            // May well have been served and billed, but cannot be metered: a
            // timeout after the request was sent, or a 2xx that will not parse.
            // Releasing here would under-charge a request the provider is
            // invoicing, so charge the reservation and say so in the ledger.
            Err(ProviderError::MeteringFailed(msg)) => {
                tracing::error!(
                    provider = provider_id,
                    model = %parsed.model,
                    error = %msg,
                    "upstream may have been billed but could not be metered; charging the reservation"
                );
                guard
                    .settle_with_overhead(
                        reserve,
                        Usage::default(),
                        "estimated",
                        overhead_now(provider_micros),
                    )
                    .await;
                return Outcome::Upstream(msg);
            }
        };
        // Tripwire: a cache write on a request we classified as unable to make
        // one means the classifier was evaded, or the provider started writing
        // caches without an explicit breakpoint. The write leg is still billed
        // correctly by cost_micros, so the ledger stays right; what is wrong is
        // that the reservation deliberately omitted the write premium, so this
        // request can settle above it.
        //
        // Deliberately NOT routed through `suspect`: that charges the
        // reservation, and this reservation is precisely the one that lacked the
        // premium, so doing so would turn a bounded overshoot into an
        // under-charge. Never-under-charge outranks never-overspend.
        if resp.usage.cache_write_tokens > 0 && !reserve_profile.can_cache_write {
            tracing::error!(
                provider = provider_id,
                model = %parsed.model,
                cache_write_tokens = resp.usage.cache_write_tokens,
                "upstream reported a cache WRITE on a request classified as unable to \
                 produce one; billed correctly but reserved without the write premium, \
                 so this request may settle above its reservation"
            );
        }
        // A prompt large enough that the provider may be re-rating the whole
        // request while we are billing it flat. Logged rather than guessed at:
        // inventing a multiple would over-charge models that bill flat, and
        // saying nothing would under-charge the ones that do not.
        if price
            .long_context
            .is_unpriced_long_context(resp.usage.threshold_prompt_tokens())
        {
            tracing::warn!(
                provider = provider_id,
                model = %parsed.model,
                // The size the PROVIDER saw, which is what the threshold tests.
                // Logging the billed total instead would print a number above
                // the threshold next to a warning that did not fire, on any
                // upstream whose prompt classes overlap.
                prompt_tokens = resp.usage.threshold_prompt_tokens(),
                "prompt exceeds the long-context threshold and no tier is configured \
                 for this model; if it re-rates long requests, this is an \
                 UNDER-charge. Set it with `admin price set --long-context-threshold \
                 --long-context-input-permille --long-context-output-permille`"
            );
        }
        // Settle by upstream status.
        let is_success = (200..300).contains(&resp.status);
        let metered = price.cost_micros(resp.usage);
        // An implausible token count is a metering FAILURE, not an expensive
        // request. Costing it would saturate to i64::MAX, and because the budget
        // counter can never be lowered (reconcile only raises) that would wedge
        // the deployment until the period rolled over, while the ledger row it
        // wrote would overflow SUM(cost_micros) on every later reconciliation.
        // Charge the reservation instead: bounded, already admitted, and it
        // keeps the never-under-charge invariant. The real counts go to the log,
        // and a zeroed Usage goes to the ledger so the numbers stay summable.
        let (usage_for_ledger, actual, decision) = if resp.usage.is_untrustworthy() {
            tracing::error!(
                provider = provider_id,
                model = %parsed.model,
                input_tokens = resp.usage.input_tokens,
                output_tokens = resp.usage.output_tokens,
                cache_read_tokens = resp.usage.cache_read_tokens,
                cache_write_tokens = resp.usage.cache_write_tokens,
                suspect = resp.usage.suspect,
                status = resp.status,
                "upstream usage is implausible or self-contradictory; charging the reservation"
            );
            (Usage::default(), reserve, "estimated")
        } else if is_success {
            if metered == 0 {
                // 2xx with no usage reported (e.g. a safety-blocked response):
                // charge the INPUT we reserved (the provider processed it), not
                // the full worst-case reservation, so it can't grief a shared
                // budget, but never zero.
                //
                // Priced at the same prompt rate the reservation used, not the
                // plain input rate. On a write-capable adapter the prompt may
                // have been cache-written at up to 2x, and this is the path that
                // exists to prevent an under-charge, so it must not create one.
                // `estimated`, not `allowed`: this figure is the reserved input
                // leg, not anything the provider reported, and an operator
                // reconciling against an invoice needs to see that.
                (
                    resp.usage,
                    price
                        .reserve_micros(input_tokens, 0, reserve_profile)
                        .max(1),
                    "estimated",
                )
            } else {
                (resp.usage, metered, "allowed")
            }
        } else {
            // Non-2xx: charge whatever the provider reported it billed (usually
            // zero), never the reservation; record as an error.
            (resp.usage, metered, "error")
        };
        guard
            .settle_with_overhead(
                actual,
                usage_for_ledger,
                decision,
                overhead_now(provider_micros),
            )
            .await;
        Outcome::Allowed {
            status: resp.status,
            body: resp.body,
            cost_micros: actual,
            overhead_micros: overhead_now(provider_micros),
            key_id,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        key_id: &str,
        provider: &str,
        model: &str,
        usage: Usage,
        cost_micros: i64,
        decision: &str,
        started: std::time::Instant,
        provider_micros: u128,
    ) {
        let overhead_micros = i64::try_from(
            started
                .elapsed()
                .as_micros()
                .saturating_sub(provider_micros),
        )
        .unwrap_or(i64::MAX);
        self.usage
            .record(UsageEvent {
                key_id,
                provider,
                model,
                usage,
                cost_micros,
                decision,
                overhead_micros,
            })
            .await;
    }
}

/// Guarantees a request's budget reservation is settled EXACTLY ONCE, on EVERY
/// path including the ones that never reach their own settle call.
///
/// Used by both the buffered and streaming paths. It was originally built for
/// streaming, and while the buffered path went without it there was a live hole:
/// axum drops a handler's future when the request times out or the client
/// disconnects, so a buffered request that outran the HTTP timeout, which large
/// LLM responses do routinely, never reached `commit`. Its full worst-case
/// reservation stayed in every applicable counter until the period expired, with
/// no ledger row to explain it, and because `reconcile_counters` only ever
/// raises, nothing could bring it back down. The budget looked spent while the
/// ledger showed headroom.
///
/// Callers settle at their single exit point. If the guard is instead dropped
/// without settling (cancellation, disconnect, panic, runtime shutdown), `Drop`
/// spawns a best-effort commit of the FULL reserved amount and an `estimated`
/// ledger row, so a reservation is never left dangling and never under-charged.
pub struct Settlement {
    budgets: Arc<dyn BudgetBackend>,
    usage: Arc<dyn UsageSink>,
    reservation: Option<Reservation>,
    reserved_micros: i64,
    key_id: String,
    provider: String,
    model: String,
    overhead_micros: i64,
}

impl Settlement {
    #[must_use]
    pub fn new(
        core: &GatewayCore,
        reservation: Reservation,
        reserved_micros: i64,
        key_id: String,
        provider: String,
        model: String,
        overhead_micros: i64,
    ) -> Self {
        Self {
            budgets: core.budgets.clone(),
            usage: core.usage.clone(),
            reservation: Some(reservation),
            reserved_micros,
            key_id,
            provider,
            model,
            overhead_micros,
        }
    }

    /// Settle to `actual_micros`, record the usage row, and disarm the guard.
    /// Idempotent: a second call (or the Drop guard) is a no-op.
    pub async fn settle(self, actual_micros: i64, usage: Usage, decision: &str) {
        let overhead = self.overhead_micros;
        self.settle_with_overhead(actual_micros, usage, decision, overhead)
            .await;
    }

    /// As [`Self::settle`], but records a final overhead measurement taken after
    /// the upstream call rather than the admission-time figure captured when the
    /// guard was built. The buffered path measures overhead across the whole
    /// request minus provider time, which is only known at the end.
    pub async fn settle_with_overhead(
        mut self,
        actual_micros: i64,
        usage: Usage,
        decision: &str,
        overhead_micros: i64,
    ) {
        if let Some(r) = self.reservation.take() {
            self.budgets.commit(&r, actual_micros).await;
            self.usage
                .record(UsageEvent {
                    key_id: &self.key_id,
                    provider: &self.provider,
                    model: &self.model,
                    usage,
                    cost_micros: actual_micros,
                    decision,
                    overhead_micros,
                })
                .await;
        }
    }
}

impl Drop for Settlement {
    fn drop(&mut self) {
        // Only fires if `settle` was never called: a cancelled handler (HTTP
        // timeout or client disconnect), a panic, or runtime shutdown. Charge the
        // FULL reservation, because the upstream may well have served and billed
        // the request, and record it as `estimated` rather than `allowed` or
        // `error` so the ledger says plainly that this figure was assumed rather
        // than measured.
        if let Some(r) = self.reservation.take() {
            // Only spawn if a runtime is present. `tokio::spawn` panics without one,
            // and a panic in a destructor risks a process abort; during runtime
            // shutdown the reservation simply stays held in the budget backend
            // (fail-closed) until its period counter expires.
            let Ok(handle) = tokio::runtime::Handle::try_current() else {
                tracing::warn!(
                    "Settlement dropped without a runtime; reservation left held (fail-closed)"
                );
                return;
            };
            let budgets = self.budgets.clone();
            let usage = self.usage.clone();
            let reserved = self.reserved_micros;
            let overhead = self.overhead_micros;
            let key_id = self.key_id.clone();
            let provider = self.provider.clone();
            let model = self.model.clone();
            handle.spawn(async move {
                budgets.commit(&r, reserved).await;
                usage
                    .record(UsageEvent {
                        key_id: &key_id,
                        provider: &provider,
                        model: &model,
                        usage: Usage::default(),
                        cost_micros: reserved,
                        decision: "estimated",
                        overhead_micros: overhead,
                    })
                    .await;
            });
        }
    }
}

// ---------------------------------------------------------------------------
// In-memory backends (demo + tests). Production backends live alongside these.
// ---------------------------------------------------------------------------

/// In-memory key store keyed by public prefix.
#[derive(Default)]
pub struct MemKeyStore {
    by_prefix: HashMap<String, KeyRecord>,
}

impl MemKeyStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, prefix: impl Into<String>, record: KeyRecord) {
        self.by_prefix.insert(prefix.into(), record);
    }
}

#[async_trait]
impl KeyStore for MemKeyStore {
    async fn lookup(&self, prefix: &str) -> Option<KeyRecord> {
        self.by_prefix.get(prefix).cloned()
    }
}

/// The in-memory `Budgets` store is a [`BudgetBackend`]. It captures `now` per
/// call; production's Redis backend runs the identical check-and-increment.
#[async_trait]
impl BudgetBackend for Budgets {
    async fn reserve(
        &self,
        ctx: &RequestCtx<'_>,
        reserve_micros: i64,
    ) -> Result<Reservation, ReserveError> {
        self.try_reserve(ctx, Utc::now(), reserve_micros)
            .map_err(ReserveError::Denied)
    }

    async fn commit(&self, reservation: &Reservation, actual_micros: i64) {
        self.settle(reservation, actual_micros);
    }
}

/// A stored usage row (no key id: internal, never exposed). The cost serializes
/// as a plain decimal string named `cost`, not raw micros.
#[derive(Debug, Clone, Serialize)]
pub struct StoredUsage {
    pub provider: String,
    pub model: String,
    /// FRESH prompt tokens only; cached classes are carried separately.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Every billable prompt token, so the demo console shows the same series
    /// the production console does instead of a fresh-only count beside a cost.
    pub total_prompt_tokens: u64,
    #[serde(rename = "cost", serialize_with = "serialize_cost")]
    pub cost_micros: i64,
    #[serde(rename = "overhead_us")]
    pub overhead_micros: i64,
    pub decision: String,
}

fn serialize_cost<S: serde::Serializer>(micros: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format_micros(*micros))
}

/// In-memory usage ledger.
#[derive(Default)]
pub struct MemUsageSink {
    events: std::sync::Mutex<Vec<StoredUsage>>,
}

impl MemUsageSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of all recorded events.
    #[must_use]
    pub fn snapshot(&self) -> Vec<StoredUsage> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait]
impl UsageSink for MemUsageSink {
    async fn record(&self, event: UsageEvent<'_>) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(StoredUsage {
                provider: event.provider.to_owned(),
                model: event.model.to_owned(),
                input_tokens: event.usage.input_tokens,
                output_tokens: event.usage.output_tokens,
                cache_read_tokens: event.usage.cache_read_tokens,
                cache_write_tokens: event.usage.cache_write_tokens,
                total_prompt_tokens: event.usage.total_prompt_tokens(),
                cost_micros: event.cost_micros,
                overhead_micros: event.overhead_micros,
                decision: event.decision.to_owned(),
            });
    }
}

/// Convenience: spend on a scope's current-period counter (for inspection).
#[must_use]
pub fn spent(budgets: &Budgets, scope: &Scope, period: Period) -> i64 {
    budgets.spent(scope, period, Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presented_key_reads_both_header_forms() {
        let mut h = HeaderMap::new();
        h.insert(KEY_HEADER, "tgk_a_b".parse().unwrap());
        assert_eq!(presented_key(&h).as_deref(), Some("tgk_a_b"));

        let mut h2 = HeaderMap::new();
        h2.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer tgk_c_d".parse().unwrap(),
        );
        assert_eq!(presented_key(&h2).as_deref(), Some("tgk_c_d"));

        // x-tollgate-key wins when both are present.
        let mut h3 = HeaderMap::new();
        h3.insert(KEY_HEADER, "tgk_x_y".parse().unwrap());
        h3.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer tgk_c_d".parse().unwrap(),
        );
        assert_eq!(presented_key(&h3).as_deref(), Some("tgk_x_y"));

        assert!(presented_key(&HeaderMap::new()).is_none());
    }
    use crate::apikey::KeyHasher;
    use crate::budget::{Budget, Period, Scope};
    use crate::pricing::ModelPrice;
    use crate::provider::MockProvider;

    fn header(key: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(KEY_HEADER, key.parse().unwrap());
        h
    }

    fn core_with_key() -> (GatewayCore, String) {
        let hasher = KeyHasher::random();
        let generated = hasher.generate();
        let mut keys = MemKeyStore::new();
        keys.insert(
            generated.prefix.clone(),
            KeyRecord {
                id: "key1".to_owned(),
                key_hash: generated.key_hash.clone(),
            },
        );
        let budgets = Arc::new(Budgets::new(vec![
            Budget {
                scope: Scope::Global,
                period: Period::Monthly,
                limit_micros: 1_000_000,
                hard_stop: true,
            },
            Budget {
                scope: Scope::ApiKey("key1".to_owned()),
                period: Period::Monthly,
                limit_micros: 50_000,
                hard_stop: true,
            },
        ]));
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        providers.insert("mock".to_owned(), Arc::new(MockProvider));
        let core = GatewayCore {
            dummy_hash: hasher.hash("dummy"),
            hasher,
            keys: Arc::new(keys),
            budgets,
            usage: Arc::new(MemUsageSink::new()),
            prices: Arc::new(PriceBook::from_prices(vec![ModelPrice::new(
                "mock", "demo", 3_000_000, 15_000_000,
            )])),
            providers,
            admission_exact: false,
        };
        (core, generated.plaintext)
    }

    /// A provider whose `forward` never returns, standing in for a slow LLM
    /// response, and one that fails in each of the two ways that matter.
    struct StubProvider(StubBehaviour);
    #[derive(Clone, Copy)]
    enum StubBehaviour {
        Hang,
        NotDelivered,
        MaybeBilled,
    }

    #[async_trait]
    impl Provider for StubProvider {
        fn id(&self) -> &str {
            "mock"
        }
        fn parse_request(
            &self,
            rest_path: &str,
            body: &str,
        ) -> Result<crate::provider::ParsedRequest, ProviderError> {
            MockProvider.parse_request(rest_path, body)
        }
        async fn forward(
            &self,
            _rest_path: &str,
            _body: &str,
            _parsed: &crate::provider::ParsedRequest,
        ) -> Result<crate::provider::ProviderResponse, ProviderError> {
            match self.0 {
                StubBehaviour::Hang => std::future::pending().await,
                StubBehaviour::NotDelivered => {
                    Err(ProviderError::Upstream("connection refused".to_owned()))
                }
                StubBehaviour::MaybeBilled => Err(ProviderError::MeteringFailed(
                    "timed out waiting for response".to_owned(),
                )),
            }
        }
    }

    fn core_with_stub(b: StubBehaviour) -> (GatewayCore, String, Arc<MemUsageSink>) {
        let (mut core, key) = core_with_key();
        let sink = Arc::new(MemUsageSink::new());
        core.usage = sink.clone();
        core.providers
            .insert("mock".to_owned(), Arc::new(StubProvider(b)));
        (core, key, sink)
    }

    /// The bug this guards: axum drops a handler's future when the HTTP timeout
    /// fires or the client disconnects. A buffered LLM call routinely outruns
    /// that timeout, and without a settle guard the reservation stayed in every
    /// counter until the period rolled over, with no ledger row to explain it,
    /// and `reconcile_counters` only ever raises so nothing could undo it.
    #[tokio::test]
    async fn a_cancelled_request_still_settles_its_reservation() {
        let (core, key, sink) = core_with_stub(StubBehaviour::Hang);
        let body = r#"{"model":"demo","prompt":"hi","max_output_tokens":1000}"#;

        // Drop the future mid-forward, exactly as the HTTP layer does.
        let timed_out = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            core.evaluate("mock", "generate", &header(&key), body),
        )
        .await;
        assert!(timed_out.is_err(), "the stub must still be hanging");

        // The Drop guard settles on a spawned task; let it run.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let events = sink.snapshot();
        assert_eq!(
            events.len(),
            1,
            "a cancelled request must still be recorded"
        );
        assert_eq!(
            events[0].decision, "estimated",
            "charged its reservation, not measured, so it must not read as allowed"
        );
        assert!(
            events[0].cost_micros > 0,
            "a dangling reservation must be charged, never released silently"
        );
    }

    #[tokio::test]
    async fn an_undelivered_request_is_released_not_charged() {
        let (core, key, sink) = core_with_stub(StubBehaviour::NotDelivered);
        let body = r#"{"model":"demo","prompt":"hi","max_output_tokens":1000}"#;
        let out = core.evaluate("mock", "generate", &header(&key), body).await;
        assert!(matches!(out, Outcome::Upstream(_)));

        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        // Connect/DNS/TLS failures never reached the provider, so nothing was
        // billed and the reservation is released in full.
        assert_eq!(events[0].cost_micros, 0);
        assert_eq!(events[0].decision, "error");
    }

    #[tokio::test]
    async fn a_possibly_billed_request_is_charged_its_reservation() {
        let (core, key, sink) = core_with_stub(StubBehaviour::MaybeBilled);
        let body = r#"{"model":"demo","prompt":"hi","max_output_tokens":1000}"#;
        let out = core.evaluate("mock", "generate", &header(&key), body).await;
        assert!(matches!(out, Outcome::Upstream(_)));

        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        // A timeout AFTER the request was sent may mean the provider generated a
        // full response we never read. Releasing would under-charge a request
        // the provider is invoicing.
        assert!(
            events[0].cost_micros > 0,
            "a possibly-billed request must not be released"
        );
        assert_eq!(events[0].decision, "estimated");
    }

    #[tokio::test]
    async fn full_flow_allows_then_hard_stops() {
        let (core, key) = core_with_key();
        let body = r#"{"model":"demo","prompt":"hi","max_output_tokens":1000}"#;
        let mut allowed = 0;
        let mut denied = 0;
        for _ in 0..5 {
            match core.evaluate("mock", "generate", &header(&key), body).await {
                Outcome::Allowed { .. } => allowed += 1,
                Outcome::BudgetDenied(_) => denied += 1,
                other => panic!("unexpected: {}", label(&other)),
            }
        }
        // ~0.015 per request, 0.05 cap -> 3 allowed, 2 denied.
        assert_eq!(allowed, 3);
        assert_eq!(denied, 2);
    }

    #[tokio::test]
    async fn missing_key_is_unauthenticated() {
        let (core, _key) = core_with_key();
        let out = core
            .evaluate("mock", "generate", &HeaderMap::new(), "{}")
            .await;
        assert!(matches!(out, Outcome::Unauthenticated));
    }

    #[tokio::test]
    async fn unpriced_model_fails_closed() {
        let (core, key) = core_with_key();
        let body = r#"{"model":"not-priced","prompt":"hi","max_output_tokens":10}"#;
        let out = core.evaluate("mock", "generate", &header(&key), body).await;
        assert!(matches!(out, Outcome::Unpriced { .. }));
    }

    fn label(o: &Outcome) -> &'static str {
        match o {
            Outcome::Allowed { .. } => "allowed",
            Outcome::Unauthenticated => "unauthenticated",
            Outcome::BadRequest(_) => "bad_request",
            Outcome::Unpriced { .. } => "unpriced",
            Outcome::BudgetDenied(_) => "budget_denied",
            Outcome::BackendError(_) => "backend_error",
            Outcome::Upstream(_) => "upstream",
        }
    }
}
