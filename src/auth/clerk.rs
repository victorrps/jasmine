//! Clerk JWT verification.
//!
//! Verifies RS256-signed JWTs issued by a Clerk instance against its
//! published JWKS endpoint. Clerk identifies the end user via the
//! standard `sub` claim (e.g. `user_2abc...`); the `iss` claim must
//! match the configured Clerk instance issuer.
//!
//! # Architecture
//!
//! - [`ClerkConfig`] — instance-level configuration (JWKS URL, issuer,
//!   leeway). Built from environment vars at startup and held in
//!   `web::Data` for handler access.
//! - [`JwksCache`] — thread-safe `kid -> DecodingKey` map. Refreshes
//!   from the JWKS URL on miss, with a minimum refresh interval to
//!   prevent thundering-herd refetches when an attacker spams requests
//!   with unknown `kid` values to force network calls.
//! - [`verify_clerk_token`] — pure async verification function.
//!   Returns parsed claims on success; collapses *every* failure mode
//!   into [`AppError::InvalidToken`] so a probing client cannot
//!   distinguish "expired" from "wrong issuer" from "tampered" from
//!   "unknown key" — all of which would otherwise leak information
//!   about our key rotation cadence and token validity windows.
//! - [`ClerkAuth`] — actix `FromRequest` extractor that pulls the
//!   Bearer token, calls `verify_clerk_token`, and exposes the Clerk
//!   user ID to handlers.
//!
//! # Test design
//!
//! Tests use a fixture RSA keypair under `tests/fixtures/clerk/`. They
//! construct a [`JwksCache::for_test`] pre-populated with the fixture
//! public key, sign tokens locally with the matching private key, and
//! assert each verification path. **No network access in unit tests.**

use actix_web::{dev::Payload, web, FromRequest, HttpRequest};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use crate::errors::AppError;

/// Default JWT clock-skew leeway in seconds. Clerk's session tokens are
/// short-lived (~60s) so we keep this small. 30s is enough to absorb
/// realistic clock drift between our server, the customer's browser,
/// and Clerk's edge.
#[allow(dead_code)]
pub const DEFAULT_LEEWAY_SECS: u64 = 30;

/// Minimum interval between JWKS refresh fetches. Bounds the rate at
/// which an attacker spamming unknown-`kid` tokens can force network
/// calls to the JWKS endpoint. 60s matches Clerk's documented key
/// rotation cadence.
const DEFAULT_MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Network timeout for JWKS fetches. JWKS endpoints are static JSON
/// behind a CDN — anything slower than 5s is broken.
const JWKS_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Clerk instance configuration. Wired from `CLERK_*` env vars at
/// startup; stored in `web::Data` for handler-level access.
#[derive(Debug, Clone)]
pub struct ClerkConfig {
    /// Public JWKS endpoint, e.g.
    /// `https://your-app.clerk.accounts.dev/.well-known/jwks.json`.
    /// Found in the Clerk dashboard under **API Keys → JWKS**. Empty
    /// string means "no Clerk wired" (combine with `dev_auth_bypass`
    /// for local development).
    pub jwks_url: String,
    /// Expected `iss` claim — Clerk's frontend API URL,
    /// e.g. `https://your-app.clerk.accounts.dev`. **Mandatory** when
    /// `jwks_url` is set: without an issuer match, any well-formed
    /// RS256 JWT signed by any key in the cache would validate.
    pub issuer: String,
    /// Clock-skew leeway in seconds. Defaults to [`DEFAULT_LEEWAY_SECS`].
    pub leeway_secs: u64,
    /// Dev-bypass mode. When `true` AND `jwks_url` is empty, the
    /// `ClerkAuth` extractor accepts an `X-Dev-User-Id` header in
    /// place of a real Clerk JWT. The `AppConfig` loader rejects the
    /// combination of `dev_auth_bypass=true` with a configured Clerk
    /// URL — but the extractor still re-checks the invariant on every
    /// request so a misconfigured deploy fails closed.
    pub dev_auth_bypass: bool,
}

