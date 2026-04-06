use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::errors::AppError;

/// A batch processing job record.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BatchJob {
    pub id: String,
    pub api_key_id: String,
    pub status: String,
    pub total_files: i32,
    pub succeeded: i32,
    pub failed: i32,
    pub created_at: String,
    pub completed_at: Option<String>,
}

/// A single result within a batch job.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BatchResult {
    pub id: String,
    pub batch_job_id: String,
    pub file_index: i32,
    pub file_name: Option<String>,
    pub status: String,
    pub result_json: Option<String>,
    pub error_message: Option<String>,
    pub created_at: String,
}

/// Create a new batch job record.
pub async fn create_batch_job(
    pool: &SqlitePool,
    id: &str,
    api_key_id: &str,
    total_files: i32,
) -> Result<BatchJob, AppError> {
    sqlx::query_as::<_, BatchJob>(
        "INSERT INTO batch_jobs (id, api_key_id, status, total_files) VALUES (?, ?, 'processing', ?) RETURNING *",
    )
    .bind(id)
    .bind(api_key_id)
    .bind(total_files)
    .fetch_one(pool)
    .await
    .map_err(AppError::Database)
}

/// Update the status and counters of a batch job.
pub async fn update_batch_job_status(
    pool: &SqlitePool,
    id: &str,
    status: &str,
    succeeded: i32,
    failed: i32,
) -> Result<(), AppError> {
    let completed_at = if status == "completed" || status == "failed" {
        Some("datetime('now')")
    } else {
        None
    };

    if completed_at.is_some() {
        sqlx::query(
            "UPDATE batch_jobs SET status = ?, succeeded = ?, failed = ?, completed_at = datetime('now') WHERE id = ?",
        )
        .bind(status)
        .bind(succeeded)
        .bind(failed)
        .bind(id)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;
    } else {
        sqlx::query("UPDATE batch_jobs SET status = ?, succeeded = ?, failed = ? WHERE id = ?")
            .bind(status)
            .bind(succeeded)
            .bind(failed)
            .bind(id)
            .execute(pool)
            .await
            .map_err(AppError::Database)?;
    }

    Ok(())
}

