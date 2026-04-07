//! Prometheus metrics surface.
//!
//! Exposes the counters/histograms operators need to answer the basic
//! "what is happening" questions about parse traffic without grepping
//! tracing logs:
//!
//! * `parse_requests_total{endpoint, status}` — request volume + outcome
//! * `parse_duration_seconds{backend}` — latency distribution per backend
//! * `classifier_class_total{class}` — how often Auto-mode picks each class
//! * `paddle_degraded_total` — fallback off Paddle
//! * `parse_gate_in_flight` — current concurrency permit usage
//! * `extract_validation_failures_total` — schema validation rejections
//!
//! Exposed via `GET /metrics` (no auth) — same convention as every other
//! prometheus scrape target. Operators put this behind a firewall / VPN
//! or scrape it locally; if you need auth-gating you can wrap the route.
//!
//! ## Test isolation
//!
//! The registry is per-process and counters accumulate across tests.
//! Tests assert on *deltas* (sample value before, sample value after)
//! rather than absolute counts. The `Metrics` struct can also be built
//! fresh in tests via `Metrics::new()` for full isolation.

use prometheus_client::encoding::text::encode;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;
use std::sync::atomic::AtomicI64;

/// Labels for the parse-request counter.
///
/// `endpoint` is `&'static str` — construct with hardcoded literals
/// only, never user input. `status` is `u16` — construct from
/// `StatusCode::as_u16()`, never user input. The type system now
/// prevents a future regression from threading attacker-controlled
/// strings into a Prometheus label (which would explode cardinality
/// and create a memory-exhaustion DoS surface).
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ParseLabels {
    pub endpoint: &'static str,
    pub status: u16,
}

/// Labels for the per-backend latency histogram. `backend` must be a
/// hardcoded literal — see [`ParseLabels`] rationale.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct BackendLabels {
    pub backend: &'static str,
}

/// Labels for the classifier outcome counter. `class` must be a
/// hardcoded literal — see [`ParseLabels`] rationale.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ClassifierLabels {
    pub class: &'static str,
}

/// All app metrics. Held as `web::Data<Metrics>` and shared across
/// handlers. The internal `prometheus_client` types are cheap to clone
/// (Arc-like) so this struct is just a bag of references.
pub struct Metrics {
    pub registry: Registry,
    pub parse_requests: Family<ParseLabels, Counter>,
    pub parse_duration: Family<BackendLabels, Histogram>,
    pub classifier_class: Family<ClassifierLabels, Counter>,
    pub paddle_degraded: Counter,
    pub parse_gate_in_flight: Gauge<i64, AtomicI64>,
    pub extract_validation_failures: Counter,
}

impl Metrics {
    /// Build a fresh metrics instance with all collectors registered.
    /// Called once at startup; tests may also call this for isolated
    /// per-test metric state.
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let parse_requests = Family::<ParseLabels, Counter>::default();
        registry.register(
            "parse_requests",
            "Count of parse/extract requests by endpoint and outcome",
            parse_requests.clone(),
        );

        let parse_duration = Family::<BackendLabels, Histogram>::new_with_constructor(|| {
            Histogram::new(
                [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 90.0].into_iter(),
            )
        });
        registry.register(
            "parse_duration_seconds",
            "Wall-clock time per parse request, bucketed by backend",
            parse_duration.clone(),
        );

        let classifier_class = Family::<ClassifierLabels, Counter>::default();
        registry.register(
            "classifier_class",
            "Count of classifier outcomes when Auto routing is in use",
            classifier_class.clone(),
        );

        let paddle_degraded = Counter::default();
        registry.register(
            "paddle_degraded",
            "Count of times the OCR chain fell off Paddle to Tesseract",
            paddle_degraded.clone(),
        );

        let parse_gate_in_flight = Gauge::<i64, AtomicI64>::default();
        registry.register(
            "parse_gate_in_flight",
            "Current count of parse/extract requests holding a gate permit",
            parse_gate_in_flight.clone(),
        );

        let extract_validation_failures = Counter::default();
        registry.register(
            "extract_validation_failures",
            "Count of /v1/extract responses where the model output failed schema validation",
            extract_validation_failures.clone(),
        );

        Self {
            registry,
            parse_requests,
            parse_duration,
            classifier_class,
            paddle_degraded,
            parse_gate_in_flight,
            extract_validation_failures,
        }
    }

    /// Encode the registry as Prometheus text exposition format.
    pub fn encode_text(&self) -> Result<String, std::fmt::Error> {
        let mut buf = String::new();
        encode(&mut buf, &self.registry)?;
        Ok(buf)
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_metrics_encodes_to_non_empty_text() {
        let m = Metrics::new();
        let txt = m.encode_text().unwrap();
        assert!(txt.contains("parse_requests"), "got: {txt}");
        assert!(txt.contains("parse_duration_seconds"));
        assert!(txt.contains("paddle_degraded"));
        assert!(txt.contains("parse_gate_in_flight"));
    }

    #[test]
    fn parse_requests_counter_increments() {
        let m = Metrics::new();
        let labels = ParseLabels {
            endpoint: "/v1/parse",
            status: 200,
        };
        m.parse_requests.get_or_create(&labels).inc();
        m.parse_requests.get_or_create(&labels).inc();
        let txt = m.encode_text().unwrap();
        assert!(
            txt.contains("parse_requests_total{endpoint=\"/v1/parse\",status=\"200\"} 2"),
            "got: {txt}"
        );
    }

    #[test]
    fn paddle_degraded_counter_increments() {
        let m = Metrics::new();
        m.paddle_degraded.inc();
        let txt = m.encode_text().unwrap();
        assert!(txt.contains("paddle_degraded_total 1"), "got: {txt}");
    }

    #[test]
    fn classifier_class_counter_emits_label() {
        let m = Metrics::new();
        m.classifier_class
            .get_or_create(&ClassifierLabels {
                class: "text_simple",
            })
            .inc();
        let txt = m.encode_text().unwrap();
        assert!(
            txt.contains("classifier_class_total{class=\"text_simple\"} 1"),
            "got: {txt}"
        );
    }

    #[test]
    fn parse_duration_histogram_records_observation() {
        let m = Metrics::new();
        m.parse_duration
            .get_or_create(&BackendLabels {
                backend: "pdf_oxide",
            })
            .observe(0.42);
        let txt = m.encode_text().unwrap();
        assert!(
            txt.contains("parse_duration_seconds_count{backend=\"pdf_oxide\"} 1"),
            "got: {txt}"
        );
    }
}