/// Subset of Clerk's session JWT claims we care about. Clerk emits
/// many more (`azp`, `email`, `metadata`, etc.) but the only fields we
/// trust are the standard ones — anything else is mirrored to our
/// local `users` table via webhook, not read from the JWT.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClerkClaims {
    /// Clerk user ID, e.g. `user_2abc...`. Stable per user across
    /// sessions, devices, and email changes.
    pub sub: String,
    /// Issuer (Clerk frontend API URL).
    pub iss: String,
    /// Expiry as Unix timestamp.
    pub exp: usize,
    /// Issued-at as Unix timestamp. Optional — Clerk includes it but
    /// we don't strictly require it.
    #[serde(default)]
    pub iat: usize,
    /// Not-before as Unix timestamp. Optional.
    #[serde(default)]
    pub nbf: usize,
}

/// Thread-safe JWKS cache. Stores `kid -> DecodingKey` and refreshes
/// from the JWKS URL on cache miss, rate-limited so a flood of
/// unknown-`kid` requests cannot trigger a JWKS-fetch storm.
///
/// The lock is `std::sync::RwLock` (not `tokio::sync::RwLock`) because
/// it is only held within synchronous scopes — we never hold it across
/// `.await`. Poisoning is recovered with `into_inner()` since the
/// stored `DecodingKey` values have no broken invariants a panic
/// could leave behind.
///
/// `http` is `Option` so test caches can omit it entirely; production
/// caches build the client once in `new()` and reuse its connection
/// pool / TLS state across refreshes.
pub struct JwksCache {
    keys: RwLock<HashMap<String, DecodingKey>>,
    jwks_url: String,
    last_refresh: RwLock<Option<Instant>>,
    min_refresh_interval: Duration,
    http: Option<reqwest::Client>,
}

impl JwksCache {
    /// Build a JWKS cache that fetches from `jwks_url` on miss.
    pub fn new(jwks_url: impl Into<String>) -> Result<Self, AppError> {
        let http = reqwest::Client::builder()
            .timeout(JWKS_FETCH_TIMEOUT)
            .build()
            .map_err(|e| AppError::Internal(format!("failed to build JWKS http client: {e}")))?;
        Ok(Self {
            keys: RwLock::new(HashMap::new()),
            jwks_url: jwks_url.into(),
            last_refresh: RwLock::new(None),
            min_refresh_interval: DEFAULT_MIN_REFRESH_INTERVAL,
            http: Some(http),
        })
    }

    /// Test-only constructor. Pre-populates the cache with the given
    /// `kid -> DecodingKey` map and disables network refresh entirely
    /// (jwks_url left empty, http client absent, min refresh interval
    /// set to forever).
    #[cfg(test)]
    pub fn for_test(keys: HashMap<String, DecodingKey>) -> Self {
        Self {
            keys: RwLock::new(keys),
            jwks_url: String::new(),
            last_refresh: RwLock::new(Some(Instant::now())),
            min_refresh_interval: Duration::from_secs(u64::MAX),
            http: None,
        }
    }

