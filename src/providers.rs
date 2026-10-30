// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Real provider adapters: Anthropic and Vertex/Gemini.
//!
//! Both implement the [`Provider`] trait via native pass-through: the client's
//! provider-native body is forwarded to the upstream with that provider's
//! credentials (native Vertex forwards the client's bytes only once
//! `VertexProvider::checked_body` has proved they mean what was checked), and token usage is read from the response. Upstream
//! base URLs are pinned from config and the client-supplied path is checked for
//! traversal, so a request cannot be redirected off the configured provider.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use crate::pricing::Usage;
use crate::provider::{
    ParsedRequest, Provider, ProviderError, ProviderResponse, RefusalDetail, ResponseMeta,
    classify_upstream_status,
};

/// Bounded best-effort input-token estimate for `fast` admission. ASCII counts
/// at ~4 bytes/token; every non-ASCII byte counts as ~1 token, because dense
/// multibyte text (CJK, emoji) tokenizes far below 3 bytes/token and must not be
/// under-reserved. Referenced media is rejected separately in fast mode; `exact`
/// mode is the strict path with a true pre-flight count.
fn estimate_input_tokens(body: &str) -> u64 {
    let non_ascii = body.bytes().filter(|b| !b.is_ascii()).count();
    let ascii = body.len() - non_ascii;
    ((ascii / 4) + non_ascii).max(1) as u64
}

/// Refuse a body carrying an integer literal outside the `i64` and `u64`
/// ranges.
///
/// `serde_json` reads such a literal as the nearest `f64`, so a body that is
/// re-serialised before it is forwarded would send the provider another
/// number than the client wrote, and a body checked as a parsed value but
/// forwarded as sent (native Vertex) would be checked as another number. Scans the raw text, outside strings, for
/// number tokens with no fraction or exponent. Run it only on a body that has
/// already parsed as JSON.
fn reject_lossy_integers(body: &str) -> Result<(), ProviderError> {
    let b = body.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    while i < b.len() {
        let c = b[i];
        if in_string {
            match c {
                b'\\' => i += 2,
                b'"' => {
                    in_string = false;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        if c == b'"' {
            in_string = true;
            i += 1;
        } else if c == b'-' || c.is_ascii_digit() {
            let start = i;
            let mut integer = true;
            while i < b.len() && matches!(b[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                integer &= !matches!(b[i], b'.' | b'e' | b'E');
                i += 1;
            }
            let token = &body[start..i];
            if integer && token.parse::<i64>().is_err() && token.parse::<u64>().is_err() {
                return Err(bad(
                    "an integer in the request body is outside the 64-bit range and would \
                     not reach the provider unchanged",
                ));
            }
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// Reject path traversal / absolute paths in the client-supplied rest path so a
/// request cannot escape the configured upstream base URL.
fn safe_rest_path(rest_path: &str) -> Result<&str, ProviderError> {
    if rest_path.is_empty()
        || rest_path.starts_with('/')
        || rest_path.contains("..")
        || rest_path.contains(['\0', '\\'])
    {
        // Fixed text: the path is the client's.
        return Err(bad("invalid upstream path"));
    }
    Ok(rest_path)
}

/// Whether the body references EXTERNAL media (a file URI, file id, or URL)
/// whose token cost is not bounded by the request body size. A tiny body can
/// point at a huge file, so `fast` admission would wildly under-reserve; such
/// requests are refused in `fast` mode and require `exact` (which counts them).
///
/// Walks the PARSED JSON so key spelling/casing, whitespace, and `\u` escapes
/// cannot evade it (a raw substring scan could). A body that does not parse as
/// JSON returns false here (it is rejected later in `parse_request`).
#[must_use]
pub fn references_external_media(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|v| value_has_media_ref(&v))
}

fn value_has_media_ref(v: &Value) -> bool {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                // Without case or underscores, as `loose_key` compares them:
                // the native Vertex adapter forwards `file__data` or
                // `File_Data` as `fileData`, so this must see them as one key.
                let key = loose_key(k);
                // External-reference keys across Anthropic, Gemini, and OpenAI
                // shapes (OpenAI vision/audio use image_url / input_audio).
                if matches!(
                    key.as_str(),
                    "fileuri" | "fileid" | "filedata" | "imageurl" | "inputaudio"
                ) {
                    return true;
                }
                // A source/part typed as a URL or OpenAI media reference.
                if key == "type"
                    && val.as_str().is_some_and(|s| {
                        matches!(loose_key(s).as_str(), "url" | "imageurl" | "inputaudio")
                    })
                {
                    return true;
                }
                if value_has_media_ref(val) {
                    return true;
                }
            }
            false
        }
        Value::Array(items) => items.iter().any(value_has_media_ref),
        _ => false,
    }
}

/// Reject request fields that change what a token COSTS without changing how
/// many tokens are reported.
///
/// A price is one input rate and one output rate per model, so anything that
/// silently re-rates the same token count, or adds a per-call fee that appears
/// in no token field at all, is metered wrong and always in the cheap direction.
/// Audio output is the clearest case: it bills several times the text output
/// rate while sitting inside `completion_tokens`, and the caller selects it.
///
/// Fail closed until each is priced, which is the same posture external media
/// already gets. Detecting these in the RESPONSE instead would be too late: the
/// reservation was computed at the text rate, so falling back to charging it
/// would still under-charge by the whole multiplier.
fn reject_rate_switches(v: &Value) -> Result<(), ProviderError> {
    let refuse = |what: &str, why: &str| {
        Err(ProviderError::BadRequest(format!(
            "{what} is not supported: {why}. Tollgate prices one input rate and one \
             output rate per model, so it cannot meter this correctly yet and refuses \
             rather than under-charging."
        )))
    };

    // Audio output bills several times the text output rate, inside the same
    // completion_tokens field. `modalities` absent or exactly ["text"] is fine.
    if let Some(m) = v.get("modalities") {
        let text_only = m
            .as_array()
            .is_some_and(|a| a.iter().all(|x| x.as_str() == Some("text")));
        if !text_only {
            return refuse(
                "non-text output modalities",
                "audio output is billed at a different rate from text",
            );
        }
    }
    if v.get("audio").is_some_and(|a| !a.is_null()) {
        return refuse(
            "the audio output config",
            "audio output is billed at a different rate from text",
        );
    }
    // Predicted Outputs: rejected prediction tokens bill at the output rate and
    // are not reliably visible in completion_tokens across upstreams.
    if v.get("prediction").is_some_and(|p| !p.is_null()) {
        return refuse(
            "prediction (Predicted Outputs)",
            "rejected prediction tokens are billed but not reliably reported",
        );
    }
    // Per-call fee in dollars, present in no token field whatsoever.
    if v.get("web_search_options").is_some_and(|w| !w.is_null()) {
        return refuse(
            "web_search_options",
            "server-side search is billed per call, which no token count reports",
        );
    }
    // Priority tiers bill more per token at identical token counts, and
    // `auto` lets the provider pick one. Anything present but the exact
    // string "default" is refused, `null` and non-strings included: how an
    // upstream reads those is not known.
    if v.get("service_tier")
        .is_some_and(|t| t.as_str() != Some("default"))
    {
        return refuse(
            "a service_tier other than 'default'",
            "other tiers, and 'auto', which may pick one, change the per-token rate",
        );
    }
    // Hosted/server-side tools carry per-call fees. Client-executed function
    // tools are token-only and stay allowed.
    if let Some(tools) = v.get("tools").and_then(Value::as_array) {
        for t in tools {
            if let Some(ty) = t.get("type").and_then(Value::as_str) {
                if !ty.eq_ignore_ascii_case("function") {
                    return refuse(
                        "server-side tools",
                        "hosted tools are billed per call, which no token count reports",
                    );
                }
            }
        }
    }
    Ok(())
}

/// Classify a `reqwest` failure by whether the provider could already have
/// served, and therefore billed, the request.
///
/// This distinction is the difference between releasing a reservation and
/// charging it. Only a connect-phase failure proves nothing was billed. Anything
/// later, most importantly a timeout while waiting for a response, may mean the
/// provider generated a full response we simply never read, and non-streaming
/// LLM APIs send no headers until generation completes, so a slow large response
/// looks exactly like a hang.
/// Only `is_connect` and `is_builder` prove nothing was delivered. Everything
/// else, timeouts included, is treated as possibly billed.
///
/// `is_request` is deliberately NOT in that list, though it reads like it should
/// be. reqwest reports a timeout waiting for a RESPONSE as a request-kind error
/// ("error sending request for url"), so including it silently classified the
/// single most important case, a slow non-streaming LLM response, as never
/// delivered and released its reservation. That is the exact under-charge this
/// function exists to prevent, and it survived a green unit test because the
/// test constructed the error variant directly instead of going through here.
fn classify_transport_error(e: &reqwest::Error) -> ProviderError {
    if e.is_connect() || e.is_builder() {
        ProviderError::Upstream(e.to_string())
    } else {
        ProviderError::MeteringFailed(e.to_string())
    }
}

/// Read a JSON response body, classifying a parse failure by status.
///
/// A 2xx that will not parse was served and billed, so it is a metering failure.
/// A non-2xx that will not parse (an HTML 429 from a CDN, an empty 3xx body) was
/// not billed; it becomes a [`ProviderError::Refused`] classified by its status,
/// so a plain-text 429 still reaches the client as a 429 with its Retry-After.
///
/// A non-2xx that does parse is returned like a success, so the caller can
/// settle on whatever usage the provider reported; the gateway then answers
/// with a classified refusal, never this body.
///
/// The headers are read before the body consumes the response.
async fn json_or_classified(
    resp: reqwest::Response,
) -> Result<(u16, ResponseMeta, Value), ProviderError> {
    let status = resp.status().as_u16();
    let success = (200..300).contains(&status);
    let meta = ResponseMeta::from_headers(resp.headers());
    let raw = match resp.bytes().await {
        Ok(raw) => raw,
        Err(e) if success => {
            return Err(ProviderError::MeteringFailed(format!(
                "2xx response body could not be read: {e}"
            )));
        }
        // The status and headers arrived, so classify by them: a 429 whose
        // body read failed is still a 429 with its Retry-After. Nothing was
        // billed that anyone reported, so the reservation is released.
        Err(_) => {
            return Err(ProviderError::Refused {
                status,
                fault: classify_upstream_status(status, meta.retry_after_secs),
                detail: Box::new(RefusalDetail::new(&meta, &[], false)),
            });
        }
    };
    match serde_json::from_slice::<Value>(&raw) {
        Ok(v) => Ok((status, meta, v)),
        Err(e) if success => Err(ProviderError::MeteringFailed(format!(
            "2xx response body did not parse: {e}"
        ))),
        Err(_) => Err(ProviderError::Refused {
            status,
            fault: classify_upstream_status(status, meta.retry_after_secs),
            detail: Box::new(RefusalDetail::new(&meta, &raw, true)),
        }),
    }
}

/// Whether the body opts any block into prompt caching, i.e. carries a
/// `cache_control` key anywhere.
///
/// Used to decide whether a request could produce a cache WRITE, which bills
/// above the input rate and so has to be reserved for. A request with no
/// breakpoint structurally cannot write, and reserving it at the write rate
/// would inflate every request on the provider for nothing.
///
/// Walks the PARSED JSON for the same reason [`references_external_media`] does:
/// `"cache_control"` is legal JSON that decodes to the real key, so a raw
/// substring scan is evadable and would UNDER-reserve, which is the direction
/// that breaks a hard cap.
///
/// Matching the key anywhere rather than only in its legal positions is
/// deliberate. A superset match can only over-reserve, and it does not rot every
/// time the provider adds a block type that accepts a breakpoint.
#[must_use]
pub fn requests_prompt_cache_write(body: &str) -> bool {
    serde_json::from_str::<Value>(body).is_ok_and(|v| value_has_cache_control(&v))
}

fn value_has_cache_control(v: &Value) -> bool {
    match v {
        Value::Object(map) => map.iter().any(|(k, val)| {
            k.eq_ignore_ascii_case("cache_control") || value_has_cache_control(val)
        }),
        Value::Array(items) => items.iter().any(value_has_cache_control),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

/// Adapter for the Anthropic Messages API.
pub struct AnthropicProvider {
    http: reqwest::Client,
    /// Streaming client: NO total request timeout, since a stream is expected to
    /// be long-lived. The buffered `http` client's total timeout would sever one
    /// mid-body. Bounded instead by the relay's idle and max-duration guards.
    stream_http: reqwest::Client,
    api_key: String,
    base_url: String,
    version: String,
}

impl AnthropicProvider {
    #[must_use]
    pub fn new(
        http: reqwest::Client,
        stream_http: reqwest::Client,
        api_key: String,
        base_url: String,
        version: String,
    ) -> Self {
        Self {
            http,
            stream_http,
            api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            version,
        }
    }

    /// Parse a request that IS allowed to stream.
    ///
    /// # Errors
    /// Returns [`ProviderError::BadRequest`] if the body cannot be metered.
    pub fn parse_streaming(
        &self,
        rest_path: &str,
        body: &str,
    ) -> Result<ParsedRequest, ProviderError> {
        self.parse_common(rest_path, body, true)
    }

    /// Open the upstream SSE stream. Forwards the body verbatim: unlike the
    /// OpenAI path there is no cap field to normalise, because Anthropic already
    /// requires `max_tokens` and `parse_common` refused the request without it,
    /// so reserved and enforced already agree.
    ///
    /// `anthropic_beta` is the handler's validated, joined beta list (see
    /// [`joined_anthropic_betas`]).
    ///
    /// # Errors
    /// Returns a provider error if the upstream call fails.
    pub async fn forward_stream(
        &self,
        body: &str,
        anthropic_beta: Option<&str>,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);
        let mut request = self
            .stream_http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .header("content-type", "application/json");
        if let Some(beta) = anthropic_beta {
            request = request.header("anthropic-beta", beta);
        }
        request
            .body(pin_outbound(body, true))
            .send()
            .await
            .map_err(|e| classify_transport_error(&e))
    }
}

/// Force `service_tier: "standard_only"` on an outbound Messages body.
///
/// An ABSENT tier means "auto", which uses priority capacity at a premium where
/// an organisation has it. So refusing only an explicit non-standard value, as
/// `parse_common` does, leaves the common case unmetered: almost nobody sets the
/// field. Pinning it here rather than in a route handler means BOTH the native
/// `/v1/messages` route and the legacy `/v1/anthropic/messages` route get it,
/// and it cannot be forgotten by a future third caller.
///
/// Mirrors how the OpenAI adapter already pins the outbound `max_tokens` so that
/// reserved and enforced agree. A body that will not parse is passed through
/// untouched: `parse_common` has already rejected it, so this cannot be the
/// thing that lets a bad body upstream.
fn pin_outbound(body: &str, streaming: bool) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(body) else {
        return body.to_owned();
    };
    let Some(obj) = v.as_object_mut() else {
        return body.to_owned();
    };
    obj.insert(
        "service_tier".to_owned(),
        Value::String("standard_only".to_owned()),
    );
    // Pin `stream` to a real boolean matching the path we chose.
    //
    // `stream_requested` is deliberately lenient (it accepts "yes", 1, " TRUE ")
    // so a truthy value cannot slip past the buffered check. That leniency means
    // the value we route on can differ from what a strict upstream would honour:
    // route to the streaming relay on `"yes"`, forward `"yes"` verbatim, and an
    // upstream that treats it as false returns a buffered body which the relay
    // then forwards with no SSE events in it. Pinning removes the disagreement,
    // the same way the OpenAI adapter pins its own stream flag.
    obj.insert("stream".to_owned(), Value::Bool(streaming));
    serde_json::to_string(&v).unwrap_or_else(|_| body.to_owned())
}

/// Anthropic reports prompt-side tokens as three DISJOINT classes:
///
/// ```text
/// total prompt = input_tokens + cache_read_input_tokens + cache_creation_input_tokens
/// ```
///
/// `input_tokens` counts ONLY the tokens after the last cache breakpoint, so a
/// request that hits a large cached prefix reports a tiny `input_tokens` while
/// the provider bills for the whole prompt. Reading that field alone under-counts
/// a cached request by up to ~99%.
///
/// All three classes are billed here at the plain input rate. That is a stopgap,
/// not the final answer: the real rates differ (reads are ~0.1x the input rate,
/// writes 1.25x for a 5-minute TTL and 2x for an hour). So this over-charges
/// reads and still under-charges writes. Both errors are bounded and the larger
/// one is conservative, where ignoring the classes entirely was neither. Pricing
/// each class at its own rate needs per-class rate columns in `model_prices`.
///
/// `output_tokens` needs no adjustment: Anthropic documents it as the inclusive,
/// authoritative billing total, already covering extended-thinking tokens. Adding
/// `output_tokens_details.thinking_tokens` to it would double-count.
fn parse_anthropic_usage(v: &Value) -> Usage {
    let u = v.get("usage");
    let get = |k: &str| {
        u.and_then(|u| u.get(k))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    // Anthropic's classes are ALREADY disjoint, which is exactly the shape
    // `Usage` wants, so they pass straight through with no arithmetic. This is
    // the one provider that needs no conversion.
    Usage::with_cache(
        get("input_tokens"),
        get("output_tokens"),
        get("cache_read_input_tokens"),
        get("cache_creation_input_tokens"),
    )
}

/// `anthropic-beta` values known NOT to change how a request is billed.
///
/// A beta can change the rate: the 1M-context beta bills input above 200k tokens
/// at twice the standard rate, which a single input rate cannot express. So
/// unknown betas are refused rather than forwarded.
///
/// The list is seeded rather than empty because real clients send betas on every
/// request. Claude Code sends several, and an empty allowlist would refuse the
/// most obvious user of this route on day one.
const ALLOWED_ANTHROPIC_BETAS: &[&str] = &[
    "prompt-caching-2024-07-31",
    "pdfs-2024-09-25",
    "token-counting-2024-11-01",
    "fine-grained-tool-streaming-2025-05-14",
    "interleaved-thinking-2025-05-14",
];

/// Whether every beta in an `anthropic-beta` header value is known to be
/// billing-neutral. The header may carry a comma-separated list, and a client
/// may send the header more than once, so callers check each value.
///
/// Returns the first unrecognised beta. It is the client's string, so the
/// refusal does not repeat it.
#[must_use]
pub fn unknown_anthropic_beta(header_value: &str) -> Option<String> {
    header_value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .find(|s| !ALLOWED_ANTHROPIC_BETAS.contains(s))
        .map(str::to_owned)
}

/// The betas to forward, from every `anthropic-beta` header value: split on
/// commas, trimmed, deduplicated in first-seen order and joined with `,`.
/// `None` when there are none.
///
/// Callers pass only values that [`unknown_anthropic_beta`] has cleared, so
/// what is forwarded is always on the allowlist.
#[must_use]
pub fn joined_anthropic_betas<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut betas: Vec<&str> = Vec::new();
    for value in values {
        for beta in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !betas.contains(&beta) {
                betas.push(beta);
            }
        }
    }
    (!betas.is_empty()).then(|| betas.join(","))
}

impl AnthropicProvider {
    /// Shared parse for both the buffered and streaming paths, so a guard added
    /// to one can never be missing from the other.
    fn parse_common(
        &self,
        rest_path: &str,
        body: &str,
        allow_stream: bool,
    ) -> Result<ParsedRequest, ProviderError> {
        // Allowlist: only the synchronous Messages endpoint. Anything else
        // (batches, files, models) would settle to ~zero cost while spending
        // real money upstream.
        if safe_rest_path(rest_path)? != "messages" {
            return Err(ProviderError::BadRequest(
                "only the messages endpoint is supported".to_owned(),
            ));
        }
        let v: Value =
            serde_json::from_str(body).map_err(|e| ProviderError::BadRequest(e.to_string()))?;
        // The body is re-serialised before it is forwarded (`pin_outbound`).
        reject_lossy_integers(body)?;
        if !allow_stream && v.get("stream").and_then(Value::as_bool) == Some(true) {
            return Err(ProviderError::BadRequest(
                "streaming is handled on a separate path".to_owned(),
            ));
        }
        // External media has token cost unbounded by body size, so the fast
        // estimate would under-reserve. The OpenAI adapter checks this itself;
        // this path relied on the gateway's check, which the streaming handler
        // bypasses, so a streamed request with a URL image could have reserved
        // from body bytes and settled far above it.
        if references_external_media(body) {
            return Err(ProviderError::BadRequest(
                "external media (url / file references) is not supported on this endpoint; \
                 its token cost is not bounded by the request body"
                    .to_owned(),
            ));
        }
        let model = v
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::BadRequest("missing 'model'".to_owned()))?
            .to_owned();
        let max_output_tokens = v
            .get("max_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| ProviderError::BadRequest("missing 'max_tokens'".to_owned()))?;
        // Server-side tools (web search, code execution) carry per-call or
        // per-session fees that appear in no token count, so they cannot be
        // metered from usage at all. Client-executed custom tools are token-only
        // and stay allowed.
        if let Some(tools) = v.get("tools").and_then(Value::as_array) {
            for t in tools {
                if let Some(ty) = t.get("type").and_then(Value::as_str) {
                    if !ty.eq_ignore_ascii_case("custom") {
                        return Err(ProviderError::BadRequest(
                            "server-side tools are not supported: they are billed per call or \
                             per session, which no token count reports, so Tollgate would \
                             under-charge. Client-executed custom tools are supported."
                                .to_owned(),
                        ));
                    }
                }
            }
        }
        // A 1-hour cache write bills at twice the input rate, a 5-minute write at
        // 1.25x, against one configured write rate. An operator who set the 5m
        // rate would reserve at 1.25x and settle at 2x, overshooting a hard cap
        // by the difference on every request.
        // Fixed text: the TTL is the client's.
        if has_non_default_cache_ttl(&v) {
            return Err(bad(
                "a cache_control ttl other than 5m is not supported: a longer TTL bills at a \
                 different multiple of the input rate than the default, and one \
                 configured cache-write rate cannot express both",
            ));
        }
        // Priority tier bills the same tokens at a premium.
        if let Some(t) = v.get("service_tier").and_then(Value::as_str) {
            if !t.eq_ignore_ascii_case("standard_only") {
                return Err(ProviderError::BadRequest(
                    "service_tier must be 'standard_only': other tiers change the \
                     per-token rate, which a single price cannot express"
                        .to_owned(),
                ));
            }
        }
        Ok(ParsedRequest {
            model,
            estimated_input_tokens: estimate_input_tokens(body),
            max_output_tokens,
            // Only a request carrying a cache breakpoint can produce a cache
            // write, so only that request is reserved at the write rate.
            may_cache_write: value_has_cache_control(&v),
            anthropic_beta: None,
            // Refused above.
            external_media: false,
        })
    }
}

/// Meters an Anthropic SSE stream.
///
/// Anthropic's usage is NOT terminal, which is the whole difficulty. Observed
/// from a live stream:
///
/// - `message_start` carries the input classes AND `output_tokens: 1`, which is
///   a placeholder, not a count.
/// - `message_delta` repeats the input classes and carries the real, cumulative
///   output. Cumulative means the LAST one wins; summing them over-charges.
/// - `message_stop` is the final event and carries no usage.
/// - `ping` events and bare `event:` lines are interleaved throughout.
///
/// The placeholder is why "usage seen" must mean `message_stop` reached rather
/// than "some event carried a usage object". A stream that dies after
/// `message_start` has a usage object in hand reporting ONE output token for
/// what may have been a full response. Treating that as a clean finish would
/// under-charge by the entire output leg. The OpenAI path guards the same shape
/// by requiring a finish, a usage report in or after it, and `[DONE]` (see
/// `app::OpenAiStreamMeter`).
#[derive(Debug, Default)]
pub struct AnthropicStreamMeter {
    start: Option<Usage>,
    last_delta: Option<Usage>,
    errored: bool,
    stopped: bool,
}

impl AnthropicStreamMeter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one SSE line. Ignores `event:` lines, pings, comments and blanks.
    pub fn observe_line(&mut self, line: &str) {
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                    self.start = Some(parse_anthropic_usage_obj(u));
                }
            }
            Some("message_delta") => {
                if let Some(u) = v.get("usage") {
                    // Cumulative: overwrite rather than accumulate.
                    self.last_delta = Some(parse_anthropic_usage_obj(u));
                }
            }
            Some("message_stop") => self.stopped = true,
            // An `error` event on an otherwise-200 stream (overloaded_error is
            // the common one). The transport then closes normally, so without
            // this the relay would see a clean EOF and settle as if finished.
            Some("error") => self.errored = true,
            _ => {}
        }
    }

    /// True only when the stream genuinely completed.
    ///
    /// Deliberately NOT transport EOF: an error event followed by a normal close
    /// is an EOF, and a `message_stop` followed by a held-open socket is not.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.stopped && !self.errored
    }

    /// Whether anything was generated at all. An error before `message_start`
    /// means the provider produced nothing, so the caller can charge the input
    /// floor instead of the full reservation.
    #[must_use]
    pub fn started(&self) -> bool {
        self.start.is_some()
    }

    /// Whether the provider EXPLICITLY reported a failure.
    ///
    /// The prompt-leg discount requires this, not merely the absence of
    /// `message_start`. Without it, any 200 that is not an SSE stream at all
    /// (an upstream that ignores `stream` and returns a buffered body, a proxy
    /// that rewrites it) would be charged the prompt leg alone, because it
    /// contains neither `message_start` nor `message_stop`. The provider has to
    /// tell us it failed before we discount the request.
    #[must_use]
    pub fn errored(&self) -> bool {
        self.errored
    }

    /// The metered usage: the maximum of each class across `message_start` and
    /// the final `message_delta`.
    ///
    /// Max rather than "trust the delta" because both events carry the input
    /// classes and taking the larger can only over-charge. A class that DECREASES
    /// between them is self-contradictory, and no real version does that, so it
    /// is marked suspect and settles at the reservation rather than being costed
    /// on numbers that cannot both be true.
    #[must_use]
    pub fn usage(&self) -> Option<Usage> {
        let (s, d) = match (self.start, self.last_delta) {
            (None, None) => return None,
            (s, d) => (s.unwrap_or_default(), d.unwrap_or_default()),
        };
        let decreased = self.start.is_some()
            && self.last_delta.is_some()
            && (d.input_tokens < s.input_tokens
                || d.cache_read_tokens < s.cache_read_tokens
                || d.cache_write_tokens < s.cache_write_tokens);
        let u = Usage::with_cache(
            s.input_tokens.max(d.input_tokens),
            s.output_tokens.max(d.output_tokens),
            s.cache_read_tokens.max(d.cache_read_tokens),
            s.cache_write_tokens.max(d.cache_write_tokens),
        );
        Some(if decreased {
            tracing::warn!(
                "anthropic stream reported a DECREASING input class between \
                            message_start and message_delta; usage is self-contradictory"
            );
            u.into_suspect()
        } else {
            u
        })
    }
}

/// Parse a bare Anthropic `usage` object (not a whole response body).
fn parse_anthropic_usage_obj(u: &Value) -> Usage {
    let get = |k: &str| u.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    Usage::with_cache(
        get("input_tokens"),
        get("output_tokens"),
        get("cache_read_input_tokens"),
        get("cache_creation_input_tokens"),
    )
}

