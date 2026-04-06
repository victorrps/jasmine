use actix_web::{web, HttpRequest, HttpResponse};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::auth::api_key::ApiKeyAuth;
use crate::config::AppConfig;
use crate::errors::AppError;
use crate::middleware::request_id::RequestId;
use crate::services::billing::{self, PricingTier};

// ── Response types ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct UsageResponse {
    pub usage: UsageData,
    pub request_id: String,
}

#[derive(Debug, Serialize)]
pub struct UsageData {
    pub pages_used: u32,
    pub pages_limit: u32,
    pub tier: String,
    pub period: String,
}

#[derive(Debug, Serialize)]
pub struct PlansResponse {
    pub plans: Vec<PlanInfo>,
}

#[derive(Debug, Serialize)]
pub struct PlanInfo {
    pub name: String,
    pub pages_per_month: u32,
    pub price_cents: u32,
}

#[derive(Debug, Serialize)]
pub struct WebhookResponse {
    pub received: bool,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// GET /v1/usage — current month's usage stats for the authenticated user.
#[tracing::instrument(skip(auth, pool, req_id))]
pub async fn get_usage(
    auth: ApiKeyAuth,
    pool: web::Data<SqlitePool>,
    req_id: web::ReqData<RequestId>,
) -> Result<HttpResponse, AppError> {
    let status = billing::check_usage_limit(pool.get_ref(), &auth.api_key_id).await?;

    let period = chrono::Utc::now().format("%Y-%m").to_string();

    Ok(HttpResponse::Ok().json(UsageResponse {
        usage: UsageData {
            pages_used: status.used,
            pages_limit: status.limit,
            tier: status.tier,
            period,
        },
        request_id: req_id.id.clone(),
    }))
}

/// GET /billing/plans — list available plans with prices (public endpoint).
#[tracing::instrument]
pub async fn list_plans() -> HttpResponse {
    let plans: Vec<PlanInfo> = PricingTier::all()
        .iter()
        .map(|t| PlanInfo {
            name: t.display_name().to_string(),
            pages_per_month: t.page_limit(),
            price_cents: t.price_cents(),
        })
        .collect();

    HttpResponse::Ok().json(PlansResponse { plans })
}

/// POST /billing/webhook — Stripe webhook receiver.
///
/// Verifies the `Stripe-Signature` header using HMAC-SHA256 over
/// `"{timestamp}.{body}"` against the configured webhook secret. If no secret
/// is configured the endpoint refuses all requests (fail-closed).
#[tracing::instrument(skip(req, body, config))]
pub async fn stripe_webhook(
    req: HttpRequest,
    body: web::Bytes,
    config: web::Data<AppConfig>,
) -> HttpResponse {
    let Some(secret) = config.stripe_webhook_secret.as_deref() else {
        tracing::error!("Stripe webhook rejected: STRIPE_WEBHOOK_SECRET not configured");
        return HttpResponse::ServiceUnavailable().json(serde_json::json!({
            "error": "Webhook receiver not configured"
        }));
    };

    let stripe_signature = req
        .headers()
        .get("Stripe-Signature")
        .and_then(|v| v.to_str().ok());

    let Some(sig_header) = stripe_signature.filter(|s| !s.is_empty()) else {
        tracing::warn!("Stripe webhook rejected: missing Stripe-Signature header");
        return HttpResponse::BadRequest().json(serde_json::json!({
            "error": "Missing Stripe-Signature header"
        }));
    };

    if let Err(reason) = verify_stripe_signature(sig_header, &body, secret) {
        tracing::warn!(reason, "Stripe webhook rejected: signature verification failed");
        return HttpResponse::Unauthorized().json(serde_json::json!({
            "error": "Invalid signature"
        }));
    }

    let sig_display = sig_header;

    // Attempt to parse the event type from the JSON body
    let event_type = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("type")?.as_str().map(String::from))
        .unwrap_or_else(|| "unknown".to_string());

    tracing::info!(
        event_type,
        stripe_signature = sig_display,
        body_len = body.len(),
        "Stripe webhook received"
    );

    // TODO: Handle specific events (customer.subscription.created, invoice.paid, etc.)

    HttpResponse::Ok().json(WebhookResponse { received: true })
}

// ── Stripe signature verification ────────────────────────────────────────────

/// Maximum age of a Stripe webhook timestamp we will accept (replay protection).
const STRIPE_TIMESTAMP_TOLERANCE_SECS: i64 = 5 * 60;