    fn read_keys(&self) -> RwLockReadGuard<'_, HashMap<String, DecodingKey>> {
        self.keys.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_keys(&self) -> RwLockWriteGuard<'_, HashMap<String, DecodingKey>> {
        self.keys.write().unwrap_or_else(|e| e.into_inner())
    }

    fn read_last_refresh(&self) -> RwLockReadGuard<'_, Option<Instant>> {
        self.last_refresh.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_last_refresh(&self) -> RwLockWriteGuard<'_, Option<Instant>> {
        self.last_refresh.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Look up a key by `kid` from the in-memory map only. Returns
    /// `None` if absent — caller decides whether to refresh.
    fn get_cached(&self, kid: &str) -> Option<DecodingKey> {
        self.read_keys().get(kid).cloned()
    }

    /// Should we attempt a refresh right now? Rate-limited by
    /// `min_refresh_interval` so unknown-`kid` floods don't trigger a
    /// fetch storm against the JWKS endpoint.
    fn should_refresh(&self) -> bool {
        match *self.read_last_refresh() {
            Some(last) => last.elapsed() >= self.min_refresh_interval,
            None => true,
        }
    }

    /// Fetch the JWKS document and replace the in-memory cache.
    async fn refresh(&self) -> Result<(), AppError> {
        let Some(http) = self.http.as_ref() else {
            // Test caches have no http client — they never refresh.
            // Treat as a hard miss so the caller surfaces InvalidToken.
            return Err(AppError::InvalidToken);
        };
        if self.jwks_url.is_empty() {
            return Err(AppError::InvalidToken);
        }
        if !self.should_refresh() {
            // Recent refresh already happened; skip the fetch and let
            // the caller report whatever the cache currently holds.
            return Ok(());
        }

        // Advance the cooldown timer **before** the network call. This
        // closes a TOCTOU race where N concurrent unknown-`kid`
        // requests could all pass `should_refresh()` and all fetch in
        // parallel — turning a "1 fetch / 60s" guarantee into "N
        // fetches / 60s." Side effect: a failing fetch consumes the
        // cooldown window, so a legitimate key rotation is invisible
        // until the next 60s slot. That tradeoff is correct: a stale
        // cache for one minute is strictly safer than letting an
        // attacker amplify their kid-flood into a JWKS-endpoint flood.
        *self.write_last_refresh() = Some(Instant::now());

        let resp = http.get(&self.jwks_url).send().await.map_err(|e| {
            tracing::warn!(error = %e, jwks_url = %self.jwks_url, "JWKS fetch failed");
            AppError::InvalidToken
        })?;
        if !resp.status().is_success() {
            tracing::warn!(
                status = %resp.status(),
                jwks_url = %self.jwks_url,
                "JWKS endpoint returned non-success status"
            );
            return Err(AppError::InvalidToken);
        }
        let doc: JwksDoc = resp.json().await.map_err(|e| {
            tracing::warn!(error = %e, "JWKS response did not parse as JSON");
            AppError::InvalidToken
        })?;

        let mut new_map = HashMap::new();
        for jwk in doc.keys {
            // Strict filtering: RSA + RS256 only. A missing `alg` is
            // **rejected**, not defaulted, so we never silently load a
            // key whose publisher omitted the field — defense in depth
            // against a future Clerk JWKS that mixes algorithms.
            if jwk.kty != "RSA" {
                continue;
            }
            if jwk.alg.as_deref() != Some("RS256") {
                continue;
            }
            if let (Some(n), Some(e)) = (jwk.n.as_deref(), jwk.e.as_deref()) {
                if let Ok(key) = DecodingKey::from_rsa_components(n, e) {
                    new_map.insert(jwk.kid, key);
                }
            }
        }

        *self.write_keys() = new_map;
        Ok(())
    }

    /// Look up a key by `kid`, refreshing from the JWKS endpoint on
    /// miss (rate-limited).
    pub async fn get(&self, kid: &str) -> Result<DecodingKey, AppError> {
        if let Some(key) = self.get_cached(kid) {
            return Ok(key);
        }
        self.refresh().await?;
        self.get_cached(kid).ok_or(AppError::InvalidToken)
    }
}

/// Parsed JWKS document (subset). The full RFC 7517 schema is much
/// larger but we only need these fields.
#[derive(Debug, Deserialize)]
struct JwksDoc {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kid: String,
    kty: String,
    #[serde(default)]
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

/// Verify a Clerk-issued JWT against the supplied JWKS cache + config.
///
/// Returns the parsed claims on success. **Every failure mode collapses
/// into [`AppError::InvalidToken`]** — distinguishing "expired" from
/// "wrong issuer" from "tampered" from "unknown kid" would let an
/// unauthenticated probing client fingerprint our key rotation cadence
/// and the validity window of individual tokens.
///
/// Defenses applied (in order):
///
/// 1. Empty token → reject.
/// 2. Header decode → reject on parse failure.
/// 3. Algorithm must be RS256 — defends against the classic
///    "alg=HS256 with public key as secret" confusion attack. The
///    check fires *before* any key lookup so an attacker cannot use
///    a fabricated `kid` to force a JWKS fetch with a non-RS256
///    algorithm.
/// 4. `kid` must be present and resolvable in the JWKS cache (with
///    one rate-limited refresh attempt on miss).
/// 5. `decode` validates signature, `exp`, and `iss` against the
///    configured issuer. `iss` and `sub` are added to
///    `required_spec_claims` so issuer enforcement does not depend on
///    the `ClerkClaims` field type — a future struct refactor cannot
///    silently disable issuer validation. `aud` is intentionally **not**
///    validated — Clerk's session JWTs do not always carry one.
///    `nbf` is intentionally **not** validated either — Clerk does not
///    issue not-before claims, and accepting their absence keeps the
///    decision visible at this layer instead of buried in defaults.
/// 6. `sub` must be non-empty (defense in depth on top of the spec
///    claim presence check, which only verifies the field exists).
pub async fn verify_clerk_token(
    token: &str,
    jwks: &JwksCache,
    config: &ClerkConfig,
) -> Result<ClerkClaims, AppError> {
    if token.is_empty() {
        return Err(AppError::InvalidToken);
    }
    let header = decode_header(token).map_err(|_| AppError::InvalidToken)?;
    if header.alg != Algorithm::RS256 {
        return Err(AppError::InvalidToken);
    }
    let kid = header.kid.ok_or(AppError::InvalidToken)?;
    let key = jwks.get(&kid).await?;

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[&config.issuer]);
    // Force `iss` and `sub` to be present in the token, on top of the
    // default `exp` requirement. Without this, jsonwebtoken silently
    // skips issuer validation when `iss` is absent — turning the
    // ClerkClaims field type into a load-bearing security control.
    validation.set_required_spec_claims(&["exp", "iss", "sub"]);
    validation.leeway = config.leeway_secs;
    validation.validate_aud = false;
    validation.validate_exp = true;
    validation.validate_nbf = false;