/// Build the body for Vertex `:countTokens` from an ALLOWLIST.
///
/// Same lesson as [`count_tokens_payload`], which the Anthropic adapter learned
/// against the live API: a count endpoint is not the generate endpoint, and
/// posting the caller's body unchanged puts fields in front of a parser that
/// rejects unknown names with a 400.
///
/// Verified against live Vertex. `:countTokens` ACCEPTS `contents`,
/// `systemInstruction`, `tools` and `generationConfig`, and REJECTS
/// `toolConfig`, `safetySettings`, `labels` and `cachedContent`, each with
/// `Invalid JSON payload received. Unknown name "X": Cannot find field`. An
/// SDK-built `generateContent` body routinely carries `safetySettings`, so
/// forwarding it unchanged, which is what this adapter used to do, refused every
/// request under `exact` admission.
///
/// The first version of this allowlist included `toolConfig` on the reasoning
/// that it accompanies `tools`. It does not exist here, and would have failed
/// every request carrying it. Do not add a field to this list without probing
/// it: `scripts/probe-count-endpoints.sh`.
///
/// `systemInstruction` and `tools` are carried because they are COUNTED, not
/// merely tolerated: live, one bare user message counted 1 token, the same
/// message with a system instruction counted 16, and with a function
/// declaration added counted 32. Dropping them would under-reserve every
/// tool-heavy request, the exact failure `exact` admission exists to prevent.
///
/// `generationConfig` is carried, against the obvious reading. It looks like it
/// describes only the OUTPUT, and the first version of this dropped it on
/// exactly that reasoning. The probe says otherwise: a `responseSchema` lives
/// inside it and is counted as prompt material. Live, the same one-word message
/// counted 1 token bare and 51 with a small four-property response schema
/// attached. Dropping it under-reserves every structured-output request by the
/// size of its schema, which is the failure `exact` admission exists to
/// prevent. Reasoning about which fields "cannot" affect an input count is how
/// that bug got written; measure instead.
///
/// `cachedContent` never reaches this function: `check_vertex_allowlist`
/// refuses a request naming a stored context cache before any count or
/// forward, because its small body hides a large metered prompt that
/// `:countTokens` cannot size.
fn vertex_count_payload(v: &Value) -> Result<Value, ProviderError> {
    let mut out = serde_json::Map::new();
    for k in ["contents", "tools", "generationConfig"] {
        if let Some(val) = v.get(k) {
            if !val.is_null() {
                out.insert(k.to_owned(), val.clone());
            }
        }
    }
    // Google's parser accepts either spelling, and its own REST examples use the
    // snake form, so a caller can legitimately send `system_instruction` beside
    // a camelCase `generationConfig`. Reading only one spelling drops the system
    // prompt from the count while the request still carries it, which
    // under-reserves by exactly the tokens this allowlist exists to include: a
    // live probe measured a system instruction at 15 of 16 tokens.
    for k in ["systemInstruction", "system_instruction"] {
        if let Some(val) = v.get(k) {
            if !val.is_null() {
                out.insert("systemInstruction".to_owned(), val.clone());
                break;
            }
        }
    }
    if !out.contains_key("contents") {
        return Err(ProviderError::BadRequest(
            "countTokens needs 'contents'".to_owned(),
        ));
    }
    Ok(Value::Object(out))
}

/// Build the body for `/v1/messages/count_tokens` from an ALLOWLIST.
///
/// The client's Messages body cannot be posted here unchanged. `count_tokens`
/// rejects unknown fields, and `max_tokens` is REQUIRED on every Messages
/// request, so forwarding the body verbatim returns
/// `400 max_tokens: Extra inputs are not permitted` on literally every request.
/// Exact admission then fails closed and the gateway refuses all Anthropic
/// traffic. Verified against the live API; it had never been exercised.
///
/// Everything that contributes input tokens is carried over. `system` and
/// `tools` matter especially: a live probe counted 8 tokens for a bare message
/// pair and 541 with system and tools attached, so dropping them would
/// under-reserve tool-heavy requests badly, which is the failure exact admission
/// exists to prevent.
fn count_tokens_payload(body: &str) -> Result<Value, ProviderError> {
    let v: Value =
        serde_json::from_str(body).map_err(|e| ProviderError::BadRequest(e.to_string()))?;
    let mut out = serde_json::Map::new();
    // Fields the count endpoint accepts AND that affect the input token count.
    for k in [
        "model",
        "messages",
        "system",
        "tools",
        "tool_choice",
        "thinking",
    ] {
        if let Some(val) = v.get(k) {
            if !val.is_null() {
                out.insert(k.to_owned(), val.clone());
            }
        }
    }
    if !out.contains_key("model") || !out.contains_key("messages") {
        return Err(ProviderError::BadRequest(
            "count_tokens needs 'model' and 'messages'".to_owned(),
        ));
    }
    Ok(Value::Object(out))
}

/// Whether any `cache_control` block carries a `ttl` other than the default.
///
/// The caller refuses it: TTL selects a billing multiple, and the price book
/// has one cache-write rate.
fn has_non_default_cache_ttl(v: &Value) -> bool {
    match v {
        Value::Object(map) => map.iter().any(|(k, val)| {
            let longer = k.eq_ignore_ascii_case("cache_control")
                && val
                    .get("ttl")
                    .and_then(Value::as_str)
                    .is_some_and(|ttl| !ttl.eq_ignore_ascii_case("5m"));
            longer || has_non_default_cache_ttl(val)
        }),
        Value::Array(items) => items.iter().any(has_non_default_cache_ttl),
        _ => false,
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    /// Anthropic is the one adapter that reports cache writes: a request marking
    /// `cache_control` breakpoints returns `cache_creation_input_tokens`, billed
    /// above the base input rate.
    fn can_report_cache_write(&self) -> bool {
        true
    }

    fn parse_request(&self, rest_path: &str, body: &str) -> Result<ParsedRequest, ProviderError> {
        self.parse_common(rest_path, body, false)
    }

    async fn forward(
        &self,
        rest_path: &str,
        body: &str,
        parsed: &ParsedRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let path = safe_rest_path(rest_path)?;
        let url = format!("{}/v1/{path}", self.base_url);
        let mut request = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .header("content-type", "application/json");
        // Validated against the allowlist by the native handler. Not sent on
        // the token count: every allowlisted beta is billing-neutral, and the
        // count matched real usage without them (probe P10).
        if let Some(beta) = parsed.anthropic_beta.as_deref() {
            request = request.header("anthropic-beta", beta);
        }
        let resp = request
            .body(pin_outbound(body, false))
            .send()
            .await
            .map_err(|e| classify_transport_error(&e))?;
        let (status, meta, json) = json_or_classified(resp).await?;
        let usage = parse_anthropic_usage(&json);
        Ok(ProviderResponse {
            status,
            body: json,
            usage,
            meta,
        })
    }

    async fn count_input_tokens(
        &self,
        _rest_path: &str,
        body: &str,
        _parsed: &ParsedRequest,
    ) -> Result<u64, ProviderError> {
        let url = format!("{}/v1/messages/count_tokens", self.base_url);
        let payload = count_tokens_payload(body)?;
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        let status = resp.status();
        let meta = ResponseMeta::from_headers(resp.headers());
        // Bytes then parse, rather than `.json()`: a refusal does not always
        // come back as JSON. An edge proxy or a WAF in front of the provider
        // answers a 400 with an HTML page, and letting the decode error decide
        // would classify the caller's malformed body as an upstream outage,
        // which an SDK then retries.
        let raw = resp
            .bytes()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        if let Some(e) = count_refusal(status, &meta, &raw, "count_tokens") {
            return Err(e);
        }
        let json: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        // Fail closed: a missing field must not silently downgrade to the weak
        // fast estimate (that would reopen the under-reservation hole).
        json.get("input_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ProviderError::Upstream("count_tokens returned no input_tokens".to_owned())
            })
    }
}

// ---------------------------------------------------------------------------
// Native Vertex request checks
// ---------------------------------------------------------------------------

/// The refusal for a `fileData` URI that is not allowed. Fixed text: it never
/// carries the URI or the bucket, which can name the operator's storage.
pub const FORBIDDEN_SOURCE: &str = "a fileData URI names a source this gateway does not allow: \
     gs:// URIs outside the operator's allowlisted buckets, Google storage and API hosts, \
     URIs carrying credentials, and other schemes are refused";

/// The refusal for an `https://` `fileData` URI that is not otherwise
/// forbidden. Remote media is not supported yet: its size is unbounded, and
/// what the URL serves can change between the token count and the generation,
/// so neither admission mode can bound it.
pub const REMOTE_MEDIA_UNSUPPORTED: &str = "https:// fileData URIs are not supported yet: \
     remote media is unbounded and can change between the token count and the generation. \
     Use a gs:// URI in a bucket the operator has allowlisted";

/// A JSON value parsed with duplicate object keys refused.
///
/// `serde_json::Value` keeps the LAST of two equal keys, while another parser
/// may keep the first, so a body with duplicates can mean one thing to the
/// checks here and another to the provider. Refusing them makes the checked
/// value the only reading. Nesting depth stays bounded by serde_json's own
/// recursion limit, since every level goes back through `deserialize_any`.
struct StrictValue(Value);

impl<'de> serde::Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(StrictValue)
    }
}

struct StrictVisitor;

impl<'de> serde::de::Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Value, E> {
        // JSON cannot spell a non-finite number, so the fallback is unreachable.
        Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(StrictValue(v)) = seq.next_element()? {
            items.push(v);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if out.contains_key(&key) {
                // Fixed text: the key is the client's.
                return Err(serde::de::Error::custom("duplicate key in a JSON object"));
            }
            let StrictValue(v) = map.next_value()?;
            out.insert(key, v);
        }
        Ok(Value::Object(out))
    }
}

/// Parse a body, refusing duplicate keys anywhere in it.
fn parse_strict(body: &str) -> Result<Value, ProviderError> {
    let v = serde_json::from_str::<StrictValue>(body)
        .map(|StrictValue(v)| v)
        .map_err(|e| ProviderError::BadRequest(e.to_string()))?;
    reject_lossy_integers(body)?;
    Ok(v)
}

/// The canonical `lowerCamelCase` spelling of a key in a Google-defined
/// object, or `None` for a key Tollgate will not forward.
///
/// Only two spellings are accepted: strict lowerCamel (`^[a-z][a-zA-Z0-9]*$`),
/// returned as is, and strict snake_case (`^[a-z][a-z0-9]*(_[a-z0-9]+)+$`),
/// rewritten as Google's JSON parser reads it (`max_output_tokens` becomes
/// `maxOutputTokens`). Google's parser also accepts looser spellings
/// (`file__data`, `File_Data`, `file_data_`), but which field it maps each to
/// is not documented, so they are refused rather than guessed.
fn camel_case(key: &str) -> Option<String> {
    let b = key.as_bytes();
    if !b.first().is_some_and(u8::is_ascii_lowercase) {
        return None;
    }
    if b.iter().all(u8::is_ascii_alphanumeric) {
        return Some(key.to_owned());
    }
    let snake = key.split('_').all(|segment| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    });
    if !snake {
        return None;
    }
    let mut segments = key.split('_');
    let mut out = segments.next().unwrap_or("").to_owned();
    for segment in segments {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.push_str(chars.as_str());
        }
    }
    Some(out)
}

/// How the value under one canonical key is handled.
type Nested = fn(&str, Value) -> Result<Value, ProviderError>;

/// Rewrite one Google-defined object's own keys to camelCase, handing each
/// value to `nested` by its canonical key. Two keys that name the same field
/// (`maxOutputTokens` beside `max_output_tokens`) are refused: Google would
/// pick one, and the checks here must see the one it picks. So are two keys
/// that differ only in case or underscores (`fileDATA` beside `fileData`,
/// `pARTS` beside `parts`), compared as [`loose_key`] compares them: which of
/// them Google reads is not documented. A value that is not an object is
/// returned unchanged; the provider refuses wrong types.
fn canon_object(v: Value, nested: Nested) -> Result<Value, ProviderError> {
    let Value::Object(map) = v else {
        return Ok(v);
    };
    let mut loose = std::collections::HashSet::new();
    let mut out = serde_json::Map::new();
    for (k, val) in map {
        if !loose.insert(loose_key(&k)) {
            // Fixed text: the keys are the client's.
            return Err(bad(
                "two keys in one Google-defined object differ only in case or underscores; \
                 send one",
            ));
        }
        let Some(key) = camel_case(&k) else {
            // Fixed text: the key is the client's.
            return Err(bad(
                "a field name in a Google-defined object must be lowerCamelCase or \
                 snake_case",
            ));
        };
        let val = nested(&key, val)?;
        if out.insert(key, val).is_some() {
            return Err(ProviderError::BadRequest(
                "two keys in one object name the same field (a snake_case and a camelCase \
                 spelling); send one"
                    .to_owned(),
            ));
        }
    }
    Ok(Value::Object(out))
}

/// Apply `f` to each element of an array; any other value is unchanged.
fn canon_each(
    v: Value,
    f: fn(Value) -> Result<Value, ProviderError>,
) -> Result<Value, ProviderError> {
    match v {
        Value::Array(items) => items
            .into_iter()
            .map(f)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other),
    }
}

/// Leave a value as the client sent it: client-owned maps (`args`, schemas,
/// `labels`) are never rewritten, so a tool parameter called `user_id` beside
/// one called `userId` stays two parameters.
fn keep(_key: &str, v: Value) -> Result<Value, ProviderError> {
    Ok(v)
}

fn keep_object(v: Value) -> Result<Value, ProviderError> {
    canon_object(v, keep)
}

fn canon_top(key: &str, v: Value) -> Result<Value, ProviderError> {
    match key {
        "generationConfig" => canon_object(v, canon_generation_config),
        "toolConfig" => canon_object(v, canon_tool_config),
        "systemInstruction" => canon_object(v, canon_content),
        "contents" => canon_each(v, canon_content_entry),
        "tools" => canon_each(v, canon_tool_entry),
        "safetySettings" => canon_each(v, keep_object),
        _ => Ok(v),
    }
}

fn canon_generation_config(key: &str, v: Value) -> Result<Value, ProviderError> {
    match key {
        // Not `responseSchema` / `responseJsonSchema`: those are the client's.
        "thinkingConfig" => canon_object(v, keep),
        _ => Ok(v),
    }
}

fn canon_tool_config(key: &str, v: Value) -> Result<Value, ProviderError> {
    match key {
        "functionCallingConfig" => canon_object(v, keep),
        _ => Ok(v),
    }
}

fn canon_content_entry(v: Value) -> Result<Value, ProviderError> {
    canon_object(v, canon_content)
}

fn canon_content(key: &str, v: Value) -> Result<Value, ProviderError> {
    match key {
        "parts" => canon_each(v, canon_part_entry),
        _ => Ok(v),
    }
}

fn canon_part_entry(v: Value) -> Result<Value, ProviderError> {
    canon_object(v, canon_part)
}

fn canon_part(key: &str, v: Value) -> Result<Value, ProviderError> {
    match key {
        // Their own keys only: `functionCall.args` is the client's.
        "inlineData"
        | "fileData"
        | "functionCall"
        | "videoMetadata"
        | "executableCode"
        | "codeExecutionResult"
        | "mediaResolution" => canon_object(v, keep),
        // Its own keys, and the media parts it may carry; `response` is the
        // client's.
        "functionResponse" => canon_object(v, canon_function_response),
        _ => Ok(v),
    }
}

fn canon_function_response(key: &str, v: Value) -> Result<Value, ProviderError> {
    match key {
        "parts" => canon_each(v, canon_part_entry),
        _ => Ok(v),
    }
}

fn canon_tool_entry(v: Value) -> Result<Value, ProviderError> {
    canon_object(v, canon_tool)
}

fn canon_tool(key: &str, v: Value) -> Result<Value, ProviderError> {
    match key {
        // Each declaration's own keys; its `parameters`, `parametersJsonSchema`
        // and `response` schemas are the client's.
        "functionDeclarations" => canon_each(v, keep_object),
        _ => Ok(v),
    }
}

/// Top-level fields a native Vertex request may carry.
///
/// Everything else is refused. `cachedContent` names the operator's caches and
/// cannot be sized; `routingConfig` and `modelConfig` can send the request to a
/// model other than the one priced; `model` in the body is refused because the
/// path names the model; unknown fields cannot be metered.
const VERTEX_TOP_LEVEL: &[&str] = &[
    "contents",
    "systemInstruction",
    "generationConfig",
    "tools",
    "toolConfig",
    "safetySettings",
    "labels",
];

/// `generationConfig` fields that leave the rate and the model alone.
/// `candidateCount`, `responseModalities` and `thinkingConfig` are allowed
/// with the further checks in [`check_generation_config`]. Everything else,
/// such as `speechConfig`, `imageConfig`, `audioTimestamp`, `routingConfig` and
/// `enableAffectiveDialog`, is refused.
const VERTEX_GENERATION_CONFIG: &[&str] = &[
    "temperature",
    "topP",
    "topK",
    "maxOutputTokens",
    "stopSequences",
    "responseMimeType",
    "responseSchema",
    "responseJsonSchema",
    "responseFormat",
    "presencePenalty",
    "frequencyPenalty",
    "seed",
    "responseLogprobs",
    "logprobs",
    "mediaResolution",
    "thinkingConfig",
    "responseModalities",
    "candidateCount",
];

const VERTEX_THINKING_CONFIG: &[&str] = &["includeThoughts", "thinkingBudget", "thinkingLevel"];

/// Label limits: 63 characters per key and value is Google's documented limit;
/// 64 labels is Tollgate's own cap.
const MAX_LABELS: usize = 64;
const MAX_LABEL_CHARS: usize = 63;

fn bad(message: &str) -> ProviderError {
    ProviderError::BadRequest(message.to_owned())
}

/// Check a canonical native Vertex body against the allowlist. `labels` are
/// accepted only when the operator allowed them.
fn check_vertex_allowlist(v: &Value, allow_labels: bool) -> Result<(), ProviderError> {
    let Some(top) = v.as_object() else {
        return Err(bad("the request body must be a JSON object"));
    };
    if top.keys().any(|k| !VERTEX_TOP_LEVEL.contains(&k.as_str())) {
        return Err(bad(
            "native Vertex requests accept only these top-level fields: contents, \
             systemInstruction, generationConfig, tools, toolConfig, safetySettings, labels. \
             Others (cachedContent, routingConfig, modelConfig, model, ...) are refused: they \
             name the operator's caches, change the model that is billed, or cannot be metered",
        ));
    }
    if let Some(tools) = top.get("tools") {
        let Some(tools) = tools.as_array() else {
            return Err(bad("tools must be an array"));
        };
        for tool in tools {
            let Some(tool) = tool.as_object() else {
                return Err(bad("each tools entry must be an object"));
            };
            if tool.keys().any(|k| k != "functionDeclarations") {
                return Err(bad(
                    "only functionDeclarations tools are supported: server-side tools \
                     (search, retrieval, code execution, URL context, maps, computer use) \
                     bill per call or read the operator's data, which no token count reports",
                ));
            }
        }
    }
    if let Some(tool_config) = top.get("toolConfig") {
        let ok = tool_config
            .as_object()
            .is_some_and(|m| m.keys().all(|k| k == "functionCallingConfig"));
        if !ok {
            return Err(bad(
                "toolConfig must be an object carrying only functionCallingConfig",
            ));
        }
    }
    if let Some(labels) = top.get("labels") {
        if !allow_labels {
            return Err(bad(
                "labels are not accepted: they land in the operator's billing export. The \
                 operator can allow them with providers.vertex.allow_client_labels",
            ));
        }
        check_labels(labels)?;
    }
    if let Some(generation_config) = top.get("generationConfig") {
        check_generation_config(generation_config)?;
    }
    Ok(())
}

/// Labels land in the operator's billing export, so they are bounded: each key
/// and value at most 63 characters (Google's documented limit), and at most 64
/// labels (Tollgate's own cap; Google documents no count here).
fn check_labels(labels: &Value) -> Result<(), ProviderError> {
    let refuse = || {
        bad(
            "labels must be an object of at most 64 string values, each key and value at \
             most 63 characters",
        )
    };
    let Some(map) = labels.as_object() else {
        return Err(refuse());
    };
    if map.len() > MAX_LABELS {
        return Err(refuse());
    }
    for (k, val) in map {
        let Some(val) = val.as_str() else {
            return Err(refuse());
        };
        if k.chars().count() > MAX_LABEL_CHARS || val.chars().count() > MAX_LABEL_CHARS {
            return Err(refuse());
        }
    }
    Ok(())
}

fn check_generation_config(generation_config: &Value) -> Result<(), ProviderError> {
    let Some(map) = generation_config.as_object() else {
        return Err(bad("generationConfig must be an object"));
    };
    if map
        .keys()
        .any(|k| !VERTEX_GENERATION_CONFIG.contains(&k.as_str()))
    {
        return Err(bad(
            "generationConfig carries a field Tollgate does not allow: speech, image and \
             audio output and routing settings change the rate or the model that is billed, \
             so only the text-generation fields listed in the README are accepted",
        ));
    }
    // Each candidate is a separate generation up to `maxOutputTokens`, so more
    // than one multiplies output beyond the reservation.
    if map
        .get("candidateCount")
        .is_some_and(|n| n.as_u64() != Some(1))
    {
        return Err(bad(
            "generationConfig.candidateCount must be absent or 1: each candidate multiplies \
             output beyond the reserved cap",
        ));
    }
    // Audio and image output bill at other rates than text.
    if let Some(m) = map.get("responseModalities") {
        let text_only = m.as_array().is_some_and(|a| {
            a.len() == 1
                && a[0]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case("TEXT"))
        });
        if !text_only {
            return Err(bad(
                "generationConfig.responseModalities must be absent or [\"TEXT\"]: other \
                 output modalities bill at a different rate",
            ));
        }
    }
    if let Some(thinking) = map.get("thinkingConfig") {
        let Some(thinking) = thinking.as_object() else {
            return Err(bad("generationConfig.thinkingConfig must be an object"));
        };
        if thinking
            .keys()
            .any(|k| !VERTEX_THINKING_CONFIG.contains(&k.as_str()))
        {
            return Err(bad(
                "thinkingConfig accepts only includeThoughts, thinkingBudget and thinkingLevel",
            ));
        }
        if let Some(budget) = thinking.get("thinkingBudget") {
            explicit_thinking_budget(budget)?;
        }
    }
    Ok(())
}

/// Read a client's thinking budget: `Some(n)` for a positive budget, `None`
/// for 0 or -1 (dynamic). Any other negative or a non-integer is refused.
fn explicit_thinking_budget(budget: &Value) -> Result<Option<u64>, ProviderError> {
    match budget.as_i64() {
        Some(-1 | 0) => Ok(None),
        Some(n) if n > 0 => Ok(u64::try_from(n).ok()),
        _ => match budget.as_u64() {
            // Above i64::MAX: still a positive integer.
            Some(n) => Ok(Some(n)),
            None => Err(bad(
                "a thinking budget must be an integer: -1 (dynamic), 0, or a positive count",
            )),
        },
    }
}

/// Refuse any `fileData` URI the provider would fetch with the operator's
/// identity from somewhere the operator has not allowed.
///
/// Walks the whole body, so a `fileData` nested in `functionResponse` or in
/// `systemInstruction` is checked like one in `contents`. Keys are compared
/// without case or underscores, a superset of the spellings Google accepts,
/// since a missed spelling would be a hole and an extra one only a refusal.
fn check_file_uris(v: &Value, buckets: &[String]) -> Result<(), ProviderError> {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if loose_key(k) == "filedata" {
                    check_file_data(val, buckets)?;
                }
                check_file_uris(val, buckets)?;
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(|i| check_file_uris(i, buckets)),
        _ => Ok(()),
    }
}

/// Check every URI in one `fileData` object. A `fileData` that is not an
/// object, or a URI that is not a string, is not one Tollgate can check, so it
/// is refused too.
fn check_file_data(file_data: &Value, buckets: &[String]) -> Result<(), ProviderError> {
    let Value::Object(file_data) = file_data else {
        return Err(ProviderError::Forbidden(FORBIDDEN_SOURCE.to_owned()));
    };
    for (k, uri) in file_data {
        if loose_key(k) == "fileuri" {
            match uri.as_str() {
                Some(uri) => check_file_uri(uri, buckets)?,
                None => return Err(ProviderError::Forbidden(FORBIDDEN_SOURCE.to_owned())),
            }
        }
    }
    Ok(())
}

/// Whether Vertex may fetch `uri`: a 403 for a source the operator has not
/// allowed, a 400 for remote `https://` media, which is not supported yet.
fn check_file_uri(uri: &str, buckets: &[String]) -> Result<(), ProviderError> {
    let forbidden = || Err(ProviderError::Forbidden(FORBIDDEN_SOURCE.to_owned()));
    if uri.starts_with("gs://") {
        return if gcs_uri_allowed(uri, buckets) {
            Ok(())
        } else {
            forbidden()
        };
    }
    let Ok(url) = reqwest::Url::parse(uri) else {
        return forbidden();
    };
    if google_storage_or_credentials(&url) {
        return forbidden();
    }
    if url.scheme() == "https" {
        return Err(bad(REMOTE_MEDIA_UNSUPPORTED));
    }
    forbidden()
}

/// A URL Vertex would fetch with the operator's Google identity, or one that
/// carries credentials: any `googleapis.com` host (Cloud Storage's JSON and
/// XML APIs among them) and `storage.cloud.google.com`, compared without case
/// and without a trailing dot, or any userinfo. Refused whatever else is
/// allowed, since a bucket off the allowlist must not be reachable over https.
fn google_storage_or_credentials(url: &reqwest::Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return true;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "googleapis.com"
        || host.ends_with(".googleapis.com")
        || host == "storage.cloud.google.com"
}

fn loose_key(k: &str) -> String {
    k.chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether Vertex may read a `gs://` URI.
///
/// `gs://` is read on the operator's behalf, so only a bucket on
/// the allowlist is accepted: the text between `gs://` and the next `/` must
/// equal an allowlisted name exactly. A URI with `%`, `..` or `\` is refused
/// outright rather than decoded or normalised, since the provider might
/// resolve it to another bucket or object than the one checked.
fn gcs_uri_allowed(uri: &str, buckets: &[String]) -> bool {
    let Some(rest) = uri.strip_prefix("gs://") else {
        return false;
    };
    if uri.contains(['%', '\\']) || uri.contains("..") {
        return false;
    }
    let bucket = rest.split('/').next().unwrap_or("");
    !bucket.is_empty()
        && !bucket.chars().any(char::is_uppercase)
        && buckets.iter().any(|b| b == bucket)
}

/// Check the `providers.media.gcs_buckets` allowlist at startup: each entry a
/// plain bucket name (lowercase letters, digits, `-`, `_`, `.`, no `..`), so
/// that an entry can only ever match one bucket exactly.
///
/// # Errors
/// Names the first bad entry.
pub fn validate_gcs_buckets(buckets: &[String]) -> Result<(), String> {
    for b in buckets {
        let plain = !b.is_empty()
            && !b.contains("..")
            && b.bytes().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_' | b'.')
            });
        if !plain {
            return Err(format!(
                "providers.media.gcs_buckets entry {b:?} is not a bucket name: use the exact \
                 lowercase name, without gs:// or a path"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Gemini thinking rules
// ---------------------------------------------------------------------------

/// The route a model rule applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleUpstream {
    /// Native Vertex `generateContent`.
    Vertex,
    /// The OpenAI-compatible route fronting Vertex.
    VertexOpenAi,
}

impl RuleUpstream {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "vertex" => Some(Self::Vertex),
            "vertex-openai" => Some(Self::VertexOpenAi),
            _ => None,
        }
    }

    /// The id a rule is keyed by: the model the provider is sent (see
    /// [`vertex_openai_model`] for the OpenAI-compatible route), lowercased, so
    /// `gemini-x` and `google/gemini-x` are one model with one rule. Only the
    /// rule lookup ignores case; the forwarded and priced id keeps it.
    fn model_key(self, model: &str) -> String {
        match self {
            Self::Vertex => model.to_ascii_lowercase(),
            Self::VertexOpenAi => vertex_openai_model(model).to_ascii_lowercase(),
        }
    }

    /// Whether a model key (see [`Self::model_key`]) is a Gemini model.
    fn is_gemini(self, key: &str) -> bool {
        match self {
            Self::Vertex => key.starts_with("gemini-"),
            Self::VertexOpenAi => key.starts_with("google/gemini-"),
        }
    }
}

/// Whether a Gemini model's thinking tokens count against the output cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingRule {
    /// Thinking is inside `maxOutputTokens` / `max_tokens`, so the cap bounds
    /// it. Proven live for gemini-3.8-flash on both Vertex routes.
    WithinOutputCap,
    /// Thinking comes on top of the cap, up to the operator's bound. The bound
    /// is an assertion, not a provider guarantee (Google calls the budget a
    /// soft limit); settlement still charges real usage.
    Additive { max_thinking_tokens: u64 },
}

