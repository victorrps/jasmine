//! Clerk webhook receiver.
//!
//! Receives `user.created`, `user.updated`, and `user.deleted` events
//! from Clerk (delivered via Svix) and mirrors them into the local
//! `users` table. The mirror is what every other handler reads — we
//! never call Clerk's API from a request path.
//!
//! # Signature verification
//!
//! Svix signs every webhook with HMAC-SHA256. The signed payload is:
//!
//! ```text
//! {svix_id}.{svix_timestamp}.{raw_body}
//! ```
//!
//! The signature header contains one or more space-separated tokens
//! of the form `v1,<base64(hmac)>` — we accept the request if **any**
//! token matches. The timestamp must be within ±5 minutes of server
//! time (replay window). All comparisons are constant-time.
//!
//! The secret is pulled from `CLERK_WEBHOOK_SECRET` and has the Svix
//! format `whsec_<base64>` — we strip the prefix and base64-decode to
//! get the raw HMAC key. If no secret is configured the endpoint
//! returns `503` (we refuse to accept unverified webhooks even in
//! dev — use a real secret from the Clerk dashboard).
//!
//! # Delete semantics
//!
//! `user.deleted` is a **hard delete**. `api_keys` already cascades
//! via its FK; `usage_logs` does not, so we wrap the delete in a
//! transaction and remove dependent rows manually. This is cheaper
//! than a SQLite table-rebuild migration and keeps the migration
//! history linear.

use actix_web::{web, HttpRequest, HttpResponse};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use sqlx::SqlitePool;

use crate::config::AppConfig;
use crate::errors::AppError;
use crate::middleware::request_id::RequestId;
use crate::models::user;

type HmacSha256 = Hmac<Sha256>;

/// Replay window for Svix timestamps, in seconds. Matches Svix's
/// documented default (`5m`).
const REPLAY_TOLERANCE_SECS: i64 = 5 * 60;

/// Maximum webhook body size. Clerk user payloads are ~2KB; we cap
/// at 64KB to bound memory for malicious senders.
const MAX_WEBHOOK_BODY: usize = 64 * 1024;

// ── Event envelope ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ClerkEvent {
    #[serde(rename = "type")]
    event_type: String,
    data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ClerkUserData {
    id: String,
    #[serde(default)]
    email_addresses: Vec<ClerkEmail>,
    #[serde(default)]
    primary_email_address_id: Option<String>,
    #[serde(default)]
    first_name: Option<String>,
    #[serde(default)]
    last_name: Option<String>,
    #[serde(default)]
    image_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClerkEmail {
    id: String,
    email_address: String,
}

#[derive(Debug, Deserialize)]
struct ClerkDeletedData {
    id: String,
    #[serde(default)]
    deleted: Option<bool>,
}

impl ClerkUserData {
    /// Pull the primary email, falling back to the first one if no
    /// primary is marked. Returns `None` if the user has no emails at
    /// all — in practice Clerk always sends at least one, but we defend
    /// against malformed payloads.
    fn primary_email(&self) -> Option<&str> {
        if let Some(ref pid) = self.primary_email_address_id {
            if let Some(e) = self.email_addresses.iter().find(|e| &e.id == pid) {
                return Some(&e.email_address);
            }
        }
        self.email_addresses.first().map(|e| e.email_address.as_str())
    }

    fn display_name(&self) -> Option<String> {
        match (&self.first_name, &self.last_name) {
            (Some(f), Some(l)) => Some(format!("{f} {l}")),
            (Some(f), None) => Some(f.clone()),
            (None, Some(l)) => Some(l.clone()),
            (None, None) => None,
        }
    }
}

// ── Handler ────────────────────────────────────────────────────────

pub async fn clerk_webhook(
    req: HttpRequest,
    body: web::Bytes,
    config: web::Data<AppConfig>,
    pool: web::Data<SqlitePool>,
    req_id: web::ReqData<RequestId>,
) -> Result<HttpResponse, AppError> {
    let req_id = req_id.id.clone();

    // 1. Require configured secret. We refuse unverified webhooks.
    let secret = config.clerk_webhook_secret.as_deref().ok_or_else(|| {
        tracing::error!(req_id = %req_id, "clerk_webhook: CLERK_WEBHOOK_SECRET not configured");
        AppError::NotImplemented("clerk webhooks not configured".into())
    })?;

    // 2. Size check.
    if body.len() > MAX_WEBHOOK_BODY {
        return Err(AppError::FileTooLarge);
    }

    // 3. Signature headers.
    let svix_id = header(&req, "svix-id")?;
    let svix_timestamp = header(&req, "svix-timestamp")?;
    let svix_signature = header(&req, "svix-signature")?;

    // 4. Verify signature + timestamp.
    verify_svix_signature(secret, &svix_id, &svix_timestamp, &svix_signature, &body)
        .map_err(|e| {
            tracing::warn!(
                req_id = %req_id,
                error = %e,
                "clerk_webhook: signature verification failed"
            );
            AppError::InvalidToken
        })?;

    // 5. Parse envelope.
    let event: ClerkEvent = serde_json::from_slice(&body)
        .map_err(|e| AppError::Validation(format!("invalid clerk event body: {e}")))?;

    tracing::info!(
        req_id = %req_id,
        svix_id = %svix_id,
        event_type = %event.event_type,
        "clerk_webhook: received"
    );

    // 6. Dispatch.
    match event.event_type.as_str() {
        "user.created" | "user.updated" => {
            let data: ClerkUserData = serde_json::from_value(event.data).map_err(|e| {
                AppError::Validation(format!("invalid clerk user payload: {e}"))
            })?;
            let email = data
                .primary_email()
                .ok_or_else(|| AppError::Validation("clerk user has no email".into()))?;
            user::upsert_from_clerk(
                pool.get_ref(),
                &data.id,
                email,
                data.display_name().as_deref(),
                data.image_url.as_deref(),
            )
            .await?;
        }
        "user.deleted" => {
            let data: ClerkDeletedData = serde_json::from_value(event.data).map_err(|e| {
                AppError::Validation(format!("invalid clerk delete payload: {e}"))
            })?;
            // Clerk sets `deleted: true` on hard-delete events; we
            // honour it defensively (the event type alone is enough
            // per the docs, but a mismatched flag is a red flag).
            if matches!(data.deleted, Some(false)) {
                return Err(AppError::Validation(
                    "user.deleted event with deleted=false".into(),
                ));
            }
            user::hard_delete_by_clerk_id(pool.get_ref(), &data.id).await?;
        }
        other => {
            // Unknown event types are acknowledged but ignored — Clerk
            // retries on non-2xx responses and we do not want a
            // forward-compatible event to block the queue.
            tracing::info!(req_id = %req_id, event_type = %other, "clerk_webhook: ignored");
        }
    }

    Ok(HttpResponse::Ok().json(serde_json::json!({ "ok": true })))
}

fn header(req: &HttpRequest, name: &str) -> Result<String, AppError> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| AppError::Validation(format!("missing {name} header")))
}

