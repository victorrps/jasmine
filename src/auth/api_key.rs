use actix_web::{dev::Payload, web, FromRequest, HttpRequest};
use hmac::{Hmac, Mac};
use rand::distributions::Alphanumeric;
use rand::Rng;
use sha2::Sha256;
use sqlx::SqlitePool;

use crate::config::AppConfig;
use crate::errors::AppError;
use crate::models;

type HmacSha256 = Hmac<Sha256>;

/// Extractor that validates an API key from the `X-API-Key` header.
#[allow(dead_code)]
pub struct ApiKeyAuth {
    pub user_id: String,
    pub api_key_id: String,
}

impl FromRequest for ApiKeyAuth {
    type Error = AppError;
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let req = req.clone();
        Box::pin(async move { extract_api_key(&req).await })
    }
}

async fn extract_api_key(req: &HttpRequest) -> Result<ApiKeyAuth, AppError> {
    let pool = req
        .app_data::<web::Data<SqlitePool>>()
        .ok_or_else(|| AppError::Internal("Database pool not found".into()))?;

    let config = req
        .app_data::<web::Data<AppConfig>>()
        .ok_or_else(|| AppError::Internal("AppConfig not found".into()))?;

    let key_header = req
        .headers()
        .get("X-API-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::InvalidApiKey)?;

    if !key_header.starts_with("df_live_") || key_header.len() != 40 {
        return Err(AppError::InvalidApiKey);
    }

    let key_hash = hash_api_key(key_header, &config.api_key_pepper);
    let api_key = models::api_key::find_by_hash(pool, &key_hash)
        .await?
        .ok_or(AppError::InvalidApiKey)?;

    // Fire-and-forget last_used_at update
    let pool_clone = pool.get_ref().clone();
    let key_id = api_key.id.clone();
    tokio::spawn(async move {
        let _ = models::api_key::update_last_used(&pool_clone, &key_id).await;
    });

    Ok(ApiKeyAuth {
        user_id: api_key.user_id,
        api_key_id: api_key.id,
    })
}

/// Generate a new API key. Returns (plaintext_key, hmac_hash, prefix).
pub fn generate_api_key(pepper: &str) -> (String, String, String) {
    let random_part: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let plaintext = format!("df_live_{random_part}");
    let prefix = plaintext[..16].to_string();
    let hash = hash_api_key(&plaintext, pepper);
    (plaintext, hash, prefix)
}

/// Compute HMAC-SHA256 of an API key using the server-side pepper.
/// This prevents offline brute-force if the database is compromised.
pub fn hash_api_key(key: &str, pepper: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(pepper.as_bytes()).expect("HMAC accepts any key size");
    mac.update(key.as_bytes());
    format!("{:x}", mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PEPPER: &str = "test_pepper_for_unit_tests_only!";

    #[test]
    fn generate_api_key_has_correct_prefix() {
        let (plaintext, _, _) = generate_api_key(TEST_PEPPER);
        assert!(plaintext.starts_with("df_live_"));
    }

    #[test]
    fn generate_api_key_has_correct_total_length() {
        let (plaintext, _, _) = generate_api_key(TEST_PEPPER);
        assert_eq!(plaintext.len(), 40);
    }

    #[test]
    fn generate_api_key_prefix_field_is_first_16_chars() {
        let (plaintext, _, prefix) = generate_api_key(TEST_PEPPER);
        assert_eq!(prefix, &plaintext[..16]);
    }

    #[test]
    fn generate_api_key_hash_matches_hash_api_key() {
        let (plaintext, hash, _) = generate_api_key(TEST_PEPPER);
        assert_eq!(hash, hash_api_key(&plaintext, TEST_PEPPER));
    }

    #[test]
    fn generate_api_key_is_unique_per_call() {
        let (k1, _, _) = generate_api_key(TEST_PEPPER);
        let (k2, _, _) = generate_api_key(TEST_PEPPER);
        assert_ne!(k1, k2);
    }

    #[test]
    fn generate_api_key_random_part_is_alphanumeric() {
        let (plaintext, _, _) = generate_api_key(TEST_PEPPER);
        assert!(plaintext[8..].chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn hash_api_key_is_deterministic() {
        let h1 = hash_api_key("df_live_test", TEST_PEPPER);
        let h2 = hash_api_key("df_live_test", TEST_PEPPER);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_api_key_returns_64_hex_chars() {
        let hash = hash_api_key("df_live_abc", TEST_PEPPER);
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_api_key_different_inputs_produce_different_hashes() {
        let h1 = hash_api_key("df_live_keyA", TEST_PEPPER);
        let h2 = hash_api_key("df_live_keyB", TEST_PEPPER);
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_api_key_different_peppers_produce_different_hashes() {
        let h1 = hash_api_key("df_live_same_key", "pepper1_long_enough_32chars!!!!!");
        let h2 = hash_api_key("df_live_same_key", "pepper2_long_enough_32chars!!!!!");
        assert_ne!(h1, h2, "different peppers must produce different hashes");
    }

    #[test]
    fn hash_api_key_empty_string() {
        let hash = hash_api_key("", TEST_PEPPER);
        assert_eq!(hash.len(), 64);
    }
}
