use serde::Serialize;
use sqlx::SqlitePool;

use crate::errors::AppError;

/// A stored API key (only the hash is persisted).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ApiKey {
    pub id: String,
    pub user_id: String,
    #[serde(skip_serializing)]
    #[allow(dead_code)]
    pub key_hash: String,
    pub key_prefix: String,
    pub name: String,
    pub is_active: bool,
    pub last_used_at: Option<String>,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

/// Insert a new API key record.
pub async fn create_api_key(
    pool: &SqlitePool,
    id: &str,
    user_id: &str,
    key_hash: &str,
    key_prefix: &str,
    name: &str,
) -> Result<ApiKey, AppError> {
    sqlx::query_as::<_, ApiKey>(
        "INSERT INTO api_keys (id, user_id, key_hash, key_prefix, name) VALUES (?, ?, ?, ?, ?) RETURNING *",
    )
    .bind(id)
    .bind(user_id)
    .bind(key_hash)
    .bind(key_prefix)
    .bind(name)
    .fetch_one(pool)
    .await
    .map_err(AppError::Database)
}

/// Look up an active API key by its SHA-256 hash.
pub async fn find_by_hash(pool: &SqlitePool, key_hash: &str) -> Result<Option<ApiKey>, AppError> {
    sqlx::query_as::<_, ApiKey>(
        "SELECT * FROM api_keys WHERE key_hash = ? AND is_active = 1 AND revoked_at IS NULL",
    )
    .bind(key_hash)
    .fetch_optional(pool)
    .await
    .map_err(AppError::Database)
}

/// List all API keys for a given user.
pub async fn list_for_user(pool: &SqlitePool, user_id: &str) -> Result<Vec<ApiKey>, AppError> {
    sqlx::query_as::<_, ApiKey>("SELECT * FROM api_keys WHERE user_id = ? ORDER BY created_at DESC")
        .bind(user_id)
        .fetch_all(pool)
        .await
        .map_err(AppError::Database)
}

/// Revoke an API key. Returns true if the key existed and was owned by the user.
pub async fn revoke(pool: &SqlitePool, key_id: &str, user_id: &str) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE api_keys SET is_active = 0, revoked_at = datetime('now') WHERE id = ? AND user_id = ? AND is_active = 1",
    )
    .bind(key_id)
    .bind(user_id)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;

    Ok(result.rows_affected() > 0)
}

/// Update the last_used_at timestamp for an API key.
pub async fn update_last_used(pool: &SqlitePool, key_id: &str) -> Result<(), AppError> {
    sqlx::query("UPDATE api_keys SET last_used_at = datetime('now') WHERE id = ?")
        .bind(key_id)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;
    Ok(())
}
