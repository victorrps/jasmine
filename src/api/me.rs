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
use serde::Serialize;
use sqlx::SqlitePool;

use crate::auth::clerk::{ClerkAuth, ClerkConfig};
use crate::errors::AppError;
use crate::models;

/// DTO for `GET /me`. Deliberately narrower than the full `User`
/// struct: we hide `stripe_subscription_id` and
/// `stripe_subscription_item_id` (Stripe's internal IDs, not useful
/// to a frontend) and only expose whether a subscription exists via
/// `has_active_subscription`. `stripe_customer_id` is exposed so the
/// frontend can correlate with Stripe Checkout redirects.
#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub id: String,
    pub clerk_user_id: String,
    pub email: String,
    pub name: Option<String>,
    pub image_url: Option<String>,
    pub tier: String,
    pub stripe_customer_id: Option<String>,
    pub has_active_subscription: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl From<models::user::User> for MeResponse {
    fn from(u: models::user::User) -> Self {
        let has_active_subscription = u.stripe_subscription_id.is_some();
        MeResponse {
            id: u.id,
            clerk_user_id: u.clerk_user_id,
            email: u.email,
            name: u.name,
            image_url: u.image_url,
            tier: u.tier,
            stripe_customer_id: u.stripe_customer_id,
            has_active_subscription,
            created_at: u.created_at,
            updated_at: u.updated_at,
        }
    }
}

/// Inner logic for `/me`, separated from the `ClerkAuth` extractor so
/// it can be unit-tested directly — the extractor requires a real JWT
/// or the dev-bypass header, neither of which fit a pure model-level
/// test for the "user missing in production mode" arm.
async fn resolve_me(
    clerk_user_id: &str,
    pool: &SqlitePool,
    clerk_cfg: &ClerkConfig,
) -> Result<MeResponse, AppError> {
    if let Some(user) = models::user::find_by_clerk_id(pool, clerk_user_id).await? {
        return Ok(user.into());
    }
    if clerk_cfg.dev_auto_provision() {
        let email = format!("{clerk_user_id}@dev.local");
        let user = models::user::upsert_from_clerk(pool, clerk_user_id, &email, None, None).await?;
        return Ok(user.into());
    }
    Err(AppError::NotFound)
}

/// `GET /me` — fetch (or in dev-bypass, lazily provision) the local user.
#[tracing::instrument(skip(auth, pool, clerk_cfg), fields(clerk_user_id = %auth.clerk_user_id))]
pub async fn get_me(
    auth: ClerkAuth,
    pool: web::Data<SqlitePool>,
    clerk_cfg: web::Data<ClerkConfig>,
) -> Result<HttpResponse, AppError> {
    let me = resolve_me(&auth.clerk_user_id, pool.get_ref(), clerk_cfg.get_ref()).await?;
    Ok(HttpResponse::Ok().json(me))
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
    async fn resolve_me_returns_not_found_in_prod_when_user_missing() {
        // Directly test the inner function — the extractor pathway is
        // unreachable in a unit test without a real JWT, but the
        // branch we care about (no row + no dev bypass → 404) lives
        // in `resolve_me`, not the extractor.
        let pool = fresh_pool().await;
        let cfg = prod_clerk_config();
        let err = resolve_me("user_missing_in_prod", &pool, &cfg)
            .await
            .expect_err("must 404 when row is missing in prod mode");
        assert!(matches!(err, AppError::NotFound));
    }

    #[actix_rt::test]
    async fn resolve_me_projects_user_into_me_response() {
        // Verify the DTO hides the internal Stripe subscription IDs.
        let pool = fresh_pool().await;
        models::user::upsert_from_clerk(
            &pool,
            "user_proj",
            "proj@test.com",
            Some("Proj"),
            Some("https://cdn/avatar.png"),
        )
        .await
        .unwrap();
        // Backfill subscription internals via raw SQL (no model helper
        // for it — these columns are Stripe-internal).
        sqlx::query(
            "UPDATE users SET stripe_subscription_id = ?, \
             stripe_subscription_item_id = ? WHERE clerk_user_id = ?",
        )
        .bind("sub_abc")
        .bind("si_abc")
        .bind("user_proj")
        .execute(&pool)
        .await
        .unwrap();

        let me = resolve_me("user_proj", &pool, &dev_clerk_config())
            .await
            .unwrap();
        assert_eq!(me.clerk_user_id, "user_proj");
        assert_eq!(me.email, "proj@test.com");
        assert_eq!(me.image_url.as_deref(), Some("https://cdn/avatar.png"));
        assert!(me.has_active_subscription);

        // Serialize and check that the DTO JSON does NOT carry the
        // raw subscription IDs — that's the whole point of the
        // projection.
        let json = serde_json::to_value(&me).unwrap();
        assert!(json.get("stripe_subscription_id").is_none());
        assert!(json.get("stripe_subscription_item_id").is_none());
        assert_eq!(json["has_active_subscription"], true);
    }
}
