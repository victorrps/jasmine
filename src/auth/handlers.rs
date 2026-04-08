use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::api_key::generate_api_key;
use super::clerk::{ClerkAuth, ClerkConfig};
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

/// Resolve the authenticated Clerk user to their local `users.id` PK,
/// auto-provisioning in dev-bypass mode only.
async fn local_user_id(
    auth: &ClerkAuth,
    pool: &SqlitePool,
    clerk_cfg: &ClerkConfig,
) -> Result<String, AppError> {
    models::user::get_local_id_by_clerk_id(
        pool,
        &auth.clerk_user_id,
        clerk_cfg.dev_auto_provision(),
    )
    .await
}

// --- Handlers ---

/// Create a new API key for the authenticated user.
#[tracing::instrument(skip(auth, pool, config, clerk_cfg, body), fields(clerk_user_id = %auth.clerk_user_id))]
pub async fn create_key(
    auth: ClerkAuth,
    pool: web::Data<SqlitePool>,
    config: web::Data<AppConfig>,
    clerk_cfg: web::Data<ClerkConfig>,
    body: web::Json<CreateKeyRequest>,
) -> Result<HttpResponse, AppError> {
    let user_id = local_user_id(&auth, pool.get_ref(), clerk_cfg.get_ref()).await?;
    let name = body.name.as_deref().unwrap_or("Default");
    let id = uuid::Uuid::new_v4().to_string();
    let (plaintext, hash, prefix) = generate_api_key(&config.api_key_pepper);

    let key =
        models::api_key::create_api_key(&pool, &id, &user_id, &hash, &prefix, name).await?;

    Ok(HttpResponse::Created().json(CreateKeyResponse {
        id: key.id,
        key: plaintext,
        prefix: key.key_prefix,
        name: key.name,
        created_at: key.created_at,
    }))
}

/// List all API keys for the authenticated user.
#[tracing::instrument(skip(auth, pool, clerk_cfg), fields(clerk_user_id = %auth.clerk_user_id))]
pub async fn list_keys(
    auth: ClerkAuth,
    pool: web::Data<SqlitePool>,
    clerk_cfg: web::Data<ClerkConfig>,
) -> Result<HttpResponse, AppError> {
    let user_id = local_user_id(&auth, pool.get_ref(), clerk_cfg.get_ref()).await?;
    let keys = models::api_key::list_for_user(&pool, &user_id).await?;
    Ok(HttpResponse::Ok().json(keys))
}

/// Revoke an API key by ID.
#[tracing::instrument(skip(auth, pool, clerk_cfg), fields(clerk_user_id = %auth.clerk_user_id))]
pub async fn revoke_key(
    auth: ClerkAuth,
    pool: web::Data<SqlitePool>,
    clerk_cfg: web::Data<ClerkConfig>,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let user_id = local_user_id(&auth, pool.get_ref(), clerk_cfg.get_ref()).await?;
    let key_id = path.into_inner();
    let revoked = models::api_key::revoke(&pool, &key_id, &user_id).await?;

    if revoked {
        Ok(HttpResponse::NoContent().finish())
    } else {
        Err(AppError::NotFound)
    }
}
