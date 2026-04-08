use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, TokenData, Validation};
use serde::{Deserialize, Serialize};

use crate::errors::AppError;

/// JWT claims payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    /// Subject — the user ID.
    pub sub: String,
    /// Expiry as Unix timestamp.
    pub exp: usize,
    /// Issued-at as Unix timestamp.
    pub iat: usize,
}

/// Create a signed JWT for the given user.
///
/// Used by integration tests to seed users for the JwtAuth-protected
/// `/api-keys` handlers; the production `/auth/login` flow that
/// historically called this was deleted in piece-4. Rust's dead-code
/// analysis can't see integration-test usage from the bin target.
// TODO(piece-6): delete jwt.rs once /api-keys handlers move to ClerkAuth.
#[allow(dead_code)]
pub fn create_token(user_id: &str, secret: &str, expiry_minutes: u64) -> Result<String, AppError> {
    let now = chrono::Utc::now().timestamp() as usize;
    let claims = Claims {
        sub: user_id.to_string(),
        exp: now + (expiry_minutes as usize * 60),
        iat: now,
    };
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(format!("JWT encoding failed: {e}")))
}

/// Validate and decode a JWT. Returns the claims on success.
pub fn validate_token(token: &str, secret: &str) -> Result<TokenData<Claims>, AppError> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    validation.leeway = 0;

    jsonwebtoken::decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map_err(|_| AppError::InvalidToken)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test_secret_at_least_32_chars_long_ok";

    #[test]
    fn create_token_returns_three_part_jwt() {
        let token = create_token("user_123", SECRET, 15).expect("token creation must succeed");
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT must have 3 dot-separated parts");
    }

    #[test]
    fn create_token_encodes_subject() {
        let token = create_token("user_abc", SECRET, 15).unwrap();
        let data = validate_token(&token, SECRET).unwrap();
        assert_eq!(data.claims.sub, "user_abc");
    }

    #[test]
    fn create_token_encodes_expiry_in_future() {
        let token = create_token("user_xyz", SECRET, 15).unwrap();
        let data = validate_token(&token, SECRET).unwrap();
        let now = chrono::Utc::now().timestamp() as usize;
        assert!(
            data.claims.exp > now,
            "exp {} should be in the future (now={})",
            data.claims.exp,
            now
        );
    }

    #[test]
    fn create_token_expiry_matches_requested_minutes() {
        let expiry_minutes: u64 = 30;
        let token = create_token("user_t", SECRET, expiry_minutes).unwrap();
        let data = validate_token(&token, SECRET).unwrap();
        let now = chrono::Utc::now().timestamp() as usize;
        let expected_exp = now + (expiry_minutes as usize * 60);
        // Allow up to 5 seconds of drift
        assert!(
            data.claims.exp >= expected_exp - 5 && data.claims.exp <= expected_exp + 5,
            "exp {} not within 5s of expected {}",
            data.claims.exp,
            expected_exp
        );
    }

    #[test]
    fn create_token_encodes_iat_near_now() {
        let token = create_token("user_iat", SECRET, 15).unwrap();
        let data = validate_token(&token, SECRET).unwrap();
        let now = chrono::Utc::now().timestamp() as usize;
        assert!(
            data.claims.iat <= now && data.claims.iat >= now - 5,
            "iat {} should be close to now {}",
            data.claims.iat,
            now
        );
    }

    #[test]
    fn validate_token_fails_with_wrong_secret() {
        let token = create_token("user_s", SECRET, 15).unwrap();
        let result = validate_token(&token, "different_secret_32_chars_long__!");
        assert!(
            matches!(result, Err(AppError::InvalidToken)),
            "wrong secret must return InvalidToken"
        );
    }

    #[test]
    fn validate_token_fails_for_malformed_string() {
        let result = validate_token("not.a.jwt", SECRET);
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[test]
    fn validate_token_fails_for_empty_string() {
        let result = validate_token("", SECRET);
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[test]
    fn validate_token_fails_for_expired_token() {
        // Create a token with a manually set past expiry
        let now = chrono::Utc::now().timestamp() as usize;
        let claims = Claims {
            sub: "user_exp".into(),
            exp: now - 3600, // 1 hour in the past
            iat: now - 7200,
        };
        let expired_token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap();

        let result = validate_token(&expired_token, SECRET);
        assert!(
            matches!(result, Err(AppError::InvalidToken)),
            "expired token must return InvalidToken"
        );
    }

    #[test]
    fn validate_token_fails_for_tampered_payload() {
        let token = create_token("user_ok", SECRET, 15).unwrap();
        // Tamper with the payload section (middle part)
        let mut parts: Vec<&str> = token.split('.').collect();
        parts[1] = "dGFtcGVyZWQ"; // base64 of "tampered"
        let tampered = parts.join(".");
        let result = validate_token(&tampered, SECRET);
        assert!(matches!(result, Err(AppError::InvalidToken)));
    }

    #[test]
    fn create_token_with_one_minute_expiry() {
        // Edge: minimal non-zero expiry
        let token = create_token("user_min", SECRET, 1).unwrap();
        let data = validate_token(&token, SECRET).unwrap();
        let now = chrono::Utc::now().timestamp() as usize;
        assert!(data.claims.exp > now);
        assert!(data.claims.exp <= now + 65); // 1 min + 5s drift
    }
}