    let data =
        decode::<ClerkClaims>(token, &key, &validation).map_err(|_| AppError::InvalidToken)?;

    if data.claims.sub.is_empty() {
        return Err(AppError::InvalidToken);
    }

    Ok(data.claims)
}

/// Actix extractor that validates a Clerk JWT and surfaces the Clerk
/// user ID to handlers.
///
/// Two operating modes:
///
/// 1. **Production / staging** — `ClerkConfig::jwks_url` is `Some(_)`,
///    `JwksCache` is registered in app data, and the extractor verifies
///    a real Clerk JWT from the `Authorization: Bearer ...` header.
///
/// 2. **Local dev bypass** — `ClerkConfig::jwks_url` is `None` and the
///    operator has set `dev_auth_bypass = true` (via the
///    `DEV_AUTH_BYPASS` env var, which the config layer rejects in
///    combination with a configured Clerk URL). In this mode the
///    extractor reads `X-Dev-User-Id: <id>` from the request and
///    returns it as the `clerk_user_id`. **Never enabled in
///    production**: the configuration loader hard-fails if both
///    `CLERK_JWKS_URL` and `DEV_AUTH_BYPASS=true` are set, and this
///    extractor double-checks the same invariant at request time so a
///    misconfigured deploy fails closed instead of silently allowing
///    header impersonation.
pub struct ClerkAuth {
    pub clerk_user_id: String,
}

/// Header used by the dev-bypass branch. Pick something obviously
/// non-standard so a real client can never collide with it by
/// accident, and so a `grep DEV_USER_ID` audit finds every site that
/// trusts it.
pub const DEV_USER_ID_HEADER: &str = "X-Dev-User-Id";

