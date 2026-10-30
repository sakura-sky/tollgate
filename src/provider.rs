// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Provider abstraction and a built-in mock.
//!
//! Every provider-specific concern hides behind [`Provider`] so the gateway hot
//! path is provider-agnostic. An adapter does three things: extract what
//! budgeting needs from a (provider-native) request body via
//! [`Provider::parse_request`], forward the request, and report token usage.
//!
//! The demo ships [`MockProvider`], which needs no credentials and returns
//! deterministic usage so budget math is predictable. Real Vertex/Anthropic
//! adapters implement the same trait and slot in without touching the gateway.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::pricing::Usage;

/// What the gateway needs to budget a request, extracted from the native body.
#[derive(Debug, Clone)]
pub struct ParsedRequest {
    pub model: String,
    /// Cheap up-front estimate of input tokens (used to reserve budget).
    pub estimated_input_tokens: u64,
    /// The maximum output the request may produce (used to reserve the worst
    /// case so a request cannot overshoot its budget).
    pub max_output_tokens: u64,
    /// Whether THIS request could cause a cache write, which bills above the
    /// input rate. Adapters that can report writes at all set this from the
    /// request body, so a request that cannot write is not reserved as if it
    /// could. Defaults false; erring toward true only over-reserves.
    pub may_cache_write: bool,
}

/// A provider response: upstream status, JSON body (returned to the client only
/// on a 2xx), and parsed usage.
///
/// A non-2xx whose body parses is still a `ProviderResponse`, so settlement can
/// charge what the provider reported. Its `body` is then for the log only: the
/// gateway never returns a non-2xx body to a client.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub status: u16,
    pub body: Value,
    pub usage: Usage,
    pub meta: ResponseMeta,
}

/// What Tollgate keeps from a provider response's headers. Only parsed or
/// cleaned values, never a raw header to copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseMeta {
    /// A valid `Retry-After`, clamped to 1..=60 seconds (see
    /// [`parse_retry_after`]).
    pub retry_after_secs: Option<u32>,
    /// The provider's own request id, cleaned, for the log only.
    pub provider_request_id: Option<String>,
}

impl ResponseMeta {
    #[must_use]
    pub fn from_headers(headers: &http::HeaderMap) -> Self {
        // Anthropic sends `request-id`; OpenAI-compatible upstreams commonly
        // send `x-request-id`.
        let provider_request_id = ["request-id", "x-request-id"].iter().find_map(|name| {
            headers
                .get(*name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| clean_token(v, 128))
        });
        Self {
            retry_after_secs: parse_retry_after(headers),
            provider_request_id,
        }
    }
}

/// How Tollgate tells the client the provider refused. Decided by the
/// provider's status alone, so nothing the provider wrote reaches the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamFault {
    /// 400, 413 or 422: the provider says the caller's request is at fault.
    /// Carries the status, which the client gets unchanged.
    Request(u16),
    /// 429: the provider is rate limiting this deployment.
    RateLimited { retry_after_secs: Option<u32> },
    /// Anything else: Tollgate's own credential or configuration (401, 403,
    /// 404), or the provider itself. The client gets 502.
    Provider,
}

impl UpstreamFault {
    /// A fixed word for the log line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Request(_) => "request",
            Self::RateLimited { .. } => "rate_limited",
            Self::Provider => "provider",
        }
    }
}

/// Classify a provider's non-2xx status.
#[must_use]
pub fn classify_upstream_status(status: u16, retry_after_secs: Option<u32>) -> UpstreamFault {
    match status {
        400 | 413 | 422 => UpstreamFault::Request(status),
        429 => UpstreamFault::RateLimited { retry_after_secs },
        _ => UpstreamFault::Provider,
    }
}

/// The longest `Retry-After` Tollgate passes on, in seconds. The Anthropic and
/// OpenAI SDKs honour only `0 < x <= 60` and fall back to their own backoff
/// otherwise, and a provider (or a misconfigured custom upstream) must not be
/// able to tell every client to wait an hour.
pub const MAX_RETRY_AFTER_SECS: u32 = 60;

