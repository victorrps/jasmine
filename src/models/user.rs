use serde::Serialize;
use sqlx::SqlitePool;

use crate::errors::AppError;

/// A registered user account. Identity is owned by Clerk; the local
/// row is a mirror keyed by `clerk_user_id`.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    // Billing columns from migration 003
    pub tier: String,
    pub stripe_customer_id: Option<String>,
    pub stripe_subscription_id: Option<String>,
    pub stripe_subscription_item_id: Option<String>,
    // Clerk columns from migrations 004 + 005 (clerk_user_id now NOT NULL)
    pub clerk_user_id: String,
    pub image_url: Option<String>,
}

/// Find a user by Clerk user ID.
pub async fn find_by_clerk_id(
    pool: &SqlitePool,
    clerk_user_id: &str,
) -> Result<Option<User>, AppError> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE clerk_user_id = ?")
        .bind(clerk_user_id)
        .fetch_optional(pool)
        .await
        .map_err(AppError::Database)
}

/// Upsert a user from a Clerk webhook payload.
///
/// Matches on `clerk_user_id`. Uses a single `INSERT ... ON CONFLICT
/// DO UPDATE` statement so concurrent webhooks for the same Clerk user
/// can't race past a `find` → `insert` window and collide on the
/// unique constraint. On insert we generate a fresh local UUID for
/// the primary key — `users.id` stays opaque to clients and is the FK
/// target everywhere else in the schema.
///
/// An email collision with a *different* Clerk user still surfaces as
/// `AppError::Conflict`; the webhook handler logs and short-circuits
/// that case (we can't silently link two Clerk identities to one local
/// row without explicit consent).
pub async fn upsert_from_clerk(
    pool: &SqlitePool,
    clerk_user_id: &str,
    email: &str,
    name: Option<&str>,
    image_url: Option<&str>,
) -> Result<User, AppError> {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query_as::<_, User>(
        "INSERT INTO users (id, email, name, clerk_user_id, image_url) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(clerk_user_id) DO UPDATE SET \
             email = excluded.email, \
             name = excluded.name, \
             image_url = excluded.image_url, \
             updated_at = datetime('now') \
         RETURNING *",
    )
    .bind(&id)
    .bind(email)
    .bind(name)
    .bind(clerk_user_id)
    .bind(image_url)
    .fetch_one(pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db_err) if db_err.message().contains("UNIQUE") => {
            // The `clerk_user_id` collision path is handled by the
            // ON CONFLICT clause — anything that reaches here is an
            // email-uniqueness violation from a *different* Clerk user
            // trying to claim the same email address.
            AppError::Conflict("Email already registered to another Clerk user".into())
        }
        other => AppError::Database(other),
    })
}

/// Resolve a Clerk user ID to the local `users.id` PK, optionally
/// auto-provisioning a stub row in dev-bypass mode.
///
/// `dev_auto_provision` MUST only be `true` when the deployment is in
/// dev-bypass mode (`DEV_AUTH_BYPASS=true && CLERK_JWKS_URL` unset).
/// Callers compute that boolean from `ClerkConfig` so the invariant
/// stays in one place.
pub async fn get_local_id_by_clerk_id(
    pool: &SqlitePool,
    clerk_user_id: &str,
    dev_auto_provision: bool,
) -> Result<String, AppError> {
    if let Some(user) = find_by_clerk_id(pool, clerk_user_id).await? {
        return Ok(user.id);
    }
    if dev_auto_provision {
        let email = format!("{clerk_user_id}@dev.local");
        let user = upsert_from_clerk(pool, clerk_user_id, &email, None, None).await?;
        return Ok(user.id);
    }
    Err(AppError::NotFound)
}