// ── Signature verification (pure, testable) ────────────────────────

/// Verify a Svix webhook signature.
///
/// Implements <https://docs.svix.com/receiving/verifying-payloads/how-manual>:
/// - signed payload: `{id}.{timestamp}.{body}`
/// - signature header: one-or-more space-separated `v1,<base64>` tokens
/// - timestamp must be within ±`REPLAY_TOLERANCE_SECS` of server clock
/// - constant-time comparison against every candidate
///
/// The secret starts with `whsec_` followed by base64 of the raw HMAC key.
pub fn verify_svix_signature(
    secret: &str,
    svix_id: &str,
    svix_timestamp: &str,
    svix_signature: &str,
    body: &[u8],
) -> Result<(), String> {
    // 1. Replay window.
    let ts: i64 = svix_timestamp
        .parse()
        .map_err(|_| "svix-timestamp not an integer".to_string())?;
    let now = chrono::Utc::now().timestamp();
    let diff = (now - ts).abs();
    if diff > REPLAY_TOLERANCE_SECS {
        return Err(format!("timestamp outside replay window ({diff}s)"));
    }

    // 2. Decode secret.
    let raw_key = decode_secret(secret)?;

    // 3. Compute expected MAC.
    let signed = format!(
        "{}.{}.{}",
        svix_id,
        svix_timestamp,
        std::str::from_utf8(body).map_err(|_| "body not UTF-8".to_string())?
    );
    let mut mac = HmacSha256::new_from_slice(&raw_key)
        .map_err(|_| "invalid hmac key length".to_string())?;
    mac.update(signed.as_bytes());
    let expected = mac.finalize().into_bytes();

    // 4. Walk every `v1,<b64>` token and constant-time compare.
    for token in svix_signature.split_whitespace() {
        let Some((version, b64)) = token.split_once(',') else {
            continue;
        };
        if version != "v1" {
            continue;
        }
        let Ok(candidate) = B64.decode(b64) else {
            continue;
        };
        if constant_time_eq(&candidate, &expected) {
            return Ok(());
        }
    }
    Err("no matching signature".to_string())
}

fn decode_secret(secret: &str) -> Result<Vec<u8>, String> {
    let stripped = secret
        .strip_prefix("whsec_")
        .ok_or_else(|| "secret missing whsec_ prefix".to_string())?;
    B64.decode(stripped)
        .map_err(|e| format!("secret base64 decode: {e}"))
}