/// Models built in as `within_output_cap`: only ids proven live (probes P7,
/// P13). Every other Gemini model needs an operator row, since every Gemini
/// 2.5+ model thinks by default and an unproven one must fail closed.
const BUILTIN_WITHIN_OUTPUT_CAP: &[(RuleUpstream, &str)] = &[
    (RuleUpstream::Vertex, "gemini-3.8-flash"),
    (RuleUpstream::VertexOpenAi, "google/gemini-3.8-flash"),
];

/// The prefix the OpenAI-compatible route adds to a bare model id on Vertex.
const VERTEX_MODEL_PREFIX: &str = "google/";

/// The model id the OpenAI-compatible route sends Vertex for the client's
/// `model`: a bare id gets the `google/` prefix, and an id with any `/` is sent
/// as written. Case is kept. The payload, the thinking rule, and the
/// `ParsedRequest` model (so the price, budget scopes, ledger row and metrics)
/// all use this one id, so two spellings of one upstream model cannot be
/// priced or budgeted apart.
#[must_use]
pub fn vertex_openai_model(model: &str) -> std::borrow::Cow<'_, str> {
    if model.contains('/') {
        std::borrow::Cow::Borrowed(model)
    } else {
        std::borrow::Cow::Owned(format!("{VERTEX_MODEL_PREFIX}{model}"))
    }
}

/// Thinking rules by upstream and model key (see [`RuleUpstream::model_key`]):
/// the built-in rows, with the operator's `[[providers.models]]` rows over
/// them.
#[derive(Debug, Clone)]
pub struct ThinkingRules(HashMap<(RuleUpstream, String), ThinkingRule>);

impl Default for ThinkingRules {
    /// The built-in rows alone.
    fn default() -> Self {
        Self(
            BUILTIN_WITHIN_OUTPUT_CAP
                .iter()
                .map(|(upstream, model)| {
                    (
                        (*upstream, upstream.model_key(model)),
                        ThinkingRule::WithinOutputCap,
                    )
                })
                .collect(),
        )
    }
}

impl ThinkingRules {
    /// The built-in rows with the operator's rows over them.
    ///
    /// # Errors
    /// Describes the first invalid row: an unknown upstream or `thinking`
    /// value, an empty model, two rows for one (upstream, model) once the model
    /// is normalised as the route forwards it, `additive` without
    /// `max_thinking_tokens`, or `max_thinking_tokens` on a rule that ignores
    /// it.
    pub fn from_config(rows: &[crate::config::ModelRuleConfig]) -> Result<Self, String> {
        let mut rules = Self::default();
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            let Some(upstream) = RuleUpstream::parse(&row.upstream) else {
                return Err(format!(
                    "[[providers.models]] upstream must be 'vertex' or 'vertex-openai' (got {:?})",
                    row.upstream
                ));
            };
            if row.model.is_empty() {
                return Err("[[providers.models]] row has an empty model".to_owned());
            }
            let key = upstream.model_key(&row.model);
            if !seen.insert((upstream, key.clone())) {
                return Err(format!(
                    "[[providers.models]] has two rows for {key} on {} (model ids are \
                     compared without case, and a bare id on vertex-openai is the \
                     google/ one)",
                    row.upstream
                ));
            }
            let rule = match (row.thinking.as_deref(), row.max_thinking_tokens) {
                (Some("within_output_cap"), 0) => ThinkingRule::WithinOutputCap,
                (Some("within_output_cap"), _) => {
                    return Err(format!(
                        "[[providers.models]] {}: max_thinking_tokens applies only to \
                         thinking = \"additive\"",
                        row.model
                    ));
                }
                (Some("additive"), 0) => {
                    return Err(format!(
                        "[[providers.models]] {}: thinking = \"additive\" needs \
                         max_thinking_tokens above 0, or the reservation has no bound for \
                         the thinking tokens",
                        row.model
                    ));
                }
                (Some("additive"), n) => ThinkingRule::Additive {
                    max_thinking_tokens: n,
                },
                (other, _) => {
                    return Err(format!(
                        "[[providers.models]] {}: thinking must be \"within_output_cap\" or \
                         \"additive\" (got {other:?})",
                        row.model
                    ));
                }
            };
            rules.0.insert((upstream, key), rule);
        }
        Ok(rules)
    }

    /// The output leg to reserve for `model` on `upstream`, given the request's
    /// output cap and its explicit thinking budget, if any.
    ///
    /// Non-Gemini models have no thinking rule. A Gemini model with no row is
    /// refused: an unproven model fails closed.
    fn output_leg(
        &self,
        upstream: RuleUpstream,
        model: &str,
        max_output: u64,
        explicit_budget: Option<u64>,
    ) -> Result<u64, ProviderError> {
        let key = upstream.model_key(model);
        if !upstream.is_gemini(&key) {
            return Ok(max_output);
        }
        match self.0.get(&(upstream, key)) {
            Some(ThinkingRule::WithinOutputCap) => Ok(max_output),
            // The budget is a soft limit the client set, so a larger one
            // raises the bound rather than being ignored.
            Some(ThinkingRule::Additive {
                max_thinking_tokens,
            }) => {
                Ok(max_output
                    .saturating_add((*max_thinking_tokens).max(explicit_budget.unwrap_or(0))))
            }
            None => Err(bad(
                "this Gemini model has no thinking rule: the operator must add a \
                 [[providers.models]] row with thinking = \"within_output_cap\" or \
                 \"additive\" for it (see docs/OPERATIONS.md)",
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Vertex / Gemini
// ---------------------------------------------------------------------------

struct CachedToken {
    value: String,
    expires_at: Instant,
}

/// Source of a GCP OAuth access token: a static token (testing) or the GCP
/// metadata server (Workload Identity on Cloud Run / GCE), cached until expiry.
pub struct TokenSource {
    r#static: Option<String>,
    cache: Mutex<Option<CachedToken>>,
}

impl TokenSource {
    #[must_use]
    pub fn new(static_token: String) -> Self {
        Self {
            r#static: if static_token.is_empty() {
                None
            } else {
                Some(static_token)
            },
            cache: Mutex::new(None),
        }
    }

    async fn token(&self, http: &reqwest::Client) -> Result<String, ProviderError> {
        if let Some(t) = &self.r#static {
            return Ok(t.clone());
        }
        // Serve from cache if it has >60s left.
        if let Some(c) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            if c.expires_at > Instant::now() + Duration::from_secs(60) {
                return Ok(c.value.clone());
            }
        }
        // Fetch from the metadata server (no lock held across the await).
        let resp = http
            .get("http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token")
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|e| ProviderError::Upstream(format!("metadata token fetch: {e}")))?;
        let json: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Upstream(format!("metadata token parse: {e}")))?;
        let value = json
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProviderError::Upstream("no access_token from metadata server".to_owned())
            })?
            .to_owned();
        let ttl = json
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(3600);
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedToken {
            value: value.clone(),
            expires_at: Instant::now() + Duration::from_secs(ttl),
        });
        Ok(value)
    }
}

/// Vertex API host for a location. Regional endpoints are
/// `{region}-aiplatform.googleapis.com`. The `global` location (which some
/// newer Gemini models are served from) has no prefix, and the `us` and `eu`
/// multi-region locations use `aiplatform.{us,eu}.rep.googleapis.com`.
fn vertex_host(location: &str) -> String {
    match location {
        "global" => "aiplatform.googleapis.com".to_owned(),
        "us" | "eu" => format!("aiplatform.{location}.rep.googleapis.com"),
        _ => format!("{location}-aiplatform.googleapis.com"),
    }
}

/// `https://{host}/v1/projects/{project}/locations/{location}`, the prefix every
/// Vertex URL Tollgate builds starts with. One function, so the native adapter,
/// the token count and the OpenAI-compatible base cannot disagree about which
/// host a location maps to. `project` and `location` are validated at boot
/// (`app::validate_vertex_target`), so neither can carry a `/`, `?`, `#`, `@`
/// or port into the URL.
fn vertex_base(project: &str, location: &str) -> String {
    format!(
        "https://{host}/v1/projects/{project}/locations/{location}",
        host = vertex_host(location)
    )
}

/// Adapter for Vertex AI (Gemini) `generateContent`.
pub struct VertexProvider {
    http: reqwest::Client,
    /// `vertex_base(project, location)`, computed once.
    base: String,
    tokens: TokenSource,
    /// `providers.media.gcs_buckets`: the only buckets a `gs://` URI may name.
    gcs_buckets: Vec<String>,
    thinking: ThinkingRules,
    /// `providers.vertex.allow_client_labels`: whether a request may carry
    /// `labels`, which land in the operator's billing export.
    allow_client_labels: bool,
}

impl VertexProvider {
    #[must_use]
    pub fn new(
        http: reqwest::Client,
        project: String,
        location: String,
        static_token: String,
    ) -> Self {
        Self {
            tokens: TokenSource::new(static_token),
            http,
            base: vertex_base(&project, &location),
            gcs_buckets: Vec::new(),
            thinking: ThinkingRules::default(),
            allow_client_labels: false,
        }
    }

    /// Accept client `labels` (default: refused).
    #[must_use]
    pub fn with_client_labels(mut self, allow: bool) -> Self {
        self.allow_client_labels = allow;
        self
    }

    /// Allow `gs://` URIs in these buckets (default: none).
    #[must_use]
    pub fn with_gcs_buckets(mut self, buckets: Vec<String>) -> Self {
        self.gcs_buckets = buckets;
        self
    }

    /// Replace the built-in thinking rules.
    #[must_use]
    pub fn with_thinking_rules(mut self, rules: ThinkingRules) -> Self {
        self.thinking = rules;
        self
    }

    /// Point the adapter at a local stub instead of Google.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_base(mut self, base: String) -> Self {
        self.base = base;
        self
    }

    /// The client's body as checked: parsed with duplicate keys and lossy
    /// integers refused, Google's own objects rewritten to camelCase (strict
    /// lowerCamel or snake_case only, and no two keys in one object naming the
    /// same field), then checked against the allowlist and the `fileData`
    /// source rules. Every decision is made on this value.
    ///
    /// The client's original bytes are what is forwarded, not this value.
    /// Re-serialising would sort every object's keys (serde_json is built
    /// without `preserve_order`), reordering client-owned maps such as a
    /// `responseJsonSchema`'s `properties`, which Gemini may follow for output
    /// order. Forwarding the bytes is safe because of the refusals above:
    /// Google's parser accepts both spellings (camelCase is the form the REST
    /// reference documents; a live check on 2026-10-30 showed a snake_case
    /// `generation_config.max_output_tokens` honoured), so with no duplicates,
    /// no colliding spellings, no other spellings and no rounded numbers, the
    /// raw body and this value are the same request.
    fn checked_body(&self, body: &str) -> Result<Value, ProviderError> {
        let raw = parse_strict(if body.is_empty() { "{}" } else { body })?;
        let canonical = canon_object(raw, canon_top)?;
        check_vertex_allowlist(&canonical, self.allow_client_labels)?;
        check_file_uris(&canonical, &self.gcs_buckets)?;
        Ok(canonical)
    }
}

/// Extract the model id from a Vertex path like
/// `publishers/google/models/gemini-1.5-pro:generateContent`.
fn extract_vertex_model(rest_path: &str) -> Option<String> {
    let after = rest_path.split("models/").nth(1)?;
    let end = after.find([':', '/']).unwrap_or(after.len());
    let model = &after[..end];
    if model.is_empty() {
        None
    } else {
        Some(model.to_owned())
    }
}

/// Gemini reports FOUR disjoint token classes, per the documented total:
///
/// ```text
/// totalTokenCount = promptTokenCount
///                 + toolUsePromptTokenCount
///                 + thoughtsTokenCount
///                 + candidatesTokenCount
/// ```
///
/// So `candidatesTokenCount` EXCLUDES thinking tokens and `promptTokenCount`
/// EXCLUDES tool-use prompt tokens. Reading only those two obvious fields drops
/// the entire thinking leg, which is billed at the output rate and is on by
/// default for 2.5-series thinking models, plus the tool-use prompt leg.
///
/// Thinking tokens bill at the output rate, so they join the output leg. Tool-use
/// prompt tokens are prompt-side, so they join the input leg.
///
/// `cachedContentTokenCount` is INSIDE `promptTokenCount`: unlike Anthropic's
/// disjoint scheme, Gemini documents `promptTokenCount` as the total effective
/// prompt size INCLUDING cached content. It is therefore SUBTRACTED out into its
/// own class rather than added, or the cache would be billed twice.
fn parse_vertex_usage(v: &Value) -> Usage {
    let m = v.get("usageMetadata");
    let get = |k: &str| {
        m.and_then(|m| m.get(k))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    // saturating_add: upstream-reported values, not trusted to be small. A
    // wrapping sum would under-charge, which is the one direction we never allow.
    // Cached tokens are documented as a subset of promptTokenCount SPECIFICALLY,
    // not of the prompt side as a whole, so they must be split out of that field
    // alone. Subtracting from prompt + toolUsePrompt would let a cached count
    // between the two silently reclassify tool-use tokens as cache reads, which
    // are cheaper: an under-charge on a broken response.
    let prompt = get("promptTokenCount");
    let tool_use = get("toolUsePromptTokenCount");
    let prompt_total = prompt.saturating_add(tool_use);
    let cached = get("cachedContentTokenCount");
    let contradictory = cached > prompt;
    let (input, cache_read) = if contradictory {
        tracing::warn!(
            cached,
            prompt,
            "vertex reports more cached tokens than prompt tokens; usage is \
             self-contradictory, charging the reservation instead of costing it"
        );
        (prompt_total, cached)
    } else {
        ((prompt - cached).saturating_add(tool_use), cached)
    };
    let mut output = get("candidatesTokenCount").saturating_add(get("thoughtsTokenCount"));

    // Residual reconciliation. The four classes above are documented to sum to
    // totalTokenCount, so any positive difference is a token class this parser
    // does not know about. That is not hypothetical: thoughts and tool-use
    // prompts were added by Google after this function was written and were
    // silently dropped for months, under-charging every affected request.
    //
    // Bill the residual on the OUTPUT leg, which is the more expensive of the
    // two, so an unrecognised class becomes a logged over-charge instead of a
    // silent under-charge. Zero on every response whose classes we already know.
    //
    // Reconcile against the FULL prompt, not the post-split `input`. Cached
    // tokens live inside totalTokenCount, so measuring against `input` alone
    // would report the cache itself as an unknown class and bill it a second
    // time at the output rate.
    let total = get("totalTokenCount");
    let counted = prompt_total.max(cached).saturating_add(output);
    let residual = total.saturating_sub(counted);
    if residual > 0 {
        tracing::warn!(
            residual,
            total,
            counted,
            "vertex usageMetadata reports more tokens than the known classes account for; \
             billing the remainder at the output rate (a new token class may have shipped)"
        );
        output = output.saturating_add(residual);
    }
    let usage = Usage::with_cache(input, output, cache_read, 0);
    if contradictory {
        usage.into_suspect()
    } else {
        usage
    }
}

#[async_trait]
impl Provider for VertexProvider {
    fn id(&self) -> &str {
        "vertex"
    }

    fn parse_request(&self, rest_path: &str, body: &str) -> Result<ParsedRequest, ProviderError> {
        // Allowlist: only non-streaming generateContent. This rejects
        // :streamGenerateContent (unmeterable here) and other resources.
        let path = safe_rest_path(rest_path)?;
        if !path.ends_with(":generateContent") {
            return Err(ProviderError::BadRequest(
                "only ...:generateContent is supported (streaming and other endpoints are not)"
                    .to_owned(),
            ));
        }
        let model = extract_vertex_model(path)
            .ok_or_else(|| bad("could not determine the model from the request path"))?;
        // Here, not later: under `exact` admission the count call comes next,
        // and `countTokens` would itself read a `gs://` object.
        let v = self.checked_body(body)?;
        // Require an explicit output cap so the reservation bounds real spend;
        // otherwise the model's (much larger) default would govern generation.
        // Zero is refused too: Google may read 0 as unset, which would leave
        // generation uncapped behind a reservation of nothing.
        let max_output_tokens = v
            .get("generationConfig")
            .and_then(|g| g.get("maxOutputTokens"))
            .and_then(Value::as_u64)
            .filter(|&n| n > 0)
            .ok_or_else(|| {
                bad("generationConfig.maxOutputTokens is required and must be a positive integer")
            })?;
        let explicit_budget = match v.pointer("/generationConfig/thinkingConfig/thinkingBudget") {
            Some(b) => explicit_thinking_budget(b)?,
            None => None,
        };
        let max_output_tokens = self.thinking.output_leg(
            RuleUpstream::Vertex,
            &model,
            max_output_tokens,
            explicit_budget,
        )?;
        Ok(ParsedRequest {
            model,
            // Over the bytes that are forwarded: the client's own.
            estimated_input_tokens: estimate_input_tokens(body),
            max_output_tokens,
            // Vertex cache creation is a separate cachedContents call that never
            // transits the proxy, so a generateContent request cannot write.
            may_cache_write: false,
            anthropic_beta: None,
            // Read from the checked value, so either accepted spelling of
            // `fileData` is seen by fast admission.
            external_media: value_has_media_ref(&v),
        })
    }

    async fn forward(
        &self,
        rest_path: &str,
        body: &str,
        _parsed: &ParsedRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let path = safe_rest_path(rest_path)?;
        // Checked again here so the forward never depends on the caller having
        // run `parse_request`; the bytes sent are the client's (see
        // `checked_body` for why that is the request that was checked).
        self.checked_body(body)?;
        let token = self.tokens.token(&self.http).await?;
        let url = format!("{base}/{path}", base = self.base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .map_err(|e| classify_transport_error(&e))?;
        let (status, meta, json) = json_or_classified(resp).await?;
        let usage = parse_vertex_usage(&json);
        Ok(ProviderResponse {
            status,
            body: json,
            usage,
            meta,
        })
    }

    async fn count_input_tokens(
        &self,
        rest_path: &str,
        body: &str,
        parsed: &ParsedRequest,
    ) -> Result<u64, ProviderError> {
        let model = extract_vertex_model(rest_path).unwrap_or_else(|| parsed.model.clone());
        // Built from the checked value: a separate, allowlisted request, so the
        // key order the forward preserves does not matter here.
        let payload = vertex_count_payload(&self.checked_body(body)?)?;
        let token = self.tokens.token(&self.http).await?;
        let url = format!(
            "{base}/publishers/google/models/{model}:countTokens",
            base = self.base
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        let status = resp.status();
        let meta = ResponseMeta::from_headers(resp.headers());
        // Bytes then parse, rather than `.json()`: a refusal does not always
        // come back as JSON. An edge proxy or a WAF in front of the provider
        // answers a 400 with an HTML page, and letting the decode error decide
        // would classify the caller's malformed body as an upstream outage,
        // which an SDK then retries.
        let raw = resp
            .bytes()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        if let Some(e) = count_refusal(status, &meta, &raw, "countTokens") {
            return Err(e);
        }
        let json: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        json.get("totalTokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ProviderError::Upstream("countTokens returned no totalTokens".to_owned())
            })
    }
}

/// Classify a non-success reply from a token-count endpoint.
///
/// Classified by status like a forward's refusal (see
/// [`crate::provider::classify_upstream_status`]): 400, 413 and 422 are the
/// caller's body and keep their status; 429 is the provider rate limiting the
/// deployment; everything else, including every other 4xx, is a 502.
///
/// The count endpoints used to discard their HTTP status and report every
/// failure as an upstream one. That turned a request the PROVIDER rejected as
/// malformed into a 502, which an SDK then retries as if the provider were
/// down: three round trips to surface what is really a 400 on the caller's own
/// body.
///
/// The caller's-body statuses are an ALLOWLIST, not a denylist.
/// Treating all 4xx as the caller's is the obvious reading and it is wrong in
/// the worst direction. A 401 or 403 from a count endpoint is Tollgate's own
/// credential, a rotated Anthropic key or a Vertex token that lost a scope, and
/// a 404 is usually a model name or project in the price book. Blaming those on
/// the caller tells every client that their body is malformed, sends them
/// hunting through a payload that is fine, and produces no `upstream_error` for
/// the operator's alert to catch, while 100% of traffic fails. A misconfigured
/// gateway must look like a misconfigured gateway.
///
/// 429 is the provider's too: it is transient, and changing the body will not
/// help. It reaches the client as a 429 with its Retry-After.
///
/// The split is checked against what the endpoints actually return, not against
/// what they ought to. Live: Anthropic `count_tokens` answers 400 to an empty
/// `messages` array, 401 to a wrong key, and 404 to an unknown model; Vertex
/// `:countTokens` answers 400 to an unknown field and 401 to a bad token. So
/// every credential and configuration fault lands off the allowlist, which is
/// the property that matters. Reprobe with
/// `scripts/probe-count-endpoints.sh` before changing this list.
///
/// No provider text reaches the caller on any status: the refusal carries only
/// its classification, and the provider's type, code and body go to the log
/// (see [`crate::provider::log_upstream_refusal`]). A Vertex message names the
/// project and region in its resource path, and even a 400's message can.
///
/// Returns `None` when the status is a success, leaving the caller to parse.
fn count_refusal(
    status: reqwest::StatusCode,
    meta: &ResponseMeta,
    raw: &[u8],
    endpoint: &str,
) -> Option<ProviderError> {
    if status.is_success() {
        return None;
    }
    let detail = RefusalDetail::new(meta, raw, true);
    // A credential failure fails every request under `exact` admission, so it
    // is logged at ERROR here; nothing else logs it at ERROR, and the client's
    // 502 says only "upstream".
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        tracing::error!(
            endpoint,
            %status,
            provider_error_type = detail.error_type.as_deref().unwrap_or(""),
            provider_error_code = detail.error_code.as_deref().unwrap_or(""),
            provider_request_id = detail.provider_request_id.as_deref().unwrap_or(""),
            "the provider rejected Tollgate's own credential on the pre-flight count; \
             under `exact` admission this fails every request until it is fixed"
        );
    }
    let code = status.as_u16();
    Some(ProviderError::Refused {
        status: code,
        fault: classify_upstream_status(code, meta.retry_after_secs),
        detail: Box::new(detail),
    })
}

/// Default worst-case output reservation when the request omits `max_tokens`.
const OPENAI_DEFAULT_MAX_OUTPUT: u64 = 4096;

/// The effective worst-case output for an OpenAI request: the first POSITIVE
/// integer cap (`max_completion_tokens`, then `max_tokens`), else the default.
/// A present-but-null/zero/non-integer cap is treated as "no cap" (default), so
/// a client cannot send `null` to run the upstream unbounded past the reservation.
/// `parse_request` reserves this value and `forward` normalizes the outbound body
/// to a single `max_tokens` equal to it, keeping reserved == enforced.
fn openai_effective_max(v: &Value) -> u64 {
    v.get("max_completion_tokens")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| v.get("max_tokens").and_then(serde_json::Value::as_u64))
        .filter(|&n| n > 0)
        .unwrap_or(OPENAI_DEFAULT_MAX_OUTPUT)
}

/// Whether an OpenAI-protocol upstream's `cached_tokens` sits INSIDE
/// `prompt_tokens` or alongside it.
///
/// This cannot be inferred from a payload: the two readings are numerically
/// indistinguishable, and getting it wrong in the subtracting direction
/// under-charges by roughly half on a cached request, steered by the caller's
/// own prompt structure. So it travels with the upstream, not with the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSemantics {
    /// Verified that `cached_tokens` is a SUBSET of `prompt_tokens`, so the
    /// cached count is split out of the prompt. True of OpenAI itself and,
    /// confirmed against live responses, of Vertex's OpenAI-compatible shim.
    Inclusive,
    /// An upstream whose convention has not been verified, which includes any
    /// operator-configured custom base URL. Both classes are billed in full:
    /// exact if the upstream reports them disjointly, an over-charge if it
    /// reports them inclusively, and never an under-charge either way.
    Unverified,
}

/// Normalise an OpenAI-protocol usage object into [`Usage`].
///
/// The output leg is NOT simply `completion_tokens`. The same field name means
/// different things on two upstreams that both speak this protocol: OpenAI's own
/// API documents `completion_tokens` as INCLUDING reasoning tokens, while
/// Vertex's OpenAI-compatible shim EXCLUDES them. Verified against live shim
/// responses, where `491 completion + 69 reasoning = 560 = total - prompt`, and
/// where a response whose output was entirely reasoning omitted
/// `completion_tokens` altogether. Reading the field alone therefore
/// under-charges every reasoning request on the shim, sometimes to zero, and
/// reasoning bills at the full output rate on both.
///
/// `total_tokens` is the one figure both dialects agree on, so the residual
/// `total - prompt` recovers the true output leg without this parser having to
/// know which upstream it is talking to. Where `total_tokens` is missing we add
/// reasoning explicitly instead: exact on an excluding upstream, an over-charge
/// on an including one, which is the only direction we are allowed to be wrong
/// in. The final `max` against `completion_tokens` guards a self-contradictory
/// response whose total is smaller than its parts.
///
/// That residual is computed BEFORE the cache split, which is safe on a verified
/// upstream but ambiguous on an unverified one: see the `ambiguous` check below.
fn parse_openai_usage(v: &Value, semantics: CacheSemantics) -> Usage {
    let u = v.get("usage");
    let get = |k: &str| {
        u.and_then(|u| u.get(k))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let prompt = get("prompt_tokens");
    let completion = get("completion_tokens");
    let reasoning = u
        .and_then(|u| u.get("completion_tokens_details"))
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let total = get("total_tokens");
    let candidate = if total > 0 {
        total.saturating_sub(prompt)
    } else {
        completion.saturating_add(reasoning)
    };
    let output = completion.max(candidate);

    // Prompt-side split. `cached_tokens` is subtracted out only where the
    // upstream's convention is known; otherwise both classes are billed whole.
    let cached = u
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let contradictory = matches!(semantics, CacheSemantics::Inclusive) && cached > prompt;
    if contradictory {
        tracing::warn!(
            cached,
            prompt,
            "upstream reports more cached tokens than prompt tokens; usage is \
             self-contradictory, charging the reservation instead of costing it"
        );
    }

    // Ambiguous composition on an unverified upstream.
    //
    // The output leg above was derived as `total - prompt` BEFORE the cache
    // split, because that residual is what recovers reasoning tokens on an
    // upstream that excludes them from `completion_tokens`. But on an upstream
    // that reports cached tokens DISJOINTLY and counts them in `total` (the
    // coherent additive shape, and exactly the convention Unverified exists to
    // tolerate), that same residual swallows the cached count and bills it at
    // the OUTPUT rate, on top of billing it again as a cache read.
    //
    // Two readings, no way to tell them apart from the payload, and one of them
    // settles above a reservation that was sized for the prompt legs alone.
    // That is a metering failure, not an expensive request, so it takes the same
    // route as any other untrustworthy usage: charge the reservation.
    let residual_output = total.saturating_sub(prompt);
    let ambiguous = matches!(semantics, CacheSemantics::Unverified)
        && cached > 0
        && total > 0
        && residual_output > completion.saturating_add(reasoning);
    if ambiguous {
        tracing::warn!(
            cached,
            prompt,
            completion,
            total,
            "unverified upstream reports cached tokens AND a total larger than its \
             own output fields account for; composition is ambiguous, charging the \
             reservation instead of costing it"
        );
    }
    let fresh = match semantics {
        CacheSemantics::Inclusive if !contradictory => prompt - cached,
        // Contradictory, or an upstream whose convention we have not verified:
        // both classes stand in full. Under Unverified that is exact if the
        // upstream reports them disjointly and an over-charge if it does not.
        // The reservation covers both PROMPT legs; where the output leg is also
        // ambiguous the usage is marked suspect above and never costed at all.
        _ => prompt,
    };
    // Tripwire for billing dimensions that do not exist yet.
    //
    // Every field below is a token class billed at a rate this price book cannot
    // express. They are all rejected at parse time today, so seeing one here
    // means either the reject list was evaded or the upstream shipped a class we
    // have never heard of. Either way the numbers cannot be costed correctly, so
    // treat it as a metering failure rather than guessing.
    //
    // This is a BACKSTOP, not the defence. Charging the reservation still
    // under-charges when the reservation itself was priced at the wrong rate, so
    // anything that fires here belongs on the parse-time reject list, not here.
    let unpriced_class = [
        "audio_tokens",
        "accepted_prediction_tokens",
        "rejected_prediction_tokens",
    ]
    .iter()
    .any(|k| {
        ["completion_tokens_details", "prompt_tokens_details"]
            .iter()
            .any(|d| {
                u.and_then(|u| u.get(d))
                    .and_then(|d| d.get(*k))
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|n| n > 0)
            })
    });
    if unpriced_class {
        tracing::error!(
            "upstream reported tokens in a class Tollgate has no rate for (audio or \
             predicted output); charging the reservation. This should have been refused \
             at parse time: treat it as a bug, not as normal operation"
        );
    }

    let mut usage = Usage::with_cache(fresh, output, cached, 0);
    // On an unverified upstream the prompt is billed on BOTH legs, so the
    // classes overlap rather than partitioning the prompt. Recorded on the usage
    // rather than acted on: sizing resolves upward here as billing does, since
    // "unverified" means we do not know that the cached count sits inside the
    // prompt, and if it sits beside it the real prompt IS the sum. The flag
    // exists so a persisted record can say which of the two it was.
    if matches!(semantics, CacheSemantics::Unverified) {
        usage = usage.with_overlapping_classes();
    }
    if contradictory || ambiguous || unpriced_class {
        usage.into_suspect()
    } else {
        usage
    }
}

