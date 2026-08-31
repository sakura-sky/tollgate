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

fn parse_anthropic_usage(v: &Value) -> Usage {
    let u = v.get("usage");
    let get = |k: &str| {
        u.and_then(|u| u.get(k))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    Usage::new(get("input_tokens"), get("output_tokens"))
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

fn parse_vertex_usage(v: &Value) -> Usage {
    let m = v.get("usageMetadata");
    let get = |k: &str| {
        m.and_then(|m| m.get(k))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    Usage::new(get("promptTokenCount"), get("candidatesTokenCount"))
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

fn parse_openai_usage(v: &Value) -> Usage {
    let u = v.get("usage");
    let get = |k: &str| {
        u.and_then(|u| u.get(k))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    Usage::new(get("prompt_tokens"), get("completion_tokens"))
}

/// OpenAI-compatible upstream adapter. Fronts any Chat Completions endpoint that
/// speaks the OpenAI protocol: Vertex's OpenAI endpoint for Gemini, or a custom
/// OpenAI-compatible base. Auth is a bearer token (a GCP Workload Identity token
/// for Vertex, or a static API key for a custom upstream).
pub struct OpenAiProvider {
    http: reqwest::Client,
    /// Base URL up to but excluding `/chat/completions`.
    base_url: String,
    tokens: TokenSource,
    /// Prefix applied to the model in the forwarded body (e.g. `google/` for
    /// Vertex), unless the model already contains a `/`.
    model_prefix: Option<String>,
}

impl OpenAiProvider {
    /// Front Vertex's OpenAI-compatible endpoint for Gemini. `static_token` may be
    /// empty to use the metadata server (Workload Identity).
    #[must_use]
    pub fn vertex(
        http: reqwest::Client,
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
            base_url,
            model_prefix: Some("google/".to_owned()),
        }
    }

    /// Front any custom OpenAI-compatible endpoint. `api_key` is sent as the
    /// bearer token; `base_url` is everything up to `/chat/completions`.
    #[must_use]
    pub fn custom(http: reqwest::Client, base_url: String, api_key: String) -> Self {
        Self {
            tokens: TokenSource::new(api_key),
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model_prefix: None,
        }
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    fn parse_request(&self, rest_path: &str, body: &str) -> Result<ParsedRequest, ProviderError> {
        if rest_path != "chat/completions" {
            return Err(ProviderError::BadRequest(
                "only chat/completions is supported".to_owned(),
            ));
        }
        let v: Value =
            serde_json::from_str(body).map_err(|e| ProviderError::BadRequest(e.to_string()))?;
        // The buffered path needs a non-streaming response; streaming is metered
        // from the terminal chunk on a separate path.
        if v.get("stream").and_then(Value::as_bool) == Some(true) {
            return Err(ProviderError::BadRequest(
                "streaming is not yet supported on this endpoint (set stream=false)".to_owned(),
            ));
        }
        // External media (OpenAI image_url / input_audio, or file refs) has token
        // cost unbounded by body size, so the fast estimate would under-reserve.
        // Reject until multimodal metering is added.
        if references_external_media(body) {
            return Err(ProviderError::BadRequest(
                "external media (image_url / input_audio / file references) is not yet \
                 supported on this endpoint; text only"
                    .to_owned(),
            ));
        }
        let model = v
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .ok_or_else(|| ProviderError::BadRequest("missing model".to_owned()))?
            .to_owned();
        let max_output_tokens = openai_effective_max(&v);
        Ok(ParsedRequest {
            model,
            estimated_input_tokens: estimate_input_tokens(body),
            max_output_tokens,
        })
    }

    async fn forward(
        &self,
        _rest_path: &str,
        body: &str,
        _parsed: &ParsedRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        // Rewrite only the model for the upstream (e.g. gemini-2.5-flash ->
        // google/gemini-2.5-flash); leave the rest of the OpenAI body verbatim.
        let mut payload: Value =
            serde_json::from_str(body).map_err(|e| ProviderError::BadRequest(e.to_string()))?;
        if let Some(prefix) = &self.model_prefix {
            if let Some(m) = payload.get("model").and_then(Value::as_str) {
                if !m.contains('/') {
                    payload["model"] = Value::String(format!("{prefix}{m}"));
                }
            }
        }
        // Normalise the output cap to a single `max_tokens` equal to what we
        // reserved, dropping any duplicate/null/zero cap, so the upstream can
        // never run unbounded past the reservation and overshoot a hard cap.
        let cap = openai_effective_max(&payload);
        if let Value::Object(map) = &mut payload {
            map.remove("max_completion_tokens");
            map.insert("max_tokens".to_owned(), Value::from(cap));
        }

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
            "https://example.test/v1".to_owned(),
            "k".to_owned(),
        )
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
}
