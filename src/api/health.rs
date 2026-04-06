use actix_web::{web, HttpResponse};
use serde::Serialize;
use std::time::Instant;

/// Health check response.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub uptime_seconds: u64,
}

/// GET /health — returns service status, version, and uptime.
pub async fn health(start_time: web::Data<Instant>) -> HttpResponse {
    HttpResponse::Ok().json(HealthResponse {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        uptime_seconds: start_time.elapsed().as_secs(),
    })
}
