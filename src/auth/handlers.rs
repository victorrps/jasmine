use actix_web::{web, HttpResponse};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use validator::Validate;

use super::api_key::generate_api_key;
use super::jwt;
use super::middleware::JwtAuth;
use crate::config::AppConfig;
use crate::errors::AppError;
use crate::models;

// --- Request / Response types ---

#[derive(Debug, Deserialize, Validate)]
pub struct RegisterRequest {
    #[validate(email(message = "Invalid email format"))]
    pub email: String,
    #[validate(length(min = 8, message = "Password must be at least 8 characters"))]
    pub password: String,
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Deserialize, Validate)]
pub struct LoginRequest {
    #[validate(email)]
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: u64,
}

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

/// Register a new user account.
#[tracing::instrument(skip(pool, body), fields(email = %body.email))]
pub async fn register(
    pool: web::Data<SqlitePool>,
    body: web::Json<RegisterRequest>,
) -> Result<HttpResponse, AppError> {
    body.validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let id = uuid::Uuid::new_v4().to_string();
    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(body.password.as_bytes(), &salt)
        .map_err(|e| AppError::Internal(format!("Password hashing failed: {e}")))?
        .to_string();

    let user = models::user::create_user(
        &pool,
        &id,
        &body.email,
        &password_hash,
        body.name.as_deref(),
    )
    .await?;

    Ok(HttpResponse::Created().json(RegisterResponse {
        id: user.id,
        email: user.email,
        name: user.name,
        created_at: user.created_at,
    }))
}

/// Login with email and password, receive a JWT.
///
/// Constant-time: always runs Argon2 verify even for non-existent users
/// to prevent email enumeration via timing side-channel.
#[tracing::instrument(skip(pool, config, body), fields(email = %body.email))]
pub async fn login(
    pool: web::Data<SqlitePool>,
    config: web::Data<AppConfig>,
    body: web::Json<LoginRequest>,
) -> Result<HttpResponse, AppError> {
    // Dummy hash used when user doesn't exist — ensures constant-time Argon2 execution
    const DUMMY_HASH: &str =
        "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE";

    body.validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let user = models::user::find_by_email(&pool, &body.email).await?;

    // Use real hash if user found, dummy hash otherwise
    let hash_str = user
        .as_ref()
        .map(|u| u.password_hash.as_str())
        .unwrap_or(DUMMY_HASH);

    let parsed_hash = PasswordHash::new(hash_str)
        .map_err(|_| AppError::Internal("Stored hash is invalid".into()))?;

    let verify_result = Argon2::default().verify_password(body.password.as_bytes(), &parsed_hash);

    // Only succeed if user exists AND password matches
    if user.is_none() || verify_result.is_err() {
        return Err(AppError::InvalidCredentials);
    }

    let user = user.expect("checked above");
    let token = jwt::create_token(&user.id, &config.jwt_secret, config.jwt_expiry_minutes)?;

    Ok(HttpResponse::Ok().json(LoginResponse {
        access_token: token,
        token_type: "bearer".into(),
        expires_in: config.jwt_expiry_minutes * 60,
    }))
}

/// Create a new API key for the authenticated user.
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
#[tracing::instrument(skip(pool, auth))]
pub async fn list_keys(
    auth: JwtAuth,
    pool: web::Data<SqlitePool>,
) -> Result<HttpResponse, AppError> {
    let keys = models::api_key::list_for_user(&pool, &auth.user_id).await?;
    Ok(HttpResponse::Ok().json(keys))
}

/// Revoke an API key by ID.
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

/// OAuth redirect placeholder — returns 501.
///
/// # Future Implementation
/// This handler should:
/// 1. Generate a PKCE code_verifier and code_challenge
/// 2. Store code_verifier in a session or encrypted cookie
/// 3. Redirect to the provider's authorization URL with:
///    - client_id, redirect_uri, scope, state, code_challenge
/// 4. Supported providers: google, github
pub async fn oauth_redirect(path: web::Path<String>) -> Result<HttpResponse, AppError> {
    let provider = path.into_inner();
    if provider != "google" && provider != "github" {
        return Err(AppError::NotFound);
    }
    Err(AppError::NotImplemented(format!(
        "OAuth with {provider} coming soon. Use email/password for the POC."
    )))
}

/// OAuth callback placeholder — returns 501.
///
/// # Future Implementation
/// This handler should:
/// 1. Validate the state parameter matches the session
/// 2. Exchange the authorization code for tokens using the code_verifier (PKCE)
/// 3. Fetch user profile from the provider
/// 4. Create or link the user account
/// 5. Issue a JWT and redirect to the frontend
pub async fn oauth_callback(path: web::Path<String>) -> Result<HttpResponse, AppError> {
    let provider = path.into_inner();
    if provider != "google" && provider != "github" {
        return Err(AppError::NotFound);
    }
    Err(AppError::NotImplemented(format!(
        "OAuth callback for {provider} coming soon. Use email/password for the POC."
    )))
}