/// Constant-time byte comparison. Returns `false` immediately on
/// length mismatch (length itself is not secret).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `whsec_` + base64("test-key-bytes-32bytes-long-secret")
    fn test_secret() -> String {
        let raw = b"test-key-bytes-32bytes-long-key!";
        format!("whsec_{}", B64.encode(raw))
    }

    fn sign(secret: &str, id: &str, ts: &str, body: &str) -> String {
        let raw = decode_secret(secret).unwrap();
        let mut mac = HmacSha256::new_from_slice(&raw).unwrap();
        mac.update(format!("{id}.{ts}.{body}").as_bytes());
        let sig = mac.finalize().into_bytes();
        format!("v1,{}", B64.encode(sig))
    }

    #[test]
    fn verifies_valid_signature() {
        let secret = test_secret();
        let body = r#"{"type":"user.created"}"#;
        let ts = chrono::Utc::now().timestamp().to_string();
        let id = "msg_test";
        let sig = sign(&secret, id, &ts, body);
        assert!(verify_svix_signature(&secret, id, &ts, &sig, body.as_bytes()).is_ok());
    }

    #[test]
    fn rejects_tampered_body() {
        let secret = test_secret();
        let ts = chrono::Utc::now().timestamp().to_string();
        let id = "msg_test";
        let sig = sign(&secret, id, &ts, "original");
        let err =
            verify_svix_signature(&secret, id, &ts, &sig, b"tampered").unwrap_err();
        assert!(err.contains("no matching"));
    }

    #[test]
    fn rejects_wrong_secret() {
        let ts = chrono::Utc::now().timestamp().to_string();
        let id = "msg_test";
        let body = "body";
        let sig = sign(&test_secret(), id, &ts, body);
        let other = format!("whsec_{}", B64.encode(b"different-32byte-key-for-negative!"));
        assert!(verify_svix_signature(&other, id, &ts, &sig, body.as_bytes()).is_err());
    }

    #[test]
    fn rejects_stale_timestamp() {
        let secret = test_secret();
        let stale = (chrono::Utc::now().timestamp() - 10 * 60).to_string();
        let id = "msg_test";
        let body = "body";
        let sig = sign(&secret, id, &stale, body);
        let err =
            verify_svix_signature(&secret, id, &stale, &sig, body.as_bytes()).unwrap_err();
        assert!(err.contains("replay"));
    }

    #[test]
    fn rejects_future_timestamp() {
        let secret = test_secret();
        let future = (chrono::Utc::now().timestamp() + 10 * 60).to_string();
        let sig = sign(&secret, "id", &future, "body");
        assert!(verify_svix_signature(&secret, "id", &future, &sig, b"body").is_err());
    }

    #[test]
    fn accepts_any_of_multiple_signature_tokens() {
        // Svix key rotation sends two v1 signatures space-separated.
        let secret = test_secret();
        let ts = chrono::Utc::now().timestamp().to_string();
        let id = "id";
        let body = "body";
        let good = sign(&secret, id, &ts, body);
        let bogus = "v1,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        let header = format!("{bogus} {good}");
        assert!(verify_svix_signature(&secret, id, &ts, &header, body.as_bytes()).is_ok());
    }

    #[test]
    fn rejects_non_v1_scheme_only() {
        let secret = test_secret();
        let ts = chrono::Utc::now().timestamp().to_string();
        // v2 tokens must be ignored (we only implement v1)
        let header = "v2,AAAA";
        assert!(verify_svix_signature(&secret, "id", &ts, header, b"body").is_err());
    }

    #[test]
    fn rejects_secret_without_whsec_prefix() {
        assert!(decode_secret("not-a-svix-secret").is_err());
    }

    #[test]
    fn constant_time_eq_length_mismatch() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
    }

    // ── user payload parsing ──────────────────────────────

    #[test]
    fn primary_email_prefers_matching_id() {
        let d: ClerkUserData = serde_json::from_value(serde_json::json!({
            "id": "user_1",
            "email_addresses": [
                {"id": "e1", "email_address": "a@x.com"},
                {"id": "e2", "email_address": "b@x.com"}
            ],
            "primary_email_address_id": "e2"
        }))
        .unwrap();
        assert_eq!(d.primary_email(), Some("b@x.com"));
    }

    #[test]
    fn primary_email_falls_back_to_first() {
        let d: ClerkUserData = serde_json::from_value(serde_json::json!({
            "id": "user_1",
            "email_addresses": [{"id": "e1", "email_address": "only@x.com"}]
        }))
        .unwrap();
        assert_eq!(d.primary_email(), Some("only@x.com"));
    }

    #[test]
    fn primary_email_none_when_empty() {
        let d: ClerkUserData = serde_json::from_value(serde_json::json!({
            "id": "user_1",
            "email_addresses": []
        }))
        .unwrap();
        assert_eq!(d.primary_email(), None);
    }

    #[test]
    fn display_name_joins_first_and_last() {
        let d: ClerkUserData = serde_json::from_value(serde_json::json!({
            "id": "u",
            "email_addresses": [],
            "first_name": "Alice",
            "last_name": "Doe"
        }))
        .unwrap();
        assert_eq!(d.display_name().as_deref(), Some("Alice Doe"));
    }

    #[test]
    fn display_name_uses_only_first_when_last_missing() {
        let d: ClerkUserData = serde_json::from_value(serde_json::json!({
            "id": "u",
            "email_addresses": [],
            "first_name": "Alice"
        }))
        .unwrap();
        assert_eq!(d.display_name().as_deref(), Some("Alice"));
    }

    #[test]
    fn display_name_none_when_both_missing() {
        let d: ClerkUserData = serde_json::from_value(serde_json::json!({
            "id": "u",
            "email_addresses": []
        }))
        .unwrap();
        assert_eq!(d.display_name(), None);
    }
}
