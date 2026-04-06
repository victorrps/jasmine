mod common;

use actix_web::{test, web, App};
use docforge::config::AppConfig;
use docforge::db;
use std::time::Instant;

macro_rules! test_app {
    () => {{
        let config = AppConfig {
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
        };
        let pool = db::init_db(&config.database_url).await.unwrap();
        let gov = docforge::middleware::rate_limit::build_governor(config.rate_limit_per_minute);
        let auth_gov = docforge::middleware::rate_limit::build_auth_governor();
        test::init_service(
            App::new()
                .wrap(docforge::middleware::request_id::RequestIdMiddleware)
                .wrap(actix_governor::Governor::new(&gov))
                .app_data(web::Data::new(config))
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(docforge::services::parse_gate::ParseGate::new(8)))
                .app_data(web::Data::new(Instant::now()))
                .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
                .route("/health", web::get().to(docforge::api::health::health))
                .service(
                    web::scope("/auth")
                        .wrap(actix_governor::Governor::new(&auth_gov))
                        .route(
                            "/register",
                            web::post().to(docforge::auth::handlers::register),
                        )
                        .route("/login", web::post().to(docforge::auth::handlers::login))
                        .route(
                            "/oauth/{provider}",
                            web::get().to(docforge::auth::handlers::oauth_redirect),
                        )
                        .route(
                            "/oauth/{provider}/callback",
                            web::get().to(docforge::auth::handlers::oauth_callback),
                        ),
                )
                .service(
                    web::scope("/api-keys")
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

/// Register a user and return the JWT access token.
macro_rules! register_and_login {
    ($app:expr, $email:expr, $password:expr) => {{
        let req = test::TestRequest::post()
            .uri("/auth/register")
            .set_json(serde_json::json!({
                "email": $email,
                "password": $password,
                "name": "Test"
            }))
            .to_request();
        test::call_service(&$app, req).await;

        let req = test::TestRequest::post()
            .uri("/auth/login")
            .set_json(serde_json::json!({
                "email": $email,
                "password": $password
            }))
            .to_request();
        let resp = test::call_service(&$app, req).await;
        let body: serde_json::Value = test::read_body_json(resp).await;
        body["access_token"].as_str().unwrap().to_string()
    }};
}

#[actix_rt::test]
async fn test_health_check() {
    let app = test_app!();
    let req = test::TestRequest::get().uri("/health").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["status"], "ok");
}

#[actix_rt::test]
async fn test_register_success() {
    let app = test_app!();
    let req = test::TestRequest::post()
        .uri("/auth/register")
        .set_json(serde_json::json!({"email":"a@b.com","password":"password123","name":"Test"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
}

#[actix_rt::test]
async fn test_register_duplicate() {
    let app = test_app!();
    let body = serde_json::json!({"email":"dup@test.com","password":"password123"});
    let req = test::TestRequest::post()
        .uri("/auth/register")
        .set_json(&body)
        .to_request();
    test::call_service(&app, req).await;
    let req = test::TestRequest::post()
        .uri("/auth/register")
        .set_json(&body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 409);
}

#[actix_rt::test]
async fn test_register_short_password() {
    let app = test_app!();
    let req = test::TestRequest::post()
        .uri("/auth/register")
        .set_json(serde_json::json!({"email":"s@b.com","password":"abc"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_rt::test]
async fn test_login_success() {
    let app = test_app!();
    let jwt = register_and_login!(app, "login@test.com", "password123");
    assert!(jwt.contains('.'));
}

#[actix_rt::test]
async fn test_login_wrong_password() {
    let app = test_app!();
    let req = test::TestRequest::post()
        .uri("/auth/register")
        .set_json(serde_json::json!({"email":"wp@test.com","password":"password123"}))
        .to_request();
    test::call_service(&app, req).await;

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(serde_json::json!({"email":"wp@test.com","password":"wrongpass1"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_rt::test]
async fn test_login_nonexistent() {
    let app = test_app!();
    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(serde_json::json!({"email":"nobody@test.com","password":"password123"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_rt::test]
async fn test_api_keys_require_jwt() {
    let app = test_app!();
    let req = test::TestRequest::get().uri("/api-keys").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_rt::test]
async fn test_create_list_revoke_api_key() {
    let app = test_app!();
    let jwt = register_and_login!(app, "keys@test.com", "password123");

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
    let key_id = body["id"].as_str().unwrap();
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
    let app = test_app!();
    let jwt = register_and_login!(app, "rne@test.com", "password123");
    let req = test::TestRequest::delete()
        .uri("/api-keys/fake-id")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_rt::test]
async fn test_oauth_placeholder() {
    let app = test_app!();
    let req = test::TestRequest::get()
        .uri("/auth/oauth/google")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 501);
}
