use actix_governor::governor::middleware::NoOpMiddleware;
use actix_governor::{
    GovernorConfig, GovernorConfigBuilder, KeyExtractor, SimpleKeyExtractionError,
};
use actix_web::dev::ServiceRequest;

/// Rate limit key extractor: uses API key prefix if present, otherwise client IP.
#[derive(Debug, Clone)]
pub struct ApiKeyOrIpExtractor;

impl KeyExtractor for ApiKeyOrIpExtractor {
    type Key = String;
    type KeyExtractionError = SimpleKeyExtractionError<&'static str>;

    fn extract(&self, req: &ServiceRequest) -> Result<Self::Key, Self::KeyExtractionError> {
        if let Some(api_key) = req.headers().get("X-API-Key").and_then(|v| v.to_str().ok()) {
            return Ok(api_key.chars().take(16).collect());
        }
        // Use TCP peer address only. `X-Real-IP` / `X-Forwarded-For` are NOT
        // trusted because we have no allowlist of trusted reverse proxies; a
        // client could otherwise spoof an arbitrary IP and bypass per-IP limits.
        // If you deploy behind a known proxy, gate header trust on `peer_addr()`
        // matching a trusted CIDR before reading the forwarded IP.
        match req.peer_addr() {
            Some(peer) => Ok(peer.ip().to_string()),
            None => Ok("unknown_client".into()),
        }
    }
}

/// IP-only key extractor for auth endpoints (tighter rate limit).
#[derive(Debug, Clone)]
pub struct IpExtractor;

impl KeyExtractor for IpExtractor {
    type Key = String;
    type KeyExtractionError = SimpleKeyExtractionError<&'static str>;

    fn extract(&self, req: &ServiceRequest) -> Result<Self::Key, Self::KeyExtractionError> {
        // See `ApiKeyOrIpExtractor` — forwarded-IP headers are not trusted.
        match req.peer_addr() {
            Some(peer) => Ok(peer.ip().to_string()),
            None => Ok("unknown_client".into()),
        }
    }
}

/// Build the global rate limiter configuration.
pub fn build_governor(rate_per_minute: u64) -> GovernorConfig<ApiKeyOrIpExtractor, NoOpMiddleware> {
    let period_secs = (60u64 / rate_per_minute.max(1)).max(1);
    GovernorConfigBuilder::default()
        .seconds_per_request(period_secs)
        .burst_size(rate_per_minute.max(1) as u32)
        .key_extractor(ApiKeyOrIpExtractor)
        .finish()
        .expect("Failed to build rate limiter config")
}

/// Build a tighter rate limiter for auth endpoints (10 req/min per IP).
pub fn build_auth_governor() -> GovernorConfig<IpExtractor, NoOpMiddleware> {
    GovernorConfigBuilder::default()
        .seconds_per_request(6) // 1 request per 6 seconds = 10/min sustained
        .burst_size(10) // allow burst of 10
        .key_extractor(IpExtractor)
        .finish()
        .expect("Failed to build auth rate limiter config")
}
