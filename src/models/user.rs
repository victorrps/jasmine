use serde::Serialize;
use sqlx::SqlitePool;

use crate::errors::AppError;

/// A registered user account.
///
/// Note: `password_hash` is still NOT NULL in the schema (additive
/// migration 004 kept the column) but is an empty string for
/// Clerk-mirrored users. Piece-4 will drop the column entirely.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub name: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    // Billing columns from migration 003
    #[sqlx(default)]
    pub tier: Option<String>,
    #[sqlx(default)]
    pub stripe_customer_id: Option<String>,
    #[sqlx(default)]
    pub stripe_subscription_id: Option<String>,
    #[sqlx(default)]
    pub stripe_subscription_item_id: Option<String>,
    // Clerk columns from migration 004
    #[sqlx(default)]
    pub clerk_user_id: Option<String>,
    #[sqlx(default)]
    pub image_url: Option<String>,
}

/// Insert a new user.
pub async fn create_user(
    pool: &SqlitePool,
    id: &str,
    email: &str,
    password_hash: &str,
    name: Option<&str>,
) -> Result<User, AppError> {
    sqlx::query_as::<_, User>(
        "INSERT INTO users (id, email, password_hash, name) VALUES (?, ?, ?, ?) RETURNING *",
    )
    .bind(id)
    .bind(email)
    .bind(password_hash)
    .bind(name)
    .fetch_one(pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db_err) if db_err.message().contains("UNIQUE") => {
            AppError::Conflict("Email already registered".into())
        }
        other => AppError::Database(other),
    })
}

/// Find a user by email address.
pub async fn find_by_email(pool: &SqlitePool, email: &str) -> Result<Option<User>, AppError> {
    sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = ?")
        .bind(email)
        .fetch_optional(pool)
        .await
        .map_err(AppError::Database)
}

/// Find a user by Clerk user ID.
// TODO(piece-5): remove allow once GET /me consumes this.
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
/// Matches on `clerk_user_id`. On insert we generate a new UUID for
/// the primary key and set `password_hash` to an empty string — the
/// column is still NOT NULL in the additive migration (004), and
/// piece-4 (legacy auth removal) will drop it entirely. Using `""`
/// rather than a hash of a random value makes it obvious in the DB
/// that the account is Clerk-managed and cannot be logged into via
/// the legacy password flow.
pub async fn upsert_from_clerk(
    pool: &SqlitePool,
    clerk_user_id: &str,
    email: &str,
    name: Option<&str>,
    image_url: Option<&str>,
) -> Result<User, AppError> {
    // Try update first; if no row affected, insert. This matches the
    // SQLite idiom without relying on ON CONFLICT, which requires a
    // unique constraint on the conflict column — `clerk_user_id` is a
    // partial unique index (WHERE NOT NULL), and SQLite's conflict
    // resolution on partial indexes is version-dependent.
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
        "INSERT INTO users (id, email, password_hash, name, clerk_user_id, image_url) \
         VALUES (?, ?, '', ?, ?, ?) RETURNING *",
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
            // Email collision with a legacy password-auth row — we
            // can't silently link accounts without explicit consent,
            // so surface a conflict the webhook handler can log.
            AppError::Conflict("Email already registered to a non-Clerk user".into())
        }
        other => AppError::Database(other),
    })
}

/// Hard-delete a user by Clerk ID, cleaning up dependent rows.
///
/// `api_keys` has `ON DELETE CASCADE` on `user_id` so it clears
/// itself. `usage_logs` references `api_keys(id)` without a cascade
/// (and usage rows survive API-key rotation by design), so we delete
/// them explicitly via a JOIN on the user's keys. Everything runs in
/// one transaction so a partial failure rolls back cleanly.
///
/// Returns the number of user rows deleted (0 if no match).
pub async fn hard_delete_by_clerk_id(
    pool: &SqlitePool,
    clerk_user_id: &str,
) -> Result<u64, AppError> {
    let mut tx = pool.begin().await.map_err(AppError::Database)?;

    // Delete usage_logs for all of this user's api_keys. We do this
    // before the users DELETE so the CASCADE on api_keys has not yet
    // removed the rows we need to join against.
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