/// The default for `providers.openai.pin_service_tier`: on for OpenAI's own
/// API, where an omitted tier means `auto`, and off for any other host, since
/// other OpenAI-compatible servers may reject a field they do not know.
fn default_pin_service_tier(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        })
        .is_some_and(|h| h == "api.openai.com")
}

/// Check the Google-specific settings a body may carry on the Vertex
/// OpenAI-compatible route, and return the explicit positive thinking budget,
/// if any.
///
/// Vertex honours them both under a top-level `google` object and under a
/// literal `extra_body.google` object (probe P13), so both are checked, and a
/// body carrying both is refused because which one wins is not known. Inside
/// either, only `thinking_config` is allowed: `cached_content` names the
/// operator's caches (refused on the native route too), and anything else
/// cannot be metered. Any other `extra_body` member is refused for the same
/// reason.
fn check_google_settings(v: &Value) -> Result<Option<u64>, ProviderError> {
    let nested = match v.get("extra_body") {
        None => None,
        Some(Value::Object(extra)) => {
            reject_uppercase_keys_in(extra)?;
            reject_alias_keys_in(extra)?;
            if extra.keys().any(|k| k != "google") {
                return Err(bad(
                    "extra_body accepts only a google object on this upstream",
                ));
            }
            extra.get("google")
        }
        Some(_) => return Err(bad("extra_body must be an object")),
    };
    let google = match (v.get("google"), nested) {
        (Some(_), Some(_)) => {
            return Err(bad(
                "send Google settings in google or in extra_body.google, not both",
            ));
        }
        (Some(g), None) | (None, Some(g)) => g,
        (None, None) => return Ok(None),
    };
    let Some(google) = google.as_object() else {
        return Err(bad("google must be an object"));
    };
    reject_uppercase_keys_in(google)?;
    reject_alias_keys_in(google)?;
    if google.keys().any(|k| k != "thinking_config") {
        return Err(bad(
            "Google settings accept only thinking_config on this route: cached_content \
             names the operator's caches, and other settings cannot be metered",
        ));
    }
    let Some(thinking) = google.get("thinking_config") else {
        return Ok(None);
    };
    let Some(thinking) = thinking.as_object() else {
        return Err(bad("thinking_config must be an object"));
    };
    reject_uppercase_keys_in(thinking)?;
    reject_alias_keys_in(thinking)?;
    if thinking.keys().any(|k| {
        !matches!(
            k.as_str(),
            "include_thoughts" | "thinking_budget" | "thinking_level"
        )
    }) {
        return Err(bad(
            "thinking_config accepts only include_thoughts, thinking_budget and thinking_level",
        ));
    }
    match thinking.get("thinking_budget") {
        Some(b) => explicit_thinking_budget(b),
        None => Ok(None),
    }
}

/// On the Vertex OpenAI-compatible route, refuse a top-level key with an
/// uppercase ASCII letter.
///
/// Vertex may read camelCase aliases (`maxTokens`, `extraBody`,
/// `candidateCount`, `serviceTier`) beside the snake_case fields every check
/// here reads, so an alias would bypass the output cap, the reservation and
/// the rate checks. A body that is not an object is left to the other checks.
fn reject_uppercase_keys(v: &Value) -> Result<(), ProviderError> {
    match v.as_object() {
        Some(map) => reject_uppercase_keys_in(map),
        None => Ok(()),
    }
}

fn reject_uppercase_keys_in(map: &serde_json::Map<String, Value>) -> Result<(), ProviderError> {
    if map
        .keys()
        .any(|k| k.bytes().any(|c| c.is_ascii_uppercase()))
    {
        // Fixed text: the key is the client's.
        return Err(bad(
            "field names on this upstream must be snake_case: a key with an uppercase \
             letter may be read as an alias that bypasses Tollgate's checks",
        ));
    }
    Ok(())
}

/// Every field Tollgate reads, pins, refuses or rewrites on the Vertex
/// OpenAI-compatible route: `parse_common`, `build_payload`,
/// `openai_effective_max`, `reject_rate_switches` and `check_google_settings`,
/// plus the Google fields those checks refuse by name.
const VERTEX_OPENAI_CHECKED_FIELDS: &[&str] = &[
    "model",
    "messages",
    "max_tokens",
    "max_completion_tokens",
    "n",
    "stream",
    "stream_options",
    "service_tier",
    "modalities",
    "audio",
    "prediction",
    "web_search_options",
    "tools",
    "tool_choice",
    "reasoning_effort",
    "google",
    "extra_body",
    "thinking_config",
    "thinking_budget",
    "include_thoughts",
    "thinking_level",
    "cached_content",
    "candidate_count",
];

/// On the Vertex OpenAI-compatible route, refuse a top-level key that is a
/// loose respelling of a checked field, or two keys with one loose form.
///
/// Lowercase alone does not make a key safe: `maxtokens`, `max__tokens` or
/// `max_tokens_` would be forwarded beside the gateway's own `max_tokens`, and
/// a shim that matches names loosely could let one override the cap. A key
/// whose loose form (lowercase, underscores removed) equals a checked field's
/// must be spelled exactly as that field. A body that is not an object is left
/// to the other checks.
fn reject_alias_keys(v: &Value) -> Result<(), ProviderError> {
    match v.as_object() {
        Some(map) => reject_alias_keys_in(map),
        None => Ok(()),
    }
}

fn reject_alias_keys_in(map: &serde_json::Map<String, Value>) -> Result<(), ProviderError> {
    let mut seen = std::collections::HashSet::with_capacity(map.len());
    for k in map.keys() {
        let loose = loose_key(k);
        let alias = VERTEX_OPENAI_CHECKED_FIELDS
            .iter()
            .any(|f| *f != k.as_str() && loose_key(f) == loose);
        if alias || !seen.insert(loose) {
            // Fixed text: the key is the client's.
            return Err(bad(
                "field names on this upstream must be spelled exactly: a key that differs \
                 from a checked field, or from another key, only in case or underscores \
                 may be read as an alias that bypasses Tollgate's checks",
            ));
        }
    }
    Ok(())
}

/// On the Vertex OpenAI-compatible route, refuse a top-level `cached_content`
/// or `candidate_count`.
///
/// Whether Vertex reads either at the top level is unverified, so both are
/// refused (fail closed): `cached_content` names the operator's caches and
/// `candidate_count` multiplies output beyond the reservation. Their alias
/// spellings are already refused by `reject_uppercase_keys` and
/// `reject_alias_keys`.
fn reject_top_level_google_fields(v: &Value) -> Result<(), ProviderError> {
    let Some(map) = v.as_object() else {
        return Ok(());
    };
    if map.contains_key("cached_content") || map.contains_key("candidate_count") {
        // Fixed text: the value is the client's.
        return Err(bad(
            "cached_content and candidate_count are not accepted at the top level on this \
             upstream: a cached context names the operator's caches, and extra candidates \
             multiply output beyond the reservation",
        ));
    }
    Ok(())
}

/// On the Vertex OpenAI-compatible route, refuse any message `content` array
/// part other than `{"type": "text", "text": "..."}`.
///
/// Vertex may read media parts under spellings the media check does not know
/// (`input_file`, `file_url`, `imageUrl` are untested; probe P1 found `file`
/// parts refused by Vertex itself), and would fetch them on the operator's
/// behalf. Allowing exactly one shape closes every spelling.
/// String content is unchanged.
fn check_text_only_parts(v: &Value) -> Result<(), ProviderError> {
    let Some(messages) = v.get("messages").and_then(Value::as_array) else {
        return Ok(());
    };
    for message in messages {
        let Some(Value::Array(parts)) = message.get("content") else {
            continue;
        };
        for part in parts {
            let text_only = part.as_object().is_some_and(|p| {
                p.get("type").and_then(Value::as_str) == Some("text")
                    && p.get("text").is_some_and(Value::is_string)
                    && p.keys().all(|k| k == "type" || k == "text")
            });
            if !text_only {
                return Err(bad(
                    "on this upstream a message content array may carry only \
                     {\"type\": \"text\", \"text\": ...} parts: media and file parts are not \
                     supported yet",
                ));
            }
        }
    }
    Ok(())
}

/// OpenAI-compatible upstream adapter. Fronts any Chat Completions endpoint that
/// speaks the OpenAI protocol: Vertex's OpenAI endpoint for Gemini, or a custom
/// OpenAI-compatible base. Auth is a bearer token (a GCP Workload Identity token
/// for Vertex, or a static API key for a custom upstream).
pub struct OpenAiProvider {
    /// Buffered (non-streaming) client; carries the total request timeout.
    http: reqwest::Client,
    /// Streaming client; NO total timeout (long streams), only connect + idle.
    stream_http: reqwest::Client,
    /// Base URL up to but excluding `/chat/completions`.
    base_url: String,
    tokens: TokenSource,
    /// Fronts Vertex's OpenAI-compatible endpoint: the forwarded model is
    /// [`vertex_openai_model`] of the client's.
    vertex: bool,
    /// Gemini thinking rules; consulted only when fronting Vertex.
    thinking: ThinkingRules,
    /// Send `service_tier: "default"` upstream (custom upstreams only).
    pin_service_tier: bool,
}

impl OpenAiProvider {
    /// How this upstream reports cached prompt tokens.
    ///
    /// Vertex's shim was verified
    /// against live responses to report them INSIDE `prompt_tokens`. A custom
    /// base URL is whatever the operator pointed it at, so it stays unverified
    /// and both classes are billed in full rather than risking an under-charge.
    ///
    /// The streaming relay must be given this same value, or a caller could pick
    /// the cheaper path by setting `stream`.
    #[must_use]
    pub fn cache_semantics(&self) -> CacheSemantics {
        if self.vertex {
            CacheSemantics::Inclusive
        } else {
            CacheSemantics::Unverified
        }
    }

    /// Front Vertex's OpenAI-compatible endpoint for Gemini. `static_token` may be
    /// empty to use the metadata server (Workload Identity). `stream_http` must
    /// have no total request timeout (only connect + idle) so long streams are
    /// not severed mid-body.
    #[must_use]
    pub fn vertex(
        http: reqwest::Client,
        stream_http: reqwest::Client,
        project: &str,
        location: &str,
        static_token: String,
    ) -> Self {
        let base_url = format!("{}/endpoints/openapi", vertex_base(project, location));
        Self {
            tokens: TokenSource::new(static_token),
            http,
            stream_http,
            base_url,
            vertex: true,
            thinking: ThinkingRules::default(),
            // Vertex drops the field instead; see `build_payload`.
            pin_service_tier: false,
        }
    }

    /// Front any custom OpenAI-compatible endpoint. `api_key` is sent as the
    /// bearer token; `base_url` is everything up to `/chat/completions`.
    #[must_use]
    pub fn custom(
        http: reqwest::Client,
        stream_http: reqwest::Client,
        base_url: String,
        api_key: String,
    ) -> Self {
        Self {
            tokens: TokenSource::new(api_key),
            http,
            stream_http,
            pin_service_tier: default_pin_service_tier(&base_url),
            base_url: base_url.trim_end_matches('/').to_owned(),
            vertex: false,
            thinking: ThinkingRules::default(),
        }
    }

    /// Override the default for `providers.openai.pin_service_tier`. Ignored
    /// when fronting Vertex, which never gets the field.
    #[must_use]
    pub fn with_pin_service_tier(mut self, pin: Option<bool>) -> Self {
        if let Some(pin) = pin {
            self.pin_service_tier = pin && !self.fronts_vertex();
        }
        self
    }

    /// Replace the built-in Gemini thinking rules.
    #[must_use]
    pub fn with_thinking_rules(mut self, rules: ThinkingRules) -> Self {
        self.thinking = rules;
        self
    }

    /// Point the adapter at a local stub instead of Google.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Whether this adapter fronts Vertex's OpenAI-compatible endpoint.
    fn fronts_vertex(&self) -> bool {
        self.vertex
    }

    /// Build the outbound payload: rewrite the model for the upstream, normalise
    /// the output cap to a single `max_tokens` equal to the reserved worst case,
    /// and set the streaming flag EXPLICITLY. The client's `stream` /
    /// `stream_options` are never trusted (a truthy-but-nonboolean value must not
    /// flip the mode, and a client cannot suppress usage reporting).
    fn build_payload(&self, body: &str, streaming: bool) -> Result<Value, ProviderError> {
        let mut payload: Value =
            serde_json::from_str(body).map_err(|e| ProviderError::BadRequest(e.to_string()))?;
        let forwarded = payload
            .get("model")
            .and_then(Value::as_str)
            .filter(|_| self.vertex)
            .map(|m| vertex_openai_model(m).into_owned());
        if let Some(m) = forwarded {
            payload["model"] = Value::String(m);
        }
        let cap = openai_effective_max(&payload);
        // Choose which output-cap field to send, always pinned to the reserved cap
        // (so reserved == enforced). On Vertex always use
        // `max_tokens`: the Vertex OpenAI shim honors it, and it may silently ignore
        // `max_completion_tokens`, which would let generation run past our cap. On a
        // custom upstream, preserve the client's field so we don't break the o-series
        // reasoning models on OpenAI's own API, which reject `max_tokens` and require
        // `max_completion_tokens`.
        let cap_field = if self.vertex {
            "max_tokens"
        } else if payload.get("max_completion_tokens").is_some() {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        if let Value::Object(map) = &mut payload {
            map.remove("max_completion_tokens");
            map.remove("max_tokens");
            map.insert(cap_field.to_owned(), Value::from(cap));
            // An omitted tier means `auto` on OpenAI's own API, which can use
            // priority capacity at a premium, so it is pinned where the
            // operator says so. Vertex drops the field.
            if self.fronts_vertex() {
                map.remove("service_tier");
            } else if self.pin_service_tier {
                map.insert(
                    "service_tier".to_owned(),
                    Value::String("default".to_owned()),
                );
            }
            map.insert("stream".to_owned(), Value::Bool(streaming));
            if streaming {
                map.insert(
                    "stream_options".to_owned(),
                    serde_json::json!({ "include_usage": true }),
                );
            } else {
                map.remove("stream_options");
            }
        }
        Ok(payload)
    }

    fn parse_common(
        &self,
        rest_path: &str,
        body: &str,
        allow_stream: bool,
    ) -> Result<ParsedRequest, ProviderError> {
        if rest_path != "chat/completions" {
            return Err(ProviderError::BadRequest(
                "only chat/completions is supported".to_owned(),
            ));
        }
        let v: Value =
            serde_json::from_str(body).map_err(|e| ProviderError::BadRequest(e.to_string()))?;
        // The body is re-serialised before it is forwarded (`build_payload`).
        reject_lossy_integers(body)?;
        if !allow_stream && stream_requested(&v) {
            return Err(ProviderError::BadRequest(
                "streaming is handled on a separate path".to_owned(),
            ));
        }
        // External media has token cost unbounded by body size; the fast estimate
        // would under-reserve, so reject until multimodal metering exists.
        if references_external_media(body) {
            return Err(ProviderError::BadRequest(
                "external media (image_url / input_audio / file references) is not yet \
                 supported on this endpoint; text only"
                    .to_owned(),
            ));
        }
        // `n` asks the upstream for N independent completions, each up to the
        // output cap, so total completion tokens scale with N while we only reserve
        // one cap's worth. Reject anything but a single response (n absent, 0, or 1)
        // so a request can never overshoot its reservation. A non-integer or >1
        // value is refused (fail closed).
        if let Some(n) = v.get("n") {
            if n.as_u64().is_none_or(|n| n > 1) {
                return Err(ProviderError::BadRequest(
                    "n > 1 is not supported (each choice multiplies output cost beyond the \
                     reserved cap)"
                        .to_owned(),
                ));
            }
        }
        reject_rate_switches(&v)?;
        let model = v
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .ok_or_else(|| ProviderError::BadRequest("missing model".to_owned()))?;
        // On Vertex, the id Vertex is sent: the price, budget scopes, ledger row
        // and metrics then key on one id per upstream model, whichever spelling
        // the client used. A custom upstream gets the client's id as written.
        let model = if self.fronts_vertex() {
            vertex_openai_model(model).into_owned()
        } else {
            model.to_owned()
        };
        let mut max_output_tokens = openai_effective_max(&v);
        if self.fronts_vertex() {
            reject_uppercase_keys(&v)?;
            reject_alias_keys(&v)?;
            reject_top_level_google_fields(&v)?;
            check_text_only_parts(&v)?;
            let explicit_budget = check_google_settings(&v)?;
            max_output_tokens = self.thinking.output_leg(
                RuleUpstream::VertexOpenAi,
                &model,
                max_output_tokens,
                explicit_budget,
            )?;
        }
        Ok(ParsedRequest {
            model,
            estimated_input_tokens: estimate_input_tokens(body),
            max_output_tokens,
            // The OpenAI Chat Completions usage object carries no cache-write
            // token field at all, so a write can never be observed on this path.
            may_cache_write: false,
            anthropic_beta: None,
            // Refused above.
            external_media: false,
        })
    }

    /// Parse a streaming request (allows `stream`); same model/media/cap rules.
    ///
    /// # Errors
    /// [`ProviderError::BadRequest`] on a malformed body, missing model, or media.
    pub fn parse_streaming(
        &self,
        rest_path: &str,
        body: &str,
    ) -> Result<ParsedRequest, ProviderError> {
        self.parse_common(rest_path, body, true)
    }

    /// Open a streaming upstream request; the caller reads `bytes_stream()` and
    /// meters usage from the relayed chunks. Uses the no-total-timeout client.
    ///
    /// # Errors
    /// [`ProviderError::Upstream`] if the upstream call fails.
    pub async fn forward_stream(&self, body: &str) -> Result<reqwest::Response, ProviderError> {
        let payload = self.build_payload(body, true)?;
        let token = self.tokens.token(&self.stream_http).await?;
        let url = format!("{}/chat/completions", self.base_url);
        self.stream_http
            .post(&url)
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))
    }
}

/// Whether the client asked for streaming, tolerant of non-boolean truthy values
/// (`true`, non-zero number, `"true"`/`"1"`/`"yes"`) so a lax value can neither
/// slip past the buffered mode check nor be forwarded verbatim.
#[must_use]
pub fn stream_requested(v: &Value) -> bool {
    match v.get("stream") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => {
            n.as_i64().is_some_and(|i| i != 0) || n.as_f64().is_some_and(|f| f != 0.0)
        }
        Some(Value::String(s)) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")
        }
        _ => false,
    }
}

/// Parse token usage from a single SSE `data:` line, or `None` when the line has
/// no non-null `usage` object. `[DONE]`, blank, and keep-alive/comment lines
/// yield `None`; a per-delta `"usage": null` is correctly treated as "not seen".
/// `semantics` MUST be the same value the buffered path uses for this upstream.
/// The client chooses which path runs by setting `stream`, so if the two ever
/// disagree a caller can simply pick the cheaper one.
#[must_use]
pub fn usage_from_sse_data(line: &str, semantics: CacheSemantics) -> Option<Usage> {
    let data = line.strip_prefix("data:")?.trim();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    usage_from_sse_value(&v, semantics)
}