/// A provider's `Retry-After`, as whole seconds clamped to
/// 1..=[`MAX_RETRY_AFTER_SECS`], or `None`.
///
/// Exactly one header value, ASCII digits only, at most 10 of them. An
/// HTTP-date, a fraction, a sign, whitespace, two headers or a list give `None`.
/// The value is re-serialised from the integer by the caller, never copied.
#[must_use]
pub fn parse_retry_after(headers: &http::HeaderMap) -> Option<u32> {
    let mut values = headers.get_all(http::header::RETRY_AFTER).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 10 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let secs: u64 = std::str::from_utf8(bytes).ok()?.parse().ok()?;
    let clamped = secs.clamp(1, u64::from(MAX_RETRY_AFTER_SECS));
    u32::try_from(clamped).ok()
}

/// What Tollgate logs about a provider refusal. Never part of a client
/// response, and never in a `ProviderError`'s `Display`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefusalDetail {
    /// The provider's error type (`error.type`, or Google's `error.status`).
    pub error_type: Option<String>,
    /// The provider's error code (`error.details.error_code`, or `error.code`).
    pub error_code: Option<String>,
    pub provider_request_id: Option<String>,
    /// The body, at most [`LOGGED_BODY_BYTES`] of it, for the opt-in TRACE line.
    pub body: String,
}

/// The target of the opt-in TRACE line carrying a provider's refusal body. The
/// OTLP layer filters it out (see `telemetry::init`). Must match the literal in
/// [`log_upstream_refusal`].
pub const UPSTREAM_BODY_TARGET: &str = "tollgate::upstream_body";

/// How much of a refusal body is logged, at TRACE only.
pub const LOGGED_BODY_BYTES: usize = 2048;

impl RefusalDetail {
    /// Build from the response's meta and its body bytes. `complete` says
    /// whether `body` is the whole body: the type and code are read only then,
    /// because a truncated body does not parse.
    #[must_use]
    pub fn new(meta: &ResponseMeta, body: &[u8], complete: bool) -> Self {
        let parsed = if complete {
            serde_json::from_slice::<Value>(body).ok()
        } else {
            None
        };
        Self::from_parts(meta, parsed.as_ref(), body)
    }

    /// Build from a body that has already been parsed. The body is
    /// re-serialised for the log only when its TRACE line is enabled.
    #[must_use]
    pub fn from_value(meta: &ResponseMeta, body: &Value) -> Self {
        let text = if tracing::enabled!(target: "tollgate::upstream_body", tracing::Level::TRACE) {
            body.to_string()
        } else {
            String::new()
        };
        Self::from_parts(meta, Some(body), text.as_bytes())
    }

    fn from_parts(meta: &ResponseMeta, parsed: Option<&Value>, raw: &[u8]) -> Self {
        // Google's OpenAI-compatible endpoint can answer with a one-element
        // array of error objects.
        let error = parsed.map(|v| match v {
            Value::Array(items) => items.first().unwrap_or(&Value::Null),
            other => other,
        });
        let field = |pointers: &[&str]| {
            error.and_then(|e| {
                pointers.iter().find_map(|p| match e.pointer(p)? {
                    Value::String(s) => clean_token(s, 64),
                    Value::Number(n) => clean_token(&n.to_string(), 64),
                    _ => None,
                })
            })
        };
        let cut = raw.len().min(LOGGED_BODY_BYTES);
        Self {
            error_type: field(&["/error/type", "/error/status"]),
            error_code: field(&["/error/details/error_code", "/error/code"]),
            provider_request_id: meta.provider_request_id.clone(),
            body: String::from_utf8_lossy(&raw[..cut]).into_owned(),
        }
    }
}

/// `value` if it is 1..=`max` characters of `[A-Za-z0-9_.-]`, else `None`.
/// Provider-supplied identifiers go into log fields only in this shape.
#[must_use]
pub fn clean_token(value: &str, max: usize) -> Option<String> {
    let ok = !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    ok.then(|| value.to_owned())
}

