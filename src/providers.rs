// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Real provider adapters: Anthropic and Vertex/Gemini.
//!
//! Both implement the [`Provider`] trait via native pass-through: the client's
//! provider-native body is forwarded verbatim to the upstream with that
//! provider's credentials, and token usage is read from the response. Upstream
//! base URLs are pinned from config and the client-supplied path is checked for
//! traversal, so a request cannot be redirected off the configured provider.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use crate::pricing::Usage;
use crate::provider::{ParsedRequest, Provider, ProviderError, ProviderResponse};

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

/// Reject path traversal / absolute paths in the client-supplied rest path so a
/// request cannot escape the configured upstream base URL.
fn safe_rest_path(rest_path: &str) -> Result<&str, ProviderError> {
    if rest_path.is_empty()
        || rest_path.starts_with('/')
        || rest_path.contains("..")
        || rest_path.contains(['\0', '\\'])
    {
        return Err(ProviderError::BadRequest(format!(
            "invalid upstream path: {rest_path}"
        )));
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
                let key = k.to_ascii_lowercase();
                // External-reference keys across Anthropic, Gemini, and OpenAI
                // shapes (OpenAI vision/audio use image_url / input_audio).
                if matches!(
                    key.as_str(),
                    "fileuri"
                        | "file_uri"
                        | "file_id"
                        | "filedata"
                        | "file_data"
                        | "image_url"
                        | "input_audio"
                ) {
                    return true;
                }
                // A source/part typed as a URL or OpenAI media reference.
                if key == "type"
                    && val.as_str().is_some_and(|s| {
                        s.eq_ignore_ascii_case("url")
                            || s.eq_ignore_ascii_case("image_url")
                            || s.eq_ignore_ascii_case("input_audio")
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
    // Priority tiers bill more per token at identical token counts.
    if let Some(t) = v.get("service_tier").and_then(Value::as_str) {
        if !matches!(t, "auto" | "default") {
            return refuse(
                "an explicit service_tier",
                "non-default tiers change the per-token rate",
            );
        }
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
/// A non-2xx that will not parse (an HTML 502 from a CDN, an empty 3xx body) was
/// not billed, so it releases the reservation.
async fn json_or_classified(resp: reqwest::Response) -> Result<(u16, Value), ProviderError> {
    let status = resp.status().as_u16();
    let success = (200..300).contains(&status);
    match resp.json::<Value>().await {
        Ok(v) => Ok((status, v)),
        Err(e) if success => Err(ProviderError::MeteringFailed(format!(
            "2xx response body did not parse: {e}"
        ))),
        Err(e) => Err(ProviderError::Upstream(format!(
            "status {status} with unparseable body: {e}"
        ))),
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
    /// # Errors
    /// Returns a provider error if the upstream call fails.
    pub async fn forward_stream(&self, body: &str) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);
        self.stream_http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .header("content-type", "application/json")
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
/// Returns the first unrecognised beta so the refusal can name it.
#[must_use]
pub fn unknown_anthropic_beta(header_value: &str) -> Option<String> {
    header_value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .find(|s| !ALLOWED_ANTHROPIC_BETAS.contains(s))
        .map(str::to_owned)
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
        if let Some(ttl) = find_cache_control_ttl(&v) {
            return Err(ProviderError::BadRequest(format!(
                "cache_control ttl '{ttl}' is not supported: a longer TTL bills at a \
                 different multiple of the input rate than the default, and one \
                 configured cache-write rate cannot express both"
            )));
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
/// under-charge by the entire output leg, which is precisely the shape of bug
/// the OpenAI path is immune to only because its usage chunk is terminal.
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
    /// that rewrites it) is relayed to the client in full and then charged the
    /// prompt leg alone, because it contains neither `message_start` nor
    /// `message_stop`. The provider has to tell us it failed before we discount
    /// the request.
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

/// The `ttl` of any `cache_control` block, when it is not the default.
///
/// Returned so the caller can refuse it: TTL selects a billing multiple, and the
/// price book has one cache-write rate.
fn find_cache_control_ttl(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k.eq_ignore_ascii_case("cache_control") {
                    if let Some(ttl) = val.get("ttl").and_then(Value::as_str) {
                        if !ttl.eq_ignore_ascii_case("5m") {
                            return Some(ttl.to_owned());
                        }
                    }
                }
                if let Some(found) = find_cache_control_ttl(val) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(find_cache_control_ttl),
        _ => None,
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
        _parsed: &ParsedRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let path = safe_rest_path(rest_path)?;
        let url = format!("{}/v1/{path}", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .header("content-type", "application/json")
            .body(pin_outbound(body, false))
            .send()
            .await
            .map_err(|e| classify_transport_error(&e))?;
        let (status, json) = json_or_classified(resp).await?;
        let usage = parse_anthropic_usage(&json);
        Ok(ProviderResponse {
            status,
            body: json,
            usage,
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
        let json: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
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

/// Adapter for Vertex AI (Gemini) `generateContent`.
pub struct VertexProvider {
    http: reqwest::Client,
    project: String,
    location: String,
    tokens: TokenSource,
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
            project,
            location,
        }
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
        let model = extract_vertex_model(path).ok_or_else(|| {
            ProviderError::BadRequest(format!("could not determine model from path: {rest_path}"))
        })?;
        let v: Value = serde_json::from_str(if body.is_empty() { "{}" } else { body })
            .map_err(|e| ProviderError::BadRequest(e.to_string()))?;
        // Require an explicit output cap so the reservation bounds real spend;
        // otherwise the model's (much larger) default would govern generation.
        let max_output_tokens = v
            .get("generationConfig")
            .and_then(|g| g.get("maxOutputTokens"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ProviderError::BadRequest("generationConfig.maxOutputTokens is required".to_owned())
            })?;
        Ok(ParsedRequest {
            model,
            estimated_input_tokens: estimate_input_tokens(body),
            max_output_tokens,
            // Vertex cache creation is a separate cachedContents call that never
            // transits the proxy, so a generateContent request cannot write.
            may_cache_write: false,
        })
    }

    async fn forward(
        &self,
        rest_path: &str,
        body: &str,
        _parsed: &ParsedRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let path = safe_rest_path(rest_path)?;
        let token = self.tokens.token(&self.http).await?;
        let url = format!(
            "https://{loc}-aiplatform.googleapis.com/v1/projects/{proj}/locations/{loc}/{path}",
            loc = self.location,
            proj = self.project,
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .map_err(|e| classify_transport_error(&e))?;
        let (status, json) = json_or_classified(resp).await?;
        let usage = parse_vertex_usage(&json);
        Ok(ProviderResponse {
            status,
            body: json,
            usage,
        })
    }

    async fn count_input_tokens(
        &self,
        rest_path: &str,
        body: &str,
        parsed: &ParsedRequest,
    ) -> Result<u64, ProviderError> {
        let model = extract_vertex_model(rest_path).unwrap_or_else(|| parsed.model.clone());
        let token = self.tokens.token(&self.http).await?;
        let url = format!(
            "https://{loc}-aiplatform.googleapis.com/v1/projects/{proj}/locations/{loc}/publishers/google/models/{model}:countTokens",
            loc = self.location,
            proj = self.project,
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        let json: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        json.get("totalTokens")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                ProviderError::Upstream("countTokens returned no totalTokens".to_owned())
            })
    }
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
    // classes overlap rather than partitioning the prompt. Anything asking how
    // large the prompt actually was must not add them up: a 150k prompt with
    // 100k cached bills as 250k, and testing a 200k size threshold against that
    // would re-rate a request the provider itself never tiered.
    if matches!(semantics, CacheSemantics::Unverified) {
        usage = usage.with_overlapping_classes();
    }
    if contradictory || ambiguous || unpriced_class {
        usage.into_suspect()
    } else {
        usage
    }
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
    /// Prefix applied to the model in the forwarded body (e.g. `google/` for
    /// Vertex), unless the model already contains a `/`.
    model_prefix: Option<String>,
}

impl OpenAiProvider {
    /// How this upstream reports cached prompt tokens.
    ///
    /// Vertex's shim (the only upstream with a model prefix) was verified
    /// against live responses to report them INSIDE `prompt_tokens`. A custom
    /// base URL is whatever the operator pointed it at, so it stays unverified
    /// and both classes are billed in full rather than risking an under-charge.
    ///
    /// The streaming relay must be given this same value, or a caller could pick
    /// the cheaper path by setting `stream`.
    #[must_use]
    pub fn cache_semantics(&self) -> CacheSemantics {
        if self.model_prefix.is_some() {
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
        let base_url = format!(
            "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/endpoints/openapi"
        );
        Self {
            tokens: TokenSource::new(static_token),
            http,
            stream_http,
            base_url,
            model_prefix: Some("google/".to_owned()),
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
            base_url: base_url.trim_end_matches('/').to_owned(),
            model_prefix: None,
        }
    }

    /// Build the outbound payload: rewrite the model for the upstream, normalise
    /// the output cap to a single `max_tokens` equal to the reserved worst case,
    /// and set the streaming flag EXPLICITLY. The client's `stream` /
    /// `stream_options` are never trusted (a truthy-but-nonboolean value must not
    /// flip the mode, and a client cannot suppress usage reporting).
    fn build_payload(&self, body: &str, streaming: bool) -> Result<Value, ProviderError> {
        let mut payload: Value =
            serde_json::from_str(body).map_err(|e| ProviderError::BadRequest(e.to_string()))?;
        if let Some(prefix) = &self.model_prefix {
            if let Some(m) = payload.get("model").and_then(Value::as_str) {
                if !m.contains('/') {
                    payload["model"] = Value::String(format!("{prefix}{m}"));
                }
            }
        }
        let cap = openai_effective_max(&payload);
        // Choose which output-cap field to send, always pinned to the reserved cap
        // (so reserved == enforced). On Vertex (model_prefix set) always use
        // `max_tokens`: the Vertex OpenAI shim honors it, and it may silently ignore
        // `max_completion_tokens`, which would let generation run past our cap. On a
        // custom upstream, preserve the client's field so we don't break the o-series
        // reasoning models on OpenAI's own API, which reject `max_tokens` and require
        // `max_completion_tokens`.
        let cap_field = if self.model_prefix.is_some() {
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
            .ok_or_else(|| ProviderError::BadRequest("missing model".to_owned()))?
            .to_owned();
        Ok(ParsedRequest {
            model,
            estimated_input_tokens: estimate_input_tokens(body),
            max_output_tokens: openai_effective_max(&v),
            // The OpenAI Chat Completions usage object carries no cache-write
            // token field at all, so a write can never be observed on this path.
            may_cache_write: false,
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
    match v.get("usage") {
        Some(u) if !u.is_null() => Some(parse_openai_usage(&v, semantics)),
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
        let (status, json) = json_or_classified(resp).await?;
        let usage = parse_openai_usage(&json, self.cache_semantics());
        Ok(ProviderResponse {
            status,
            body: json,
            usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Terminal chunk with usage.
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
                r#"{"model":"m","messages":[],"modalities":["text"],"service_tier":"auto",
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
        // The streaming path meters through usage_from_sse_data while the
        // buffered path meters through parse_openai_usage. If they ever diverge,
        // a caller can pick the cheaper one by setting stream:true. Pin them
        // together on the exact terminal-chunk shape the live shim emits.
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
            let from_stream = usage_from_sse_data(&sse, sem).expect("terminal chunk carries usage");
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
}
