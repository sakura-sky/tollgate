// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Configuration loader.
//!
//! Configuration is layered: defaults → optional `tollgate.toml` file →
//! environment variables prefixed with `TOLLGATE_`. Nested fields use double
//! underscores, e.g. `TOLLGATE_HTTP__PORT=8080`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

/// The placeholder pepper shipped in `.env.example`. Rejected at startup and at
/// key issuance so a copied example file cannot silently defeat the pepper.
pub const DEV_PLACEHOLDER_PEPPER: &str = "dev-only-change-me-please-16+";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub http: HttpConfig,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub telemetry: TelemetryConfig,
    pub billing: BillingConfig,
    pub security: SecurityConfig,
    pub providers: ProvidersConfig,
    pub reload: ReloadConfig,
    pub retention: RetentionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionConfig {
    /// How long to keep `usage_events` rows. The ledger is monthly-partitioned;
    /// partitions whose whole month is older than this window are dropped (the
    /// table is append-only, so retention is by partition drop, not DELETE). Set
    /// `0s` to keep everything. Granularity is monthly: the current month is
    /// always retained.
    #[serde(with = "humantime_serde")]
    pub window: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadConfig {
    /// How often to reload budgets and model prices from Postgres so `admin
    /// budget set` / `price set` take effect without a restart. Set to `0s` to
    /// disable (config is then read only at startup). Changed limits apply to
    /// the existing spend counters; a brand-new budget starts counting from when
    /// it is picked up (a full period backfill still needs a restart).
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
}

fn default_provider_timeout() -> Duration {
    Duration::from_secs(600)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvidersConfig {
    /// Total time allowed for one BUFFERED upstream call.
    ///
    /// Separate from `http.request_timeout`, which bounds Tollgate's own local
    /// routes, because the two want opposite values. A console query should
    /// finish in seconds; a non-streaming LLM sends no headers at all until
    /// generation completes, so a large response legitimately takes minutes and
    /// looks identical to a hang until it arrives.
    ///
    /// Sharing one knob meant either cutting off real responses or leaving the
    /// local routes effectively unbounded. When this expires the request is
    /// treated as POSSIBLY BILLED and charged its reservation, because the
    /// provider may well have generated a full response we never read.
    ///
    /// Streaming ignores this: it is bounded by the idle and max-duration
    /// guards instead, since a stream is expected to be long-lived.
    #[serde(with = "humantime_serde", default = "default_provider_timeout")]
    pub request_timeout: Duration,
    /// How input tokens are counted for the pre-forward budget reservation:
    /// `fast` (over-estimate from body size, no extra call, lowest latency) or
    /// `exact` (a pre-flight token-count call to the provider). Both settle to
    /// the provider's exact reported usage after the response.
    pub admission: String,
    /// Register the built-in mock provider in production `serve` (off by
    /// default; the mock consumes budget at zero real cost, so it is for local
    /// end-to-end testing only). The `tollgate demo` command always has it.
    pub enable_mock: bool,
    pub vertex: VertexConfig,
    pub anthropic: AnthropicConfig,
    pub openai: OpenAiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiConfig {
    /// Serve the OpenAI-compatible `/v1/chat/completions` endpoint (accepts the
    /// Tollgate key via `Authorization: Bearer`), so OpenAI clients, LiteLLM, and
    /// ADK agents can route through Tollgate with only a base URL + key change.
    pub enabled: bool,
    /// Upstream kind: `vertex` (front Vertex's own OpenAI-compatible endpoint for
    /// Gemini, using the Vertex project/location and Workload Identity) or
    /// `custom` (front any OpenAI-compatible base with a static API key).
    pub upstream: String,
    /// Custom upstream base URL, up to but excluding `/chat/completions` (used
    /// when `upstream = custom`).
    pub base_url: String,
    /// Custom upstream bearer API key (used when `upstream = custom`).
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VertexConfig {
    pub enabled: bool,
    /// GCP project id.
    pub project: String,
    /// Region, e.g. `us-central1`.
    pub location: String,
    /// Optional static OAuth access token. If empty, the adapter fetches a token
    /// from the GCP metadata server (Workload Identity on Cloud Run/GCE).
    pub access_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicConfig {
    pub enabled: bool,
    /// Anthropic API key (customer-owned). Keep secret.
    pub api_key: String,
    /// Base URL, default `https://api.anthropic.com`.
    pub base_url: String,
    /// `anthropic-version` header value.
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingConfig {
    /// ISO 4217 currency code for all amounts (display only; the money path is
    /// currency-agnostic integer micros). Single currency per deployment.
    pub currency: String,
    /// Multiple of a model's input rate used to price cache-READ tokens when the
    /// operator has not set a rate, in integer PER-MILLE (1000 = 1.0x). Integer
    /// so no float touches the money path. Must be at least 1000: a lower value
    /// would guess DOWNWARD on a rate nobody supplied, which under-charges
    /// silently. Real read rates are well below the input rate, so the default
    /// deliberately over-charges until a real rate is configured.
    #[serde(default = "default_cache_read_permille")]
    pub cache_read_fallback_permille: u32,
    /// As above for cache-WRITE tokens. The default covers the most expensive
    /// real write rate observed (2x the input rate for a long-TTL entry).
    #[serde(default = "default_cache_write_permille")]
    pub cache_write_fallback_permille: u32,
    /// Prompt size above which providers commonly re-rate the WHOLE request.
    /// Zero disables long-context re-rating entirely.
    #[serde(default = "default_long_context_threshold")]
    pub long_context_threshold_tokens: u64,
    /// Multiple applied to the prompt legs above that threshold, in per-mille.
    ///
    /// A price is one rate per class and cannot express a rate that changes with
    /// size, so without this a large request is UNDER-charged by the tier
    /// multiple, and under-charging is the one direction this product may never
    /// err in. Same posture as the unpriced-cache-class fallback: an unmodelled
    /// dimension costs more, not less.
    #[serde(default = "default_long_context_permille")]
    pub long_context_multiple_permille: u32,
}

// Delegated to the pricing type so there is ONE source of truth. These were
// separate constants and drifted immediately: the pricing default said 1000
// (off) while this said 2000, so every deployment ran at 2x above the threshold
// while the tests, README and commit message all said the feature was off. The
// tests passed because they build prices through `ModelPrice::new`, which reads
// the pricing default; production goes through config, which read this one.
fn default_long_context_threshold() -> u64 {
    crate::pricing::LongContextTier::default().threshold_tokens
}
fn default_long_context_permille() -> u32 {
    crate::pricing::LongContextTier::default().multiple_permille
}

fn default_cache_read_permille() -> u32 {
    1_000
}
fn default_cache_write_permille() -> u32 {
    2_000
}

impl BillingConfig {
    /// The cache-rate fallback these settings describe.
    #[must_use]
    pub fn cache_rate_fallback(&self) -> crate::pricing::CacheRateFallback {
        crate::pricing::CacheRateFallback {
            read_permille: self.cache_read_fallback_permille,
            write_permille: self.cache_write_fallback_permille,
        }
    }

    /// The long-context tier these settings describe.
    #[must_use]
    pub fn long_context_tier(&self) -> crate::pricing::LongContextTier {
        crate::pricing::LongContextTier {
            threshold_tokens: self.long_context_threshold_tokens,
            multiple_permille: self.long_context_multiple_permille,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Server-side pepper for API-key HMAC. MUST be set in production (a fixed,
    /// secret value, ideally from a secret manager) so keys keep verifying
    /// across restarts. Empty means "not configured".
    pub api_key_pepper: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    pub bind: SocketAddr,
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    #[serde(with = "humantime_serde")]
    pub shutdown_grace: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// Postgres connection URL, e.g. `postgres://user:pass@host:5432/dbname`.
    pub url: String,
    pub max_connections: u32,
    #[serde(with = "humantime_serde")]
    pub acquire_timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisConfig {
    /// Redis connection URL, e.g. `redis://host:6379`.
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// Tracing filter (RUST_LOG-style). Defaults to `info,tollgate=debug`.
    pub log_filter: String,
    /// Optional OTLP endpoint. If set, a tracing → OTLP pipeline is initialised.
    pub otlp_endpoint: Option<String>,
    /// Service name advertised over OTLP.
    pub service_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http: HttpConfig {
                bind: "0.0.0.0:8080".parse().expect("valid bind addr"),
                request_timeout: Duration::from_secs(60),
                shutdown_grace: Duration::from_secs(15),
            },
            database: DatabaseConfig {
                url: "postgres://tollgate:tollgate@127.0.0.1:5432/tollgate".to_string(),
                max_connections: 10,
                acquire_timeout: Duration::from_secs(5),
            },
            redis: RedisConfig {
                url: "redis://127.0.0.1:6379".to_string(),
            },
            telemetry: TelemetryConfig {
                log_filter: "info,tollgate=debug".to_string(),
                otlp_endpoint: None,
                service_name: "tollgate".to_string(),
            },
            billing: BillingConfig {
                currency: "USD".to_string(),
                cache_read_fallback_permille: default_cache_read_permille(),
                cache_write_fallback_permille: default_cache_write_permille(),
                long_context_threshold_tokens: default_long_context_threshold(),
                long_context_multiple_permille: default_long_context_permille(),
            },
            security: SecurityConfig {
                api_key_pepper: String::new(),
            },
            providers: ProvidersConfig {
                request_timeout: default_provider_timeout(),
                admission: "fast".to_string(),
                enable_mock: false,
                vertex: VertexConfig {
                    enabled: false,
                    project: String::new(),
                    location: "us-central1".to_string(),
                    access_token: String::new(),
                },
                anthropic: AnthropicConfig {
                    enabled: false,
                    api_key: String::new(),
                    base_url: "https://api.anthropic.com".to_string(),
                    version: "2023-06-01".to_string(),
                },
                openai: OpenAiConfig {
                    enabled: false,
                    upstream: "vertex".to_string(),
                    base_url: String::new(),
                    api_key: String::new(),
                },
            },
            reload: ReloadConfig {
                interval: Duration::from_secs(15),
            },
            retention: RetentionConfig {
                // 90 days.
                window: Duration::from_secs(90 * 24 * 60 * 60),
            },
        }
    }
}

impl Config {
    /// Load configuration from defaults, an optional file, and the environment.
    pub fn load(file: Option<PathBuf>) -> Result<Self> {
        let mut figment = Figment::from(Serialized::defaults(Config::default()));
        if let Some(path) = file {
            figment = figment.merge(Toml::file(path));
        } else if std::path::Path::new("tollgate.toml").exists() {
            figment = figment.merge(Toml::file("tollgate.toml"));
        }
        figment = figment.merge(Env::prefixed("TOLLGATE_").split("__"));
        figment.extract().context("loading configuration")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config defaults and the pricing defaults must agree.
    ///
    /// They drifted the moment they were separate constants: pricing said the
    /// long-context multiple was 1.0x (off) while config said 2.0x, so every
    /// deployment silently doubled the cost of any prompt above the threshold,
    /// on every model including ones that bill flat, while the test suite,
    /// README and commit message all reported the feature as off. The tests did
    /// not catch it because they build prices through `ModelPrice::new`, which
    /// reads the pricing default; only production reads the config default.
    #[test]
    fn config_billing_defaults_match_the_pricing_defaults() {
        let cfg = Config::default();
        assert_eq!(
            cfg.billing.long_context_tier(),
            crate::pricing::LongContextTier::default(),
            "config default must not diverge from the pricing default"
        );
        assert_eq!(
            cfg.billing.cache_rate_fallback(),
            crate::pricing::CacheRateFallback::default(),
            "config default must not diverge from the pricing default"
        );
    }

    /// Whatever the defaults are, they must be valid, or a fresh deployment
    /// fails to boot on config it never chose.
    #[test]
    fn default_billing_config_passes_its_own_validation() {
        let cfg = Config::default();
        assert!(cfg.billing.cache_rate_fallback().validate().is_ok());
        assert!(cfg.billing.long_context_tier().validate().is_ok());
    }
}
