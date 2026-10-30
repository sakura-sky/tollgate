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
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

use crate::config::TelemetryConfig;

/// Guard returned to `main`; dropping it shuts down the OTel pipeline cleanly.
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
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

    let guard = if let Some(endpoint) = otlp_endpoint(cfg.otlp_endpoint.as_deref())? {
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

/// The OTLP endpoint to export to, or `None` when export is off.
///
/// An empty value counts as unset (before, it fell back to the OTEL_EXPORTER_*
/// variables and then `http://localhost:4317`). Anything else must be
/// `http://host:port` or `unix:///path`: this build compiles the gRPC exporter
/// without TLS. Since opentelemetry-otlp 0.33 a scheme-less endpoint such as
/// `collector:4317` is rewritten to `https://`, and the resulting build error
/// shows a URL the operator never typed and asks for a TLS cargo feature. It is
/// refused here instead, with a message that says what to set.
fn otlp_endpoint(raw: Option<&str>) -> Result<Option<&str>> {
    let Some(endpoint) = raw.map(str::trim).filter(|e| !e.is_empty()) else {
        return Ok(None);
    };
    // tonic matches `unix:` case-sensitively, and otlp 0.33 rewrites `unix:/p`
    // (no `//`) to https://, so only `unix://` with a path is accepted.
    if endpoint.len() > "unix://".len() && endpoint.starts_with("unix://") {
        return Ok(Some(endpoint));
    }
    // `get`, not slicing: a byte index inside a multi-byte character would panic.
    let is_http = endpoint
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"));
    // The port is required: without one the client dials 80, not OTLP's 4317.
    let has_host_and_port = endpoint
        .parse::<http::Uri>()
        .is_ok_and(|uri| uri.host().is_some_and(|host| !host.is_empty()) && uri.port().is_some());
    if is_http && has_host_and_port {
        return Ok(Some(endpoint));
    }
    // Only the scheme is shown, never the value: an endpoint can carry
    // credentials (`https://user:token@host`) or a token in its query, and this
    // message goes to startup output and the logs.
    // A scheme is a letter then letters, digits or `+`, and short; anything
    // else is described rather than shown.
    let plain = |s: &str| {
        s.len() <= 16
            && s.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+')
    };
    let got = match endpoint.split_once("://") {
        Some((s, _)) if plain(s) => format!("a {s:?} scheme"),
        Some(_) => "an unrecognised scheme".to_owned(),
        None => "no \"://\" scheme".to_owned(),
    };
    anyhow::bail!(
        "telemetry.otlp_endpoint (TOLLGATE_TELEMETRY__OTLP_ENDPOINT) must be \
         http://host:port or unix:///path (got {got}): OTLP export is plaintext \
         gRPC only, because TLS is not compiled into the exporter. Point it at an \
         OpenTelemetry Collector and let the collector use TLS to your backend, or \
         leave the setting unset to disable export"
    )
}

fn build_otlp_provider(endpoint: &str, service_name: &str) -> Result<SdkTracerProvider> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("building OTLP span exporter")?;

    let resource = Resource::builder()
        .with_attribute(KeyValue::new("service.name", service_name.to_string()))
        .build();

    // The batch exporter runs on its own dedicated thread, so it takes no Tokio
    // runtime handle.
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            1.0,
        ))))
        .with_resource(resource)
        .build();

    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_blank_endpoint_disables_export() {
        assert_eq!(otlp_endpoint(None).unwrap(), None);
        assert_eq!(otlp_endpoint(Some("")).unwrap(), None);
        assert_eq!(otlp_endpoint(Some("   ")).unwrap(), None);
    }

    #[test]
    fn http_and_unix_endpoints_are_accepted() {
        for (raw, want) in [
            ("http://127.0.0.1:4317", "http://127.0.0.1:4317"),
            (" HTTP://collector:4317 ", "HTTP://collector:4317"),
            ("unix:///tmp/otel.sock", "unix:///tmp/otel.sock"),
        ] {
            assert_eq!(otlp_endpoint(Some(raw)).unwrap(), Some(want), "{raw:?}");
        }
    }

    /// Everything that is not `http://host:port` or `unix:///path` is refused
    /// with a message that says what to set: scheme-less (opentelemetry-otlp
    /// 0.33 rewrites it to https://, and this build has no TLS), https and other
    /// schemes, a missing host or port, `unix:` without `//`, and a value whose
    /// byte 7 falls inside a multi-byte character.
    #[test]
    fn other_endpoints_are_refused_with_a_reason() {
        for bad in [
            "collector:4317",
            "https://collector:4317",
            "grpc://collector:4317",
            "http://",
            "http://:4317",
            "http://collector",
            "unix:/tmp/otel.sock",
            "unix://",
            // Byte 7 falls inside the multi-byte "é": must be refused, not panic.
            "http:/éxample",
        ] {
            let err = otlp_endpoint(Some(bad)).unwrap_err().to_string();
            assert!(
                err.contains("must be http://host:port or unix:///path"),
                "{bad}: {err}"
            );
            assert!(
                err.contains("TLS is not compiled into the exporter"),
                "{bad}: {err}"
            );
        }
        // A refused endpoint is never echoed: it can carry credentials.
        let err = otlp_endpoint(Some(
            "https://user:SECRETTOKEN@collector:4317/?k=SECRETQUERY",
        ))
        .unwrap_err()
        .to_string();
        assert!(!err.contains("SECRET"), "{err}");
        assert!(err.contains(r#"got a "https" scheme"#), "{err}");
        // A prefix that is not plainly a scheme is described, not shown.
        let err = otlp_endpoint(Some("123SECRET://collector:4317"))
            .unwrap_err()
            .to_string();
        assert!(!err.contains("SECRET"), "{err}");
        assert!(err.contains("an unrecognised scheme"), "{err}");
    }

    /// Every form `otlp_endpoint` accepts must actually build with the exporter
    /// version in Cargo.lock, so an upgrade that changes endpoint handling
    /// again fails here rather than at an operator's startup. Building connects
    /// lazily, so no collector is needed.
    #[tokio::test]
    async fn every_accepted_endpoint_form_builds_an_exporter() {
        for raw in [
            "http://127.0.0.1:4317",
            " HTTP://collector:4317 ",
            "unix:///tmp/tollgate-otel-test.sock",
        ] {
            let endpoint = otlp_endpoint(Some(raw)).unwrap().unwrap();
            let provider = build_otlp_provider(endpoint, "tollgate-test")
                .unwrap_or_else(|e| panic!("{raw:?}: {e:#}"));
            provider.shutdown().expect("shutdown with no spans is Ok");
        }
    }
}
