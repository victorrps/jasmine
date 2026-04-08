mod common;

use actix_web::{test, web, App};
use docforge::auth::clerk::{ClerkConfig, JwksCache};
use docforge::config::AppConfig;
use docforge::db;
use sqlx::SqlitePool;
use std::time::Instant;

fn dev_clerk_config() -> ClerkConfig {
    ClerkConfig {
        jwks_url: String::new(),
        issuer: String::new(),
        leeway_secs: 30,
        dev_auth_bypass: true,
    }
}

/// Build a fresh AppConfig for an isolated in-memory test DB.
fn test_config() -> AppConfig {
    AppConfig {
        host: "127.0.0.1".into(),
        port: 0,
        database_url: format!(
            "sqlite://file:test_{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        ),
        rate_limit_per_minute: 1000,
        api_key_pepper: "test_pepper_for_integration_tests_only!".into(),
        anthropic_api_key: None,
        stripe_secret_key: None,
        stripe_webhook_secret: None,
        tesseract_path: "tesseract".into(),
        pdftoppm_path: "pdftoppm".into(),
        paddleocr_url: None,
        paddleocr_timeout_secs: 120,
        paddleocr_mode: docforge::config::PaddleOcrMode::Fallback,
        max_concurrent_parses: 8,
        parse_deadline_secs: 90,
        extract_max_input_chars: 200_000,
        clerk_jwks_url: None,
        clerk_issuer: None,
        clerk_leeway_secs: 30,
        dev_auth_bypass: true,
        clerk_webhook_secret: None,
    }
}

/// Seed a Clerk-mirrored user via the upsert path and return the
/// `clerk_user_id` string. Tests pass this in the `X-Dev-User-Id`
/// header to authenticate against the dev-bypass branch of `ClerkAuth`.
async fn seed_clerk_user(pool: &SqlitePool, email: &str) -> String {
    let clerk_id = format!("user_{}", uuid::Uuid::new_v4().simple());
    docforge::models::user::upsert_from_clerk(pool, &clerk_id, email, Some("Test"), None)
        .await
        .expect("seed user");
    clerk_id
}

macro_rules! build_app {
    ($config:expr, $pool:expr) => {{
        let gov = docforge::middleware::rate_limit::build_governor($config.rate_limit_per_minute);
        let auth_gov = docforge::middleware::rate_limit::build_auth_governor();
        test::init_service(
            App::new()
                .wrap(docforge::middleware::request_id::RequestIdMiddleware)
                .wrap(actix_governor::Governor::new(&gov))
                .app_data(web::Data::new($config))
                .app_data(web::Data::new($pool))
                .app_data(web::Data::new(docforge::services::parse_gate::ParseGate::new(8)))
                .app_data(web::Data::new(docforge::services::metrics::Metrics::new()))
                .app_data(web::Data::new(docforge::services::idempotency::IdempotencyCache::with_defaults()))
                .app_data(web::Data::new(dev_clerk_config()))
                .app_data(web::Data::new(JwksCache::new(String::new()).unwrap()))
                .app_data(web::Data::new(Instant::now()))
                .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
                .route("/health", web::get().to(docforge::api::health::health))
                .service(
                    web::scope("/api-keys")
                        .wrap(actix_governor::Governor::new(&auth_gov))
                        .route("", web::post().to(docforge::auth::handlers::create_key))
                        .route("", web::get().to(docforge::auth::handlers::list_keys))
                        .route(
                            "/{key_id}",
                            web::delete().to(docforge::auth::handlers::revoke_key),
                        ),
                )
                .service(
                    web::scope("/v1")
                        .route("/parse", web::post().to(docforge::api::parse::parse_pdf))
                        .route(
                            "/extract",
                            web::post().to(docforge::api::extract::extract_pdf),
                        ),
                ),
        )
        .await
    }};
}

#[actix_rt::test]
async fn test_health_check() {
    let config = test_config();
    let pool = db::init_db(&config.database_url).await.unwrap();
    let app = build_app!(config, pool);
    let req = test::TestRequest::get().uri("/health").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

#[actix_rt::test]
async fn test_api_keys_require_jwt() {
    let config = test_config();
    let pool = db::init_db(&config.database_url).await.unwrap();
    let app = build_app!(config, pool);
    let req = test::TestRequest::get().uri("/api-keys").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_rt::test]
async fn test_create_list_revoke_api_key() {
    let config = test_config();
    let pool = db::init_db(&config.database_url).await.unwrap();
    let clerk_id = seed_clerk_user(&pool, "keys@test.com").await;
    let app = build_app!(config, pool);

    // Create
    let req = test::TestRequest::post()
        .uri("/api-keys")
        .insert_header(("X-Dev-User-Id", clerk_id.as_str()))
        .set_json(serde_json::json!({"name":"Test Key"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = test::read_body_json(resp).await;
    let key = body["key"].as_str().unwrap();
    let key_id = body["id"].as_str().unwrap().to_string();
    assert!(key.starts_with("df_live_"));
    assert_eq!(key.len(), 40);

    // List
    let req = test::TestRequest::get()
        .uri("/api-keys")
        .insert_header(("X-Dev-User-Id", clerk_id.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Revoke
    let req = test::TestRequest::delete()
        .uri(&format!("/api-keys/{key_id}"))
        .insert_header(("X-Dev-User-Id", clerk_id.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);
}

#[actix_rt::test]
async fn test_revoke_nonexistent() {
    let config = test_config();
    let pool = db::init_db(&config.database_url).await.unwrap();
    let clerk_id = seed_clerk_user(&pool, "rne@test.com").await;
    let app = build_app!(config, pool);
    let req = test::TestRequest::delete()
        .uri("/api-keys/fake-id")
        .insert_header(("X-Dev-User-Id", clerk_id.as_str()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}
