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

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

/// Adapter for the Anthropic Messages API.
pub struct AnthropicProvider {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    version: String,
}

impl AnthropicProvider {
    #[must_use]
    pub fn new(http: reqwest::Client, api_key: String, base_url: String, version: String) -> Self {
        Self {
            http,
            api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            version,
        }
    }
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
    // saturating_add, not `+`: these values come from an upstream response and are
    // not trusted to be small. In release builds a plain `+` wraps on overflow,
    // which would meter an enormous request as a tiny one - an under-charge in the
    // one place we can least afford it.
    let prompt = get("input_tokens")
        .saturating_add(get("cache_read_input_tokens"))
        .saturating_add(get("cache_creation_input_tokens"));
    Usage::new(prompt, get("output_tokens"))
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn parse_request(&self, rest_path: &str, body: &str) -> Result<ParsedRequest, ProviderError> {
        // Allowlist: only the synchronous Messages endpoint. Anything else
        // (batches, files, models, streaming) would settle to ~zero cost while
        // spending real money upstream.
        if safe_rest_path(rest_path)? != "messages" {
            return Err(ProviderError::BadRequest(
                "only /v1/anthropic/messages is supported".to_owned(),
            ));
        }
        let v: Value =
            serde_json::from_str(body).map_err(|e| ProviderError::BadRequest(e.to_string()))?;
        // Reject streaming: usage cannot be metered from an SSE stream here.
        if v.get("stream").and_then(Value::as_bool) == Some(true) {
            return Err(ProviderError::BadRequest(
                "streaming is not supported (set stream=false)".to_owned(),
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
        Ok(ParsedRequest {
            model,
            estimated_input_tokens: estimate_input_tokens(body),
            max_output_tokens,
        })
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
            .body(body.to_owned())
            .send()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        let status = resp.status().as_u16();
        let json: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
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
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .header("content-type", "application/json")
            .body(body.to_owned())
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
/// `cachedContentTokenCount` is deliberately NOT added: unlike Anthropic's
/// disjoint scheme, Gemini documents `promptTokenCount` as the total effective
/// prompt size INCLUDING cached content. Adding it would double-count the cache.
fn parse_vertex_usage(v: &Value) -> Usage {
    let m = v.get("usageMetadata");
    let get = |k: &str| {
        m.and_then(|m| m.get(k))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    // saturating_add: upstream-reported values, not trusted to be small. A
    // wrapping sum would under-charge, which is the one direction we never allow.
    let input = get("promptTokenCount").saturating_add(get("toolUsePromptTokenCount"));
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
    let total = get("totalTokenCount");
    let residual = total.saturating_sub(input.saturating_add(output));
    if residual > 0 {
        tracing::warn!(
            residual,
            total,
            counted_input = input,
            counted_output = output,
            "vertex usageMetadata reports more tokens than the known classes account for; \
             billing the remainder at the output rate (a new token class may have shipped)"
        );
        output = output.saturating_add(residual);
    }
    Usage::new(input, output)
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
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        let status = resp.status().as_u16();
        let json: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
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
fn parse_openai_usage(v: &Value) -> Usage {
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
    Usage::new(prompt, completion.max(candidate))
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
#[must_use]
pub fn usage_from_sse_data(line: &str) -> Option<Usage> {
    let data = line.strip_prefix("data:")?.trim();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    match v.get("usage") {
        Some(u) if !u.is_null() => Some(parse_openai_usage(&v)),
        _ => None,
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
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
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        let status = resp.status().as_u16();
        let json: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        let usage = parse_openai_usage(&json);
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
        )
        .unwrap();
        assert_eq!(u.input_tokens, 9);
        assert_eq!(u.output_tokens, 3);
        // Per-delta null usage is not "seen".
        assert!(usage_from_sse_data(r#"data: {"choices":[{"delta":{}}],"usage":null}"#).is_none());
        assert!(usage_from_sse_data("data: [DONE]").is_none());
        assert!(usage_from_sse_data(": keep-alive").is_none());
        assert!(usage_from_sse_data("event: message").is_none());
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
        let p = AnthropicProvider::new(
            reqwest::Client::new(),
            "k".into(),
            "https://api.anthropic.com".into(),
            "2023-06-01".into(),
        );
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
        assert_eq!(u.input_tokens, 201_005);
        assert_eq!(u.output_tokens, 22);
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
        assert_eq!(u.input_tokens, 60_008);
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
        let u = parse_openai_usage(&v);
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
        let u = parse_openai_usage(&v);
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
        let u = parse_openai_usage(&v);
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
        assert_eq!(parse_openai_usage(&v).output_tokens, 140);
    }

    #[test]
    fn openai_contradictory_total_never_lowers_the_output_leg() {
        // total < prompt + completion is self-contradictory; the reported
        // completion must still be charged in full.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":10}}"#,
        )
        .unwrap();
        assert_eq!(parse_openai_usage(&v).output_tokens, 50);
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
        assert_eq!(parse_openai_usage(&v).output_tokens, 560);

        // Mistyped total, sound completion: falls back and still charges it.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":29,"completion_tokens":491,"total_tokens":"589"}}"#,
        )
        .unwrap();
        assert_eq!(parse_openai_usage(&v).output_tokens, 491);

        // Everything mistyped: meters zero. The buffered path then floors to the
        // reserved input cost, and the streaming path charges the reservation
        // because a zero-cost settle would otherwise bill a whole stream at one
        // micro. Neither silently bills zero.
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":-29,"completion_tokens":491.5,"total_tokens":"589"}}"#,
        )
        .unwrap();
        assert_eq!(parse_openai_usage(&v), Usage::default());
    }

    #[test]
    fn openai_missing_prompt_tokens_bills_the_whole_total_as_output() {
        // With no prompt_tokens to subtract, the residual attributes everything
        // to the output leg. That over-charges on any sane price book, where the
        // output rate is at least the input rate, and never under-charges.
        let v: Value =
            serde_json::from_str(r#"{"usage":{"completion_tokens":10,"total_tokens":589}}"#)
                .unwrap();
        let u = parse_openai_usage(&v);
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 589);
    }

    #[test]
    fn buffered_and_streaming_agree_on_the_same_usage_payload() {
        // The streaming path meters through usage_from_sse_data while the
        // buffered path meters through parse_openai_usage. If they ever diverge,
        // a caller can pick the cheaper one by setting stream:true. Pin them
        // together on the exact terminal-chunk shape the live shim emits.
        let usage = r#"{"prompt_tokens":29,"completion_tokens":491,
            "completion_tokens_details":{"reasoning_tokens":69},"total_tokens":589}"#;

        let buffered: Value = serde_json::from_str(&format!(r#"{{"usage":{usage}}}"#)).unwrap();
        let from_buffered = parse_openai_usage(&buffered);

        let sse = format!(r#"data: {{"choices":[],"usage":{usage}}}"#);
        let from_stream = usage_from_sse_data(&sse).expect("terminal chunk carries usage");

        assert_eq!(from_buffered, from_stream);
        assert_eq!(from_stream.output_tokens, 560);
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
