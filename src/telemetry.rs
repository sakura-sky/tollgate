// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! Telemetry initialisation.
//!
//! Always installs a JSON-formatted `tracing` subscriber. If
//! `telemetry.otlp_endpoint` is configured, also installs an OTLP exporter so
//! traces flow to Cloud Trace (via the OTel Collector) or any other OTLP sink.

use anyhow::{Context, Result};
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, TracerProvider};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

use crate::config::TelemetryConfig;

/// Guard returned to `main`; dropping it shuts down the OTel pipeline cleanly.
pub struct TelemetryGuard {
    provider: Option<TracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            // Best-effort flush; ignore errors during shutdown.
            if let Err(err) = provider.shutdown() {
                eprintln!("otel shutdown error: {err}");
            }
        }
    }
}

pub fn init(cfg: &TelemetryConfig) -> Result<TelemetryGuard> {
    let env_filter = EnvFilter::try_new(&cfg.log_filter).unwrap_or_else(|_| EnvFilter::new("info"));

    let json_layer = fmt::layer()
        .json()
        .with_target(true)
        .with_current_span(true)
        .with_span_list(false);

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(json_layer);

    let guard = if let Some(endpoint) = cfg.otlp_endpoint.as_deref() {
        let provider = build_otlp_provider(endpoint, &cfg.service_name)?;
        let tracer = provider.tracer(cfg.service_name.clone());
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        global::set_text_map_propagator(TraceContextPropagator::new());
        registry.with(otel_layer).try_init().ok();
        TelemetryGuard {
            provider: Some(provider),
        }
    } else {
        registry.try_init().ok();
        TelemetryGuard { provider: None }
    };

    Ok(guard)
}

fn build_otlp_provider(endpoint: &str, service_name: &str) -> Result<TracerProvider> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("building OTLP span exporter")?;

    let resource = Resource::new(vec![KeyValue::new(
        "service.name",
        service_name.to_string(),
    )]);

    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            1.0,
        ))))
        .with_resource(resource)
        .build();

    Ok(provider)
}
