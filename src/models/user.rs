use serde::Serialize;
use sqlx::SqlitePool;

use crate::errors::AppError;

/// A registered user account.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct User {
    pub id: String,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub name: Option<String>,
    pub created_at: String,
    pub updated_at: String,
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