impl FromRequest for ClerkAuth {
    type Error = AppError;
    type Future = Pin<Box<dyn std::future::Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let req = req.clone();
        Box::pin(async move {
            let config = req
                .app_data::<web::Data<ClerkConfig>>()
                .ok_or_else(|| AppError::Internal("ClerkConfig not configured".into()))?
                .clone();

            // ── Branch 1: dev bypass ────────────────────────────────────
            // Activated only when the deployment has explicitly opted
            // in (`dev_auth_bypass = true`) AND no Clerk JWKS URL is
            // configured. The double check defends against the case
            // where someone hot-edits config in memory; the loader
            // already rejects the combination at startup.
            if config.dev_auth_bypass && config.jwks_url.is_empty() {
                let dev_id = req
                    .headers()
                    .get(DEV_USER_ID_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or(AppError::InvalidToken)?
                    .to_string();
                if dev_id.len() > 256 {
                    // Bound the header value to a sane length so a
                    // pathological client cannot inflate user-id
                    // strings into the database via this path.
                    return Err(AppError::InvalidToken);
                }
                // Character-class allowlist matching real Clerk user
                // IDs (`user_[a-zA-Z0-9]+`). Defense in depth: SQLite
                // parameterization already prevents injection, but
                // restricting to the production-value shape stops a
                // dev from accidentally seeding garbage strings into
                // the `users` table that would not match anything
                // Clerk would later emit.
                if !dev_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(AppError::InvalidToken);
                }
                // Per-request audit log. The startup banner warns
                // once; this line gives every bypass invocation a
                // searchable record so a stray `DEV_AUTH_BYPASS=true`
                // in staging shows up immediately in the log stream.
                tracing::warn!(
                    user_id = %dev_id,
                    "ClerkAuth: dev-bypass active — X-Dev-User-Id accepted"
                );
                return Ok(ClerkAuth {
                    clerk_user_id: dev_id,
                });
            }

            // ── Branch 2: real Clerk JWT verification ───────────────────
            let jwks = req
                .app_data::<web::Data<JwksCache>>()
                .ok_or_else(|| AppError::Internal("JwksCache not configured".into()))?
                .clone();

            let token = req
                .headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "))
                .ok_or(AppError::InvalidToken)?
                .to_string();

            let claims = verify_clerk_token(&token, jwks.as_ref(), config.as_ref()).await?;
            Ok(ClerkAuth {
                clerk_user_id: claims.sub,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};

    fn private_pem() -> &'static [u8] {
        include_bytes!("../../tests/fixtures/clerk/test_private.pem")
    }

    fn public_pem() -> &'static [u8] {
        include_bytes!("../../tests/fixtures/clerk/test_public.pem")
    }

    /// Build a test JWKS cache holding one fixture key under `kid`.
    fn cache_with(kid: &str) -> JwksCache {
        let key = DecodingKey::from_rsa_pem(public_pem()).expect("fixture pubkey must parse");
        let mut map = HashMap::new();
        map.insert(kid.to_string(), key);
        JwksCache::for_test(map)
    }

    fn test_config() -> ClerkConfig {
        ClerkConfig {
            jwks_url: String::new(),
            issuer: "https://test.clerk.accounts.dev".into(),
            leeway_secs: 0,
            dev_auth_bypass: false,
        }
    }

    fn now() -> usize {
        chrono::Utc::now().timestamp() as usize
    }

    fn good_claims_value() -> serde_json::Value {
        let n = now();
        serde_json::json!({
            "sub": "user_test_abc",
            "iss": "https://test.clerk.accounts.dev",
            "exp": n + 600,
            "iat": n,
        })
    }

    /// Sign an arbitrary claims JSON with the fixture RSA private key.
    fn sign_rs256(claims: &serde_json::Value, kid: Option<&str>) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = kid.map(str::to_string);
        let key = EncodingKey::from_rsa_pem(private_pem()).expect("fixture privkey must parse");
        jsonwebtoken::encode(&header, claims, &key).expect("sign must succeed")
    }