/// Retrieve a batch job by ID.
#[allow(dead_code)]
pub async fn get_batch_job(pool: &SqlitePool, id: &str) -> Result<Option<BatchJob>, AppError> {
    sqlx::query_as::<_, BatchJob>("SELECT * FROM batch_jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(AppError::Database)
}

/// Retrieve a batch job by ID, scoped to an API key for authorization.
pub async fn get_batch_job_for_key(
    pool: &SqlitePool,
    id: &str,
    api_key_id: &str,
) -> Result<Option<BatchJob>, AppError> {
    sqlx::query_as::<_, BatchJob>(
        "SELECT * FROM batch_jobs WHERE id = ? AND api_key_id = ?",
    )
    .bind(id)
    .bind(api_key_id)
    .fetch_optional(pool)
    .await
    .map_err(AppError::Database)
}

/// Save a single batch result row.
#[allow(clippy::too_many_arguments)]
pub async fn save_batch_result(
    pool: &SqlitePool,
    id: &str,
    batch_job_id: &str,
    file_index: i32,
    file_name: Option<&str>,
    status: &str,
    result_json: Option<&str>,
    error_message: Option<&str>,
) -> Result<BatchResult, AppError> {
    sqlx::query_as::<_, BatchResult>(
        "INSERT INTO batch_results (id, batch_job_id, file_index, file_name, status, result_json, error_message) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING *",
    )
    .bind(id)
    .bind(batch_job_id)
    .bind(file_index)
    .bind(file_name)
    .bind(status)
    .bind(result_json)
    .bind(error_message)
    .fetch_one(pool)
    .await
    .map_err(AppError::Database)
}

/// Retrieve all results for a batch job, ordered by file index.
pub async fn get_batch_results(
    pool: &SqlitePool,
    batch_job_id: &str,
) -> Result<Vec<BatchResult>, AppError> {
    sqlx::query_as::<_, BatchResult>(
        "SELECT * FROM batch_results WHERE batch_job_id = ? ORDER BY file_index ASC",
    )
    .bind(batch_job_id)
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create an in-memory SQLite pool with the schema applied.
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS api_keys (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                key_hash TEXT UNIQUE NOT NULL,
                key_prefix TEXT NOT NULL,
                name TEXT NOT NULL DEFAULT 'Default',
                is_active INTEGER NOT NULL DEFAULT 1,
                last_used_at TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                revoked_at TEXT
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS batch_jobs (
                id TEXT PRIMARY KEY,
                api_key_id TEXT NOT NULL REFERENCES api_keys(id),
                status TEXT NOT NULL DEFAULT 'processing',
                total_files INTEGER NOT NULL DEFAULT 0,
                succeeded INTEGER NOT NULL DEFAULT 0,
                failed INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                completed_at TEXT
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS batch_results (
                id TEXT PRIMARY KEY,
                batch_job_id TEXT NOT NULL REFERENCES batch_jobs(id) ON DELETE CASCADE,
                file_index INTEGER NOT NULL,
                file_name TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                result_json TEXT,
                error_message TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Insert a dummy API key for foreign key constraints
        sqlx::query(
            "INSERT INTO api_keys (id, user_id, key_hash, key_prefix, name) VALUES ('key1', 'user1', 'hash1', 'prefix1', 'Test')",
        )
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    #[tokio::test]
    async fn creates_batch_job_with_processing_status() {
        let pool = test_pool().await;
        let job = create_batch_job(&pool, "batch_001", "key1", 5).await.unwrap();
        assert_eq!(job.id, "batch_001");
        assert_eq!(job.api_key_id, "key1");
        assert_eq!(job.status, "processing");
        assert_eq!(job.total_files, 5);
        assert_eq!(job.succeeded, 0);
        assert_eq!(job.failed, 0);
        assert!(job.completed_at.is_none());
    }

    #[tokio::test]
    async fn gets_batch_job_by_id() {
        let pool = test_pool().await;
        create_batch_job(&pool, "batch_002", "key1", 3).await.unwrap();
        let found = get_batch_job(&pool, "batch_002").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().total_files, 3);
    }

    #[tokio::test]
    async fn get_batch_job_returns_none_for_missing_id() {
        let pool = test_pool().await;
        let found = get_batch_job(&pool, "nonexistent").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn gets_batch_job_scoped_to_api_key() {
        let pool = test_pool().await;
        create_batch_job(&pool, "batch_003", "key1", 2).await.unwrap();

        let found = get_batch_job_for_key(&pool, "batch_003", "key1").await.unwrap();
        assert!(found.is_some());

        let not_found = get_batch_job_for_key(&pool, "batch_003", "other_key").await.unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn updates_batch_job_status_to_completed() {
        let pool = test_pool().await;
        create_batch_job(&pool, "batch_004", "key1", 4).await.unwrap();
        update_batch_job_status(&pool, "batch_004", "completed", 3, 1)
            .await
            .unwrap();

        let job = get_batch_job(&pool, "batch_004").await.unwrap().unwrap();
        assert_eq!(job.status, "completed");
        assert_eq!(job.succeeded, 3);
        assert_eq!(job.failed, 1);
        assert!(job.completed_at.is_some());
    }

    #[tokio::test]
    async fn updates_batch_job_status_without_completing() {
        let pool = test_pool().await;
        create_batch_job(&pool, "batch_005", "key1", 2).await.unwrap();
        update_batch_job_status(&pool, "batch_005", "processing", 1, 0)
            .await
            .unwrap();

        let job = get_batch_job(&pool, "batch_005").await.unwrap().unwrap();
        assert_eq!(job.status, "processing");
        assert!(job.completed_at.is_none());
    }

    #[tokio::test]
    async fn saves_and_retrieves_batch_results() {
        let pool = test_pool().await;
        create_batch_job(&pool, "batch_006", "key1", 2).await.unwrap();

        save_batch_result(
            &pool,
            "res_001",
            "batch_006",
            0,
            Some("file1.pdf"),
            "success",
            Some(r#"{"pages":1}"#),
            None,
        )
        .await
        .unwrap();

        save_batch_result(
            &pool,
            "res_002",
            "batch_006",
            1,
            Some("file2.pdf"),
            "error",
            None,
            Some("Invalid PDF"),
        )
        .await
        .unwrap();

        let results = get_batch_results(&pool, "batch_006").await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].file_index, 0);
        assert_eq!(results[0].file_name.as_deref(), Some("file1.pdf"));
        assert_eq!(results[0].status, "success");
        assert!(results[0].result_json.is_some());
        assert!(results[0].error_message.is_none());

        assert_eq!(results[1].file_index, 1);
        assert_eq!(results[1].status, "error");
        assert!(results[1].error_message.is_some());
    }

    #[tokio::test]
    async fn get_batch_results_returns_empty_for_no_results() {
        let pool = test_pool().await;
        create_batch_job(&pool, "batch_007", "key1", 0).await.unwrap();
        let results = get_batch_results(&pool, "batch_007").await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn batch_results_ordered_by_file_index() {
        let pool = test_pool().await;
        create_batch_job(&pool, "batch_008", "key1", 3).await.unwrap();

        // Insert out of order
        save_batch_result(&pool, "r2", "batch_008", 2, None, "success", None, None)
            .await
            .unwrap();
        save_batch_result(&pool, "r0", "batch_008", 0, None, "success", None, None)
            .await
            .unwrap();
        save_batch_result(&pool, "r1", "batch_008", 1, None, "success", None, None)
            .await
            .unwrap();

        let results = get_batch_results(&pool, "batch_008").await.unwrap();
        assert_eq!(results[0].file_index, 0);
        assert_eq!(results[1].file_index, 1);
        assert_eq!(results[2].file_index, 2);
    }
}
