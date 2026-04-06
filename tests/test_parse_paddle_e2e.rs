//! End-to-end test of POST /v1/parse with the live PaddleOCR sidecar wired in.
//!
//! Gated by `PADDLEOCR_URL` — skipped silently when unset so CI doesn't require
//! the sidecar. Run locally with:
//!
//!   PADDLEOCR_URL=http://127.0.0.1:8868 cargo test --test test_parse_paddle_e2e -- --nocapture

mod common;

use actix_web::{test, web, App};
use docforge::config::AppConfig;
use docforge::db;
use std::time::Instant;

fn build_multipart_pdf(pdf_bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----PaddleE2EBoundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"test.pdf\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: application/pdf\r\n\r\n");
    body.extend_from_slice(pdf_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (boundary.to_string(), body)
}

#[actix_rt::test]
async fn parse_endpoint_uses_live_paddle_sidecar() {
    let Ok(paddle_url) = std::env::var("PADDLEOCR_URL") else {
        eprintln!("PADDLEOCR_URL not set — skipping e2e parse test");
        return;
    };

    let config = AppConfig {
        host: "127.0.0.1".into(),
        port: 0,
        database_url: format!(
            "sqlite://file:test_paddle_e2e_{}?mode=memory&cache=shared",
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
        paddleocr_url: Some(paddle_url),
        paddleocr_timeout_secs: 120,
        paddleocr_mode: docforge::config::PaddleOcrMode::Primary,
            max_concurrent_parses: 8,
            parse_deadline_secs: 90,
    };
    let pool = db::init_db(&config.database_url).await.unwrap();
    let gov = docforge::middleware::rate_limit::build_governor(config.rate_limit_per_minute);
    let auth_gov = docforge::middleware::rate_limit::build_auth_governor();

    let app = test::init_service(
        App::new()
            .wrap(docforge::middleware::request_id::RequestIdMiddleware)
            .wrap(actix_governor::Governor::new(&gov))
            .app_data(web::Data::new(config))
            .app_data(web::Data::new(pool))
                .app_data(web::Data::new(docforge::services::parse_gate::ParseGate::new(8)))
            .app_data(web::Data::new(Instant::now()))
            .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
            .service(
                web::scope("/auth")
                    .wrap(actix_governor::Governor::new(&auth_gov))
                    .route(
                        "/register",
                        web::post().to(docforge::auth::handlers::register),
                    )
                    .route("/login", web::post().to(docforge::auth::handlers::login)),
            )
            .service(
                web::scope("/api-keys")
                    .route("", web::post().to(docforge::auth::handlers::create_key)),
            )
            .service(
                web::scope("/v1")
                    .route("/parse", web::post().to(docforge::api::parse::parse_pdf)),
            ),
    )
    .await;

    // Register
    let req = test::TestRequest::post()
        .uri("/auth/register")
        .set_json(serde_json::json!({"email":"paddle@test.com","password":"password123"}))
        .to_request();
    test::call_service(&app, req).await;

    // Login
    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(serde_json::json!({"email":"paddle@test.com","password":"password123"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    let jwt = body["access_token"].as_str().unwrap().to_string();

    // Create API key
    let req = test::TestRequest::post()
        .uri("/api-keys")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
        .set_json(serde_json::json!({"name":"PaddleE2E"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    let api_key = body["key"].as_str().unwrap().to_string();

    // POST /v1/parse with the sample PDF
    let (boundary, body) = build_multipart_pdf(&common::sample_pdf_bytes());
    let req = test::TestRequest::post()
        .uri("/v1/parse")
        .insert_header(("X-API-Key", api_key.as_str()))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "expected 200 OK from /v1/parse");

    let result: serde_json::Value = test::read_body_json(resp).await;
    assert!(
        result["document"]["text"].as_str().is_some(),
        "missing document.text"
    );
    let page_count = result["document"]["metadata"]["page_count"]
        .as_u64()
        .expect("missing page_count");
    assert!(page_count > 0, "expected at least one page");
    assert!(
        result["request_id"].as_str().unwrap().starts_with("req_"),
        "request_id should be prefixed"
    );
}
