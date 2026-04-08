mod common;

use actix_web::{test, web, App};
use docforge::config::AppConfig;
use docforge::db;
use sqlx::SqlitePool;
use std::time::Instant;

/// Build a fresh AppConfig for an isolated in-memory test DB.
fn test_config() -> AppConfig {
    AppConfig {
        host: "127.0.0.1".into(),
        port: 0,
        database_url: format!(
            "sqlite://file:test_{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        ),
        jwt_secret: "test_secret_at_least_32_chars_long_for_validation".into(),
        jwt_expiry_minutes: 15,
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
        dev_auth_bypass: false,
        clerk_webhook_secret: None,
    }
}

/// Seed a Clerk-mirrored user via the upsert path and mint a JWT
/// scoped to the local users.id PK. Used to drive tests against
/// JwtAuth-protected endpoints (api_keys) until piece-6 swaps them
/// to ClerkAuth — at that point this helper returns the dev-bypass
/// header instead.
async fn seed_user_and_jwt(pool: &SqlitePool, jwt_secret: &str, email: &str) -> (String, String) {
    let clerk_id = format!("user_{}", uuid::Uuid::new_v4().simple());
    let user = docforge::models::user::upsert_from_clerk(pool, &clerk_id, email, Some("Test"), None)
        .await
        .expect("seed user");
    let jwt = docforge::auth::jwt::create_token(&user.id, jwt_secret, 15).expect("mint jwt");
    (jwt, user.id)
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
    let (jwt, _) = seed_user_and_jwt(&pool, &config.jwt_secret, "keys@test.com").await;
    let app = build_app!(config, pool);

    // Create
    let req = test::TestRequest::post()
        .uri("/api-keys")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
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
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Revoke
    let req = test::TestRequest::delete()
        .uri(&format!("/api-keys/{key_id}"))
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);
}

#[actix_rt::test]
async fn test_revoke_nonexistent() {
    let config = test_config();
    let pool = db::init_db(&config.database_url).await.unwrap();
    let (jwt, _) = seed_user_and_jwt(&pool, &config.jwt_secret, "rne@test.com").await;
    let app = build_app!(config, pool);
    let req = test::TestRequest::delete()
        .uri("/api-keys/fake-id")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}