/// Log a provider refusal: the identifiers at WARN, the body at TRACE under
/// `tollgate::upstream_body` (off under the default filter, and kept out of
/// OTLP). `request_id` is Tollgate's, the one the client was given.
pub fn log_upstream_refusal(
    request_id: Option<&str>,
    provider: &str,
    model: &str,
    status: u16,
    fault: UpstreamFault,
    detail: &RefusalDetail,
) {
    let request_id = request_id.unwrap_or("");
    tracing::warn!(
        request_id,
        provider,
        model,
        status,
        fault = fault.as_str(),
        provider_error_type = detail.error_type.as_deref().unwrap_or(""),
        provider_error_code = detail.error_code.as_deref().unwrap_or(""),
        provider_request_id = detail.provider_request_id.as_deref().unwrap_or(""),
        "upstream refused the request"
    );
    tracing::trace!(
        target: "tollgate::upstream_body",
        request_id,
        status,
        body = %detail.body,
        "upstream refusal body"
    );
}

/// Errors from a provider adapter.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("bad request: {0}")]
    BadRequest(String),
    /// The call failed BEFORE the provider could have served it: connect, DNS or
    /// TLS failure, or a failure building or authorising the request. Nothing was
    /// billed upstream, so the reservation is released in full.
    #[error("upstream provider error: {0}")]
    Upstream(String),
    /// The request may have reached the provider and been billed, but the result
    /// could not be metered: a timeout after the request was sent, a transport
    /// error mid-response, or a 2xx whose body will not parse.
    ///
    /// Releasing the reservation here would under-charge a request the provider
    /// is invoicing, so the caller charges the reservation instead and records
    /// the row as `estimated`. Over-charging visibly beats under-charging
    /// silently.
    #[error("metering failed: {0}")]
    MeteringFailed(String),
    /// The provider answered with a non-2xx that Tollgate passes on only as a
    /// classified refusal (see [`UpstreamFault`]). No usage was reported for it,
    /// so the reservation is released.
    ///
    /// The `Display` names the status only: `detail` is for
    /// [`log_upstream_refusal`] and never reaches a log line through this
    /// error's text, nor a client.
    #[error("provider refused the request ({status})")]
    Refused {
        status: u16,
        fault: UpstreamFault,
        detail: Box<RefusalDetail>,
    },
}

/// A backend Tollgate can proxy to.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider id, e.g. `mock`, `vertex`, `anthropic`.
    fn id(&self) -> &str;

    /// Extract budgeting info from the request. `rest_path` is everything after
    /// `/v1/{provider}/`; `body` is the raw request body.
    ///
    /// # Errors
    /// Returns [`ProviderError::BadRequest`] if the body cannot be parsed.
    fn parse_request(&self, rest_path: &str, body: &str) -> Result<ParsedRequest, ProviderError>;

    /// Forward the request to the upstream and return the response with usage.
    ///
    /// # Errors
    /// Returns [`ProviderError::Upstream`] if the upstream call fails.
    async fn forward(
        &self,
        rest_path: &str,
        body: &str,
        parsed: &ParsedRequest,
    ) -> Result<ProviderResponse, ProviderError>;

    /// Exact input-token count for the reservation, used only in `exact`
    /// admission mode. The default returns the parse-time estimate; real
    /// adapters override this with a call to the provider's token counter.
    ///
    /// # Errors
    /// Returns [`ProviderError::Upstream`] if the count call fails.
    async fn count_input_tokens(
        &self,
        _rest_path: &str,
        _body: &str,
        parsed: &ParsedRequest,
    ) -> Result<u64, ProviderError> {
        Ok(parsed.estimated_input_tokens)
    }

    /// Whether this adapter can ever report `cache_write_tokens > 0`.
    ///
    /// This gates the worst-case reservation. A cache write can cost more than
    /// fresh input (up to 2x), so where writes are possible the reservation has
    /// to assume the whole prompt might be written, or a request could settle
    /// above what it reserved and overshoot a hard cap.
    ///
    /// It must stay false where writes are impossible. Because an unpriced write
    /// class always resolves to a fallback above the input rate, reserving at
    /// the write rate unconditionally would inflate EVERY reservation on EVERY
    /// model, including adapters that never report writes, and start refusing
    /// traffic that previously fit under the same budget.
    fn can_report_cache_write(&self) -> bool {
        false
    }

    /// The prompt-side worst case THIS request can produce, used to size the
    /// reservation.
    ///
    /// The default ANDs the adapter's capability with the per-request flag, so a
    /// request that cannot cause a cache write is not reserved as though it
    /// could. Keeping the conjunction here rather than in each adapter means a
    /// future write-capable adapter has to actively override to get it wrong.
    /// Adapters that can bill a prompt token on more than one leg must override
    /// and preserve that conjunction.
    fn prompt_reserve_profile(
        &self,
        parsed: &ParsedRequest,
    ) -> crate::pricing::PromptReserveProfile {
        crate::pricing::PromptReserveProfile {
            can_cache_write: self.can_report_cache_write() && parsed.may_cache_write,
            may_double_bill_prompt: false,
        }
    }
}