/// Persist a Stripe customer ID against the local user row. Errors
/// with `AppError::NotFound` when the `user_id` doesn't match any
/// row — the silent no-op would mask callsite bugs where a stale PK
/// slipped through (e.g. a webhook-driven hard delete racing a
/// Checkout Session create).
pub async fn set_stripe_customer_id(
    pool: &SqlitePool,
    user_id: &str,
    customer_id: &str,
) -> Result<(), AppError> {
    let result = sqlx::query(
        "UPDATE users SET stripe_customer_id = ?, updated_at = datetime('now') WHERE id = ?",
    )
    .bind(customer_id)
    .bind(user_id)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Hard-delete a user by Clerk ID, cleaning up dependent rows.
///
/// FK cascade coverage in the schema is incomplete:
/// - `api_keys.user_id`      → `users(id)` **ON DELETE CASCADE** ✓
/// - `usage_logs.api_key_id` → `api_keys(id)` (no cascade)
/// - `batch_jobs.api_key_id` → `api_keys(id)` (no cascade)
/// - `batch_results.batch_job_id` → `batch_jobs(id)` **ON DELETE CASCADE** ✓
///
/// So a raw `DELETE FROM users` would either fail on the `batch_jobs`/
/// `usage_logs` FK (with `PRAGMA foreign_keys = ON`) or leave
/// dangling rows (with it off). We delete children explicitly in
/// dependency order inside a single transaction so a partial failure
/// rolls back cleanly.
///
/// Returns the number of user rows deleted (0 if no match).
pub async fn hard_delete_by_clerk_id(
    pool: &SqlitePool,
    clerk_user_id: &str,
) -> Result<u64, AppError> {
    // Subquery resolving to the set of api_key IDs owned by the user
    // being deleted. Inlined into each child delete.
    const USER_KEY_IDS: &str = "SELECT id FROM api_keys WHERE user_id IN (\
        SELECT id FROM users WHERE clerk_user_id = ?\
     )";

    let mut tx = pool.begin().await.map_err(AppError::Database)?;

    // 1. usage_logs → api_keys (no cascade).
    sqlx::query(&format!(
        "DELETE FROM usage_logs WHERE api_key_id IN ({USER_KEY_IDS})"
    ))
    .bind(clerk_user_id)
    .execute(&mut *tx)
    .await
    .map_err(AppError::Database)?;

    // 2. batch_jobs → api_keys (no cascade). `batch_results` cascades
    //    from batch_jobs, so it clears automatically once the jobs go.
    sqlx::query(&format!(
        "DELETE FROM batch_jobs WHERE api_key_id IN ({USER_KEY_IDS})"
    ))
    .bind(clerk_user_id)
    .execute(&mut *tx)
    .await
    .map_err(AppError::Database)?;

    // 3. users → cascades to api_keys via the user_id FK.
    let deleted = sqlx::query("DELETE FROM users WHERE clerk_user_id = ?")
        .bind(clerk_user_id)
        .execute(&mut *tx)
        .await
        .map_err(AppError::Database)?;

    tx.commit().await.map_err(AppError::Database)?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_pool() -> SqlitePool {
        let url = format!(
            "sqlite://file:user_model_test_{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        crate::db::init_db(&url).await.unwrap()
    }

    #[tokio::test]
    async fn upsert_from_clerk_is_atomic_on_conflict() {
        // The ON CONFLICT DO UPDATE path must return the EXISTING
        // row's local id — we must not flip the PK on updates, since
        // api_keys.user_id FKs to it.
        let pool = fresh_pool().await;
        let a =
            upsert_from_clerk(&pool, "user_atomic", "a@x.com", Some("A"), None).await.unwrap();
        let b =
            upsert_from_clerk(&pool, "user_atomic", "b@x.com", Some("B"), None).await.unwrap();
        assert_eq!(a.id, b.id, "PK must be stable across upserts");
        assert_eq!(b.email, "b@x.com");
        assert_eq!(b.name.as_deref(), Some("B"));
    }

    #[tokio::test]
    async fn upsert_from_clerk_rejects_email_collision_with_different_clerk_id() {
        let pool = fresh_pool().await;
        upsert_from_clerk(&pool, "user_one", "shared@x.com", None, None)
            .await
            .unwrap();
        let err = upsert_from_clerk(&pool, "user_two", "shared@x.com", None, None)
            .await
            .expect_err("email collision must surface");
        assert!(matches!(err, AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn hard_delete_removes_user_api_keys_usage_logs_and_batch_jobs() {
        // C1 regression: prior implementation only cleaned usage_logs
        // and relied on the users cascade for api_keys. batch_jobs
        // has a non-cascading FK on api_keys.id, so deleting a user
        // with live batch_jobs would fail the transaction.
        let pool = fresh_pool().await;
        let user =
            upsert_from_clerk(&pool, "user_del", "del@x.com", None, None).await.unwrap();

        // Seed an api key, a usage_log, a batch_job, and a batch_result.
        let key_id = "key_del".to_string();
        sqlx::query(
            "INSERT INTO api_keys (id, user_id, key_hash, key_prefix, name) \
             VALUES (?, ?, 'hash', 'df_live_x', 'test')",
        )
        .bind(&key_id)
        .bind(&user.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO usage_logs (id, api_key_id, endpoint, pages_processed, \
             credits_used, processing_ms, request_id) \
             VALUES ('ul1', ?, '/v1/parse', 1, 1, 10, 'req_x')",
        )
        .bind(&key_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO batch_jobs (id, api_key_id) VALUES ('bj1', ?)",
        )
        .bind(&key_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO batch_results (id, batch_job_id, file_index) \
             VALUES ('br1', 'bj1', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let removed = hard_delete_by_clerk_id(&pool, "user_del").await.unwrap();
        assert_eq!(removed, 1);

        // All dependent rows gone.
        let (n_users,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM users WHERE clerk_user_id = 'user_del'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (n_keys,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM api_keys WHERE id = 'key_del'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (n_logs,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM usage_logs WHERE id = 'ul1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (n_jobs,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM batch_jobs WHERE id = 'bj1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (n_results,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM batch_results WHERE id = 'br1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n_users, 0, "users row must be removed");
        assert_eq!(n_keys, 0, "api_keys row must be removed via cascade");
        assert_eq!(n_logs, 0, "usage_logs row must be removed explicitly");
        assert_eq!(n_jobs, 0, "batch_jobs row must be removed explicitly");
        assert_eq!(n_results, 0, "batch_results cascades from batch_jobs");
    }

    #[tokio::test]
    async fn set_stripe_customer_id_errors_on_missing_row() {
        let pool = fresh_pool().await;
        let err = set_stripe_customer_id(&pool, "nope", "cus_x").await;
        assert!(matches!(err, Err(AppError::NotFound)));
    }
}
