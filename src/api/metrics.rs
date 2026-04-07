//! `GET /metrics` — Prometheus text exposition endpoint.
//!
//! Unauthenticated by design (matches every other Prometheus scrape
//! target). Operators are expected to put this behind a firewall, VPN,
//! or scrape it locally. If you need auth-gating, wrap the route in your
//! own middleware before mounting.

use actix_web::{web, HttpResponse};

use crate::services::metrics::Metrics;

/// Serve the current metrics in Prometheus text format.
pub async fn metrics(metrics: web::Data<Metrics>) -> HttpResponse {
    match metrics.encode_text() {
        Ok(text) => HttpResponse::Ok()
            .content_type("text/plain; version=0.0.4; charset=utf-8")
            .body(text),
        Err(e) => {
            tracing::error!(error = %e, "failed to encode prometheus metrics");
            HttpResponse::InternalServerError().body("metrics encoding failed")
        }
    }
}