    // ── Happy path ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn verify_accepts_well_formed_token() {
        let token = sign_rs256(&good_claims_value(), Some("test-kid"));
        let cache = cache_with("test-kid");
        let claims = verify_clerk_token(&token, &cache, &test_config())
            .await
            .expect("valid token must verify");
        assert_eq!(claims.sub, "user_test_abc");
        assert_eq!(claims.iss, "https://test.clerk.accounts.dev");
    }

    // ── Reject paths — every failure must surface as InvalidToken ──────────

    #[tokio::test]
    async fn verify_rejects_empty_token() {
        let cache = cache_with("test-kid");
        let result = verify_clerk_token("", &cache, &test_config()).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn verify_rejects_garbage_token() {
        let cache = cache_with("test-kid");
        let result = verify_clerk_token("not.a.jwt", &cache, &test_config()).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn verify_rejects_expired_token() {
        let n = now();
        let claims = serde_json::json!({
            "sub": "user_test_abc",
            "iss": "https://test.clerk.accounts.dev",
            "exp": n - 60,
            "iat": n - 600,
        });
        let token = sign_rs256(&claims, Some("test-kid"));
        let cache = cache_with("test-kid");
        let result = verify_clerk_token(&token, &cache, &test_config()).await;
        assert!(
            matches!(result, Err(AppError::InvalidToken)),
            "expired token must surface as InvalidToken"
        );
    }

    #[tokio::test]
    async fn verify_rejects_wrong_issuer() {
        let mut c = good_claims_value();
        c["iss"] = serde_json::Value::String("https://attacker.example.com".into());
        let token = sign_rs256(&c, Some("test-kid"));
        let cache = cache_with("test-kid");
        let result = verify_clerk_token(&token, &cache, &test_config()).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn verify_rejects_unknown_kid() {
        let token = sign_rs256(&good_claims_value(), Some("rotated-kid"));
        let cache = cache_with("test-kid"); // only knows test-kid
        let result = verify_clerk_token(&token, &cache, &test_config()).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn verify_rejects_missing_kid_header() {
        let token = sign_rs256(&good_claims_value(), None);
        let cache = cache_with("test-kid");
        let result = verify_clerk_token(&token, &cache, &test_config()).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn verify_rejects_tampered_signature() {
        let token = sign_rs256(&good_claims_value(), Some("test-kid"));
        let mut parts: Vec<&str> = token.split('.').collect();
        parts[2] = "AAAA"; // garbage signature
        let tampered = parts.join(".");
        let cache = cache_with("test-kid");
        let result = verify_clerk_token(&tampered, &cache, &test_config()).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn verify_rejects_empty_subject_claim() {
        let mut c = good_claims_value();
        c["sub"] = serde_json::Value::String(String::new());
        let token = sign_rs256(&c, Some("test-kid"));
        let cache = cache_with("test-kid");
        let result = verify_clerk_token(&token, &cache, &test_config()).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn verify_rejects_hs256_algorithm_confusion_attack() {
        // Classic algorithm-confusion: an attacker who scrapes the
        // public key from the JWKS endpoint signs a token with HS256
        // using that public key as the symmetric secret. A naive
        // verifier that does not pin the algorithm will accept it.
        // Our verifier MUST reject because it strictly requires RS256
        // before even consulting the JWKS cache.
        let claims = good_claims_value();
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("test-kid".into());
        let key = EncodingKey::from_secret(public_pem());
        let token = jsonwebtoken::encode(&header, &claims, &key).expect("sign");
        let cache = cache_with("test-kid");
        let result = verify_clerk_token(&token, &cache, &test_config()).await;
        assert!(
            matches!(result, Err(AppError::InvalidToken)),
            "HS256 algorithm confusion must be rejected"
        );
    }

    // ── Leeway behaviour ───────────────────────────────────────────────────

    #[tokio::test]
    async fn verify_accepts_just_expired_token_within_leeway() {
        let n = now();
        let mut c = good_claims_value();
        c["exp"] = serde_json::Value::Number((n - 5).into());
        let token = sign_rs256(&c, Some("test-kid"));
        let cache = cache_with("test-kid");
        let mut config = test_config();
        config.leeway_secs = 30;
        let claims = verify_clerk_token(&token, &cache, &config)
            .await
            .expect("token within leeway must verify");
        assert_eq!(claims.sub, "user_test_abc");
    }

    #[tokio::test]
    async fn verify_rejects_token_outside_leeway() {
        let n = now();
        let mut c = good_claims_value();
        c["exp"] = serde_json::Value::Number((n - 120).into());
        let token = sign_rs256(&c, Some("test-kid"));
        let cache = cache_with("test-kid");
        let mut config = test_config();
        config.leeway_secs = 30;
        let result = verify_clerk_token(&token, &cache, &config).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    // ── JwksCache behaviour ────────────────────────────────────────────────

    #[tokio::test]
    async fn jwks_cache_returns_cached_key_without_network() {
        // for_test caches have an empty jwks_url; a successful lookup
        // proves no network call was attempted.
        let cache = cache_with("k1");
        let key = cache.get("k1").await.expect("cached lookup must succeed");
        // Re-use the key to verify it actually decodes a token.
        let token = sign_rs256(&good_claims_value(), Some("k1"));
        let mut v = Validation::new(Algorithm::RS256);
        v.set_issuer(&["https://test.clerk.accounts.dev"]);
        v.validate_aud = false;
        let _ = jsonwebtoken::decode::<ClerkClaims>(&token, &key, &v)
            .expect("token must decode against cached key");
    }

    #[tokio::test]
    async fn jwks_cache_returns_invalid_token_when_kid_missing_and_no_url() {
        let cache = cache_with("k1");
        let result = cache.get("k2").await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn verify_rejects_token_missing_iss_claim() {
        // Token with `iss` entirely absent. The `set_required_spec_claims`
        // change must surface this as InvalidToken instead of silently
        // skipping issuer validation. Note: serde would also reject this
        // because `ClerkClaims.iss` is non-Option, but the validation
        // layer must catch it independently — that's the whole point of
        // the spec-claim requirement.
        let n = now();
        let claims = serde_json::json!({
            "sub": "user_test_abc",
            "exp": n + 600,
            "iat": n,
        });
        let token = sign_rs256(&claims, Some("test-kid"));
        let cache = cache_with("test-kid");
        let result = verify_clerk_token(&token, &cache, &test_config()).await;
        assert!(
            matches!(result, Err(AppError::InvalidToken)),
            "token without iss claim must be rejected"
        );
    }

    #[tokio::test]
    async fn verify_rejects_token_missing_sub_claim() {
        let n = now();
        let claims = serde_json::json!({
            "iss": "https://test.clerk.accounts.dev",
            "exp": n + 600,
            "iat": n,
        });
        let token = sign_rs256(&claims, Some("test-kid"));
        let cache = cache_with("test-kid");
        let result = verify_clerk_token(&token, &cache, &test_config()).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn verify_ignores_nbf_in_future() {
        // Clerk does not issue `nbf`, but if a token includes one in
        // the future the verifier must still accept it (we set
        // validate_nbf=false intentionally). This test pins that
        // behaviour so a future change to validate_nbf cannot silently
        // start rejecting tokens we previously accepted.
        let n = now();
        let claims = serde_json::json!({
            "sub": "user_nbf",
            "iss": "https://test.clerk.accounts.dev",
            "exp": n + 600,
            "iat": n,
            "nbf": n + 300, // 5 min in the future
        });
        let token = sign_rs256(&claims, Some("test-kid"));
        let cache = cache_with("test-kid");
        let claims = verify_clerk_token(&token, &cache, &test_config())
            .await
            .expect("nbf in future must be ignored when validate_nbf=false");
        assert_eq!(claims.sub, "user_nbf");
    }

    // ── Dev-bypass extractor branch ────────────────────────────────────────
    //
    // The bypass is exercised through the full `FromRequest` path so the
    // tests pin actual handler-time behaviour, not just the helper logic.

    use actix_web::test::TestRequest;

    fn dev_bypass_config() -> ClerkConfig {
        ClerkConfig {
            jwks_url: String::new(),
            issuer: String::new(),
            leeway_secs: 30,
            dev_auth_bypass: true,
        }
    }

    async fn extract(req: actix_web::HttpRequest) -> Result<ClerkAuth, AppError> {
        let mut payload = actix_web::dev::Payload::None;
        ClerkAuth::from_request(&req, &mut payload).await
    }

    #[tokio::test]
    async fn dev_bypass_accepts_x_dev_user_id_header() {
        let cfg = web::Data::new(dev_bypass_config());
        // JwksCache is needed in app_data even though the bypass
        // branch never touches it — we register it so a misconfigured
        // production deploy doesn't accidentally serve via the
        // bypass.
        let cache = web::Data::new(JwksCache::for_test(HashMap::new()));
        let req = TestRequest::default()
            .insert_header((DEV_USER_ID_HEADER, "user_dev_alice"))
            .app_data(cfg)
            .app_data(cache)
            .to_http_request();
        let auth = extract(req).await.expect("dev bypass must accept header");
        assert_eq!(auth.clerk_user_id, "user_dev_alice");
    }

    #[tokio::test]
    async fn dev_bypass_rejects_missing_header() {
        let cfg = web::Data::new(dev_bypass_config());
        let cache = web::Data::new(JwksCache::for_test(HashMap::new()));
        let req = TestRequest::default()
            .app_data(cfg)
            .app_data(cache)
            .to_http_request();
        let result = extract(req).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn dev_bypass_rejects_empty_header_value() {
        let cfg = web::Data::new(dev_bypass_config());
        let cache = web::Data::new(JwksCache::for_test(HashMap::new()));
        let req = TestRequest::default()
            .insert_header((DEV_USER_ID_HEADER, "   "))
            .app_data(cfg)
            .app_data(cache)
            .to_http_request();
        let result = extract(req).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn dev_bypass_rejects_overlength_header_value() {
        let cfg = web::Data::new(dev_bypass_config());
        let cache = web::Data::new(JwksCache::for_test(HashMap::new()));
        let long_id = "a".repeat(257);
        let req = TestRequest::default()
            .insert_header((DEV_USER_ID_HEADER, long_id.as_str()))
            .app_data(cfg)
            .app_data(cache)
            .to_http_request();
        let result = extract(req).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn dev_bypass_rejects_header_with_disallowed_characters() {
        // Defense in depth on top of SQLite parameterization: the dev
        // user ID must look like a real Clerk user ID
        // (`[a-zA-Z0-9_-]+`). Special characters are rejected.
        let cfg = web::Data::new(dev_bypass_config());
        let cache = web::Data::new(JwksCache::for_test(HashMap::new()));
        for bad in &[
            "user_test'; DROP TABLE users;--",
            "user with spaces",
            "user/slash",
            "user@example.com",
            "user;test",
        ] {
            let req = TestRequest::default()
                .insert_header((DEV_USER_ID_HEADER, *bad))
                .app_data(cfg.clone())
                .app_data(cache.clone())
                .to_http_request();
            let result = extract(req).await;
            assert!(
                matches!(result, Err(AppError::InvalidToken)),
                "value {bad:?} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn dev_bypass_accepts_realistic_clerk_user_ids() {
        let cfg = web::Data::new(dev_bypass_config());
        let cache = web::Data::new(JwksCache::for_test(HashMap::new()));
        for ok in &[
            "user_2abcdefghijklmnopqrstuv",
            "user_dev_alice",
            "alice-dev",
            "abc123",
        ] {
            let req = TestRequest::default()
                .insert_header((DEV_USER_ID_HEADER, *ok))
                .app_data(cfg.clone())
                .app_data(cache.clone())
                .to_http_request();
            let auth = extract(req).await.expect("realistic ID must be accepted");
            assert_eq!(auth.clerk_user_id, *ok);
        }
    }

    #[tokio::test]
    async fn dev_bypass_does_not_activate_when_jwks_url_is_set() {
        // Belt-and-suspenders: even if a misconfigured deploy somehow
        // produces a ClerkConfig with both `dev_auth_bypass=true` and
        // a non-empty `jwks_url`, the extractor must NOT take the
        // bypass branch. The AppConfig loader rejects this combo at
        // startup; this test pins the second line of defense.
        let cfg = web::Data::new(ClerkConfig {
            jwks_url: "https://example.com/jwks.json".into(),
            issuer: "https://example.com".into(),
            leeway_secs: 30,
            dev_auth_bypass: true,
        });
        let cache = web::Data::new(JwksCache::for_test(HashMap::new()));
        // No Authorization header, no Bearer token — this should fail
        // with InvalidToken via the JWT branch, NOT silently succeed
        // via the bypass branch even though the X-Dev-User-Id header
        // is present.
        let req = TestRequest::default()
            .insert_header((DEV_USER_ID_HEADER, "user_attacker"))
            .app_data(cfg)
            .app_data(cache)
            .to_http_request();
        let result = extract(req).await;
        assert!(
            matches!(result, Err(AppError::InvalidToken)),
            "bypass must NOT activate when jwks_url is set, even if dev_auth_bypass is true"
        );
    }

    #[tokio::test]
    async fn extractor_falls_through_to_jwt_branch_when_bypass_disabled() {
        // dev_auth_bypass=false → bypass header is ignored → JWT
        // branch runs → no Authorization header → InvalidToken.
        let cfg = web::Data::new(ClerkConfig {
            jwks_url: String::new(),
            issuer: "https://test".into(),
            leeway_secs: 30,
            dev_auth_bypass: false,
        });
        let cache = web::Data::new(JwksCache::for_test(HashMap::new()));
        let req = TestRequest::default()
            .insert_header((DEV_USER_ID_HEADER, "user_x"))
            .app_data(cfg)
            .app_data(cache)
            .to_http_request();
        let result = extract(req).await;
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[tokio::test]
    async fn jwks_cache_network_failure_surfaces_invalid_token() {
        // Build a real (non-test) cache pointed at an unroutable port.
        // The `refresh()` path must collapse the connection error into
        // AppError::InvalidToken with no panic and no leakage. This is
        // the only test that exercises the production `new()`
        // constructor + the `http` field.
        let cache = JwksCache::new("http://127.0.0.1:1/jwks.json")
            .expect("client build must succeed");
        let result = cache.get("any-kid").await;
        assert!(
            matches!(result, Err(AppError::InvalidToken)),
            "network failure must surface as InvalidToken"
        );
    }
}
