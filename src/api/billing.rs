use actix_web::{web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::auth::api_key::ApiKeyAuth;
use crate::auth::clerk::{ClerkAuth, ClerkConfig};
use crate::config::AppConfig;
use crate::errors::AppError;
use crate::middleware::request_id::RequestId;
use crate::models;
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

/// Maximum accepted Stripe webhook body size. Real Stripe events top
/// out around ~8KB; 64KB is a comfortable ceiling that still bounds
/// memory in the pre-HMAC buffering phase.
const MAX_STRIPE_WEBHOOK_BODY: usize = 64 * 1024;

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

    // Bound memory before we even look at headers. The global actix
    // PayloadConfig cap is 50MB for PDF uploads — too generous for a
    // webhook that will reject anything non-Stripe-shaped anyway.
    if body.len() > MAX_STRIPE_WEBHOOK_BODY {
        tracing::warn!(
            body_len = body.len(),
            cap = MAX_STRIPE_WEBHOOK_BODY,
            "Stripe webhook rejected: body exceeds cap"
        );
        return HttpResponse::PayloadTooLarge().json(serde_json::json!({
            "error": "Webhook body exceeds size cap"
        }));
    }

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

    // Attempt to parse the event type from the JSON body
    let event_type = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("type")?.as_str().map(String::from))
        .unwrap_or_else(|| "unknown".to_string());

    // Log only the leading timestamp fragment of the signature (first
    // ~24 chars, enough to correlate with Stripe dashboard retries)
    // rather than the full ~200 byte hex string — the signature is
    // not a secret but it bloats log volume.
    let sig_prefix = sig_header.get(..sig_header.len().min(24)).unwrap_or("");
    tracing::info!(
        event_type,
        sig_prefix = sig_prefix,
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

// ── Stripe Checkout + Customer Portal ────────────────────────────────────────

const STRIPE_API_BASE: &str = "https://api.stripe.com/v1";
const STRIPE_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Shared HTTP client for Stripe API calls. Built once at startup and
/// stored in `web::Data` so handlers reuse its connection pool + TLS
/// session cache across requests. Previously each handler built its
/// own client per call, leaking FDs and thrashing TCP state.
pub fn build_stripe_client() -> Result<reqwest::Client, AppError> {
    reqwest::Client::builder()
        .timeout(STRIPE_HTTP_TIMEOUT)
        .build()
        .map_err(|e| AppError::Internal(format!("stripe http client: {e}")))
}

/// Parse a Stripe error body down to its stable `code` + `message`
/// fields. Stripe error JSON looks like `{"error": {"code":"...",
/// "message":"...", ...}}`, and the rest of the body can contain PII
/// (billing email in 409 duplicate customer errors) or card metadata
/// we don't want in structured logs. Return a compact display string.
fn summarize_stripe_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            let err = v.get("error")?;
            let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("-");
            let message = err.get("message").and_then(|m| m.as_str()).unwrap_or("-");
            Some(format!("code={code} message={message}"))
        })
        .unwrap_or_else(|| "<unparseable>".to_string())
}

#[derive(Debug, Deserialize)]
pub struct CheckoutSessionRequest {
    pub tier: String,
}

#[derive(Debug, Serialize)]
pub struct CheckoutSessionResponse {
    pub session_id: String,
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct PortalSessionResponse {
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct StripeCustomerResponse {
    id: String,
}

#[derive(Debug, Deserialize)]
struct StripeCheckoutSessionResponse {
    id: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct StripePortalSessionResponse {
    url: String,
}

/// Build the form-encoded body for a Stripe Checkout Session create call.
/// Pure helper so the URL/price encoding is unit-testable without HTTP.
pub fn build_checkout_form_body(
    customer_id: &str,
    price_id: &str,
    success_url: &str,
    cancel_url: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("mode", "subscription".to_string()),
        ("customer", customer_id.to_string()),
        ("line_items[0][price]", price_id.to_string()),
        ("line_items[0][quantity]", "1".to_string()),
        ("success_url", success_url.to_string()),
        ("cancel_url", cancel_url.to_string()),
    ]
}

fn require<'a>(opt: &'a Option<String>, what: &str) -> Result<&'a str, AppError> {
    opt.as_deref().ok_or_else(|| {
        AppError::NotImplemented(format!("Stripe billing not configured: missing {what}"))
    })
}

