use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::api_key::generate_api_key;
use super::middleware::JwtAuth;
use crate::config::AppConfig;
use crate::errors::AppError;
use crate::models;

// --- Request / Response types ---

#[derive(Debug, Deserialize)]
pub struct CreateKeyRequest {
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateKeyResponse {
    pub id: String,
    pub key: String,
    pub prefix: String,
    pub name: String,
    pub created_at: String,
}

// --- Handlers ---

/// Create a new API key for the authenticated user.
// TODO(piece-6): swap JwtAuth for ClerkAuth.
#[tracing::instrument(skip(pool, auth, config))]
pub async fn create_key(
    auth: JwtAuth,
    pool: web::Data<SqlitePool>,
    config: web::Data<AppConfig>,
    body: web::Json<CreateKeyRequest>,
) -> Result<HttpResponse, AppError> {
    let name = body.name.as_deref().unwrap_or("Default");
    let id = uuid::Uuid::new_v4().to_string();
    let (plaintext, hash, prefix) = generate_api_key(&config.api_key_pepper);

    let key =
        models::api_key::create_api_key(&pool, &id, &auth.user_id, &hash, &prefix, name).await?;

    Ok(HttpResponse::Created().json(CreateKeyResponse {
        id: key.id,
        key: plaintext,
        prefix: key.key_prefix,
        name: key.name,
        created_at: key.created_at,
    }))
}

/// List all API keys for the authenticated user.
// TODO(piece-6): swap JwtAuth for ClerkAuth.
#[tracing::instrument(skip(pool, auth))]
pub async fn list_keys(
    auth: JwtAuth,
    pool: web::Data<SqlitePool>,
) -> Result<HttpResponse, AppError> {
    let keys = models::api_key::list_for_user(&pool, &auth.user_id).await?;
    Ok(HttpResponse::Ok().json(keys))
}

/// Revoke an API key by ID.
// TODO(piece-6): swap JwtAuth for ClerkAuth.
#[tracing::instrument(skip(pool, auth))]
pub async fn revoke_key(
    auth: JwtAuth,
    pool: web::Data<SqlitePool>,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let key_id = path.into_inner();
    let revoked = models::api_key::revoke(&pool, &key_id, &auth.user_id).await?;

    if revoked {
        Ok(HttpResponse::NoContent().finish())
    } else {
        Err(AppError::NotFound)
    }
}