/// As [`usage_from_sse_data`], for a line the caller has already parsed. The
/// stream meter reads `choices` from the same object, so it parses each line once.
#[must_use]
pub fn usage_from_sse_value(v: &Value, semantics: CacheSemantics) -> Option<Usage> {
    match v.get("usage") {
        Some(u) if !u.is_null() => Some(parse_openai_usage(v, semantics)),
        _ => None,
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    /// On an unverified upstream the parser bills the full prompt AND the cached
    /// count, since that is the only reading that cannot under-charge. The
    /// reservation must therefore cover BOTH legs, or every cache hit settles
    /// above what it was admitted for and walks a hard cap.
    fn prompt_reserve_profile(
        &self,
        parsed: &ParsedRequest,
    ) -> crate::pricing::PromptReserveProfile {
        crate::pricing::PromptReserveProfile {
            // Preserves the default's conjunction; this path can never write.
            can_cache_write: self.can_report_cache_write() && parsed.may_cache_write,
            may_double_bill_prompt: matches!(self.cache_semantics(), CacheSemantics::Unverified),
        }
    }

    fn parse_request(&self, rest_path: &str, body: &str) -> Result<ParsedRequest, ProviderError> {
        self.parse_common(rest_path, body, false)
    }

    async fn forward(
        &self,
        _rest_path: &str,
        body: &str,
        _parsed: &ParsedRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let payload = self.build_payload(body, false)?;
        let token = self.tokens.token(&self.http).await?;
        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| classify_transport_error(&e))?;
        let (status, meta, json) = json_or_classified(resp).await?;
        let usage = parse_openai_usage(&json, self.cache_semantics());
        Ok(ProviderResponse {
            status,
            body: json,
            usage,
            meta,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_host_has_no_prefix_for_global() {
        assert_eq!(vertex_host("global"), "aiplatform.googleapis.com");
        assert_eq!(
            vertex_host("us-central1"),
            "us-central1-aiplatform.googleapis.com"
        );
    }

    /// The prefix every Vertex call uses, for the global location, both
    /// multi-regions and a regional location. `locations/{location}` stays in the path in each case.
    #[test]
    fn vertex_base_maps_each_kind_of_location_to_its_host() {
        assert_eq!(
            vertex_base("p", "global"),
            "https://aiplatform.googleapis.com/v1/projects/p/locations/global"
        );
        assert_eq!(
            vertex_base("p", "eu"),
            "https://aiplatform.eu.rep.googleapis.com/v1/projects/p/locations/eu"
        );
        assert_eq!(
            vertex_base("p", "us"),
            "https://aiplatform.us.rep.googleapis.com/v1/projects/p/locations/us"
        );
        assert_eq!(
            vertex_base("p", "europe-west4"),
            "https://europe-west4-aiplatform.googleapis.com/v1/projects/p/locations/europe-west4"
        );
    }

    fn anthropic_test_provider() -> AnthropicProvider {
        AnthropicProvider::new(
            reqwest::Client::new(),
            reqwest::Client::new(),
            "k".into(),
            "https://api.anthropic.com".into(),
            "2023-06-01".into(),
        )
    }

    fn openai_test_provider() -> OpenAiProvider {
        OpenAiProvider::custom(
            reqwest::Client::new(),
            reqwest::Client::new(),
            "https://example.test/v1".to_owned(),
            "k".to_owned(),
        )
    }

    #[test]
    fn stream_requested_is_truthy_tolerant() {
        use serde_json::json;
        assert!(stream_requested(&json!({"stream": true})));
        assert!(stream_requested(&json!({"stream": 1})));
        assert!(stream_requested(&json!({"stream": "true"})));
        assert!(!stream_requested(&json!({"stream": false})));
        assert!(!stream_requested(&json!({"stream": 0})));
        assert!(!stream_requested(&json!({})));
    }

    #[test]
    fn openai_rejects_multi_choice_n() {
        let p = openai_test_provider();
        // n > 1 multiplies output cost beyond the reserved cap: refuse (both paths).
        let body = r#"{"model":"m","n":4,"messages":[]}"#;
        assert!(p.parse_request("chat/completions", body).is_err());
        assert!(p.parse_streaming("chat/completions", body).is_err());
        // Non-integer / string n is also refused (fail closed).
        assert!(
            p.parse_request("chat/completions", r#"{"model":"m","n":"2"}"#)
                .is_err()
        );
        assert!(
            p.parse_request("chat/completions", r#"{"model":"m","n":1.5}"#)
                .is_err()
        );
        // n absent, 0, or 1 is fine (single response).
        assert!(
            p.parse_request("chat/completions", r#"{"model":"m","n":1,"messages":[]}"#)
                .is_ok()
        );
        assert!(
            p.parse_request("chat/completions", r#"{"model":"m","messages":[]}"#)
                .is_ok()
        );
    }

    #[test]
    fn build_payload_preserves_cap_field_and_pins_value() {
        let p = openai_test_provider();
        // A client using max_completion_tokens (o-series style) keeps that field,
        // pinned to the effective cap; max_tokens is not introduced.
        let out = p
            .build_payload(r#"{"model":"m","max_completion_tokens":128}"#, false)
            .unwrap();
        assert_eq!(
            out.get("max_completion_tokens").and_then(Value::as_u64),
            Some(128)
        );
        assert!(out.get("max_tokens").is_none());
        // A client using max_tokens keeps max_tokens (Vertex shim style).
        let out = p
            .build_payload(r#"{"model":"m","max_tokens":256}"#, true)
            .unwrap();
        assert_eq!(out.get("max_tokens").and_then(Value::as_u64), Some(256));
        assert!(out.get("max_completion_tokens").is_none());
        // Streaming forces stream=true + include_usage regardless of client input.
        assert_eq!(out.get("stream").and_then(Value::as_bool), Some(true));
        assert_eq!(
            out.pointer("/stream_options/include_usage")
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    /// The OpenAI-compatible base is the shared Vertex prefix plus
    /// `/endpoints/openapi`, for the global location as for a regional one.
    #[test]
    fn openai_vertex_base_url_follows_the_location_host() {
        for (location, want) in [
            (
                "global",
                "https://aiplatform.googleapis.com/v1/projects/p/locations/global/endpoints/openapi",
            ),
            (
                "us-central1",
                "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/endpoints/openapi",
            ),
        ] {
            let p = OpenAiProvider::vertex(
                reqwest::Client::new(),
                reqwest::Client::new(),
                "p",
                location,
                "t".to_owned(),
            );
            assert_eq!(p.base_url, want, "{location}");
        }
    }

    #[test]
    fn build_payload_vertex_always_uses_max_tokens() {
        let p = OpenAiProvider::vertex(
            reqwest::Client::new(),
            reqwest::Client::new(),
            "proj",
            "us-central1",
            "tok".to_owned(),
        );
        // On Vertex the cap goes to max_tokens even if the client sent
        // max_completion_tokens (the shim may ignore the latter and run uncapped).
        let out = p
            .build_payload(
                r#"{"model":"gemini-2.5-flash","max_completion_tokens":100}"#,
                false,
            )
            .unwrap();
        assert_eq!(out.get("max_tokens").and_then(Value::as_u64), Some(100));
        assert!(out.get("max_completion_tokens").is_none());
        // Model rewritten with the google/ prefix.
        assert_eq!(
            out.get("model").and_then(Value::as_str),
            Some("google/gemini-2.5-flash")
        );
    }

    #[test]
    fn parse_streaming_allows_stream_flag() {
        let p = openai_test_provider();
        // Buffered parse rejects a streaming request; streaming parse accepts it.
        let body = r#"{"model":"m","stream":true,"messages":[]}"#;
        assert!(p.parse_request("chat/completions", body).is_err());
        assert!(p.parse_streaming("chat/completions", body).is_ok());
    }

    #[test]
    fn usage_from_sse_data_parses_only_real_usage() {
        // A usage chunk.
        let u = usage_from_sse_data(
            r#"data: {"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":3}}"#,
            CacheSemantics::Inclusive,
        )
        .unwrap();
        assert_eq!(u.input_tokens, 9);
        assert_eq!(u.output_tokens, 3);
        // Per-delta null usage is not "seen".
        assert!(
            usage_from_sse_data(
                r#"data: {"choices":[{"delta":{}}],"usage":null}"#,
                CacheSemantics::Inclusive
            )
            .is_none()
        );
        assert!(usage_from_sse_data("data: [DONE]", CacheSemantics::Inclusive).is_none());
        assert!(usage_from_sse_data(": keep-alive", CacheSemantics::Inclusive).is_none());
        assert!(usage_from_sse_data("event: message", CacheSemantics::Inclusive).is_none());
    }

    #[test]
    fn openai_parse_reads_model_and_max_tokens() {
        let p = openai_test_provider();
        let parsed = p
            .parse_request(
                "chat/completions",
                r#"{"model":"gemini-2.5-flash","max_tokens":256,"messages":[]}"#,
            )
            .unwrap();
        assert_eq!(parsed.model, "gemini-2.5-flash");
        assert_eq!(parsed.max_output_tokens, 256);
        assert!(parsed.estimated_input_tokens >= 1);
    }

    #[test]
    fn openai_rejects_streaming_bad_path_and_missing_model() {
        let p = openai_test_provider();
        assert!(
            p.parse_request("chat/completions", r#"{"model":"m","stream":true}"#)
                .is_err()
        );
        assert!(p.parse_request("responses", r#"{"model":"m"}"#).is_err());
        assert!(p.parse_request("chat/completions", "{}").is_err());
    }

    #[test]
    fn openai_rejects_external_media() {
        let p = openai_test_provider();
        let body = r#"{"model":"m","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://x/y.png"}}]}]}"#;
        assert!(references_external_media(body));
        assert!(p.parse_request("chat/completions", body).is_err());
    }

    #[test]
    fn openai_effective_max_handles_null_zero_and_valid() {
        use serde_json::json;
        assert_eq!(openai_effective_max(&json!({})), OPENAI_DEFAULT_MAX_OUTPUT);
        // Present-but-null / zero must fall back to the default (not "unbounded").
        assert_eq!(
            openai_effective_max(&json!({"max_tokens": null})),
            OPENAI_DEFAULT_MAX_OUTPUT
        );
        assert_eq!(
            openai_effective_max(&json!({"max_tokens": 0})),
            OPENAI_DEFAULT_MAX_OUTPUT
        );
        assert_eq!(openai_effective_max(&json!({"max_tokens": 512})), 512);
        // max_completion_tokens wins over max_tokens.
        assert_eq!(
            openai_effective_max(&json!({"max_completion_tokens": 256, "max_tokens": 999})),
            256
        );
    }

    #[test]
    fn openai_usage_and_default_output() {
        let u = parse_openai_usage(
            &serde_json::json!({"usage":{"prompt_tokens":12,"completion_tokens":7}}),
            CacheSemantics::Inclusive,
        );
        assert_eq!(u.input_tokens, 12);
        assert_eq!(u.output_tokens, 7);

        let parsed = openai_test_provider()
            .parse_request("chat/completions", r#"{"model":"m","messages":[]}"#)
            .unwrap();
        assert_eq!(parsed.max_output_tokens, OPENAI_DEFAULT_MAX_OUTPUT);
    }

    #[test]
    fn media_guard_catches_evasions() {
        // snake_case, nested camelCase, whitespace-before-colon, file_id.
        assert!(references_external_media(r#"{"file_uri":"gs://x"}"#));
        assert!(references_external_media(
            r#"{"fileData":{"fileUri":"gs://x"}}"#
        ));
        assert!(references_external_media(
            "{\"type\" : \"url\", \"url\":\"h\"}"
        ));
        assert!(references_external_media(r#"{"file_id":"abc"}"#));
        // Plain text messages and non-JSON do not trip it.
        assert!(!references_external_media(
            r#"{"messages":[{"role":"user","content":"hi"}]}"#
        ));
        assert!(!references_external_media("not json"));
    }

    #[test]
    fn estimate_counts_multibyte_higher() {
        // Non-ASCII counts ~1 token/byte, so dense multibyte estimates higher
        // than the same byte-length of ASCII (which counts ~1/4).
        let ascii = estimate_input_tokens("aaaaaaaa");
        let cjk = estimate_input_tokens("好好好");
        assert!(cjk > ascii, "cjk={cjk} ascii={ascii}");
    }

    #[test]
    fn safe_rest_path_rejects_traversal() {
        assert!(safe_rest_path("messages").is_ok());
        assert!(safe_rest_path("").is_err());
        assert!(safe_rest_path("/etc/passwd").is_err());
        assert!(safe_rest_path("../secrets").is_err());
        assert!(safe_rest_path("a/../../b").is_err());
    }

    #[test]
    fn anthropic_parse_request_reads_model_and_max_tokens() {
        let p = anthropic_test_provider();
        let body = r#"{"model":"claude-3-5-sonnet","max_tokens":512,"messages":[]}"#;
        let parsed = p.parse_request("messages", body).unwrap();
        assert_eq!(parsed.model, "claude-3-5-sonnet");
        assert_eq!(parsed.max_output_tokens, 512);
        assert!(parsed.estimated_input_tokens >= 1);
    }

    #[test]
    fn anthropic_usage_parsing() {
        let v: Value =
            serde_json::from_str(r#"{"usage":{"input_tokens":11,"output_tokens":22}}"#).unwrap();
        let u = parse_anthropic_usage(&v);
        assert_eq!(u.input_tokens, 11);
        assert_eq!(u.output_tokens, 22);
    }

    #[test]
    fn vertex_model_extraction_and_usage() {
        assert_eq!(
            extract_vertex_model("publishers/google/models/gemini-1.5-pro:generateContent")
                .as_deref(),
            Some("gemini-1.5-pro")
        );
        assert_eq!(extract_vertex_model("no-model-here"), None);
        let v: Value = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":13}}"#,
        )
        .unwrap();
        let u = parse_vertex_usage(&v);
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 13);
    }

    // Lines copied verbatim from a live claude-haiku-4-5 stream capture, trailing
    // whitespace and all, so these fixtures cannot drift from what the API sends.
    const START: &str = r#"data: {"type":"message_start","message":{"model":"claude-haiku-4-5-20251001","id":"msg_x","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":3920,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0},"output_tokens":1,"service_tier":"standard"}}  }"#;
    const DELTA: &str = r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":3920,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":26}        }"#;
    const STOP: &str = r#"data: {"type":"message_stop"          }"#;

    fn meter_over(lines: &[&str]) -> AnthropicStreamMeter {
        let mut m = AnthropicStreamMeter::new();
        for l in lines {
            m.observe_line(l);
        }
        m
    }

    #[test]
    fn anthropic_stream_clean_finish_meters_the_final_delta() {
        let m = meter_over(&[
            "event: message_start",
            START,
            r#"data: {"type": "ping"}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"1, 2"}}"#,
            DELTA,
            STOP,
        ]);
        assert!(m.is_clean());
        let u = m.usage().unwrap();
        assert_eq!(u.input_tokens, 3920);
        // 26 from the delta, NOT the placeholder 1 in message_start.
        assert_eq!(u.output_tokens, 26);
        assert!(!u.suspect);
    }

    #[test]
    fn anthropic_stream_dying_after_start_is_not_clean() {
        // THE case this meter exists for. message_start carries a usage object
        // with output_tokens: 1, a placeholder. If "usage seen" meant "an event
        // carried usage", a stream that died after generating a full response
        // would settle on ONE output token.
        let m = meter_over(&["event: message_start", START]);
        assert!(!m.is_clean(), "no message_stop means not clean");
        assert_eq!(m.usage().unwrap().output_tokens, 1);
        assert!(m.started());
    }

    #[test]
    fn anthropic_stream_error_event_is_not_clean() {
        // An error event arrives on an otherwise-200 stream and the transport
        // then closes normally, so EOF alone would look like success.
        let m = meter_over(&[
            START,
            DELTA,
            r#"data: {"type":"error","error":{"type":"overloaded_error"}}"#,
        ]);
        assert!(!m.is_clean());
    }

    #[test]
    fn anthropic_stream_error_before_start_generated_nothing() {
        let m = meter_over(&[r#"data: {"type":"error","error":{"type":"overloaded_error"}}"#]);
        assert!(!m.is_clean());
        assert!(!m.started(), "nothing was generated, so nothing was billed");
        assert!(m.usage().is_none());
    }

    #[test]
    fn anthropic_stream_output_is_cumulative_not_summed() {
        // Successive deltas restate a running total. Summing them would bill
        // 10 + 20 + 26 = 56 for a 26-token response.
        let d = |n: u64| {
            format!(
                r#"data: {{"type":"message_delta","delta":{{}},"usage":{{"input_tokens":3920,"output_tokens":{n}}}}}"#
            )
        };
        let m = meter_over(&[START, &d(10), &d(20), &d(26), STOP]);
        assert_eq!(m.usage().unwrap().output_tokens, 26);
    }

    #[test]
    fn anthropic_stream_takes_the_larger_of_each_input_class() {
        // Synthetic: the live capture never produced non-zero cache classes, so
        // this pins the rule rather than an observed response. Both events carry
        // the input classes, and the larger can only over-charge.
        let start = r#"data: {"type":"message_start","message":{"usage":{"input_tokens":50,"cache_read_input_tokens":9000,"cache_creation_input_tokens":100,"output_tokens":1}}}"#;
        let delta = r#"data: {"type":"message_delta","delta":{},"usage":{"input_tokens":50,"cache_read_input_tokens":9000,"cache_creation_input_tokens":100,"output_tokens":40}}"#;
        let m = meter_over(&[start, delta, STOP]);
        let u = m.usage().unwrap();
        assert_eq!(u.cache_read_tokens, 9_000);
        assert_eq!(u.cache_write_tokens, 100);
        assert_eq!(u.output_tokens, 40);
        assert!(!u.suspect);
    }

    #[test]
    fn anthropic_stream_decreasing_input_class_is_suspect() {
        // Self-contradictory: no real version lowers an input class mid-stream.
        // Costing it on either reading could settle above a reservation sized
        // from the other, so it charges the reservation instead.
        let start = r#"data: {"type":"message_start","message":{"usage":{"input_tokens":9000,"output_tokens":1}}}"#;
        let delta = r#"data: {"type":"message_delta","delta":{},"usage":{"input_tokens":5,"output_tokens":40}}"#;
        let m = meter_over(&[start, delta, STOP]);
        assert!(m.usage().unwrap().suspect);
    }

    #[test]
    fn anthropic_stream_ignores_noise() {
        let m = meter_over(&[
            "event: message_start",
            ": keep-alive",
            "",
            r#"data: {"type": "ping"}"#,
            "data: not json at all",
            START,
            DELTA,
            STOP,
        ]);
        assert!(m.is_clean());
        assert_eq!(m.usage().unwrap().output_tokens, 26);
    }

    #[test]
    fn count_tokens_payload_drops_what_the_endpoint_refuses() {
        // Verified live: posting the client body unchanged returns
        // "400 max_tokens: Extra inputs are not permitted", and max_tokens is
        // mandatory on every Messages request, so exact admission failed closed
        // on 100% of Anthropic traffic.
        let body = r#"{"model":"claude-sonnet-4","max_tokens":1024,"stream":true,
            "temperature":0.5,"top_p":0.9,"stop_sequences":["x"],"metadata":{"user_id":"u"},
            "system":"be brief","tools":[{"name":"f","input_schema":{}}],
            "messages":[{"role":"user","content":"hello"}]}"#;
        let p = count_tokens_payload(body).unwrap();
        let obj = p.as_object().unwrap();

        for refused in [
            "max_tokens",
            "stream",
            "temperature",
            "top_p",
            "stop_sequences",
            "metadata",
        ] {
            assert!(!obj.contains_key(refused), "{refused} must not be sent");
        }
        // Everything that contributes input tokens must survive. A live probe
        // counted 8 tokens bare and 541 with system+tools, so dropping these
        // would under-reserve tool-heavy requests.
        for kept in ["model", "messages", "system", "tools"] {
            assert!(obj.contains_key(kept), "{kept} must be counted");
        }
    }

    #[test]
    fn count_tokens_payload_requires_the_countable_minimum() {
        assert!(count_tokens_payload(r#"{"max_tokens":10}"#).is_err());
        assert!(count_tokens_payload("not json").is_err());
    }

    #[test]
    fn outbound_body_pins_the_service_tier() {
        // Absent means "auto", which uses priority capacity at a premium where
        // an org has it, and almost nobody sets the field. Pinning it in the
        // adapter rather than a route handler means the legacy
        // /v1/anthropic/messages route is covered too.
        let pinned = pin_outbound(r#"{"model":"m","max_tokens":10,"messages":[]}"#, false);
        let v: Value = serde_json::from_str(&pinned).unwrap();
        assert_eq!(v["service_tier"], "standard_only");
        // An explicit value is overwritten, not merely defaulted.
        let pinned = pin_outbound(r#"{"model":"m","service_tier":"auto"}"#, false);
        let v: Value = serde_json::from_str(&pinned).unwrap();
        assert_eq!(v["service_tier"], "standard_only");
        // Unparseable bodies pass through: parse_common already rejected them,
        // so this must not be what lets a bad body upstream.
        assert_eq!(pin_outbound("not json", false), "not json");
    }

    #[test]
    fn outbound_body_pins_stream_to_a_real_boolean() {
        // `stream_requested` accepts "yes", 1 and " TRUE " so a truthy value
        // cannot slip past the buffered check. That leniency means the value we
        // ROUTE on can differ from what a strict upstream honours: route to the
        // relay on "yes", forward "yes", and an upstream reading it as false
        // returns a buffered body the relay then forwards containing no SSE
        // events at all.
        let pinned = pin_outbound(r#"{"model":"m","stream":"yes"}"#, true);
        let v: Value = serde_json::from_str(&pinned).unwrap();
        assert_eq!(v["stream"], Value::Bool(true));

        let pinned = pin_outbound(r#"{"model":"m","stream":true}"#, false);
        let v: Value = serde_json::from_str(&pinned).unwrap();
        assert_eq!(v["stream"], Value::Bool(false));
    }

    #[test]
    fn anthropic_refuses_a_longer_cache_ttl() {
        let p = anthropic_test_provider();
        // A 1h write bills at 2x the input rate, a 5m write at 1.25x, against a
        // single configured cache-write rate. Reserving at one and settling at
        // the other overshoots a hard cap on every request.
        let one_hour = r#"{"model":"m","max_tokens":10,"messages":[],
            "system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#;
        assert!(p.parse_request("messages", one_hour).is_err());
        // The refusal is fixed text: the TTL is the client's.
        let marked = r#"{"model":"m","max_tokens":10,"messages":[],
            "system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"<script>probe"}}]}"#;
        match p.parse_request("messages", marked) {
            Err(ProviderError::BadRequest(m)) => assert!(!m.contains("probe"), "{m}"),
            other => panic!("expected a 400, got {other:?}"),
        }
        // The default TTL is fine and still marks the request cache-writing.
        let default_ttl = r#"{"model":"m","max_tokens":10,"messages":[],
            "system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}"#;
        assert!(
            p.parse_request("messages", default_ttl)
                .unwrap()
                .may_cache_write
        );
    }

    #[test]
    fn anthropic_refuses_external_media_on_both_paths() {
        // This guard used to live only in the gateway, which the streaming
        // handler bypasses, so a streamed request with a URL image would have
        // reserved from body bytes and settled far above it.
        let p = anthropic_test_provider();
        let media = r#"{"model":"m","max_tokens":10,"messages":[{"role":"user",
            "content":[{"type":"image","source":{"type":"url","url":"https://x/y.png"}}]}]}"#;
        assert!(p.parse_request("messages", media).is_err());
    }

    #[test]
    fn cache_breakpoint_detection_survives_unicode_escaping() {
        // A raw substring scan is evadable: this body's decoded key IS
        // cache_control, but the literal string never appears in the bytes. A
        // false negative here reserves at the input rate while the upstream
        // bills a cache write at up to 2x, so the request settles above its
        // reservation and walks a hard cap.
        // Build the escaped key at runtime so the source itself cannot contain
        // the literal string. The JSON escape for '_' decodes to '_', so the
        // decoded key is cache_control while the raw bytes never spell it.
        let bs = '\\';
        let escaped = format!(
            r#"{{"system":[{{"type":"text","text":"x","cache{bs}u005fcontrol":{{"type":"ephemeral"}}}}]}}"#
        );
        assert!(
            !escaped.contains("cache_control"),
            "fixture must not contain the literal key, or it proves nothing"
        );
        assert!(
            requests_prompt_cache_write(&escaped),
            "unicode-escaped cache_control key must be detected"
        );
        // Plain spelling, nested in messages rather than system.
        let plain = r#"{"messages":[{"content":[{"cache_control":{"type":"ephemeral"}}]}]}"#;
        assert!(requests_prompt_cache_write(plain));
        // No breakpoint: must NOT reserve at the write rate, or every request on
        // the provider is inflated for nothing.
        let none = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
        assert!(!requests_prompt_cache_write(none));
    }

    #[test]
    fn anthropic_reserves_the_write_rate_only_for_requests_that_can_write() {
        let p = anthropic_test_provider();
        let plain = p
            .parse_request("messages", r#"{"model":"m","max_tokens":10,"messages":[]}"#)
            .unwrap();
        assert!(!plain.may_cache_write);
        assert!(!p.prompt_reserve_profile(&plain).can_cache_write);

        let cached = p
            .parse_request(
                "messages",
                r#"{"model":"m","max_tokens":10,"system":[{"type":"text","text":"x",
                    "cache_control":{"type":"ephemeral"}}],"messages":[]}"#,
            )
            .unwrap();
        assert!(cached.may_cache_write);
        assert!(p.prompt_reserve_profile(&cached).can_cache_write);
    }

    #[test]
    fn rate_switching_request_fields_are_refused() {
        let p = openai_test_provider();
        let cases = [
            (
                r#"{"model":"m","messages":[],"modalities":["text","audio"]}"#,
                "audio modality",
            ),
            (
                r#"{"model":"m","messages":[],"audio":{"voice":"alloy","format":"wav"}}"#,
                "audio config",
            ),
            (
                r#"{"model":"m","messages":[],"prediction":{"type":"content","content":"x"}}"#,
                "prediction",
            ),
            (
                r#"{"model":"m","messages":[],"web_search_options":{}}"#,
                "web search",
            ),
            (
                r#"{"model":"m","messages":[],"service_tier":"priority"}"#,
                "priority tier",
            ),
            (
                r#"{"model":"m","messages":[],"tools":[{"type":"web_search_preview"}]}"#,
                "hosted tool",
            ),
        ];
        for (body, what) in cases {
            assert!(
                p.parse_request("chat/completions", body).is_err(),
                "{what} must be refused: it re-rates tokens or adds a fee no token count reports"
            );
        }
        // The ordinary shapes still pass, including an explicit text-only
        // modality and a normal function tool.
        assert!(
            p.parse_request(
                "chat/completions",
                r#"{"model":"m","messages":[],"modalities":["text"],"service_tier":"default",
                    "tools":[{"type":"function","function":{"name":"f"}}]}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn anthropic_server_side_tools_are_refused() {
        let p = anthropic_test_provider();
        // Billed per search, reported in no token field.
        assert!(
            p.parse_request(
                "messages",
                r#"{"model":"m","max_tokens":10,"messages":[],
                    "tools":[{"type":"web_search_20250305","name":"web_search"}]}"#
            )
            .is_err()
        );
        // A client-executed custom tool is token-only and stays allowed.
        assert!(
            p.parse_request(
                "messages",
                r#"{"model":"m","max_tokens":10,"messages":[],
                    "tools":[{"name":"f","input_schema":{}}]}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn unpriced_token_classes_in_a_response_are_marked_suspect() {
        // Backstop for a class we have not enumerated. Audio is rejected at
        // parse, so seeing it here means evasion or a new upstream behaviour.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,
                "completion_tokens_details":{"audio_tokens":15}}}"#,
        )
        .unwrap();
        assert!(parse_openai_usage(&v, CacheSemantics::Inclusive).suspect);
    }

    #[test]
    fn anthropic_prompt_side_counts_all_three_disjoint_cache_classes() {
        // The classes are disjoint: total prompt = 5 + 200_000 + 1_000.
        // Reading input_tokens alone would meter 5 and under-charge by ~99.997%.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"input_tokens":5,"cache_read_input_tokens":200000,
                "cache_creation_input_tokens":1000,"output_tokens":22}}"#,
        )
        .unwrap();
        let u = parse_anthropic_usage(&v);
        // Anthropic's classes are already disjoint, so they map straight across
        // with no arithmetic. input_tokens is FRESH prompt only.
        assert_eq!(u.input_tokens, 5);
        assert_eq!(u.cache_read_tokens, 200_000);
        assert_eq!(u.cache_write_tokens, 1_000);
        assert_eq!(u.output_tokens, 22);
        // Nothing is lost by the split: every billable prompt token is still
        // accounted for, just at its own rate.
        assert_eq!(u.total_prompt_tokens(), 201_005);
    }

    #[test]
    fn anthropic_output_is_authoritative_and_not_adjusted_for_thinking() {
        // output_tokens ALREADY includes thinking tokens; output_tokens_details is
        // a read-only breakdown. Adding it would double-count the thinking leg.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"input_tokens":10,"output_tokens":500,
                "output_tokens_details":{"thinking_tokens":400}}}"#,
        )
        .unwrap();
        let u = parse_anthropic_usage(&v);
        assert_eq!(u.output_tokens, 500);
    }

    #[test]
    fn vertex_output_includes_thinking_tokens() {
        // candidatesTokenCount EXCLUDES thoughts. Thinking bills at the output
        // rate, so the output leg is candidates + thoughts. Reading candidates
        // alone drops the thinking leg entirely, and on 2.5 thinking models a
        // response can be ALL thinking (candidates absent), metering output as 0.
        let v: Value = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":29,"candidatesTokenCount":491,
                "thoughtsTokenCount":69,"totalTokenCount":589}}"#,
        )
        .unwrap();
        let u = parse_vertex_usage(&v);
        assert_eq!(u.input_tokens, 29);
        assert_eq!(u.output_tokens, 560);
        // The documented total must reconcile: prompt + output == totalTokenCount.
        assert_eq!(u.input_tokens + u.output_tokens, 589);
    }

    #[test]
    fn vertex_output_is_all_thinking_when_candidates_absent() {
        // Observed shape: a thinking model that emits no visible text omits
        // candidatesTokenCount entirely rather than reporting zero.
        let v: Value = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":60008,"thoughtsTokenCount":4,
                "totalTokenCount":60012}}"#,
        )
        .unwrap();
        let u = parse_vertex_usage(&v);
        assert_eq!(u.output_tokens, 4);
    }

    #[test]
    fn vertex_input_includes_tool_use_prompt_tokens() {
        // toolUsePromptTokenCount is disjoint from promptTokenCount and is
        // prompt-side, so it belongs on the input leg.
        let v: Value = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":100,"toolUsePromptTokenCount":40,
                "candidatesTokenCount":10,"thoughtsTokenCount":0,"totalTokenCount":150}}"#,
        )
        .unwrap();
        let u = parse_vertex_usage(&v);
        assert_eq!(u.input_tokens, 140);
        assert_eq!(u.output_tokens, 10);
        assert_eq!(u.input_tokens + u.output_tokens, 150);
    }

    #[test]
    fn vertex_does_not_double_count_cached_content() {
        // Unlike Anthropic, Gemini's promptTokenCount INCLUDES cached content.
        // Adding cachedContentTokenCount would bill the cached prefix twice.
        let v: Value = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":60008,"cachedContentTokenCount":59364,
                "candidatesTokenCount":4,"totalTokenCount":60012}}"#,
        )
        .unwrap();
        let u = parse_vertex_usage(&v);
        // Gemini reports cached tokens INSIDE promptTokenCount, so they are
        // split out rather than added. Billing input_tokens as the full 60_008
        // AND cache_read as 59_364 would charge the cached prefix twice.
        assert_eq!(u.input_tokens, 644);
        assert_eq!(u.cache_read_tokens, 59_364);
        assert_eq!(u.total_prompt_tokens(), 60_008);
        // Gemini has no cache-write class on a generateContent call: creation is
        // a separate cachedContents request that never transits the proxy.
        assert_eq!(u.cache_write_tokens, 0);
    }

    #[test]
    fn token_class_sums_saturate_instead_of_wrapping() {
        // A hostile or broken upstream must not be able to wrap the sum into a
        // small number, which would meter an enormous request as a tiny one.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"input_tokens":18446744073709551615,
                "cache_read_input_tokens":18446744073709551615,"output_tokens":1}}"#,
        )
        .unwrap();
        let u = parse_anthropic_usage(&v);
        assert_eq!(u.input_tokens, u64::MAX);

        let v: Value = serde_json::from_str(
            r#"{"usageMetadata":{"candidatesTokenCount":18446744073709551615,
                "thoughtsTokenCount":18446744073709551615}}"#,
        )
        .unwrap();
        let u = parse_vertex_usage(&v);
        assert_eq!(u.output_tokens, u64::MAX);
    }

    #[test]
    fn vertex_unknown_token_class_is_billed_not_dropped() {
        // Simulates Google shipping a token class this parser predates: the total
        // exceeds the sum of every class we know. The remainder must be billed at
        // the output rate, not silently discarded, which is how thinking tokens
        // went unbilled for months.
        let v: Value = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":50,
                "thoughtsTokenCount":25,"someFutureTokenCount":300,"totalTokenCount":475}}"#,
        )
        .unwrap();
        let u = parse_vertex_usage(&v);
        assert_eq!(u.input_tokens, 100);
        // 50 + 25 known, plus the 300 the parser cannot name.
        assert_eq!(u.output_tokens, 375);
        assert_eq!(u.input_tokens + u.output_tokens, 475);
    }

    #[test]
    fn vertex_known_classes_leave_no_residual() {
        // The reconciliation must be inert when every class is accounted for,
        // otherwise it would double-charge ordinary traffic. Uses the exact shape
        // observed from a live gemini-2.5-flash thinking response.
        let v: Value = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":115,"candidatesTokenCount":766,
                "thoughtsTokenCount":3326,"totalTokenCount":4207}}"#,
        )
        .unwrap();
        let u = parse_vertex_usage(&v);
        assert_eq!(u.input_tokens, 115);
        assert_eq!(u.output_tokens, 4092);
    }

    #[test]
    fn vertex_total_below_known_classes_does_not_underflow() {
        // A total smaller than the parts is self-contradictory. saturating_sub
        // must floor the residual at zero rather than wrapping into a colossal
        // over-charge.
        let v: Value = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":50,
                "totalTokenCount":10}}"#,
        )
        .unwrap();
        let u = parse_vertex_usage(&v);
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 50);
    }

    #[test]
    fn openai_output_recovers_reasoning_on_an_excluding_upstream() {
        // Live Vertex shim shape: completion_tokens EXCLUDES reasoning, so the
        // field alone under-charges by the reasoning count.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":29,"completion_tokens":491,
                "completion_tokens_details":{"reasoning_tokens":69},"total_tokens":589}}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v, CacheSemantics::Inclusive);
        assert_eq!(u.input_tokens, 29);
        assert_eq!(u.output_tokens, 560);
    }

    #[test]
    fn openai_output_is_not_double_counted_on_an_including_upstream() {
        // OpenAI's own shape: completion_tokens ALREADY includes reasoning.
        // Naively adding reasoning would over-charge by 69.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":29,"completion_tokens":560,
                "completion_tokens_details":{"reasoning_tokens":69},"total_tokens":589}}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v, CacheSemantics::Inclusive);
        assert_eq!(u.output_tokens, 560);
    }

    #[test]
    fn openai_output_when_everything_was_reasoning() {
        // Observed live: an all-reasoning response omits completion_tokens
        // entirely. Reading the field alone meters ZERO output.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":60008,
                "completion_tokens_details":{"reasoning_tokens":4},"total_tokens":60012}}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v, CacheSemantics::Inclusive);
        assert_eq!(u.input_tokens, 60_008);
        assert_eq!(u.output_tokens, 4);
    }

    #[test]
    fn openai_without_total_falls_back_to_adding_reasoning() {
        // No total to reconcile against, so add reasoning explicitly: exact on an
        // excluding upstream, an over-charge on an including one.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":10,"completion_tokens":100,
                "completion_tokens_details":{"reasoning_tokens":40}}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_openai_usage(&v, CacheSemantics::Inclusive).output_tokens,
            140
        );
    }

    #[test]
    fn openai_contradictory_total_never_lowers_the_output_leg() {
        // total < prompt + completion is self-contradictory; the reported
        // completion must still be charged in full.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":10}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_openai_usage(&v, CacheSemantics::Inclusive).output_tokens,
            50
        );
    }

    #[test]
    fn openai_inclusive_splits_cached_out_of_the_prompt() {
        // Verified live against the Vertex shim: cached_tokens sits INSIDE
        // prompt_tokens, so it is split out. Billing both in full here would
        // double-charge the cached prefix.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":60008,"prompt_tokens_details":{"cached_tokens":59364},
                "completion_tokens":4,"total_tokens":60012}}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v, CacheSemantics::Inclusive);
        assert_eq!(u.input_tokens, 644);
        assert_eq!(u.cache_read_tokens, 59_364);
        assert_eq!(u.total_prompt_tokens(), 60_008);
        assert!(!u.suspect);
    }

    #[test]
    fn openai_unverified_bills_both_classes_in_full() {
        // An operator can point the custom adapter at anything. If that upstream
        // reports cached tokens ADDITIVELY, subtracting would under-charge by
        // roughly the cached fraction, steered by the caller's own prompt. So
        // both classes stand: exact if additive, an over-charge if inclusive.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":10000,"prompt_tokens_details":{"cached_tokens":8000},
                "completion_tokens":100,"total_tokens":10100}}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v, CacheSemantics::Unverified);
        assert_eq!(u.input_tokens, 10_000);
        assert_eq!(u.cache_read_tokens, 8_000);
        assert!(!u.suspect);
    }

    #[test]
    fn openai_unverified_ambiguous_total_is_marked_suspect() {
        // The additive shape: cached reported disjointly AND counted in total.
        // The output leg is derived as total - prompt, so the cached count would
        // otherwise land there at the OUTPUT rate on top of being billed as a
        // cache read, settling far above a reservation sized for the prompt.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":8000},
                "completion_tokens":50,"total_tokens":8150}}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v, CacheSemantics::Unverified);
        assert!(
            u.suspect,
            "an unverified upstream whose total exceeds its own output fields is \
             ambiguous and must not be costed"
        );

        // The ordinary inclusive-looking shape stays costable: total accounts for
        // prompt and completion exactly, so nothing is hiding in the residual.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":10000,"prompt_tokens_details":{"cached_tokens":8000},
                "completion_tokens":100,"total_tokens":10100}}"#,
        )
        .unwrap();
        assert!(!parse_openai_usage(&v, CacheSemantics::Unverified).suspect);
    }

    #[test]
    fn openai_unverified_settles_within_its_reservation() {
        // End to end on the worst COSTABLE unverified shape: the whole prompt
        // reported as cached, plus a full output leg. Nothing may exceed the
        // reservation the profile asked for.
        let p = crate::pricing::ModelPrice::new("openai", "m", 3_000_000, 15_000_000)
            .with_cache_rates(
                Some(300_000),
                None,
                crate::pricing::CacheRateFallback::default(),
            );
        let profile = crate::pricing::PromptReserveProfile {
            can_cache_write: false,
            may_double_bill_prompt: true,
        };
        let reserved = p.reserve_micros(1_000, 500, profile);

        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":1000,"prompt_tokens_details":{"cached_tokens":1000},
                "completion_tokens":500,"total_tokens":1500}}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v, CacheSemantics::Unverified);
        assert!(!u.suspect);
        let settled = p.cost_micros(u);
        assert!(
            settled <= reserved,
            "settle {settled} exceeded reserve {reserved}"
        );
    }

    #[test]
    fn the_vertex_count_body_carries_only_what_counts() {
        // This allowlist has already been wrong once: it shipped with
        // `toolConfig`, which `:countTokens` does not accept, and every request
        // carrying it would have been refused. The live probe is the source of
        // truth for the field list; this pins what the probe found so a future
        // edit cannot re-add a rejected field unnoticed.
        let body = r#"{
            "contents":[{"role":"user","parts":[{"text":"hi"}]}],
            "systemInstruction":{"parts":[{"text":"be brief"}]},
            "tools":[{"functionDeclarations":[]}],
            "toolConfig":{"functionCallingConfig":{"mode":"AUTO"}},
            "generationConfig":{"maxOutputTokens":16},
            "safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT"}],
            "labels":{"team":"x"},
            "cachedContent":"projects/p/locations/l/cachedContents/1"
        }"#;
        let out = vertex_count_payload(&serde_json::from_str(body).unwrap()).expect("a valid body");
        let obj = out.as_object().expect("object");

        // Accepted by the endpoint AND counted.
        for k in ["contents", "systemInstruction", "tools"] {
            assert!(obj.contains_key(k), "{k} must be carried: it is counted");
        }
        // Rejected by the endpoint with `Unknown name`, verified live. Carrying
        // any of these fails every request that includes it.
        for k in ["toolConfig", "safetySettings", "labels", "cachedContent"] {
            assert!(!obj.contains_key(k), "{k} is rejected by :countTokens");
        }
        // Carried, because a responseSchema inside it IS counted: live, 1 token
        // bare against 51 with a small schema attached. It looks like it
        // describes only the output, which is why the first version dropped it.
        assert!(
            obj.contains_key("generationConfig"),
            "generationConfig carries responseSchema, which counts as prompt"
        );

        // Google accepts either spelling and its own examples use the snake
        // form. Reading only the camel one drops the system prompt from the
        // count while the request still carries it: a silent under-reservation.
        let snake = vertex_count_payload(&serde_json::json!(
            {"contents":[{"role":"user","parts":[{"text":"hi"}]}],
             "system_instruction":{"parts":[{"text":"be brief"}]}}
        ))
        .expect("valid");
        assert_eq!(
            snake.get("systemInstruction"),
            Some(&serde_json::json!({"parts":[{"text":"be brief"}]})),
            "system_instruction must be carried, normalised to camelCase"
        );

        // An explicit null is not a value.
        let nulled =
            vertex_count_payload(&serde_json::json!({"contents":[{"role":"user"}],"tools":null}))
                .expect("valid");
        assert!(!nulled.as_object().unwrap().contains_key("tools"));

        // No contents means nothing to count, and Vertex would refuse it too.
        assert!(matches!(
            vertex_count_payload(&serde_json::json!({"generationConfig":{}})),
            Err(ProviderError::BadRequest(_))
        ));
    }

    #[test]
    fn a_count_endpoint_refusal_is_classified_by_status_and_carries_no_provider_text() {
        use crate::provider::UpstreamFault;
        let meta = ResponseMeta::default();
        let refuse = |status: reqwest::StatusCode, body: &str| {
            count_refusal(status, &meta, body.as_bytes(), "count_tokens")
        };
        let anthropic = r#"{"type":"error","error":{"type":"invalid_request_error",
            "message":"messages: at least one message is required"}}"#;

        // The caller's body: 400, 413 and 422 keep their status. The count
        // endpoints used to report every failure as upstream, so a client bug
        // came back as a 502 that SDKs retried.
        for (status, code) in [
            (reqwest::StatusCode::BAD_REQUEST, 400),
            (reqwest::StatusCode::PAYLOAD_TOO_LARGE, 413),
            (reqwest::StatusCode::UNPROCESSABLE_ENTITY, 422),
        ] {
            match refuse(status, anthropic) {
                Some(ProviderError::Refused {
                    status: s,
                    fault,
                    detail,
                }) => {
                    assert_eq!(s, code);
                    assert_eq!(fault, UpstreamFault::Request(code));
                    // The type is kept for the log, the message only in the
                    // log-only body.
                    assert_eq!(detail.error_type.as_deref(), Some("invalid_request_error"));
                }
                other => panic!("{status}: {other:?}"),
            }
        }

        // A 429 is the provider rate limiting this deployment.
        assert!(matches!(
            refuse(reqwest::StatusCode::TOO_MANY_REQUESTS, anthropic),
            Some(ProviderError::Refused {
                fault: UpstreamFault::RateLimited { .. },
                ..
            })
        ));

        // Everything else is the provider's or Tollgate's own, including 4xx: a
        // denylist here would blame the caller for Tollgate's rotated key, for
        // 100% of traffic, with no upstream signal to alert on.
        for status in [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
            reqwest::StatusCode::NOT_FOUND,
            reqwest::StatusCode::REQUEST_TIMEOUT,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            assert!(
                matches!(
                    refuse(status, anthropic),
                    Some(ProviderError::Refused {
                        fault: UpstreamFault::Provider,
                        ..
                    })
                ),
                "{status} is not the caller's fault"
            );
        }

        // A success is not a refusal at all: the caller parses the body.
        assert!(refuse(reqwest::StatusCode::OK, anthropic).is_none());

        // No provider text in the error's text, on any status: a Vertex message
        // names the project and region in its resource path, a 400 included.
        let vertex = r#"{"error":{"message":"Permission denied on resource //aiplatform.googleapis.com/projects/example-project/locations/us-central1/publishers/google/models/x","code":403,"status":"PERMISSION_DENIED"}}"#;
        for status in [
            reqwest::StatusCode::BAD_REQUEST,
            reqwest::StatusCode::FORBIDDEN,
        ] {
            let e = refuse(status, vertex).expect("a refusal");
            let text = e.to_string();
            assert!(!text.contains("example-project"), "{text}");
            assert!(!text.contains("Permission denied"), "{text}");
            if let ProviderError::Refused { detail, .. } = e {
                assert_eq!(detail.error_type.as_deref(), Some("PERMISSION_DENIED"));
                assert_eq!(detail.error_code.as_deref(), Some("403"));
            }
        }

        // An HTML refusal from an edge proxy still classifies by status.
        assert!(matches!(
            refuse(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                "<html>slow down</html>"
            ),
            Some(ProviderError::Refused {
                fault: UpstreamFault::RateLimited { .. },
                ..
            })
        ));
    }

    #[test]
    fn openai_unverified_settles_within_its_reservation_across_a_tier() {
        // The same guarantee where a long-context tier is configured and the
        // request sits between half the threshold and the threshold.
        //
        // This is the shape that broke: the counted prompt is 150k, below the
        // 200k threshold and below the reservation's guard band, but the
        // unverified adapter bills the prompt on both legs so settlement sizes
        // it at 250k and re-rates the whole request. The reservation has to size
        // on the same worst case the RATE already assumes, or the settlement
        // clears it by the full multiple and walks a hard cap.
        let p = crate::pricing::ModelPrice::new("openai", "m", 3_000_000, 15_000_000)
            .with_cache_rates(
                Some(300_000),
                None,
                crate::pricing::CacheRateFallback::default(),
            )
            .with_long_context(crate::pricing::LongContextTier {
                threshold_tokens: 200_000,
                multiple_permille: 2_000,
                output_multiple_permille: 2_000,
            });
        let profile = crate::pricing::PromptReserveProfile {
            can_cache_write: false,
            may_double_bill_prompt: true,
        };
        let reserved = p.reserve_micros(150_000, 1_000, profile);

        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":150000,"prompt_tokens_details":{"cached_tokens":100000},
                "completion_tokens":1000,"total_tokens":151000}}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v, CacheSemantics::Unverified);
        assert!(!u.suspect, "this shape is costable, not a guess");
        assert!(
            u.threshold_prompt_tokens() > 200_000,
            "the double-billed prompt is what settlement sizes on"
        );
        let settled = p.cost_micros(u);
        assert!(
            settled <= reserved,
            "settle {settled} exceeded reserve {reserved}"
        );
    }

    #[test]
    fn openai_contradictory_cached_count_is_marked_suspect() {
        // More cached than prompt cannot be true. Neither reading is safe to
        // charge, so the usage is flagged and the caller bills the reservation.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":9000},
                "completion_tokens":5,"total_tokens":105}}"#,
        )
        .unwrap();
        let u = parse_openai_usage(&v, CacheSemantics::Inclusive);
        assert!(u.suspect);
        assert!(u.is_untrustworthy());
    }

    #[test]
    fn cache_semantics_and_write_capability_are_pinned_per_upstream() {
        // A refactor flipping either of these silently changes what every
        // request is charged, so pin them explicitly.
        // Vertex's shim was verified live to report cached tokens inclusively,
        // so it subtracts and does not need the double-bill reservation.
        let vertex = OpenAiProvider::vertex(
            reqwest::Client::new(),
            reqwest::Client::new(),
            "proj",
            "us-central1",
            "t".to_owned(),
        );
        assert_eq!(vertex.cache_semantics(), CacheSemantics::Inclusive);
        assert!(!vertex.can_report_cache_write());
        let req = ParsedRequest {
            model: "m".to_owned(),
            estimated_input_tokens: 1,
            max_output_tokens: 1,
            may_cache_write: false,
            anthropic_beta: None,
            external_media: false,
        };
        assert!(!vertex.prompt_reserve_profile(&req).may_double_bill_prompt);

        // A custom base URL is whatever the operator pointed it at, so it stays
        // unverified and MUST reserve for a prompt billed on two legs.
        let unverified = openai_test_provider();
        assert_eq!(unverified.cache_semantics(), CacheSemantics::Unverified);
        assert!(
            unverified
                .prompt_reserve_profile(&req)
                .may_double_bill_prompt
        );
    }

    #[test]
    fn vertex_cached_is_split_from_prompt_not_from_tool_use() {
        // cachedContentTokenCount is a subset of promptTokenCount alone. If it
        // were subtracted from prompt + toolUsePrompt, a cached count between
        // the two would silently reclassify tool-use tokens as cache reads,
        // which are cheaper: an under-charge on a broken response.
        let v: Value = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":1000,"toolUsePromptTokenCount":500,
                "cachedContentTokenCount":800,"candidatesTokenCount":10,
                "totalTokenCount":1510}}"#,
        )
        .unwrap();
        let u = parse_vertex_usage(&v);
        // 1000 - 800 cached, plus the 500 tool-use tokens, all prompt-rate.
        assert_eq!(u.input_tokens, 700);
        assert_eq!(u.cache_read_tokens, 800);
        assert_eq!(u.output_tokens, 10);
        assert!(!u.suspect);
    }

    #[test]
    fn vertex_cached_exceeding_prompt_is_marked_suspect() {
        let v: Value = serde_json::from_str(
            r#"{"usageMetadata":{"promptTokenCount":100,"toolUsePromptTokenCount":500,
                "cachedContentTokenCount":400,"candidatesTokenCount":10}}"#,
        )
        .unwrap();
        let u = parse_vertex_usage(&v);
        assert!(u.suspect, "cached > promptTokenCount must not be costed");
    }

    #[test]
    fn openai_mistyped_usage_fields_meter_zero_by_design() {
        // serde_json's as_u64 rejects floats, strings and negatives, so any
        // present-but-mistyped field reads as absent. Some OpenAI-compatible
        // shims have shipped float usage fields, so pin this deliberately rather
        // than letting it be an accident.
        //
        // Mistyped completion, sound total: the residual rescues it.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":29,"completion_tokens":491.0,"total_tokens":589}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_openai_usage(&v, CacheSemantics::Inclusive).output_tokens,
            560
        );

        // Mistyped total, sound completion: falls back and still charges it.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":29,"completion_tokens":491,"total_tokens":"589"}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_openai_usage(&v, CacheSemantics::Inclusive).output_tokens,
            491
        );

        // Everything mistyped: meters zero. The buffered path then floors to the
        // reserved input cost, and the streaming path charges the reservation
        // because a zero-cost settle would otherwise bill a whole stream at one
        // micro. Neither silently bills zero.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":-29,"completion_tokens":491.5,"total_tokens":"589"}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_openai_usage(&v, CacheSemantics::Inclusive),
            Usage::default()
        );
    }

    #[test]
    fn openai_missing_prompt_tokens_bills_the_whole_total_as_output() {
        // With no prompt_tokens to subtract, the residual attributes everything
        // to the output leg. That over-charges on any sane price book, where the
        // output rate is at least the input rate, and never under-charges.
        let v: Value =
            serde_json::from_str(r#"{"usage":{"completion_tokens":10,"total_tokens":589}}"#)
                .unwrap();
        let u = parse_openai_usage(&v, CacheSemantics::Inclusive);
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 589);
    }

    #[test]
    fn buffered_and_streaming_agree_on_the_same_usage_payload() {
        // The streaming path meters through usage_from_sse_value (reached via
        // usage_from_sse_data here) while the buffered path meters through
        // parse_openai_usage. If they ever diverge, a caller can pick the
        // cheaper one by setting stream:true. Pin them together on OpenAI's
        // separate usage-chunk shape.
        // The payload MUST carry cached_tokens, or the two semantics produce
        // identical numbers and the loop below proves nothing.
        let usage = r#"{"prompt_tokens":29,"completion_tokens":491,
            "prompt_tokens_details":{"cached_tokens":20},
            "completion_tokens_details":{"reasoning_tokens":69},"total_tokens":589}"#;

        let buffered: Value = serde_json::from_str(&format!(r#"{{"usage":{usage}}}"#)).unwrap();
        let sse = format!(r#"data: {{"choices":[],"usage":{usage}}}"#);

        // Parity must hold under EVERY semantics, not just the default one:
        // the whole point is that the client's choice of path cannot change what
        // it is charged.
        for sem in [CacheSemantics::Inclusive, CacheSemantics::Unverified] {
            let from_buffered = parse_openai_usage(&buffered, sem);
            let from_stream = usage_from_sse_data(&sse, sem).expect("usage chunk carries usage");
            assert_eq!(from_buffered, from_stream, "paths diverged under {sem:?}");
            assert_eq!(from_stream.output_tokens, 560);
        }
    }

    #[test]
    fn vertex_absent_usage_metadata_is_zero() {
        // No usageMetadata at all must yield a zero Usage so the gateway's
        // 2xx-with-no-usage fallback engages instead of metering something wrong.
        let v: Value = serde_json::from_str(r#"{"candidates":[]}"#).unwrap();
        let u = parse_vertex_usage(&v);
        assert_eq!(u, Usage::default());
    }

    // -----------------------------------------------------------------------
    // Native Vertex: gs:// allowlist, request allowlist, thinking rules
    // -----------------------------------------------------------------------

    const GENERATE: &str = "publishers/google/models/gemini-3.8-flash:generateContent";
    const ALLOWED_BUCKET: &str = "tollgate-media";

    fn vertex_test_provider() -> VertexProvider {
        VertexProvider::new(
            reqwest::Client::new(),
            "p".to_owned(),
            "global".to_owned(),
            "t".to_owned(),
        )
        .with_gcs_buckets(vec![ALLOWED_BUCKET.to_owned()])
    }

    /// A minimal valid native body with `part` as the only user part.
    fn vertex_body_with_part(part: &Value) -> String {
        serde_json::json!({
            "contents": [{"role": "user", "parts": [part]}],
            "generationConfig": {"maxOutputTokens": 16}
        })
        .to_string()
    }

    fn vertex_parse(body: &str) -> Result<ParsedRequest, ProviderError> {
        vertex_test_provider().parse_request(GENERATE, body)
    }

    #[test]
    fn file_uris_are_allowed_only_from_allowlisted_buckets() {
        let buckets = vec![ALLOWED_BUCKET.to_owned()];
        for ok in [
            "gs://tollgate-media/a.png",
            "gs://tollgate-media/dir/b.pdf",
            "gs://tollgate-media",
        ] {
            assert!(check_file_uri(ok, &buckets).is_ok(), "{ok} must be allowed");
        }
        for refused in [
            "gs://other-bucket/a.png",
            "gs://tollgate-media-2/a.png",
            "gs://Tollgate-media/a.png",
            "gs://TOLLGATE-MEDIA/a.png",
            "gs://tollgate-media/a%2Fb.png",
            "gs://tollgate%2Dmedia/a.png",
            "gs://tollgate-media/../other/a.png",
            "gs://tollgate-media\\a.png",
            "gs:///a.png",
            "gs://",
            "GS://tollgate-media/a.png",
            "ftp://example.com/a.png",
            "http://example.com/a.png",
            "file:///etc/passwd",
            "",
            " gs://tollgate-media/a.png",
        ] {
            assert!(
                matches!(
                    check_file_uri(refused, &buckets),
                    Err(ProviderError::Forbidden(_))
                ),
                "{refused:?} must be forbidden"
            );
        }
        // No allowlist: no gs:// at all.
        assert!(check_file_uri("gs://tollgate-media/a.png", &[]).is_err());
    }

    /// Remote https media is a 400 until it can be bounded; Google storage and
    /// API hosts, and any URI carrying credentials, are a 403 whatever else
    /// is allowed, so a bucket off the allowlist is not reachable over https.
    #[test]
    fn https_file_uris_are_refused_and_google_storage_hosts_are_forbidden() {
        let buckets = vec![ALLOWED_BUCKET.to_owned()];
        for remote in [
            "https://example.com/a.png",
            "https://www.youtube.com/watch?v=abc",
            "HTTPS://example.com/a.png",
            "https://notgoogleapis.com/a.png",
            "https://storage.googleapis.com.example.com/a.png",
        ] {
            match check_file_uri(remote, &buckets) {
                Err(ProviderError::BadRequest(m)) => assert_eq!(m, REMOTE_MEDIA_UNSUPPORTED),
                other => panic!("{remote}: expected a 400, got {other:?}"),
            }
        }
        for forbidden in [
            "https://storage.googleapis.com/operator-private/secret.pdf",
            "https://STORAGE.GoogleAPIs.com/operator-private/secret.pdf",
            "https://storage.googleapis.com./operator-private/secret.pdf",
            "https://operator-private.storage.googleapis.com/secret.pdf",
            "https://Operator-Private.Storage.Googleapis.Com./secret.pdf",
            "https://storage.cloud.google.com/operator-private/secret.pdf",
            "https://STORAGE.CLOUD.GOOGLE.COM./operator-private/secret.pdf",
            "https://generativelanguage.googleapis.com/v1beta/files/abc",
            "https://aiplatform.googleapis.com/v1/projects/p",
            "https://googleapis.com/a",
            "https://storage.googleapis.com:443/operator-private/secret.pdf",
            "https:\\\\storage.googleapis.com/operator-private/secret.pdf",
            "https://user@example.com/a.png",
            "https://user:pass@example.com/a.png",
            "https://:pass@example.com/a.png",
            "http://storage.googleapis.com/operator-private/secret.pdf",
        ] {
            match check_file_uri(forbidden, &buckets) {
                Err(ProviderError::Forbidden(m)) => assert_eq!(m, FORBIDDEN_SOURCE),
                other => panic!("{forbidden}: expected a 403, got {other:?}"),
            }
        }
        // Through the parser too, in the snake_case spelling, with fixed text.
        let part = serde_json::json!({"file_data": {"mime_type": "application/pdf",
            "file_uri": "https://operator-private.storage.googleapis.com/secret.pdf"}});
        match vertex_parse(&vertex_body_with_part(&part)) {
            Err(ProviderError::Forbidden(m)) => assert!(!m.contains("operator-private")),
            other => panic!("expected a 403, got {other:?}"),
        }
        let part = serde_json::json!({"fileData": {"mimeType": "application/pdf",
            "fileUri": "https://example.com/private-name.pdf"}});
        match vertex_parse(&vertex_body_with_part(&part)) {
            Err(ProviderError::BadRequest(m)) => assert!(!m.contains("private-name")),
            other => panic!("expected a 400, got {other:?}"),
        }
    }

    #[test]
    fn a_forbidden_file_uri_is_a_403_with_fixed_text_wherever_it_sits() {
        let bad = "gs://operator-private-bucket/secret.pdf";
        let file = serde_json::json!({"fileData": {"mimeType": "application/pdf", "fileUri": bad}});
        let snake =
            serde_json::json!({"file_data": {"mime_type": "application/pdf", "file_uri": bad}});
        let bodies = [
            vertex_body_with_part(&file),
            vertex_body_with_part(&snake),
            serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                "systemInstruction": {"parts": [file.clone()]},
                "generationConfig": {"maxOutputTokens": 16}
            })
            .to_string(),
            serde_json::json!({
                "contents": [{"role": "user", "parts": [{"functionResponse": {
                    "name": "f", "response": {}, "parts": [snake.clone()]
                }}]}],
                "generationConfig": {"maxOutputTokens": 16}
            })
            .to_string(),
            // A non-string URI is not a URI Tollgate can check.
            vertex_body_with_part(&serde_json::json!({"fileData": {"fileUri": 7}})),
        ];
        for body in bodies {
            match vertex_parse(&body) {
                Err(ProviderError::Forbidden(m)) => {
                    assert_eq!(m, FORBIDDEN_SOURCE);
                    assert!(!m.contains("operator-private-bucket"));
                }
                other => panic!("expected Forbidden for {body}, got {other:?}"),
            }
        }
        // The allowlisted bucket passes, in both spellings.
        for part in [
            serde_json::json!({"file_data": {"mime_type": "application/pdf", "file_uri": "gs://tollgate-media/a.pdf"}}),
            serde_json::json!({"fileData": {"mimeType": "application/pdf", "fileUri": "gs://tollgate-media/a.pdf"}}),
        ] {
            assert!(
                vertex_parse(&vertex_body_with_part(&part)).is_ok(),
                "{part}"
            );
        }
    }

    #[test]
    fn the_gcs_allowlist_takes_plain_bucket_names_only() {
        assert!(validate_gcs_buckets(&[]).is_ok());
        assert!(validate_gcs_buckets(&["tollgate-media".to_owned(), "a.b_c-1".to_owned()]).is_ok());
        for bad in ["", "gs://x", "x/y", "X", "a..b", "a%2f", "a\\b", "a b"] {
            assert!(validate_gcs_buckets(&[bad.to_owned()]).is_err(), "{bad:?}");
        }
    }

    fn assert_bad_request(body: &str, what: &str) {
        assert!(
            matches!(vertex_parse(body), Err(ProviderError::BadRequest(_))),
            "{what} must be refused: {body}"
        );
    }

    #[test]
    fn native_vertex_refuses_top_level_fields_off_the_allowlist() {
        for key in [
            "cachedContent",
            "cached_content",
            "routingConfig",
            "modelConfig",
            "modelArmorConfig",
            "model",
            "somethingNew",
        ] {
            let body = serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                "generationConfig": {"maxOutputTokens": 16},
                key: "x"
            })
            .to_string();
            assert_bad_request(&body, key);
        }
        // Everything on the list is accepted.
        let ok = serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "systemInstruction": {"parts": [{"text": "be brief"}]},
            "generationConfig": {"maxOutputTokens": 16},
            "tools": [{"functionDeclarations": [{"name": "f"}]}],
            "toolConfig": {"functionCallingConfig": {"mode": "AUTO"}},
            "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"}],
            "labels": {"team": "x"}
        })
        .to_string();
        let labelled = vertex_test_provider().with_client_labels(true);
        assert!(labelled.parse_request(GENERATE, &ok).is_ok());
    }

    #[test]
    fn native_vertex_refuses_every_server_side_tool() {
        for tool in [
            "retrieval",
            "googleSearch",
            "google_search",
            "googleSearchRetrieval",
            "codeExecution",
            "urlContext",
            "googleMaps",
            "enterpriseWebSearch",
            "computerUse",
            "somethingNew",
        ] {
            let body = serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                "generationConfig": {"maxOutputTokens": 16},
                "tools": [{tool: {}}]
            })
            .to_string();
            assert_bad_request(&body, tool);
        }
        let body = serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {"maxOutputTokens": 16},
            "toolConfig": {"functionCallingConfig": {}, "retrievalConfig": {}}
        })
        .to_string();
        assert_bad_request(&body, "toolConfig.retrievalConfig");
    }

    fn with_generation_config(config: &Value) -> String {
        serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": config
        })
        .to_string()
    }

    #[test]
    fn native_vertex_generation_config_is_an_allowlist() {
        for field in [
            "speechConfig",
            "imageConfig",
            "audioTimestamp",
            "routingConfig",
            "enableAffectiveDialog",
            "somethingNew",
        ] {
            let body = with_generation_config(&serde_json::json!({
                "maxOutputTokens": 16,
                field: {}
            }));
            assert_bad_request(&body, field);
        }
        let refused = [
            (
                serde_json::json!({"maxOutputTokens": 16, "candidateCount": 2}),
                "candidateCount 2",
            ),
            (
                serde_json::json!({"maxOutputTokens": 16, "candidateCount": "1"}),
                "candidateCount string",
            ),
            (
                serde_json::json!({"maxOutputTokens": 16, "responseModalities": ["TEXT", "AUDIO"]}),
                "audio modality",
            ),
            (
                serde_json::json!({"maxOutputTokens": 16, "responseModalities": ["IMAGE"]}),
                "image modality",
            ),
            (
                serde_json::json!({"maxOutputTokens": 16, "thinkingConfig": {"thinkingBudget": -2}}),
                "thinkingBudget -2",
            ),
            (
                serde_json::json!({"maxOutputTokens": 16, "thinkingConfig": {"thinkingBudget": 1.5}}),
                "fractional thinkingBudget",
            ),
            (
                serde_json::json!({"maxOutputTokens": 16, "thinkingConfig": {"other": 1}}),
                "unknown thinkingConfig field",
            ),
            (
                serde_json::json!({"temperature": 0.2}),
                "missing maxOutputTokens",
            ),
        ];
        for (config, what) in refused {
            assert_bad_request(&with_generation_config(&config), what);
        }
        let accepted = serde_json::json!({
            "temperature": 0.2, "topP": 0.9, "topK": 40, "maxOutputTokens": 16,
            "stopSequences": ["x"], "responseMimeType": "application/json",
            "responseSchema": {"type": "OBJECT"}, "presencePenalty": 0.1,
            "frequencyPenalty": 0.1, "seed": 1, "responseLogprobs": true, "logprobs": 2,
            "mediaResolution": "MEDIA_RESOLUTION_HIGH", "candidateCount": 1,
            "responseModalities": ["text"],
            "thinkingConfig": {"includeThoughts": true, "thinkingBudget": -1, "thinkingLevel": "HIGH"}
        });
        assert!(vertex_parse(&with_generation_config(&accepted)).is_ok());
    }

    /// Labels land in the operator's billing export, so they are refused
    /// unless the operator opted in.
    #[test]
    fn native_vertex_labels_need_the_operator_opt_in() {
        let body = serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {"maxOutputTokens": 16},
            "labels": {"cost_center": "someone-else"}
        })
        .to_string();
        match vertex_parse(&body) {
            Err(ProviderError::BadRequest(m)) => {
                assert!(m.contains("allow_client_labels"), "{m}");
                assert!(!m.contains("someone-else"), "{m}");
            }
            other => panic!("expected a 400, got {other:?}"),
        }
        // `labels` in snake_case too: there is no other spelling.
        let snake = body.replace("generationConfig", "generation_config");
        assert!(vertex_parse(&snake).is_err());
        let labelled = vertex_test_provider().with_client_labels(true);
        assert!(labelled.parse_request(GENERATE, &body).is_ok());
    }

    #[test]
    fn native_vertex_labels_are_bounded() {
        let labelled = vertex_test_provider().with_client_labels(true);
        let vertex_parse = |body: &str| labelled.parse_request(GENERATE, body);
        let assert_bad_request = |body: &str, what: &str| {
            assert!(
                matches!(vertex_parse(body), Err(ProviderError::BadRequest(_))),
                "{what} must be refused: {body}"
            );
        };
        let labels = |n: usize, value: &str| {
            let map: serde_json::Map<String, Value> = (0..n)
                .map(|i| (format!("k{i}"), Value::String(value.to_owned())))
                .collect();
            serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                "generationConfig": {"maxOutputTokens": 16},
                "labels": map
            })
            .to_string()
        };
        assert!(vertex_parse(&labels(64, &"v".repeat(63))).is_ok());
        assert_bad_request(&labels(65, "v"), "65 labels");
        assert_bad_request(&labels(1, &"v".repeat(64)), "a 64-character value");
        // Characters, not bytes: 63 multibyte characters fit.
        assert!(vertex_parse(&labels(1, &"é".repeat(63))).is_ok());
        let long_key = serde_json::json!({
            "contents": [], "generationConfig": {"maxOutputTokens": 16},
            "labels": {"k".repeat(64): "v"}
        })
        .to_string();
        assert_bad_request(&long_key, "a 64-character key");
        let not_string =
            r#"{"contents":[],"generationConfig":{"maxOutputTokens":16},"labels":{"k":1}}"#;
        assert_bad_request(not_string, "a non-string label");
    }

    #[test]
    fn both_spellings_of_one_field_are_refused_and_duplicates_too() {
        for (body, what) in [
            (
                r#"{"contents":[],"generationConfig":{"maxOutputTokens":16},"generation_config":{"maxOutputTokens":16}}"#,
                "generationConfig twice",
            ),
            (
                r#"{"contents":[],"generationConfig":{"maxOutputTokens":16,"max_output_tokens":99999}}"#,
                "maxOutputTokens twice",
            ),
            (
                r#"{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"a","mime_type":"b","data":""}}]}],"generationConfig":{"maxOutputTokens":16}}"#,
                "inlineData.mimeType twice",
            ),
            (
                r#"{"contents":[],"contents":[],"generationConfig":{"maxOutputTokens":16}}"#,
                "a duplicate top-level key",
            ),
            (
                r#"{"contents":[],"generationConfig":{"maxOutputTokens":16,"maxOutputTokens":99999}}"#,
                "a duplicate nested key",
            ),
            (
                r#"{"contents":[{"role":"user","parts":[{"functionCall":{"name":"f","args":{"a":1,"a":2}}}]}],"generationConfig":{"maxOutputTokens":16}}"#,
                "a duplicate key inside client args",
            ),
        ] {
            assert_bad_request(body, what);
        }
    }

    /// Depth stays bounded by serde_json's recursion limit: a deeply nested
    /// body is a 400, not a stack overflow.
    #[test]
    fn a_deeply_nested_body_is_refused_not_a_crash() {
        let deep = format!(
            r#"{{"contents":[],"labels":{}{}}}"#,
            "[".repeat(100_000),
            "]".repeat(100_000)
        );
        assert_bad_request(&deep, "100,000 nested arrays");
    }

    #[test]
    fn snake_case_is_checked_as_camel_case_and_client_maps_are_untouched() {
        let body = r#"{
            "contents":[{"role":"user","parts":[
                {"text":"hi"},
                {"function_call":{"name":"f","args":{"user_id":1,"userId":2}}},
                {"function_response":{"name":"f","response":{"user_id":1,"userId":2}}},
                {"inline_data":{"mime_type":"image/png","data":""}}
            ]}],
            "system_instruction":{"parts":[{"text":"be brief"}]},
            "generation_config":{
                "max_output_tokens":16,
                "response_mime_type":"application/json",
                "response_schema":{"type":"OBJECT","properties":{"user_id":{"type":"STRING"},"userId":{"type":"STRING"}}},
                "thinking_config":{"thinking_budget":-1}
            },
            "tools":[{"function_declarations":[{"name":"f","parameters":{"type":"OBJECT","properties":{"user_id":{"type":"STRING"},"userId":{"type":"STRING"}}}}]}],
            "tool_config":{"function_calling_config":{"mode":"AUTO"}},
            "safety_settings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"BLOCK_NONE"}],
            "labels":{"team_name":"x"}
        }"#;
        let p = vertex_test_provider().with_client_labels(true);
        assert!(p.parse_request(GENERATE, body).is_ok());
        let checked = p.checked_body(body).unwrap();
        let want = serde_json::json!({
            "contents":[{"role":"user","parts":[
                {"text":"hi"},
                {"functionCall":{"name":"f","args":{"user_id":1,"userId":2}}},
                {"functionResponse":{"name":"f","response":{"user_id":1,"userId":2}}},
                {"inlineData":{"mimeType":"image/png","data":""}}
            ]}],
            "systemInstruction":{"parts":[{"text":"be brief"}]},
            "generationConfig":{
                "maxOutputTokens":16,
                "responseMimeType":"application/json",
                "responseSchema":{"type":"OBJECT","properties":{"user_id":{"type":"STRING"},"userId":{"type":"STRING"}}},
                "thinkingConfig":{"thinkingBudget":-1}
            },
            "tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"OBJECT","properties":{"user_id":{"type":"STRING"},"userId":{"type":"STRING"}}}}]}],
            "toolConfig":{"functionCallingConfig":{"mode":"AUTO"}},
            "safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"BLOCK_NONE"}],
            "labels":{"team_name":"x"}
        });
        assert_eq!(checked, want);
        assert_eq!(camel_case("a_b_c").as_deref(), Some("aBC"));
        assert_eq!(
            camel_case("max_output_tokens2").as_deref(),
            Some("maxOutputTokens2")
        );
        assert_eq!(
            camel_case("maxOutputTokens").as_deref(),
            Some("maxOutputTokens")
        );
        assert_eq!(camel_case("a_1b").as_deref(), Some("a1b"));
        for loose in [
            "Max_output_tokens",
            "MaxOutputTokens",
            "file__data",
            "file_data_",
            "_file_data",
            "File__Data",
            "fileData_",
            "FILE_DATA",
            "file_Data",
            "fileÜri",
            "1file",
            "",
        ] {
            assert_eq!(camel_case(loose), None, "{loose:?}");
        }
    }

    /// The reviewers' finding: the forwarded body renamed `file__data` (and
    /// other loose spellings) to `fileData`, while fast admission read the
    /// client's bytes and saw no media. Loose spellings are now refused, and
    /// the client's bytes are what is forwarded, so both readings agree.
    #[test]
    fn fast_admission_sees_media_under_every_spelling_that_is_forwarded() {
        let p = vertex_test_provider();
        for key in [
            "file__data",
            "fileData_",
            "file_data_",
            "File__Data",
            "FILE_DATA",
        ] {
            let body = format!(
                r#"{{"contents":[{{"role":"user","parts":[{{"{key}":{{"mimeType":"video/mp4","fileUri":"gs://tollgate-media/x"}}}}]}}],"generationConfig":{{"maxOutputTokens":16}}}}"#
            );
            assert!(
                matches!(
                    p.parse_request(GENERATE, &body),
                    Err(ProviderError::BadRequest(_))
                ),
                "{key}"
            );
            assert!(references_external_media(&body), "{key}");
        }
        for (key, uri_key) in [("file_data", "file_uri"), ("fileData", "fileUri")] {
            let body = format!(
                r#"{{"contents":[{{"role":"user","parts":[{{"{key}":{{"mime_type":"video/mp4","{uri_key}":"gs://tollgate-media/x"}}}}]}}],"generationConfig":{{"maxOutputTokens":16}}}}"#
            );
            let parsed = p.parse_request(GENERATE, &body).unwrap();
            assert!(parsed.external_media, "{key}");
            assert!(references_external_media(&body), "{key}");
        }
        let text = vertex_body_with_part(&serde_json::json!({"text": "hi"}));
        assert!(!p.parse_request(GENERATE, &text).unwrap().external_media);
        // The raw-body check ignores case and underscores as `loose_key` does.
        for raw in [
            r#"{"file__data":{}}"#,
            r#"{"File_Uri":"x"}"#,
            r#"{"FILEID":"x"}"#,
            r#"{"image__url":"x"}"#,
            r#"{"inputAudio":{}}"#,
            r#"{"type":"Image_URL"}"#,
            r#"{"type":"input__audio"}"#,
        ] {
            assert!(references_external_media(raw), "{raw}");
        }
    }

    /// A local upstream that records each request (path, headers, body) and
    /// answers every one with `reply`.
    async fn recording_stub(
        reply: &'static str,
    ) -> (
        String,
        std::sync::Arc<Mutex<Vec<(String, http::HeaderMap, String)>>>,
    ) {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let app = axum::Router::new().fallback(
            move |uri: http::Uri, headers: http::HeaderMap, body: String| {
                let s = s.clone();
                async move {
                    s.lock()
                        .unwrap()
                        .push((uri.path().to_owned(), headers, body));
                    ([(http::header::CONTENT_TYPE, "application/json")], reply)
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }

    /// Tollgate forwards the client's own bytes once they are checked, so key
    /// order in client-owned maps survives (re-serialising would sort it) and
    /// a snake_case body goes out as sent. The fast estimate is over those
    /// bytes; the count is a separate request built from the checked value.
    #[tokio::test]
    async fn vertex_forwards_the_clients_bytes_and_counts_the_checked_value() {
        let (base, seen) = recording_stub(
            r#"{"totalTokens":3,"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4}}"#,
        )
        .await;
        let p = VertexProvider::new(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            "p".to_owned(),
            "global".to_owned(),
            "t".to_owned(),
        )
        .with_base(base);
        let snake = r#"{"contents":[{"role":"user","parts":[{"text":"caf\u00e9 café"}]}],
                       "system_instruction":{"parts":[{"text":"x"}]},
                       "generation_config":{"max_output_tokens":16}}"#;
        let ordered = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],
            "generationConfig":{"maxOutputTokens":16,"responseMimeType":"application/json",
            "responseJsonSchema":{"type":"object","properties":{"zeta":{"type":"string"},"alpha":{"type":"string"},"mid":{"type":"string"}}}}}"#;
        for body in [snake, ordered] {
            let parsed = p.parse_request(GENERATE, body).unwrap();
            assert_eq!(parsed.estimated_input_tokens, estimate_input_tokens(body));
            p.count_input_tokens(GENERATE, body, &parsed).await.unwrap();
            let resp = p.forward(GENERATE, body, &parsed).await.unwrap();
            assert_eq!(resp.status, 200);
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        for (i, body) in [(0, snake), (2, ordered)] {
            assert!(seen[i].0.ends_with(":countTokens"), "{}", seen[i].0);
            assert!(
                seen[i + 1].0.ends_with(":generateContent"),
                "{}",
                seen[i + 1].0
            );
            assert_eq!(seen[i + 1].2, body, "the forwarded bytes are the client's");
        }
        let counted: Value = serde_json::from_str(&seen[0].2).unwrap();
        assert!(counted.get("systemInstruction").is_some());
        let fwd = &seen[3].2;
        let (z, a, m) = (
            fwd.find("\"zeta\"").unwrap(),
            fwd.find("\"alpha\"").unwrap(),
            fwd.find("\"mid\"").unwrap(),
        );
        assert!(z < a && a < m, "properties keep the client's order: {fwd}");
    }

    /// On the OpenAI-compatible route to Vertex, a bare id and its `google/`
    /// spelling are one upstream model: one `ParsedRequest` model (so one price
    /// row, one budget scope and one ledger key) and one thinking rule, on both
    /// the buffered and the streaming parse. A custom upstream keeps the id.
    #[test]
    fn a_bare_vertex_openai_id_is_parsed_as_the_forwarded_google_id() {
        let shim = openai_vertex_test_provider().with_thinking_rules(
            ThinkingRules::from_config(&[rule(
                "vertex-openai",
                "google/gemini-2.5-pro",
                "additive",
                500,
            )])
            .unwrap(),
        );
        let prices = crate::pricing::PriceBook::from_prices([
            crate::pricing::ModelPrice::new("openai", "google/gemini-2.5-pro", 7, 9),
            crate::pricing::ModelPrice::new("openai", "gemini-2.5-pro", 1, 1),
        ]);
        let body = |model: &str| {
            serde_json::json!({"model": model, "max_tokens": 100, "messages": []}).to_string()
        };
        for model in ["gemini-2.5-pro", "google/gemini-2.5-pro"] {
            for parsed in [
                shim.parse_request("chat/completions", &body(model)),
                shim.parse_streaming("chat/completions", &body(model)),
            ] {
                let parsed = parsed.unwrap();
                assert_eq!(parsed.model, "google/gemini-2.5-pro", "{model}");
                assert_eq!(
                    parsed.max_output_tokens, 600,
                    "{model}: the operator's rule"
                );
                assert_eq!(
                    prices
                        .lookup("openai", &parsed.model)
                        .map(|p| p.cost_micros(Usage::new(1_000_000, 0))),
                    Some(7),
                    "{model}: the google/ price row"
                );
            }
            let sent = shim.build_payload(&body(model), false).unwrap();
            assert_eq!(sent["model"], "google/gemini-2.5-pro", "{model}");
        }
        // The forwarded id keeps the client's case; only the rule lookup ignores it.
        let parsed = shim
            .parse_request("chat/completions", &body("Gemini-2.5-Pro"))
            .unwrap();
        assert_eq!(parsed.model, "google/Gemini-2.5-Pro");
        assert_eq!(parsed.max_output_tokens, 600);
        assert_eq!(
            shim.build_payload(&body("Gemini-2.5-Pro"), false).unwrap()["model"],
            "google/Gemini-2.5-Pro"
        );
        // Any other prefix is sent, and priced, as written.
        assert_eq!(
            shim.parse_request("chat/completions", &body("meta/llama-4"))
                .unwrap()
                .model,
            "meta/llama-4"
        );

        let custom = OpenAiProvider::custom(
            reqwest::Client::new(),
            reqwest::Client::new(),
            "http://127.0.0.1:1".to_owned(),
            "k".to_owned(),
        );
        for model in ["gpt-x", "org/gpt-x"] {
            assert_eq!(
                custom
                    .parse_request("chat/completions", &body(model))
                    .unwrap()
                    .model,
                model
            );
            assert_eq!(
                custom.build_payload(&body(model), false).unwrap()["model"],
                model
            );
        }
    }

    #[test]
    fn gemini_models_need_a_thinking_rule_on_both_vertex_routes() {
        // Built in: gemini-3.8-flash on native Vertex.
        assert_eq!(
            vertex_parse(&with_generation_config(
                &serde_json::json!({"maxOutputTokens": 16})
            ))
            .unwrap()
            .max_output_tokens,
            16
        );
        let other = vertex_test_provider().parse_request(
            "publishers/google/models/gemini-2.5-flash:generateContent",
            &with_generation_config(&serde_json::json!({"maxOutputTokens": 16})),
        );
        match other {
            Err(ProviderError::BadRequest(m)) => assert!(m.contains("[[providers.models]]"), "{m}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        // A non-Gemini model has no thinking rule.
        assert!(
            vertex_test_provider()
                .parse_request(
                    "publishers/meta/models/llama-4:generateContent",
                    &with_generation_config(&serde_json::json!({"maxOutputTokens": 16})),
                )
                .is_ok()
        );

        // Built in on the OpenAI-compatible route, bare and prefixed.
        let shim = openai_vertex_test_provider();
        for model in ["gemini-3.8-flash", "google/gemini-3.8-flash"] {
            let body =
                serde_json::json!({"model": model, "max_tokens": 64, "messages": []}).to_string();
            assert_eq!(
                shim.parse_request("chat/completions", &body)
                    .unwrap()
                    .max_output_tokens,
                64,
                "{model}"
            );
        }
        for model in ["gemini-2.5-flash", "google/gemini-2.5-pro"] {
            let body =
                serde_json::json!({"model": model, "max_tokens": 64, "messages": []}).to_string();
            assert!(
                matches!(
                    shim.parse_request("chat/completions", &body),
                    Err(ProviderError::BadRequest(_))
                ),
                "{model}"
            );
            assert!(
                shim.parse_streaming("chat/completions", &body).is_err(),
                "{model}"
            );
        }
        // A custom upstream has no thinking rule at all.
        let body = r#"{"model":"gemini-2.5-flash","max_tokens":64,"messages":[]}"#;
        assert!(
            openai_test_provider()
                .parse_request("chat/completions", body)
                .is_ok()
        );
    }

    fn openai_vertex_test_provider() -> OpenAiProvider {
        OpenAiProvider::vertex(
            reqwest::Client::new(),
            reqwest::Client::new(),
            "p",
            "global",
            "t".to_owned(),
        )
    }

    fn rule(
        upstream: &str,
        model: &str,
        thinking: &str,
        max: u64,
    ) -> crate::config::ModelRuleConfig {
        crate::config::ModelRuleConfig {
            upstream: upstream.to_owned(),
            model: model.to_owned(),
            thinking: Some(thinking.to_owned()),
            max_thinking_tokens: max,
        }
    }

    #[test]
    fn operator_thinking_rows_extend_and_override_the_built_ins() {
        let rules = ThinkingRules::from_config(&[
            rule("vertex", "gemini-2.5-flash", "within_output_cap", 0),
            rule("vertex", "gemini-2.5-pro", "additive", 1_000),
            rule("vertex-openai", "google/gemini-2.5-pro", "additive", 1_000),
            rule("vertex-openai", "gemini-3.8-flash", "additive", 500),
        ])
        .unwrap();
        let native = vertex_test_provider().with_thinking_rules(rules.clone());
        let path = |m: &str| format!("publishers/google/models/{m}:generateContent");
        let cap = |budget: Option<i64>| {
            let mut config = serde_json::json!({"maxOutputTokens": 100});
            if let Some(b) = budget {
                config["thinkingConfig"] = serde_json::json!({"thinkingBudget": b});
            }
            with_generation_config(&config)
        };
        let leg = |model: &str, budget: Option<i64>| {
            native
                .parse_request(&path(model), &cap(budget))
                .unwrap()
                .max_output_tokens
        };
        assert_eq!(leg("gemini-2.5-flash", None), 100);
        // Additive: the cap plus the operator's bound...
        assert_eq!(leg("gemini-2.5-pro", None), 1_100);
        assert_eq!(leg("gemini-2.5-pro", Some(-1)), 1_100);
        assert_eq!(leg("gemini-2.5-pro", Some(400)), 1_100);
        // ...raised by a larger explicit budget, which is a soft limit.
        assert_eq!(leg("gemini-2.5-pro", Some(5_000)), 5_100);
        // The built-in row is untouched on the route the operator did not name.
        assert_eq!(leg("gemini-3.8-flash", None), 100);

        let shim = openai_vertex_test_provider().with_thinking_rules(rules);
        let leg = |body: Value| {
            shim.parse_request("chat/completions", &body.to_string())
                .unwrap()
                .max_output_tokens
        };
        // The operator row overrides the built-in one for the same model:
        // the bare id is forwarded as the `google/` one, so both spellings,
        // in any case, take the operator's row.
        for model in [
            "gemini-3.8-flash",
            "google/gemini-3.8-flash",
            "GEMINI-3.8-Flash",
            "Google/Gemini-3.8-Flash",
        ] {
            assert_eq!(
                leg(serde_json::json!({"model": model, "max_tokens": 100, "messages": []})),
                600,
                "{model}"
            );
        }
        // A bare id reaches the operator's `google/` row.
        assert_eq!(
            leg(serde_json::json!({"model": "gemini-2.5-pro", "max_tokens": 100, "messages": []})),
            1_100
        );
        assert_eq!(
            leg(
                serde_json::json!({"model": "google/gemini-2.5-pro", "max_tokens": 100, "messages": []})
            ),
            1_100
        );
        assert_eq!(
            leg(
                serde_json::json!({"model": "google/gemini-2.5-pro", "max_tokens": 100, "messages": [],
                "google": {"thinking_config": {"thinking_budget": 3_000}}})
            ),
            3_100
        );
        assert_eq!(
            leg(
                serde_json::json!({"model": "google/gemini-2.5-pro", "max_tokens": 100, "messages": [],
                "extra_body": {"google": {"thinking_config": {"thinking_budget": 4_000}}}})
            ),
            4_100
        );
        // The outbound cap stays the client's: thinking is reserved, not granted.
        let out = shim
            .build_payload(
                r#"{"model":"google/gemini-2.5-pro","max_tokens":100}"#,
                false,
            )
            .unwrap();
        assert_eq!(out["max_tokens"], 100);
    }

    #[test]
    fn invalid_thinking_rows_refuse_startup() {
        for (rows, what) in [
            (
                vec![rule("vertex", "gemini-2.5-pro", "additive", 0)],
                "additive without a bound",
            ),
            (
                vec![rule("vertex", "gemini-2.5-pro", "within_output_cap", 10)],
                "a bound that is ignored",
            ),
            (
                vec![rule("vertex", "gemini-2.5-pro", "sometimes", 0)],
                "an unknown rule",
            ),
            (
                vec![rule("anthropic", "claude-x", "additive", 10)],
                "an unknown upstream",
            ),
            (
                vec![rule("vertex", "", "within_output_cap", 0)],
                "an empty model",
            ),
            (
                vec![
                    rule("vertex", "gemini-2.5-pro", "within_output_cap", 0),
                    rule("vertex", "gemini-2.5-pro", "additive", 10),
                ],
                "two rows for one model",
            ),
            (
                vec![
                    rule("vertex", "gemini-2.5-pro", "additive", 10),
                    rule("vertex", "Gemini-2.5-PRO", "additive", 20),
                ],
                "two rows for one model in different case",
            ),
            (
                vec![
                    rule("vertex-openai", "gemini-2.5-pro", "additive", 10),
                    rule("vertex-openai", "google/gemini-2.5-pro", "additive", 20),
                ],
                "a bare and a google/ row for one forwarded model",
            ),
            (
                vec![
                    rule("vertex-openai", "GEMINI-2.5-pro", "within_output_cap", 0),
                    rule("vertex-openai", "Google/Gemini-2.5-Pro", "additive", 20),
                ],
                "the same, in different case",
            ),
        ] {
            assert!(ThinkingRules::from_config(&rows).is_err(), "{what}");
        }
        // One model id on each route is two rows, not a duplicate.
        assert!(
            ThinkingRules::from_config(&[
                rule("vertex", "gemini-2.5-pro", "additive", 10),
                rule("vertex-openai", "gemini-2.5-pro", "additive", 10),
            ])
            .is_ok()
        );
        let mut missing = rule("vertex", "gemini-2.5-pro", "additive", 10);
        missing.thinking = None;
        assert!(
            ThinkingRules::from_config(&[missing]).is_err(),
            "no thinking"
        );
    }

    // -----------------------------------------------------------------------
    // Vertex OpenAI-compatible: google / extra_body.google, service_tier
    // -----------------------------------------------------------------------

    #[test]
    fn google_settings_are_checked_on_the_vertex_shim_only() {
        let shim = openai_vertex_test_provider();
        let parse = |extra: Value| {
            let mut body =
                serde_json::json!({"model": "gemini-3.8-flash", "max_tokens": 64, "messages": []});
            for (k, v) in extra.as_object().unwrap() {
                body[k] = v.clone();
            }
            shim.parse_request("chat/completions", &body.to_string())
        };
        let thinking = serde_json::json!({"thinking_config": {"include_thoughts": true, "thinking_budget": 32, "thinking_level": "low"}});
        assert!(parse(serde_json::json!({"google": thinking})).is_ok());
        assert!(parse(serde_json::json!({"extra_body": {"google": thinking}})).is_ok());
        for (extra, what) in [
            (
                serde_json::json!({"google": thinking, "extra_body": {"google": thinking}}),
                "both shapes",
            ),
            (
                serde_json::json!({"google": {"cached_content": "projects/p/cachedContents/1"}}),
                "google.cached_content",
            ),
            (
                serde_json::json!({"extra_body": {"google": {"cached_content": "projects/p/cachedContents/1"}}}),
                "extra_body.google.cached_content",
            ),
            (
                serde_json::json!({"google": {"thinking_config": {}, "safety_settings": []}}),
                "another google setting",
            ),
            (
                serde_json::json!({"google": {"thinking_config": {"other": 1}}}),
                "another thinking field",
            ),
            (
                serde_json::json!({"google": {"thinking_config": {"thinking_budget": -3}}}),
                "a negative budget",
            ),
            (
                serde_json::json!({"extra_body": {"other": 1}}),
                "another extra_body member",
            ),
            (
                serde_json::json!({"extra_body": {"google": thinking, "other": 1}}),
                "google beside another member",
            ),
            (
                serde_json::json!({"extra_body": "x"}),
                "a non-object extra_body",
            ),
            (serde_json::json!({"google": "x"}), "a non-object google"),
            (
                serde_json::json!({"cached_content": "projects/p/cachedContents/1"}),
                "top-level cached_content",
            ),
            (
                serde_json::json!({"candidate_count": 1}),
                "top-level candidate_count",
            ),
            (
                serde_json::json!({"candidate_count": 2}),
                "top-level candidate_count 2",
            ),
            (
                serde_json::json!({"cachedContent": "projects/p/cachedContents/1"}),
                "top-level cachedContent",
            ),
            (
                serde_json::json!({"cachedcontent": "projects/p/cachedContents/1"}),
                "top-level cachedcontent",
            ),
            (
                serde_json::json!({"candidateCount": 2}),
                "top-level candidateCount",
            ),
            (
                serde_json::json!({"candidate__count": 2}),
                "top-level candidate__count",
            ),
        ] {
            assert!(
                matches!(parse(extra), Err(ProviderError::BadRequest(_))),
                "{what}"
            );
        }
        // A custom upstream is unchanged: these pass through to it untouched.
        let custom = openai_test_provider();
        let body = r#"{"model":"m","messages":[],"google":{"cached_content":"c"},"extra_body":{"other":1},"cached_content":"c","candidate_count":2}"#;
        assert!(custom.parse_request("chat/completions", body).is_ok());
        let out = custom.build_payload(body, false).unwrap();
        assert_eq!(out["google"]["cached_content"], "c");
        assert_eq!(out["cached_content"], "c");
        assert_eq!(out["extra_body"]["other"], 1);
    }

    #[test]
    fn an_explicit_auto_service_tier_is_refused() {
        let p = openai_test_provider();
        let body = r#"{"model":"m","messages":[],"service_tier":"auto"}"#;
        assert!(matches!(
            p.parse_request("chat/completions", body),
            Err(ProviderError::BadRequest(_))
        ));
        assert!(p.parse_streaming("chat/completions", body).is_err());
        assert!(
            p.parse_request(
                "chat/completions",
                r#"{"model":"m","messages":[],"service_tier":"default"}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn the_service_tier_is_pinned_where_the_operator_says_and_dropped_on_vertex() {
        let custom = |base: &str| {
            OpenAiProvider::custom(
                reqwest::Client::new(),
                reqwest::Client::new(),
                base.to_owned(),
                "k".to_owned(),
            )
        };
        let tier = |p: &OpenAiProvider, body: &str| {
            p.build_payload(body, false)
                .unwrap()
                .get("service_tier")
                .cloned()
        };
        let bare = r#"{"model":"m","max_tokens":8}"#;
        // Default: on for OpenAI's own API, off for anything else.
        assert_eq!(
            tier(&custom("https://api.openai.com/v1"), bare),
            Some(Value::from("default"))
        );
        assert_eq!(
            tier(&custom("https://API.OpenAI.com./v1"), bare),
            Some(Value::from("default"))
        );
        assert_eq!(tier(&custom("https://example.test/v1"), bare), None);
        // Explicit settings win, both ways.
        let off = custom("https://api.openai.com/v1").with_pin_service_tier(Some(false));
        assert_eq!(tier(&off, bare), None);
        let on = custom("https://example.test/v1").with_pin_service_tier(Some(true));
        assert_eq!(tier(&on, bare), Some(Value::from("default")));
        assert_eq!(
            tier(&on, r#"{"model":"m","service_tier":"default"}"#),
            Some(Value::from("default"))
        );
        // Vertex never gets the field, even when the client sent it.
        let shim = openai_vertex_test_provider().with_pin_service_tier(Some(true));
        assert_eq!(
            tier(
                &shim,
                r#"{"model":"gemini-3.8-flash","service_tier":"default"}"#
            ),
            None
        );
        let stream = shim
            .build_payload(
                r#"{"model":"gemini-3.8-flash","service_tier":"default"}"#,
                true,
            )
            .unwrap();
        assert!(stream.get("service_tier").is_none());
    }

    /// Model ids are matched without case for the Gemini predicate and the
    /// rule lookup, so a client cannot skip a rule by changing case.
    #[test]
    fn gemini_models_are_matched_without_case() {
        let path = |m: &str| format!("publishers/google/models/{m}:generateContent");
        let body = with_generation_config(&serde_json::json!({"maxOutputTokens": 100}));
        let native = vertex_test_provider();
        for model in ["GEMINI-2.5-flash", "Gemini-2.5-Pro"] {
            assert!(
                matches!(
                    native.parse_request(&path(model), &body),
                    Err(ProviderError::BadRequest(_))
                ),
                "{model} has no rule"
            );
        }
        assert_eq!(
            native
                .parse_request(&path("Gemini-3.8-FLASH"), &body)
                .unwrap()
                .max_output_tokens,
            100
        );
        let rules =
            ThinkingRules::from_config(&[rule("vertex", "Gemini-2.5-Pro", "additive", 1_000)])
                .unwrap();
        let native = vertex_test_provider().with_thinking_rules(rules);
        for model in ["gemini-2.5-pro", "GEMINI-2.5-PRO"] {
            assert_eq!(
                native
                    .parse_request(&path(model), &body)
                    .unwrap()
                    .max_output_tokens,
                1_100,
                "{model}"
            );
        }
        let shim = openai_vertex_test_provider();
        for model in [
            "GEMINI-2.5-flash",
            "Google/Gemini-2.5-Pro",
            "GOOGLE/gemini-2.5-pro",
        ] {
            let body =
                serde_json::json!({"model": model, "max_tokens": 64, "messages": []}).to_string();
            assert!(
                matches!(
                    shim.parse_request("chat/completions", &body),
                    Err(ProviderError::BadRequest(_))
                ),
                "{model} has no rule"
            );
        }
    }

    /// An `additive` model reserves `max_thinking_tokens` for every thinking
    /// effort a client can ask for without a numeric budget: OpenAI's
    /// `reasoning_effort`, `thinkingLevel` / `thinking_level`, and the
    /// dynamic budget -1. The operator's bound must cover the highest.
    #[test]
    fn additive_models_reserve_the_operator_bound_for_any_thinking_effort() {
        let rules = ThinkingRules::from_config(&[
            rule("vertex", "gemini-2.5-pro", "additive", 1_000),
            rule("vertex-openai", "google/gemini-2.5-pro", "additive", 1_000),
        ])
        .unwrap();
        let native = vertex_test_provider().with_thinking_rules(rules.clone());
        let path = "publishers/google/models/gemini-2.5-pro:generateContent";
        for thinking in [
            serde_json::json!({"thinkingLevel": "HIGH"}),
            serde_json::json!({"thinkingLevel": "LOW"}),
            serde_json::json!({"thinkingBudget": -1}),
            serde_json::json!({"thinking_level": "high", "include_thoughts": true}),
        ] {
            let body = serde_json::json!({
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                "generation_config": {"max_output_tokens": 100, "thinking_config": thinking}
            })
            .to_string();
            assert_eq!(
                native.parse_request(path, &body).unwrap().max_output_tokens,
                1_100,
                "{thinking}"
            );
        }
        let shim = openai_vertex_test_provider().with_thinking_rules(rules);
        for extra in [
            serde_json::json!({"reasoning_effort": "high"}),
            serde_json::json!({"reasoning_effort": "low"}),
            serde_json::json!({"google": {"thinking_config": {"thinking_level": "high"}}}),
            serde_json::json!({"google": {"thinking_config": {"thinking_budget": -1}}}),
        ] {
            for model in ["gemini-2.5-pro", "google/gemini-2.5-pro"] {
                let mut body =
                    serde_json::json!({"model": model, "max_tokens": 100, "messages": []});
                for (k, v) in extra.as_object().unwrap() {
                    body[k] = v.clone();
                }
                let body = body.to_string();
                assert_eq!(
                    shim.parse_request("chat/completions", &body)
                        .unwrap()
                        .max_output_tokens,
                    1_100,
                    "{body}"
                );
                assert_eq!(
                    shim.parse_streaming("chat/completions", &body)
                        .unwrap()
                        .max_output_tokens,
                    1_100,
                    "{body}"
                );
            }
        }
    }

    #[test]
    fn a_service_tier_other_than_the_exact_default_string_is_refused() {
        let custom = openai_test_provider();
        let shim = openai_vertex_test_provider();
        for tier in [
            "null",
            "5",
            "0",
            "true",
            r#"["priority"]"#,
            r#"{"tier":"default"}"#,
            r#""auto""#,
            r#""priority""#,
            r#""flex""#,
            r#""Default""#,
            r#"" default""#,
        ] {
            for p in [&custom, &shim] {
                let body = format!(
                    r#"{{"model":"gemini-3.8-flash","max_tokens":8,"messages":[],"service_tier":{tier}}}"#
                );
                assert!(
                    matches!(
                        p.parse_request("chat/completions", &body),
                        Err(ProviderError::BadRequest(_))
                    ),
                    "{tier}"
                );
                assert!(
                    p.parse_streaming("chat/completions", &body).is_err(),
                    "{tier}"
                );
            }
        }
    }

    /// On the Vertex shim a content array carries text parts only, so no
    /// spelling of a media or file part reaches Vertex.
    #[test]
    fn the_vertex_shim_accepts_only_text_content_parts() {
        let shim = openai_vertex_test_provider();
        let with_content = |content: Value| {
            serde_json::json!({
                "model": "gemini-3.8-flash", "max_tokens": 8,
                "messages": [{"role": "system", "content": "be brief"},
                             {"role": "user", "content": content}]
            })
            .to_string()
        };
        for ok in [
            serde_json::json!("plain string content"),
            serde_json::json!([{"type": "text", "text": "hi"}, {"type": "text", "text": "there"}]),
            serde_json::json!([]),
        ] {
            assert!(
                shim.parse_request("chat/completions", &with_content(ok.clone()))
                    .is_ok(),
                "{ok}"
            );
        }
        for refused in [
            serde_json::json!([{"type": "file", "file": {"file_url": "gs://other/x"}}]),
            serde_json::json!([{"type": "input_file", "file_url": "gs://other/x"}]),
            serde_json::json!([{"type": "file_url", "file_url": "gs://other/x"}]),
            serde_json::json!([{"type": "imageUrl", "imageUrl": {"url": "gs://other/x"}}]),
            serde_json::json!([{"type": "text", "text": "hi", "file_url": "gs://other/x"}]),
            serde_json::json!([{"type": "text", "text": "hi", "cache_control": {}}]),
            serde_json::json!([{"type": "TEXT", "text": "hi"}]),
            serde_json::json!([{"type": "text"}]),
            serde_json::json!([{"type": "text", "text": 5}]),
            serde_json::json!([{"text": "hi"}]),
            serde_json::json!(["hi"]),
        ] {
            let body = with_content(refused.clone());
            assert!(
                matches!(
                    shim.parse_request("chat/completions", &body),
                    Err(ProviderError::BadRequest(_))
                ),
                "{refused}"
            );
            assert!(
                shim.parse_streaming("chat/completions", &body).is_err(),
                "{refused}"
            );
        }
        // An assistant turn's array is checked too.
        let body = r#"{"model":"gemini-3.8-flash","max_tokens":8,"messages":[{"role":"assistant","content":[{"type":"input_file","file_url":"gs://other/x"}]}]}"#;
        assert!(shim.parse_request("chat/completions", body).is_err());
        // A custom upstream is unchanged.
        let body = with_content(serde_json::json!([{"type": "refusal", "refusal": "no"}]));
        assert!(
            openai_test_provider()
                .parse_request("chat/completions", &body)
                .is_ok()
        );
    }

    /// serde_json reads an integer past 64 bits as the nearest float, so a
    /// re-serialised body would carry another number. Refused on every
    /// adapter that re-serialises.
    #[test]
    fn integers_outside_64_bits_are_refused_not_forwarded_as_floats() {
        let big = "123456789012345678901234567890";
        let native = vertex_test_provider();
        let body = format!(
            r#"{{"contents":[{{"role":"user","parts":[{{"functionCall":{{"name":"f","args":{{"big":{big}}}}}}}]}}],"generationConfig":{{"maxOutputTokens":16}}}}"#
        );
        match native.parse_request(GENERATE, &body) {
            Err(ProviderError::BadRequest(m)) => assert!(!m.contains(big), "{m}"),
            other => panic!("expected a 400, got {other:?}"),
        }
        for n in ["-9223372036854775809", "18446744073709551616"] {
            let body = format!(
                r#"{{"contents":[],"generationConfig":{{"maxOutputTokens":16,"seed":{n}}}}}"#
            );
            assert!(native.parse_request(GENERATE, &body).is_err(), "{n}");
        }
        let openai = format!(r#"{{"model":"m","max_tokens":8,"messages":[],"user":{big}}}"#);
        assert!(
            openai_test_provider()
                .parse_request("chat/completions", &openai)
                .is_err()
        );
        let anthropic = format!(r#"{{"model":"m","max_tokens":8,"messages":[],"x":{big}}}"#);
        assert!(
            anthropic_test_provider()
                .parse_request("messages", &anthropic)
                .is_err()
        );
        // In range, fractions, exponents, and digits inside strings all pass.
        for ok in [
            "-9223372036854775808",
            "18446744073709551615",
            "-0",
            "1.5",
            "1e30",
            "123456789012345678901234567890.0",
            r#""123456789012345678901234567890""#,
            r#""a\"123456789012345678901234567890""#,
        ] {
            assert!(reject_lossy_integers(&format!("[{ok}]")).is_ok(), "{ok}");
        }
        assert!(reject_lossy_integers("[1, 123456789012345678901234567890]").is_err());
    }

    #[test]
    fn path_refusals_do_not_echo_the_path() {
        for path in [
            "../secret-name",
            "publishers/google/secret-name:generateContent",
        ] {
            match vertex_test_provider().parse_request(path, "{}") {
                Err(ProviderError::BadRequest(m)) => assert!(!m.contains("secret-name"), "{m}"),
                other => panic!("expected a 400, got {other:?}"),
            }
        }
        match safe_rest_path("/secret-name") {
            Err(ProviderError::BadRequest(m)) => assert!(!m.contains("secret-name"), "{m}"),
            other => panic!("expected a 400, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Anthropic betas
    // -----------------------------------------------------------------------

    #[test]
    fn betas_are_joined_trimmed_and_deduplicated_across_header_instances() {
        assert_eq!(joined_anthropic_betas([]), None);
        assert_eq!(joined_anthropic_betas([" , "]), None);
        assert_eq!(
            joined_anthropic_betas([
                "prompt-caching-2024-07-31, pdfs-2024-09-25",
                "pdfs-2024-09-25",
                " interleaved-thinking-2025-05-14 ,prompt-caching-2024-07-31",
            ])
            .as_deref(),
            Some("prompt-caching-2024-07-31,pdfs-2024-09-25,interleaved-thinking-2025-05-14")
        );
    }

    /// Betas go on the Messages call, buffered and streamed, and never on the
    /// token count.
    #[tokio::test]
    async fn betas_are_forwarded_on_messages_and_not_on_the_count() {
        let (base, seen) = recording_stub(
            r#"{"type":"message","input_tokens":3,"usage":{"input_tokens":3,"output_tokens":1}}"#,
        )
        .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let p = AnthropicProvider::new(
            client.clone(),
            client,
            "k".to_owned(),
            base,
            "2023-06-01".to_owned(),
        );
        let body = r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#;
        let mut parsed = p.parse_request("messages", body).unwrap();
        parsed.anthropic_beta = Some("prompt-caching-2024-07-31,pdfs-2024-09-25".to_owned());
        p.count_input_tokens("messages", body, &parsed)
            .await
            .unwrap();
        p.forward("messages", body, &parsed).await.unwrap();
        p.forward_stream(body, parsed.anthropic_beta.as_deref())
            .await
            .unwrap();
        p.forward_stream(body, None).await.unwrap();
        let seen = seen.lock().unwrap();
        let beta = |i: usize| {
            seen[i]
                .1
                .get_all("anthropic-beta")
                .iter()
                .map(|v| v.to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0].0, "/v1/messages/count_tokens");
        assert!(beta(0).is_empty(), "the count carries no beta");
        for i in [1, 2] {
            assert_eq!(seen[i].0, "/v1/messages");
            assert_eq!(beta(i), vec!["prompt-caching-2024-07-31,pdfs-2024-09-25"]);
        }
        assert!(beta(3).is_empty());
    }

    // -----------------------------------------------------------------------
    // Review round 2 regressions (ported from the reviewer's probes)
    // -----------------------------------------------------------------------

    #[test]
    fn keys_differing_only_in_case_or_underscores_are_refused() {
        let cfg = r#""generationConfig":{"maxOutputTokens":16}"#;
        for (body, what) in [
            (
                format!(
                    r#"{{"contents":[{{"role":"user","parts":[{{"fileDATA":{{"fileUri":"gs://other-media/big"}},"fileData":{{"fileUri":"gs://tollgate-media/a"}}}}]}}],{cfg}}}"#
                ),
                "fileDATA beside fileData",
            ),
            (
                format!(
                    r#"{{"contents":[{{"role":"user","parts":[{{"text":"hi"}}],"pARTS":[{{"text":"long"}}]}}],{cfg}}}"#
                ),
                "pARTS beside parts",
            ),
            (
                format!(
                    r#"{{"contents":[{{"role":"user","parts":[{{"text":"x","videoMetadata":{{"start_offset":"1s","startOffset":"0s"}}}}]}}],{cfg}}}"#
                ),
                "videoMetadata.startOffset in two spellings",
            ),
            (
                format!(
                    r#"{{"contents":[{{"role":"user","parts":[{{"executableCode":{{"code":"a","cODE":"b"}}}}]}}],{cfg}}}"#
                ),
                "executableCode.code in two cases",
            ),
            (
                format!(
                    r#"{{"contents":[{{"role":"user","parts":[{{"codeExecutionResult":{{"output":"a","oUTPUT":"b"}}}}]}}],{cfg}}}"#
                ),
                "codeExecutionResult.output in two cases",
            ),
            (
                format!(
                    r#"{{"contents":[{{"role":"user","parts":[{{"functionResponse":{{"name":"f","response":{{}},"parts":[{{"inlineData":{{"mimeType":"a","mIMEType":"b","data":""}}}}]}}}}]}}],{cfg}}}"#
                ),
                "functionResponse.parts[].inlineData.mimeType in two cases",
            ),
            (
                r#"{"contents":[],"generationConfig":{"maxOutputTokens":16},"gENERATIONCONFIG":{"maxOutputTokens":16}}"#.to_owned(),
                "a top-level key in two cases",
            ),
        ] {
            match vertex_parse(&body) {
                Err(ProviderError::BadRequest(m)) => {
                    assert!(m.contains("differ only in case"), "{what}: {m}");
                }
                other => panic!("{what} must be a 400, got {other:?}"),
            }
        }
        // Client-owned maps are not Google's: `args` may carry both.
        let args = vertex_body_with_part(
            &serde_json::json!({"functionCall": {"name": "f", "args": {"userId": 1, "user_id": 2}}}),
        );
        assert!(vertex_parse(&args).is_ok());
        // One spelling of each new sub-object is accepted.
        let video = vertex_body_with_part(
            &serde_json::json!({"text": "x", "videoMetadata": {"start_offset": "1s"}}),
        );
        assert!(vertex_parse(&video).is_ok());
    }

    #[test]
    fn a_file_data_that_is_not_an_object_is_refused() {
        for (file_data, what) in [
            (serde_json::json!("gs://evil/a"), "a string"),
            (serde_json::json!([{"fileUri": "gs://evil/a"}]), "an array"),
            (Value::Null, "null"),
            (serde_json::json!(1), "a number"),
        ] {
            for key in ["fileData", "file_data"] {
                let body = vertex_body_with_part(&serde_json::json!({ key: file_data.clone() }));
                assert!(
                    matches!(vertex_parse(&body), Err(ProviderError::Forbidden(_))),
                    "{key} as {what} must be refused"
                );
            }
        }
    }

    #[test]
    fn native_vertex_refuses_a_zero_output_cap() {
        let body = r#"{"contents":[{"role":"user","parts":[{"text":"x"}]}],"generationConfig":{"maxOutputTokens":0}}"#;
        assert_bad_request(body, "maxOutputTokens 0");
        let snake = r#"{"contents":[{"role":"user","parts":[{"text":"x"}]}],"generation_config":{"max_output_tokens":0}}"#;
        assert_bad_request(snake, "max_output_tokens 0");
        let one = r#"{"contents":[{"role":"user","parts":[{"text":"x"}]}],"generationConfig":{"maxOutputTokens":1}}"#;
        assert!(vertex_parse(one).is_ok());
    }

    #[test]
    fn vertex_openai_refuses_camel_case_keys() {
        let p = openai_vertex_test_provider();
        let refused = |body: &str, what: &str| match p.parse_request("chat/completions", body) {
            Err(ProviderError::BadRequest(m)) => {
                assert!(m.contains("must be snake_case"), "{what}: {m}");
            }
            other => panic!("{what} must be a 400, got {other:?}"),
        };
        for (extra, what) in [
            (r#""maxTokens":100000"#, "maxTokens"),
            (
                r#""extraBody":{"google":{"thinkingConfig":{"thinkingBudget":24000}}}"#,
                "extraBody",
            ),
            (r#""candidateCount":4"#, "candidateCount"),
            (r#""serviceTier":"priority""#, "serviceTier"),
            (r#""Model":"gemini-2.5-pro""#, "an uppercase first letter"),
            (
                r#""google":{"thinkingConfig":{"thinking_budget":24000}}"#,
                "google.thinkingConfig",
            ),
            (
                r#""extra_body":{"google":{"thinkingConfig":{"thinking_budget":24000}}}"#,
                "extra_body.google.thinkingConfig",
            ),
            (
                r#""extra_body":{"Google":{"thinking_config":{}}}"#,
                "extra_body.Google",
            ),
            (
                r#""google":{"thinking_config":{"thinkingBudget":24000}}"#,
                "thinking_config.thinkingBudget",
            ),
            (
                r#""extra_body":{"google":{"thinking_config":{"includeThoughts":true}}}"#,
                "extra_body.google.thinking_config.includeThoughts",
            ),
        ] {
            let body = format!(
                r#"{{"model":"gemini-3.8-flash","max_tokens":16,{extra},"messages":[{{"role":"user","content":"x"}}]}}"#
            );
            refused(&body, what);
        }
        // Client-owned content is not a key: a schema property in camelCase
        // and camelCase text stay accepted.
        let schema = r#"{"model":"gemini-3.8-flash","max_tokens":16,"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"userId":{"type":"string"}}}}}],"messages":[{"role":"user","content":"maxTokens"}]}"#;
        assert!(p.parse_request("chat/completions", schema).is_ok());
        // A custom upstream is unchanged.
        let custom = r#"{"model":"gpt-x","max_tokens":16,"maxTokens":100000,"messages":[]}"#;
        assert!(
            openai_test_provider()
                .parse_request("chat/completions", custom)
                .is_ok()
        );
    }

    #[test]
    fn vertex_openai_refuses_loose_respellings_of_checked_fields() {
        let p = openai_vertex_test_provider();
        let refused = |body: &str, what: &str| match p.parse_request("chat/completions", body) {
            Err(ProviderError::BadRequest(m)) => {
                assert!(m.contains("must be spelled exactly"), "{what}: {m}");
            }
            other => panic!("{what} must be a 400, got {other:?}"),
        };
        for (extra, what) in [
            (r#""maxtokens":100000"#, "maxtokens"),
            (r#""max__tokens":100000"#, "max__tokens"),
            (r#""max_tokens_":100000"#, "max_tokens_"),
            (r#""maxcompletiontokens":100000"#, "maxcompletiontokens"),
            (r#""service__tier":"priority""#, "service__tier"),
            (r#""extra__body":{"google":{}}"#, "extra__body"),
            (r#""reasoningeffort":"high""#, "reasoningeffort"),
            (r#""google":{"cachedcontent":"c"}"#, "google.cachedcontent"),
            (
                r#""extra_body":{"google":{"cachedcontent":"c"}}"#,
                "extra_body.google.cachedcontent",
            ),
            (
                r#""google":{"thinking_config":{"thinkingbudget":24000}}"#,
                "thinking_config.thinkingbudget",
            ),
            (r#""_n":4"#, "_n"),
        ] {
            let body = format!(
                r#"{{"model":"gemini-3.8-flash","max_tokens":16,{extra},"messages":[{{"role":"user","content":"x"}}]}}"#
            );
            refused(&body, what);
        }
        // Two keys with one loose form, neither a checked field.
        refused(
            r#"{"model":"gemini-3.8-flash","max_tokens":16,"top_k":1,"topk":2,"messages":[]}"#,
            "top_k beside topk",
        );
        // Ordinary snake_case fields still pass.
        let ok = r#"{"model":"gemini-3.8-flash","max_tokens":16,"temperature":0.2,"metadata":{"a":"b"},"user":"u","store":false,"top_p":0.9,"messages":[{"role":"user","content":"x"}]}"#;
        assert!(p.parse_request("chat/completions", ok).is_ok());
        // A custom upstream is unchanged.
        let custom = r#"{"model":"gpt-x","max_tokens":16,"maxtokens":100000,"messages":[]}"#;
        assert!(
            openai_test_provider()
                .parse_request("chat/completions", custom)
                .is_ok()
        );
    }

    #[test]
    fn openai_zero_output_cap_is_replaced_by_the_default_not_forwarded() {
        // 0 is not passed through: the reservation and the outbound cap are
        // both the default.
        for field in ["max_tokens", "max_completion_tokens"] {
            let body = format!(r#"{{"model":"gemini-3.8-flash","{field}":0,"messages":[]}}"#);
            let p = openai_vertex_test_provider();
            let parsed = p.parse_request("chat/completions", &body).unwrap();
            let payload = p.build_payload(&body, false).unwrap();
            assert_eq!(payload["max_tokens"], OPENAI_DEFAULT_MAX_OUTPUT, "{field}");
            assert!(payload.get("max_completion_tokens").is_none(), "{field}");
            assert!(
                parsed.max_output_tokens >= OPENAI_DEFAULT_MAX_OUTPUT,
                "{field}"
            );
        }
    }
}