async fn ensure_stripe_customer(
    pool: &SqlitePool,
    http: &reqwest::Client,
    stripe_key: &str,
    user: &models::user::User,
) -> Result<String, AppError> {
    if let Some(existing) = user.stripe_customer_id.as_ref() {
        return Ok(existing.clone());
    }
    let resp = http
        .post(format!("{STRIPE_API_BASE}/customers"))
        .basic_auth(stripe_key, None::<&str>)
        .form(&[
            ("email", user.email.as_str()),
            ("metadata[user_id]", user.id.as_str()),
        ])
        .send()
        .await
        .map_err(|e| AppError::UpstreamApi(format!("Stripe customers: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let summary = summarize_stripe_error(&body);
        tracing::error!(%status, error = %summary, "Stripe customer create failed");
        return Err(AppError::UpstreamApi("Stripe customer create failed".into()));
    }
    let parsed: StripeCustomerResponse = resp
        .json()
        .await
        .map_err(|e| AppError::UpstreamApi(format!("Stripe customer parse: {e}")))?;
    models::user::set_stripe_customer_id(pool, &user.id, &parsed.id).await?;
    Ok(parsed.id)
}

async fn load_user_for_clerk(
    pool: &SqlitePool,
    clerk_cfg: &ClerkConfig,
    clerk_user_id: &str,
) -> Result<models::user::User, AppError> {
    if let Some(u) = models::user::find_by_clerk_id(pool, clerk_user_id).await? {
        return Ok(u);
    }
    if clerk_cfg.dev_auto_provision() {
        let email = format!("{clerk_user_id}@dev.local");
        return models::user::upsert_from_clerk(pool, clerk_user_id, &email, None, None).await;
    }
    Err(AppError::NotFound)
}

/// `POST /billing/checkout-session` — start a Stripe Checkout flow for the
/// authenticated user. Returns 501 (NotImplemented) when Stripe is not
/// configured for this deployment.
#[tracing::instrument(skip(auth, pool, config, clerk_cfg, http, body), fields(clerk_user_id = %auth.clerk_user_id))]
pub async fn create_checkout_session(
    auth: ClerkAuth,
    pool: web::Data<SqlitePool>,
    config: web::Data<AppConfig>,
    clerk_cfg: web::Data<ClerkConfig>,
    http: web::Data<reqwest::Client>,
    body: web::Json<CheckoutSessionRequest>,
) -> Result<HttpResponse, AppError> {
    let stripe_key = require(&config.stripe_secret_key, "STRIPE_SECRET_KEY")?;
    let success_url = require(&config.stripe_success_url, "STRIPE_SUCCESS_URL")?;
    let cancel_url = require(&config.stripe_cancel_url, "STRIPE_CANCEL_URL")?;

    let price_id = match body.tier.to_ascii_lowercase().as_str() {
        "starter" => require(&config.stripe_price_starter, "STRIPE_PRICE_STARTER")?,
        "pro" => require(&config.stripe_price_pro, "STRIPE_PRICE_PRO")?,
        other => {
            return Err(AppError::Validation(format!(
                "Unsupported tier: {other}. Supported: starter, pro"
            )));
        }
    };

    let user = load_user_for_clerk(pool.get_ref(), clerk_cfg.get_ref(), &auth.clerk_user_id).await?;
    let customer_id =
        ensure_stripe_customer(pool.get_ref(), http.get_ref(), stripe_key, &user).await?;

    let form = build_checkout_form_body(&customer_id, price_id, success_url, cancel_url);
    let resp = http
        .post(format!("{STRIPE_API_BASE}/checkout/sessions"))
        .basic_auth(stripe_key, None::<&str>)
        .form(&form)
        .send()
        .await
        .map_err(|e| AppError::UpstreamApi(format!("Stripe checkout: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let summary = summarize_stripe_error(&body);
        tracing::error!(%status, error = %summary, "Stripe checkout session create failed");
        return Err(AppError::UpstreamApi("Stripe checkout failed".into()));
    }
    let parsed: StripeCheckoutSessionResponse = resp
        .json()
        .await
        .map_err(|e| AppError::UpstreamApi(format!("Stripe checkout parse: {e}")))?;

    Ok(HttpResponse::Ok().json(CheckoutSessionResponse {
        session_id: parsed.id,
        url: parsed.url,
    }))
}

/// `POST /billing/portal-session` — return a Customer Portal URL for the
/// authenticated user. Requires an existing `stripe_customer_id`; users
/// who have never started a checkout flow get a 400 with a clear message.
#[tracing::instrument(skip(auth, pool, config, clerk_cfg, http), fields(clerk_user_id = %auth.clerk_user_id))]
pub async fn create_portal_session(
    auth: ClerkAuth,
    pool: web::Data<SqlitePool>,
    config: web::Data<AppConfig>,
    clerk_cfg: web::Data<ClerkConfig>,
    http: web::Data<reqwest::Client>,
) -> Result<HttpResponse, AppError> {
    let stripe_key = require(&config.stripe_secret_key, "STRIPE_SECRET_KEY")?;
    let return_url = require(&config.stripe_portal_return_url, "STRIPE_PORTAL_RETURN_URL")?;

    let user = load_user_for_clerk(pool.get_ref(), clerk_cfg.get_ref(), &auth.clerk_user_id).await?;
    let customer_id = user.stripe_customer_id.as_deref().ok_or_else(|| {
        AppError::Validation(
            "no Stripe customer — open a checkout session first".into(),
        )
    })?;

    let resp = http
        .post(format!("{STRIPE_API_BASE}/billing_portal/sessions"))
        .basic_auth(stripe_key, None::<&str>)
        .form(&[("customer", customer_id), ("return_url", return_url)])
        .send()
        .await
        .map_err(|e| AppError::UpstreamApi(format!("Stripe portal: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let summary = summarize_stripe_error(&body);
        tracing::error!(%status, error = %summary, "Stripe portal session create failed");
        return Err(AppError::UpstreamApi("Stripe portal failed".into()));
    }
    let parsed: StripePortalSessionResponse = resp
        .json()
        .await
        .map_err(|e| AppError::UpstreamApi(format!("Stripe portal parse: {e}")))?;
    Ok(HttpResponse::Ok().json(PortalSessionResponse { url: parsed.url }))
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

    // ── Checkout / portal ───────────────────────────────────────────────

    #[test]
    fn build_checkout_form_body_encodes_all_required_fields() {
        let form = build_checkout_form_body(
            "cus_123",
            "price_456",
            "https://app/success",
            "https://app/cancel",
        );
        let map: std::collections::HashMap<_, _> = form.into_iter().collect();
        assert_eq!(map["mode"], "subscription");
        assert_eq!(map["customer"], "cus_123");
        assert_eq!(map["line_items[0][price]"], "price_456");
        assert_eq!(map["line_items[0][quantity]"], "1");
        assert_eq!(map["success_url"], "https://app/success");
        assert_eq!(map["cancel_url"], "https://app/cancel");
    }

    fn dev_clerk_cfg() -> ClerkConfig {
        ClerkConfig {
            jwks_url: String::new(),
            issuer: String::new(),
            leeway_secs: 30,
            dev_auth_bypass: true,
        }
    }

    fn unconfigured_app_config() -> AppConfig {
        AppConfig {
            host: "127.0.0.1".into(),
            port: 0,
            database_url: "sqlite::memory:".into(),
            rate_limit_per_minute: 60,
            api_key_pepper: "x".repeat(32),
            anthropic_api_key: None,
            stripe_secret_key: None,
            stripe_webhook_secret: None,
            stripe_success_url: None,
            stripe_cancel_url: None,
            stripe_portal_return_url: None,
            stripe_price_starter: None,
            stripe_price_pro: None,
            tesseract_path: "tesseract".into(),
            pdftoppm_path: "pdftoppm".into(),
            paddleocr_url: None,
            paddleocr_timeout_secs: 120,
            paddleocr_mode: crate::config::PaddleOcrMode::Fallback,
            max_concurrent_parses: 8,
            parse_deadline_secs: 90,
            extract_max_input_chars: 200_000,
            clerk_jwks_url: None,
            clerk_issuer: None,
            clerk_leeway_secs: 30,
            dev_auth_bypass: true,
            clerk_webhook_secret: None,
        }
    }

    async fn fresh_pool() -> SqlitePool {
        let url = format!(
            "sqlite://file:billing_test_{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        crate::db::init_db(&url).await.unwrap()
    }

    #[actix_test]
    async fn checkout_session_returns_not_implemented_when_stripe_unconfigured() {
        let pool = fresh_pool().await;
        let app = actix_web::test::init_service(
            actix_web::App::new()
                .app_data(actix_web::web::Data::new(pool))
                .app_data(actix_web::web::Data::new(unconfigured_app_config()))
                .app_data(actix_web::web::Data::new(dev_clerk_cfg()))
                .app_data(actix_web::web::Data::new(
                    crate::auth::clerk::JwksCache::new(String::new()).unwrap(),
                ))
                .app_data(actix_web::web::Data::new(build_stripe_client().unwrap()))
                .route(
                    "/billing/checkout-session",
                    actix_web::web::post().to(create_checkout_session),
                ),
        )
        .await;
        let req = actix_web::test::TestRequest::post()
            .uri("/billing/checkout-session")
            .insert_header(("X-Dev-User-Id", "user_test_co"))
            .set_json(serde_json::json!({"tier": "starter"}))
            .to_request();
        let resp = actix_web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), 501);
    }

    #[actix_test]
    async fn portal_session_returns_not_implemented_when_stripe_unconfigured() {
        let pool = fresh_pool().await;
        let app = actix_web::test::init_service(
            actix_web::App::new()
                .app_data(actix_web::web::Data::new(pool))
                .app_data(actix_web::web::Data::new(unconfigured_app_config()))
                .app_data(actix_web::web::Data::new(dev_clerk_cfg()))
                .app_data(actix_web::web::Data::new(
                    crate::auth::clerk::JwksCache::new(String::new()).unwrap(),
                ))
                .app_data(actix_web::web::Data::new(build_stripe_client().unwrap()))
                .route(
                    "/billing/portal-session",
                    actix_web::web::post().to(create_portal_session),
                ),
        )
        .await;
        let req = actix_web::test::TestRequest::post()
            .uri("/billing/portal-session")
            .insert_header(("X-Dev-User-Id", "user_test_po"))
            .to_request();
        let resp = actix_web::test::call_service(&app, req).await;
        assert_eq!(resp.status(), 501);
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