/// Verify a `Stripe-Signature` header against the raw request body.
///
/// The header has the form `t=<unix_ts>,v1=<hex_hmac>[,v1=<hex_hmac>...]`.
/// We compute `HMAC-SHA256(secret, "{t}.{body}")` and compare in constant
/// time against every `v1` candidate. Also rejects timestamps older than
/// `STRIPE_TIMESTAMP_TOLERANCE_SECS` to prevent replay.
fn verify_stripe_signature(
    header: &str,
    body: &[u8],
    secret: &str,
) -> Result<(), &'static str> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let mut timestamp: Option<i64> = None;
    let mut v1_sigs: Vec<&str> = Vec::new();
    for part in header.split(',') {
        let (k, v) = part.split_once('=').ok_or("malformed header part")?;
        match k.trim() {
            "t" => timestamp = v.trim().parse().ok(),
            "v1" => v1_sigs.push(v.trim()),
            _ => {}
        }
    }

    let ts = timestamp.ok_or("missing timestamp")?;
    if v1_sigs.is_empty() {
        return Err("missing v1 signature");
    }

    let now = chrono::Utc::now().timestamp();
    if (now - ts).abs() > STRIPE_TIMESTAMP_TOLERANCE_SECS {
        return Err("timestamp outside tolerance");
    }

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| "invalid secret")?;
    mac.update(ts.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let expected = mac.finalize().into_bytes();

    for sig_hex in v1_sigs {
        let Some(candidate) = decode_hex(sig_hex) else { continue };
        if candidate.len() == expected.len() && constant_time_eq(&candidate, &expected) {
            return Ok(());
        }
    }
    Err("no matching signature")
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test as actix_test;

    #[actix_test]
    async fn list_plans_returns_all_tiers() {
        let resp = list_plans().await;
        assert_eq!(resp.status(), 200);
    }

    #[test]
    fn plans_response_contains_four_plans() {
        let plans: Vec<PlanInfo> = PricingTier::all()
            .iter()
            .map(|t| PlanInfo {
                name: t.display_name().to_string(),
                pages_per_month: t.page_limit(),
                price_cents: t.price_cents(),
            })
            .collect();

        assert_eq!(plans.len(), 4);
        assert_eq!(plans[0].name, "Free");
        assert_eq!(plans[0].pages_per_month, 50);
        assert_eq!(plans[0].price_cents, 0);
        assert_eq!(plans[1].name, "Starter");
        assert_eq!(plans[1].pages_per_month, 1_000);
        assert_eq!(plans[1].price_cents, 900);
        assert_eq!(plans[2].name, "Pro");
        assert_eq!(plans[2].pages_per_month, 5_000);
        assert_eq!(plans[2].price_cents, 2_900);
        assert_eq!(plans[3].name, "Enterprise");
        assert_eq!(plans[3].pages_per_month, 25_000);
        assert_eq!(plans[3].price_cents, 7_900);
    }

    #[test]
    fn usage_data_serializes_correctly() {
        let data = UsageData {
            pages_used: 42,
            pages_limit: 50,
            tier: "free".to_string(),
            period: "2026-04".to_string(),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json["pages_used"], 42);
        assert_eq!(json["pages_limit"], 50);
        assert_eq!(json["tier"], "free");
        assert_eq!(json["period"], "2026-04");
    }

    #[test]
    fn usage_response_includes_request_id() {
        let resp = UsageResponse {
            usage: UsageData {
                pages_used: 10,
                pages_limit: 1000,
                tier: "starter".to_string(),
                period: "2026-04".to_string(),
            },
            request_id: "req_abc123456789".to_string(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["request_id"], "req_abc123456789");
        assert!(json["usage"].is_object());
    }

    fn sign(secret: &str, ts: i64, body: &[u8]) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(ts.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn verify_signature_accepts_valid_header() {
        let secret = "whsec_test";
        let body = br#"{"id":"evt_1"}"#;
        let ts = chrono::Utc::now().timestamp();
        let sig = sign(secret, ts, body);
        let header = format!("t={ts},v1={sig}");
        assert!(verify_stripe_signature(&header, body, secret).is_ok());
    }

    #[test]
    fn verify_signature_rejects_wrong_secret() {
        let body = br#"{}"#;
        let ts = chrono::Utc::now().timestamp();
        let sig = sign("whsec_real", ts, body);
        let header = format!("t={ts},v1={sig}");
        assert!(verify_stripe_signature(&header, body, "whsec_wrong").is_err());
    }

    #[test]
    fn verify_signature_rejects_tampered_body() {
        let secret = "whsec_test";
        let body = br#"{"amount":100}"#;
        let ts = chrono::Utc::now().timestamp();
        let sig = sign(secret, ts, body);
        let header = format!("t={ts},v1={sig}");
        let tampered = br#"{"amount":999}"#;
        assert!(verify_stripe_signature(&header, tampered, secret).is_err());
    }

    #[test]
    fn verify_signature_rejects_old_timestamp() {
        let secret = "whsec_test";
        let body = br#"{}"#;
        let ts = chrono::Utc::now().timestamp() - 3600;
        let sig = sign(secret, ts, body);
        let header = format!("t={ts},v1={sig}");
        assert!(verify_stripe_signature(&header, body, secret).is_err());
    }

    #[test]
    fn verify_signature_rejects_missing_v1() {
        let header = "t=1234567890";
        assert!(verify_stripe_signature(header, b"x", "k").is_err());
    }

    #[test]
    fn webhook_response_serializes_correctly() {
        let resp = WebhookResponse { received: true };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["received"], true);
    }

    #[test]
    fn plan_info_serializes_with_correct_fields() {
        let plan = PlanInfo {
            name: "Pro".to_string(),
            pages_per_month: 5_000,
            price_cents: 2_900,
        };
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["name"], "Pro");
        assert_eq!(json["pages_per_month"], 5_000);
        assert_eq!(json["price_cents"], 2_900);
    }
}
