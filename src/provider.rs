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
}

/// A provider response: upstream status, JSON body to return, and parsed usage.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub status: u16,
    pub body: Value,
    pub usage: Usage,
}

/// Errors from a provider adapter.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("upstream provider error: {0}")]
    Upstream(String),
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
