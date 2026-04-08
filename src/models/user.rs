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
// TODO(piece-5): drop allow once GET /me consumes this.
#[allow(dead_code)]
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
/// Matches on `clerk_user_id`. On insert we generate a fresh local
/// UUID for the primary key — `users.id` stays opaque to clients and
/// is the FK target everywhere else in the schema.
pub async fn upsert_from_clerk(
    pool: &SqlitePool,
    clerk_user_id: &str,
    email: &str,
    name: Option<&str>,
    image_url: Option<&str>,
) -> Result<User, AppError> {
    let updated = sqlx::query(
        "UPDATE users SET email = ?, name = ?, image_url = ?, updated_at = datetime('now') \
         WHERE clerk_user_id = ?",
    )
    .bind(email)
    .bind(name)
    .bind(image_url)
    .bind(clerk_user_id)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;

    if updated.rows_affected() > 0 {
        return sqlx::query_as::<_, User>("SELECT * FROM users WHERE clerk_user_id = ?")
            .bind(clerk_user_id)
            .fetch_one(pool)
            .await
            .map_err(AppError::Database);
    }

    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query_as::<_, User>(
        "INSERT INTO users (id, email, name, clerk_user_id, image_url) \
         VALUES (?, ?, ?, ?, ?) RETURNING *",
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
            AppError::Conflict("Email already registered to another Clerk user".into())
        }
        other => AppError::Database(other),
    })
}

/// Hard-delete a user by Clerk ID, cleaning up dependent rows.
///
/// `api_keys` has `ON DELETE CASCADE` on `user_id`, but `usage_logs`
/// references `api_keys(id)` without a cascade — so we delete usage
/// rows explicitly first, inside one transaction.
///
/// Returns the number of user rows deleted (0 if no match).
pub async fn hard_delete_by_clerk_id(
    pool: &SqlitePool,
    clerk_user_id: &str,
) -> Result<u64, AppError> {
    let mut tx = pool.begin().await.map_err(AppError::Database)?;

    sqlx::query(
        "DELETE FROM usage_logs WHERE api_key_id IN (\
             SELECT id FROM api_keys WHERE user_id IN (\
                 SELECT id FROM users WHERE clerk_user_id = ?\
             )\
         )",
    )
    .bind(clerk_user_id)
    .execute(&mut *tx)
    .await
    .map_err(AppError::Database)?;

    let deleted = sqlx::query("DELETE FROM users WHERE clerk_user_id = ?")
        .bind(clerk_user_id)
        .execute(&mut *tx)
        .await
        .map_err(AppError::Database)?;

    tx.commit().await.map_err(AppError::Database)?;
    Ok(deleted.rows_affected())
}