/// A credential-free mock provider for the demo. Input tokens are estimated from
/// the prompt; output tokens equal the requested `max_output_tokens`, so each
/// request has a predictable, explainable cost.
#[derive(Debug, Default)]
pub struct MockProvider;

impl MockProvider {
    /// Cheap token estimate: about 4 characters per token, at least 1.
    #[must_use]
    pub fn estimate_input_tokens(prompt: &str) -> u64 {
        (prompt.chars().count() as u64 / 4).max(1)
    }
}

#[derive(serde::Deserialize)]
struct MockBody {
    #[serde(default = "default_model")]
    model: String,
    #[serde(default)]
    prompt: String,
    #[serde(default = "default_max_output")]
    max_output_tokens: u64,
}

fn default_model() -> String {
    "demo".to_owned()
}
fn default_max_output() -> u64 {
    1_000
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str {
        "mock"
    }

    fn parse_request(&self, _rest_path: &str, body: &str) -> Result<ParsedRequest, ProviderError> {
        let b: MockBody = serde_json::from_str(if body.is_empty() { "{}" } else { body })
            .map_err(|e| ProviderError::BadRequest(e.to_string()))?;
        Ok(ParsedRequest {
            model: b.model,
            estimated_input_tokens: Self::estimate_input_tokens(&b.prompt),
            max_output_tokens: b.max_output_tokens,
            may_cache_write: false,
        })
    }

    async fn forward(
        &self,
        _rest_path: &str,
        _body: &str,
        parsed: &ParsedRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let input = parsed.estimated_input_tokens;
        let output = parsed.max_output_tokens;
        let body = json!({
            "provider": "mock",
            "model": parsed.model,
            "completion": format!(
                "[mock completion for {input} input tokens; generated {output} output tokens]"
            ),
            "usage": { "input_tokens": input, "output_tokens": output },
        });
        Ok(ProviderResponse {
            status: 200,
            body,
            usage: Usage::new(input, output),
            meta: ResponseMeta::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retry_after(values: &[&[u8]]) -> Option<u32> {
        let mut headers = http::HeaderMap::new();
        for v in values {
            headers.append(
                http::header::RETRY_AFTER,
                http::HeaderValue::from_bytes(v).expect("valid header bytes"),
            );
        }
        parse_retry_after(&headers)
    }

    #[test]
    fn retry_after_is_one_integer_clamped_to_a_minute() {
        assert_eq!(retry_after(&[]), None);
        assert_eq!(retry_after(&[b"0"]), Some(1));
        assert_eq!(retry_after(&[b"1"]), Some(1));
        assert_eq!(retry_after(&[b"30"]), Some(30));
        assert_eq!(retry_after(&[b"60"]), Some(60));
        assert_eq!(retry_after(&[b"61"]), Some(60));
        assert_eq!(retry_after(&[b"3600"]), Some(60));
        assert_eq!(retry_after(&[b"9999999999"]), Some(60));
        for bad in [
            &b""[..],
            b"99999999999",
            b"-1",
            b"1.5",
            b" 30",
            b"30 ",
            b"+30",
            b"Wed, 21 Oct 2026 07:28:00 GMT",
            b"30, 40",
            b"\xc3\xa9",
        ] {
            assert_eq!(retry_after(&[bad]), None, "{bad:?}");
        }
        // Two headers: ambiguous, so neither.
        assert_eq!(retry_after(&[b"30", b"30"]), None);
    }

    #[test]
    fn provider_statuses_are_classified_by_fault() {
        for status in [400, 413, 422] {
            assert_eq!(
                classify_upstream_status(status, None),
                UpstreamFault::Request(status)
            );
        }
        assert_eq!(
            classify_upstream_status(429, Some(5)),
            UpstreamFault::RateLimited {
                retry_after_secs: Some(5)
            }
        );
        for status in [401, 403, 404, 408, 409, 500, 502, 503, 529] {
            assert_eq!(
                classify_upstream_status(status, Some(5)),
                UpstreamFault::Provider,
                "{status}"
            );
        }
    }

    #[test]
    fn refusal_detail_keeps_only_clean_identifiers_and_a_bounded_body() {
        let meta = ResponseMeta {
            retry_after_secs: None,
            provider_request_id: Some("req_1".to_owned()),
        };
        // Anthropic's shape, including a spend-limit code.
        let anthropic = br#"{"type":"error","error":{"type":"rate_limit_error","message":"x","details":{"error_code":"enforced_spend_limit_reached"}}}"#;
        let d = RefusalDetail::new(&meta, anthropic, true);
        assert_eq!(d.error_type.as_deref(), Some("rate_limit_error"));
        assert_eq!(
            d.error_code.as_deref(),
            Some("enforced_spend_limit_reached")
        );
        assert_eq!(d.provider_request_id.as_deref(), Some("req_1"));

        // Google's array form, with a numeric code and a status word.
        let google = br#"[{"error":{"code":400,"message":"x","status":"INVALID_ARGUMENT"}}]"#;
        let d = RefusalDetail::new(&meta, google, true);
        assert_eq!(d.error_type.as_deref(), Some("INVALID_ARGUMENT"));
        assert_eq!(d.error_code.as_deref(), Some("400"));

        // A type that is not a clean token is dropped, not logged.
        let dirty = br#"{"error":{"type":"bad\ntype with spaces"}}"#;
        assert_eq!(RefusalDetail::new(&meta, dirty, true).error_type, None);

        // An incomplete body is not parsed, and the logged body is bounded.
        let long = vec![b'a'; LOGGED_BODY_BYTES * 2];
        let d = RefusalDetail::new(&meta, &long, false);
        assert_eq!(d.body.len(), LOGGED_BODY_BYTES);
        assert_eq!(d.error_type, None);

        // A character cut in half at the limit does not panic.
        let mut cut = vec![b'a'; LOGGED_BODY_BYTES - 1];
        cut.extend_from_slice("é".as_bytes());
        let _ = RefusalDetail::new(&meta, &cut, true);
    }

    #[test]
    fn clean_tokens_are_short_and_plain() {
        assert_eq!(
            clean_token("req_011.abc-1", 64).as_deref(),
            Some("req_011.abc-1")
        );
        assert_eq!(clean_token("", 64), None);
        assert_eq!(clean_token(&"a".repeat(65), 64), None);
        for bad in ["a b", "a\nb", "a\"b", "é", "a/b"] {
            assert_eq!(clean_token(bad, 64), None, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn mock_parses_and_reports_deterministic_usage() {
        let p = MockProvider;
        // 33-char prompt -> 8 input tokens; max_output 100.
        let parsed = p
            .parse_request("generate", r#"{"model":"demo","prompt":"hello world this is a test prompt","max_output_tokens":100}"#)
            .unwrap();
        assert_eq!(parsed.model, "demo");
        assert_eq!(parsed.estimated_input_tokens, 8);
        assert_eq!(parsed.max_output_tokens, 100);

        let resp = p.forward("generate", "", &parsed).await.unwrap();
        assert_eq!(resp.usage.input_tokens, 8);
        assert_eq!(resp.usage.output_tokens, 100);
        assert_eq!(resp.status, 200);
    }

    #[test]
    fn mock_rejects_bad_json() {
        let p = MockProvider;
        assert!(p.parse_request("generate", "{not json").is_err());
    }
}
