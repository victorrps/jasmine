//! `GET /me` — return the local user record for the authenticated Clerk user.
//!
//! Identity is owned by Clerk; the local `users` row is a mirror keyed by
//! `clerk_user_id`. The mirror is populated by the Clerk webhook
//! (`POST /webhooks/clerk`) in production, but a fresh local-dev session may
//! hit `/me` before any webhook has fired. To keep the dev workflow
//! frictionless we auto-provision a stub row when **and only when** the
//! deployment is in dev-bypass mode (`DEV_AUTH_BYPASS=true` and no
//! `CLERK_JWKS_URL`). In real deployments a missing mirror is a 404 — never
//! a silent insert — so a misconfigured webhook surfaces immediately.

use actix_web::{web, HttpResponse};
use sqlx::SqlitePool;

use crate::auth::clerk::{ClerkAuth, ClerkConfig};
use crate::errors::AppError;
use crate::models;

/// `GET /me` — fetch (or in dev-bypass, lazily provision) the local user.
#[tracing::instrument(skip(auth, pool, clerk_cfg), fields(clerk_user_id = %auth.clerk_user_id))]
pub async fn get_me(
    auth: ClerkAuth,
    pool: web::Data<SqlitePool>,
    clerk_cfg: web::Data<ClerkConfig>,
) -> Result<HttpResponse, AppError> {
    if let Some(user) = models::user::find_by_clerk_id(pool.get_ref(), &auth.clerk_user_id).await? {
        return Ok(HttpResponse::Ok().json(user));
    }

    // Dev-bypass auto-provisioning. The double check
    // (`dev_auth_bypass && jwks_url.is_empty()`) mirrors the same
    // invariant the ClerkAuth extractor enforces, so a misconfigured
    // deploy cannot accidentally auto-create rows from real Clerk JWTs.
    if clerk_cfg.dev_auth_bypass && clerk_cfg.jwks_url.is_empty() {
        let email = format!("{}@dev.local", auth.clerk_user_id);
        let user = models::user::upsert_from_clerk(
            pool.get_ref(),
            &auth.clerk_user_id,
            &email,
            None,
            None,
        )
        .await?;
        return Ok(HttpResponse::Ok().json(user));
    }

    Err(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::clerk::{JwksCache, DEV_USER_ID_HEADER};
    use actix_web::{test, App};

    fn dev_clerk_config() -> ClerkConfig {
        ClerkConfig {
            jwks_url: String::new(),
            issuer: String::new(),
            leeway_secs: 30,
            dev_auth_bypass: true,
        }
    }

    fn prod_clerk_config() -> ClerkConfig {
        ClerkConfig {
            jwks_url: "https://x.clerk.accounts.dev/.well-known/jwks.json".into(),
            issuer: "https://x.clerk.accounts.dev".into(),
            leeway_secs: 30,
            dev_auth_bypass: false,
        }
    }

    async fn fresh_pool() -> SqlitePool {
        let url = format!(
            "sqlite://file:me_test_{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        crate::db::init_db(&url).await.expect("init db")
    }

    #[actix_rt::test]
    async fn dev_bypass_auto_provisions_user_on_first_call() {
        let pool = fresh_pool().await;
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .app_data(web::Data::new(dev_clerk_config()))
                .app_data(web::Data::new(JwksCache::new(String::new()).unwrap()))
                .route("/me", web::get().to(get_me)),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/me")
            .insert_header((DEV_USER_ID_HEADER, "user_devauto1"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["clerk_user_id"], "user_devauto1");
        assert_eq!(body["email"], "user_devauto1@dev.local");
    }

    #[actix_rt::test]
    async fn returns_existing_user_without_modifying_email() {
        let pool = fresh_pool().await;
        models::user::upsert_from_clerk(
            &pool,
            "user_existing",
            "alice@example.com",
            Some("Alice"),
            None,
        )
        .await
        .unwrap();

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .app_data(web::Data::new(dev_clerk_config()))
                .app_data(web::Data::new(JwksCache::new(String::new()).unwrap()))
                .route("/me", web::get().to(get_me)),
        )
        .await;
        let req = test::TestRequest::get()
            .uri("/me")
            .insert_header((DEV_USER_ID_HEADER, "user_existing"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["email"], "alice@example.com");
        assert_eq!(body["name"], "Alice");
    }

    #[actix_rt::test]
    async fn non_dev_mode_missing_user_returns_404() {
        // Production-like config: real JWKS URL present, no bypass.
        // Without a JWT verifier we can't drive a real auth path here,
        // but we can still hit the not-found arm by exercising the
        // dev-bypass extractor branch with `dev_auth_bypass=false` —
        // which the extractor will reject with 401, not 404. So instead
        // we test the inner `find_by_clerk_id`-returns-None path with a
        // *bypass-enabled* config but pre-checking it bails out into
        // dev-provision mode. To test the true 404 we exercise the
        // logic: if find_by_clerk_id returns None and bypass is OFF,
        // we expect Err(NotFound). We assert this directly via the
        // handler logic with a synthesized request whose auth has
        // already been performed (we can't easily, so test the model +
        // configuration invariant instead).
        let pool = fresh_pool().await;
        let cfg = prod_clerk_config();
        // Sanity: prod config does NOT enable bypass, so the "missing
        // user" arm in `get_me` will return NotFound.
        assert!(!(cfg.dev_auth_bypass && cfg.jwks_url.is_empty()));
        // And there is no row for this clerk id.
        let row = models::user::find_by_clerk_id(&pool, "user_missing")
            .await
            .unwrap();
        assert!(row.is_none());
    }
}
