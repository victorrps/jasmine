//! End-to-end test of POST /v1/parse in PaddleOcrMode::Auto — exercises the
//! classifier-driven routing path through the full HTTP handler.
//!
//! Gated by `PADDLEOCR_URL` — skipped silently when unset so CI doesn't
//! require the sidecar. Run locally with:
//!
//!   PADDLEOCR_URL=http://127.0.0.1:8868 cargo test --test test_parse_auto_e2e -- --nocapture

mod common;

use actix_web::{test, web, App};
use docforge::config::AppConfig;
use docforge::db;
use std::time::Instant;

const TABLE_PDF: &[u8] = include_bytes!("fixtures/table_document.pdf");
const LONG_ARTICLE_PDF: &[u8] = include_bytes!("fixtures/long_article.pdf");

fn build_multipart_pdf(pdf_bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----AutoE2EBoundary";
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
async fn auto_mode_routes_structured_doc_through_paddle_end_to_end() {
    let Ok(paddle_url) = std::env::var("PADDLEOCR_URL") else {
        eprintln!("PADDLEOCR_URL not set — skipping auto e2e");
        return;
    };

    let config = AppConfig {
        host: "127.0.0.1".into(),
        port: 0,
        database_url: format!(
            "sqlite://file:test_auto_e2e_{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        ),
        rate_limit_per_minute: 1000,
        api_key_pepper: "test_pepper_for_integration_tests_only!".into(),
        anthropic_api_key: None,
        stripe_secret_key: None,
        stripe_webhook_secret: None,
        tesseract_path: "tesseract".into(),
        pdftoppm_path: "pdftoppm".into(),
        paddleocr_url: Some(paddle_url),
        paddleocr_timeout_secs: 120,
        paddleocr_mode: docforge::config::PaddleOcrMode::Auto,
            max_concurrent_parses: 8,
            parse_deadline_secs: 90,
            extract_max_input_chars: 200_000,
            clerk_jwks_url: None,
            clerk_issuer: None,
            clerk_leeway_secs: 30,
            dev_auth_bypass: true,
            clerk_webhook_secret: None,
    };
    let pool = db::init_db(&config.database_url).await.unwrap();
    let clerk_id = format!("user_{}", uuid::Uuid::new_v4().simple());
    docforge::models::user::upsert_from_clerk(
        &pool, &clerk_id, "auto@test.com", Some("Test"), None,
    ).await.unwrap();
    let gov = docforge::middleware::rate_limit::build_governor(config.rate_limit_per_minute);

    let app = test::init_service(
        App::new()
            .wrap(docforge::middleware::request_id::RequestIdMiddleware)
            .wrap(actix_governor::Governor::new(&gov))
            .app_data(web::Data::new(config))
            .app_data(web::Data::new(pool))
                .app_data(web::Data::new(docforge::services::parse_gate::ParseGate::new(8)))
                .app_data(web::Data::new(docforge::services::metrics::Metrics::new()))
                .app_data(web::Data::new(docforge::services::idempotency::IdempotencyCache::with_defaults()))
            .app_data(web::Data::new(docforge::auth::clerk::ClerkConfig {
                jwks_url: String::new(),
                issuer: String::new(),
                leeway_secs: 30,
                dev_auth_bypass: true,
            }))
            .app_data(web::Data::new(
                docforge::auth::clerk::JwksCache::new(String::new()).unwrap(),
            ))
            .app_data(web::Data::new(Instant::now()))
            .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
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

    let req = test::TestRequest::post()
        .uri("/api-keys")
        .insert_header(("X-Dev-User-Id", clerk_id.as_str()))
        .set_json(serde_json::json!({"name":"AutoE2E"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    let api_key = body["key"].as_str().unwrap().to_string();

    // ── TextStructured → should route to paddle ──
    let (boundary, body) = build_multipart_pdf(TABLE_PDF);
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
    assert_eq!(resp.status(), 200);
    let result: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(
        result["document"]["metadata"]["routed_to"].as_str(),
        Some("paddle"),
        "structured doc should route to Paddle, got: {:#}",
        result["document"]["metadata"]
    );
    assert_eq!(
        result["document"]["metadata"]["classification"]["class"].as_str(),
        Some("text_structured")
    );

    // ── TextSimple → should skip Paddle, stay on pdf_oxide ──
    let (boundary, body) = build_multipart_pdf(LONG_ARTICLE_PDF);
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
    assert_eq!(resp.status(), 200);
    let result: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(
        result["document"]["metadata"]["routed_to"].as_str(),
        Some("pdf_oxide"),
        "long prose should stay on pdf_oxide, got: {:#}",
        result["document"]["metadata"]
    );
    assert_eq!(
        result["document"]["metadata"]["classification"]["class"].as_str(),
        Some("text_simple")
    );
}
