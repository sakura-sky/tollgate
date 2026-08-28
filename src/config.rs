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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub http: HttpConfig,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub telemetry: TelemetryConfig,
    pub billing: BillingConfig,
    pub security: SecurityConfig,
    pub providers: ProvidersConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvidersConfig {
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
            },
            security: SecurityConfig {
                api_key_pepper: String::new(),
            },
            providers: ProvidersConfig {
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
