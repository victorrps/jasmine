use actix_web::{dev::Payload, web, FromRequest, HttpRequest};
use std::future::{ready, Ready};

use super::jwt;
use crate::config::AppConfig;
use crate::errors::AppError;

/// Extractor that validates a JWT Bearer token and provides the user ID.
pub struct JwtAuth {
    pub user_id: String,
}

impl FromRequest for JwtAuth {
    type Error = AppError;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let result = extract_jwt(req);
        ready(result)
    }
}

fn extract_jwt(req: &HttpRequest) -> Result<JwtAuth, AppError> {
    let config = req
        .app_data::<web::Data<AppConfig>>()
        .ok_or_else(|| AppError::Internal("AppConfig not found".into()))?;

    let auth_header = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::InvalidToken)?;

    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or(AppError::InvalidToken)?;

    let token_data = jwt::validate_token(token, &config.jwt_secret)?;

    Ok(JwtAuth {
        user_id: token_data.claims.sub,
    })
}
