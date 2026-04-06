use sqlx::SqlitePool;

use crate::errors::AppError;

/// Record a usage event for billing/analytics.
pub async fn log_usage(
    pool: &SqlitePool,
    api_key_id: &str,
    endpoint: &str,
    pages_processed: u32,
    credits_used: u32,
    processing_ms: u64,
    request_id: &str,
) -> Result<(), AppError> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO usage_logs (id, api_key_id, endpoint, pages_processed, credits_used, processing_ms, request_id) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(api_key_id)
    .bind(endpoint)
    .bind(pages_processed)
    .bind(credits_used)
    .bind(processing_ms as i64)
    .bind(request_id)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;
    Ok(())
}
