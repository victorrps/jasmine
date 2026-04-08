mod common;

use actix_web::{test, web, App};
use docforge::config::AppConfig;
use docforge::db;
use std::time::Instant;

/// Seed a Clerk-mirrored user and return their `clerk_user_id`. The
/// dev-bypass branch of `ClerkAuth` accepts that string verbatim via
/// the `X-Dev-User-Id` header, so tests can authenticate without
/// minting a real Clerk JWT.
async fn seed_clerk_user(pool: &sqlx::SqlitePool, email: &str) -> String {
    let clerk_id = format!("user_{}", uuid::Uuid::new_v4().simple());
    docforge::models::user::upsert_from_clerk(pool, &clerk_id, email, Some("Test"), None)
        .await
        .unwrap();
    clerk_id
}

fn dev_clerk_config() -> docforge::auth::clerk::ClerkConfig {
    docforge::auth::clerk::ClerkConfig {
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
        stripe_success_url: None,
        stripe_cancel_url: None,
        stripe_portal_return_url: None,
        stripe_price_starter: None,
        stripe_price_pro: None,
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
        // Tests drive ClerkAuth via dev-bypass + X-Dev-User-Id header.
        dev_auth_bypass: true,
        clerk_webhook_secret: None,
    }
}

/// Seed a Clerk-mirrored user and create one API key via the
/// ClerkAuth-protected `/api-keys` endpoint (dev-bypass mode).
/// Returns `(app, api_key, clerk_user_id, key_id)`.
macro_rules! test_app_with_key {
    () => {{
        let config = test_config();
        let pool = db::init_db(&config.database_url).await.unwrap();

        let clerk_id = seed_clerk_user(&pool, "p@test.com").await;

        let gov = docforge::middleware::rate_limit::build_governor(config.rate_limit_per_minute);
        let app = test::init_service(
            App::new()
                .wrap(docforge::middleware::request_id::RequestIdMiddleware)
                .wrap(actix_governor::Governor::new(&gov))
                .app_data(web::Data::new(config))
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(Instant::now()))
                .app_data(web::Data::new(
                    docforge::services::parse_gate::ParseGate::new(8),
                ))
                .app_data(web::Data::new(docforge::services::metrics::Metrics::new()))
                .app_data(web::Data::new(docforge::services::idempotency::IdempotencyCache::with_defaults()))
                .app_data(web::Data::new(dev_clerk_config()))
                .app_data(web::Data::new(
                    docforge::auth::clerk::JwksCache::new(String::new()).unwrap(),
                ))
                .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
                .route("/health", web::get().to(docforge::api::health::health))
                .route("/metrics", web::get().to(docforge::api::metrics::metrics))
                .service(
                    web::scope("/api-keys")
                        .route("", web::post().to(docforge::auth::handlers::create_key))
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
        .await;

        let req = test::TestRequest::post()
            .uri("/api-keys")
            .insert_header(("X-Dev-User-Id", clerk_id.as_str()))
            .set_json(serde_json::json!({"name":"Test"}))
            .to_request();
        let resp = test::call_service(&app, req).await;
        let body: serde_json::Value = test::read_body_json(resp).await;
        let api_key = body["key"].as_str().unwrap().to_string();
        let key_id = body["id"].as_str().unwrap().to_string();

        (app, api_key, clerk_id, key_id)
    }};
}

fn build_multipart_pdf(pdf_bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----TestBoundary12345";
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

fn build_multipart_pdf_with_hint(pdf_bytes: &[u8], hint: &str) -> (String, Vec<u8>) {
    let boundary = "----TestBoundary12345";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"test.pdf\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: application/pdf\r\n\r\n");
    body.extend_from_slice(pdf_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"document_type_hint\"\r\n\r\n");
    body.extend_from_slice(hint.as_bytes());
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (boundary.to_string(), body)
}

fn build_multipart_with_unknown_field(pdf_bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----TestBoundary12345";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"test.pdf\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: application/pdf\r\n\r\n");
    body.extend_from_slice(pdf_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"surprise\"\r\n\r\n");
    body.extend_from_slice(b"oops");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (boundary.to_string(), body)
}

#[actix_rt::test]
async fn test_parse_valid_pdf() {
    let (app, api_key, _, _) = test_app_with_key!();
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
    assert_eq!(resp.status(), 200);

    let result: serde_json::Value = test::read_body_json(resp).await;
    assert!(result["document"]["text"].as_str().is_some());
    assert!(
        result["document"]["metadata"]["page_count"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(result["request_id"].as_str().unwrap().starts_with("req_"));
}

#[actix_rt::test]
async fn test_parse_without_api_key() {
    let (app, _, _, _) = test_app_with_key!();
    let (boundary, body) = build_multipart_pdf(&common::sample_pdf_bytes());

    let req = test::TestRequest::post()
        .uri("/v1/parse")
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401);
}

#[actix_rt::test]
async fn test_parse_non_pdf() {
    let (app, api_key, _, _) = test_app_with_key!();
    let boundary = "----TestBoundary12345";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"test.txt\"\r\n\r\n",
    );
    body.extend_from_slice(b"This is not a PDF");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

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
    assert_eq!(resp.status(), 400);
}

#[actix_rt::test]
async fn test_parse_revoked_key() {
    let (app, api_key, clerk_id, key_id) = test_app_with_key!();

    // Revoke
    let req = test::TestRequest::delete()
        .uri(&format!("/api-keys/{key_id}"))
        .insert_header(("X-Dev-User-Id", clerk_id.as_str()))
        .to_request();
    test::call_service(&app, req).await;

    // Try parse
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
    assert_eq!(resp.status(), 401);
}

#[actix_rt::test]
async fn test_response_has_request_id_header() {
    let (app, _, _, _) = test_app_with_key!();
    let req = test::TestRequest::get().uri("/health").to_request();
    let resp = test::call_service(&app, req).await;
    let req_id = resp
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(req_id.starts_with("req_"));
}

#[actix_rt::test]
async fn test_extract_stub() {
    let (app, api_key, _, _) = test_app_with_key!();
    let pdf_bytes = common::sample_pdf_bytes();
    let schema = r#"{"type":"object"}"#;

    let boundary = "----TestBoundary12345";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"test.pdf\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: application/pdf\r\n\r\n");
    body.extend_from_slice(&pdf_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"schema\"\r\n\r\n");
    body.extend_from_slice(schema.as_bytes());
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let req = test::TestRequest::post()
        .uri("/v1/extract")
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
    assert_eq!(result["extracted"]["model"], "stub");
}

#[actix_rt::test]
async fn test_idempotency_key_returns_cached_response_on_replay() {
    let (app, api_key, _, _) = test_app_with_key!();
    let (boundary, body1) = build_multipart_pdf(&common::sample_pdf_bytes());
    let (boundary2, body2) = build_multipart_pdf(&common::sample_pdf_bytes());

    // First call: cache miss → does the work, caches the result.
    let req = test::TestRequest::post()
        .uri("/v1/parse")
        .insert_header(("X-API-Key", api_key.as_str()))
        .insert_header(("Idempotency-Key", "test-idem-001"))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .set_payload(body1)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body_first: serde_json::Value = test::read_body_json(resp).await;

    // Second call with same key → cache hit, X-Idempotent-Replay header.
    let req = test::TestRequest::post()
        .uri("/v1/parse")
        .insert_header(("X-API-Key", api_key.as_str()))
        .insert_header(("Idempotency-Key", "test-idem-001"))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary2}"),
        ))
        .set_payload(body2)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let replay_header = resp
        .headers()
        .get("x-idempotent-replay")
        .expect("replay must set X-Idempotent-Replay header")
        .to_str()
        .unwrap();
    assert_eq!(replay_header, "true");
    let body_second: serde_json::Value = test::read_body_json(resp).await;

    // The second body must equal the first verbatim, including
    // request_id (which is the whole point of an idempotent replay).
    assert_eq!(body_first, body_second, "replayed body must match original");
}

#[actix_rt::test]
async fn test_idempotency_key_too_long_returns_400() {
    let (app, api_key, _, _) = test_app_with_key!();
    let (boundary, body) = build_multipart_pdf(&common::sample_pdf_bytes());
    let too_long = "x".repeat(200);

    let req = test::TestRequest::post()
        .uri("/v1/parse")
        .insert_header(("X-API-Key", api_key.as_str()))
        .insert_header(("Idempotency-Key", too_long.as_str()))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_rt::test]
async fn test_metrics_endpoint_serves_text_format_with_counters_after_parse() {
    let (app, api_key, _, _) = test_app_with_key!();
    let (boundary, body) = build_multipart_pdf(&common::sample_pdf_bytes());

    // Drive a successful parse first so the parse_requests counter
    // has a value to assert on.
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

    // Now scrape /metrics and assert counter shape.
    let req = test::TestRequest::get().uri("/metrics").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body = test::read_body(resp).await;
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("parse_requests"), "got: {text}");
    assert!(
        text.contains("parse_requests_total{endpoint=\"/v1/parse\",status=\"200\"}"),
        "successful parse must increment 200 counter; got: {text}"
    );
}

#[actix_rt::test]
async fn test_extract_records_outcome_on_early_validation_error() {
    // POST /v1/extract with a payload that fails early validation
    // (no file field) → 400. /metrics must show a parse_requests counter
    // for /v1/extract with status="400", proving record_outcome fires on
    // every exit path.
    let (app, api_key, _, _) = test_app_with_key!();

    let boundary = "----NoFileBoundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"schema\"\r\n\r\n");
    body.extend_from_slice(br#"{"type":"object"}"#);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let req = test::TestRequest::post()
        .uri("/v1/extract")
        .insert_header(("X-API-Key", api_key.as_str()))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400, "missing file must 400");

    // Scrape /metrics and assert the /v1/extract 400 counter incremented.
    let req = test::TestRequest::get().uri("/metrics").to_request();
    let resp = test::call_service(&app, req).await;
    let body = test::read_body(resp).await;
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("parse_requests_total{endpoint=\"/v1/extract\",status=\"400\"}"),
        "extract early-error path must increment metrics; got:\n{text}"
    );
    // And the latency histogram must have observed it under backend=none.
    assert!(
        text.contains("parse_duration_seconds_count{backend=\"none\"}"),
        "extract early-error path must record histogram under backend=none; got:\n{text}"
    );
}

#[actix_rt::test]
async fn test_extract_returns_502_when_stub_data_violates_schema() {
    // Stub mode returns `data: {}`. A schema requiring `invoice_number`
    // → empty object fails validation → 502 SCHEMA_VALIDATION_FAILED.
    let (app, api_key, _, _) = test_app_with_key!();
    let pdf_bytes = common::sample_pdf_bytes();
    let schema = r#"{"type":"object","required":["invoice_number"],"properties":{"invoice_number":{"type":"string"}}}"#;

    let boundary = "----TestBoundary12345";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"test.pdf\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: application/pdf\r\n\r\n");
    body.extend_from_slice(&pdf_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"schema\"\r\n\r\n");
    body.extend_from_slice(schema.as_bytes());
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let req = test::TestRequest::post()
        .uri("/v1/extract")
        .insert_header(("X-API-Key", api_key.as_str()))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 502, "schema-violating extraction must 502");

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["error"]["code"], "SCHEMA_VALIDATION_FAILED");
}

#[actix_rt::test]
async fn test_extract_returns_413_when_input_exceeds_ceiling() {
    // Build a custom test app with a tiny extract_max_input_chars so the
    // sample PDF's markdown overflows it.
    let config = AppConfig {
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
        stripe_success_url: None,
        stripe_cancel_url: None,
        stripe_portal_return_url: None,
        stripe_price_starter: None,
        stripe_price_pro: None,
        tesseract_path: "tesseract".into(),
        pdftoppm_path: "pdftoppm".into(),
        paddleocr_url: None,
        paddleocr_timeout_secs: 120,
        paddleocr_mode: docforge::config::PaddleOcrMode::Fallback,
        max_concurrent_parses: 8,
        parse_deadline_secs: 90,
        extract_max_input_chars: 32, // tiny ceiling
        clerk_jwks_url: None,
        clerk_issuer: None,
        clerk_leeway_secs: 30,
        dev_auth_bypass: true,
            clerk_webhook_secret: None,
    };
    let pool = db::init_db(&config.database_url).await.unwrap();
    let clerk_id = seed_clerk_user(&pool, "big@e.com").await;
    let gov = docforge::middleware::rate_limit::build_governor(config.rate_limit_per_minute);

    let app = test::init_service(
        App::new()
            .wrap(docforge::middleware::request_id::RequestIdMiddleware)
            .wrap(actix_governor::Governor::new(&gov))
            .app_data(web::Data::new(config))
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(Instant::now()))
            .app_data(web::Data::new(
                docforge::services::parse_gate::ParseGate::new(8),
            ))
            .app_data(web::Data::new(docforge::services::metrics::Metrics::new()))
                .app_data(web::Data::new(docforge::services::idempotency::IdempotencyCache::with_defaults()))
            .app_data(web::Data::new(dev_clerk_config()))
            .app_data(web::Data::new(
                docforge::auth::clerk::JwksCache::new(String::new()).unwrap(),
            ))
            .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
            .service(
                web::scope("/api-keys")
                    .route("", web::post().to(docforge::auth::handlers::create_key)),
            )
            .service(
                web::scope("/v1")
                    .route("/extract", web::post().to(docforge::api::extract::extract_pdf)),
            ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/api-keys")
        .insert_header(("X-Dev-User-Id", clerk_id.as_str()))
        .set_json(serde_json::json!({"name":"Test"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    let api_key = body["key"].as_str().unwrap().to_string();

    // Build a multipart with file + schema
    let pdf_bytes = common::sample_pdf_bytes();
    let schema = r#"{"type":"object"}"#;
    let boundary = "----TestBoundary12345";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"test.pdf\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: application/pdf\r\n\r\n");
    body.extend_from_slice(&pdf_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"schema\"\r\n\r\n");
    body.extend_from_slice(schema.as_bytes());
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let req = test::TestRequest::post()
        .uri("/v1/extract")
        .insert_header(("X-API-Key", api_key.as_str()))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 413, "oversized markdown must 413");
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["error"]["code"], "EXTRACT_INPUT_TOO_LARGE");
}

#[actix_rt::test]
async fn test_parse_document_type_hint_wins() {
    let (app, api_key, _, _) = test_app_with_key!();
    let (boundary, body) =
        build_multipart_pdf_with_hint(&common::sample_pdf_bytes(), "quote");

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
    let meta = &result["document"]["metadata"];
    assert_eq!(meta["document_type_hint"], "quote");
    assert_eq!(meta["document_type"], "quote");
    assert_eq!(meta["document_type_source"], "hint");
}

#[actix_rt::test]
async fn test_parse_unknown_hint_is_ignored() {
    let (app, api_key, _, _) = test_app_with_key!();
    let (boundary, body) = build_multipart_pdf_with_hint(
        &common::sample_pdf_bytes(),
        "not_a_real_type",
    );

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
    // Unknown hint is parsed to None → field is omitted via
    // skip_serializing_if, the response must NOT carry a hint echo.
    assert!(
        result["document"]["metadata"]["document_type_hint"].is_null(),
        "unknown hint should be dropped, not echoed back"
    );
}

#[actix_rt::test]
async fn test_parse_returns_503_when_gate_saturated() {
    // Custom test app with a gate of capacity 1 so we can saturate it
    // deterministically. We hold a permit by acquiring on the gate
    // directly (the gate is shared via web::Data — simulating an
    // in-flight request without the racy timing of an actual upload).
    let config = AppConfig {
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
        stripe_success_url: None,
        stripe_cancel_url: None,
        stripe_portal_return_url: None,
        stripe_price_starter: None,
        stripe_price_pro: None,
        tesseract_path: "tesseract".into(),
        pdftoppm_path: "pdftoppm".into(),
        paddleocr_url: None,
        paddleocr_timeout_secs: 120,
        paddleocr_mode: docforge::config::PaddleOcrMode::Fallback,
        max_concurrent_parses: 1,
        parse_deadline_secs: 90,
        extract_max_input_chars: 200_000,
        clerk_jwks_url: None,
        clerk_issuer: None,
        clerk_leeway_secs: 30,
        dev_auth_bypass: true,
            clerk_webhook_secret: None,
    };
    let pool = db::init_db(&config.database_url).await.unwrap();
    let clerk_id = seed_clerk_user(&pool, "gate@example.com").await;
    let gate = docforge::services::parse_gate::ParseGate::new(1);
    let gate_for_holding = gate.clone();
    let gov = docforge::middleware::rate_limit::build_governor(config.rate_limit_per_minute);

    let app = test::init_service(
        App::new()
            .wrap(docforge::middleware::request_id::RequestIdMiddleware)
            .wrap(actix_governor::Governor::new(&gov))
            .app_data(web::Data::new(config))
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(Instant::now()))
            .app_data(web::Data::new(gate))
            .app_data(web::Data::new(docforge::services::metrics::Metrics::new()))
                .app_data(web::Data::new(docforge::services::idempotency::IdempotencyCache::with_defaults()))
            .app_data(web::Data::new(dev_clerk_config()))
            .app_data(web::Data::new(
                docforge::auth::clerk::JwksCache::new(String::new()).unwrap(),
            ))
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
        .set_json(serde_json::json!({"name":"Test"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    let api_key = body["key"].as_str().unwrap().to_string();

    // Hold the only permit. The next /v1/parse must fail-fast with 503.
    let _held = gate_for_holding
        .try_acquire()
        .expect("test setup must hold the gate's only permit");

    let (boundary, payload_body) = build_multipart_pdf(&common::sample_pdf_bytes());
    let req = test::TestRequest::post()
        .uri("/v1/parse")
        .insert_header(("X-API-Key", api_key.as_str()))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .set_payload(payload_body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 503, "saturated gate must short-circuit to 503");
    let retry = resp
        .headers()
        .get("retry-after")
        .expect("503 must include Retry-After header")
        .to_str()
        .unwrap();
    assert_eq!(retry, "5");
}

/// Build a synthetic PDF whose first 4 KB contains an `/Encrypt` trailer
/// entry. The bytes do not need to be a fully parseable PDF — only the
/// `is_encrypted_pdf` heuristic in `pdf_parser` reads them, and it only
/// scans the first 4 KB after the header.
fn synthetic_encrypted_pdf_bytes() -> Vec<u8> {
    let mut bytes = b"%PDF-1.7\n".to_vec();
    bytes.extend_from_slice(b"%binary marker\n");
    bytes.extend_from_slice(b"trailer\n<< /Size 5 /Root 1 0 R /Encrypt 4 0 R >>\n");
    bytes.resize(512, b' ');
    bytes
}

#[actix_rt::test]
async fn test_parse_returns_422_for_encrypted_pdf() {
    let (app, api_key, _, _) = test_app_with_key!();
    let (boundary, body) = build_multipart_pdf(&synthetic_encrypted_pdf_bytes());

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
    assert_eq!(
        resp.status(),
        422,
        "encrypted PDF must surface as 422 Unprocessable Entity"
    );
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["error"]["code"].as_str(), Some("ENCRYPTED_PDF"));
    assert_eq!(body["error"]["retryable"].as_bool(), Some(false));
}

#[actix_rt::test]
async fn test_parse_returns_504_when_deadline_exceeded() {
    // Custom test app with parse_deadline_secs=0 so the timeout fires
    // immediately, before any backend work can complete. The handler
    // must surface DeadlineExceeded as 504, with the metric increment
    // recorded under the right status label.
    let config = AppConfig {
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
        stripe_success_url: None,
        stripe_cancel_url: None,
        stripe_portal_return_url: None,
        stripe_price_starter: None,
        stripe_price_pro: None,
        tesseract_path: "tesseract".into(),
        pdftoppm_path: "pdftoppm".into(),
        paddleocr_url: None,
        paddleocr_timeout_secs: 120,
        paddleocr_mode: docforge::config::PaddleOcrMode::Fallback,
        max_concurrent_parses: 8,
        parse_deadline_secs: 0,
        extract_max_input_chars: 200_000,
        clerk_jwks_url: None,
        clerk_issuer: None,
        clerk_leeway_secs: 30,
        dev_auth_bypass: true,
            clerk_webhook_secret: None,
    };
    let pool = db::init_db(&config.database_url).await.unwrap();
    let clerk_id = seed_clerk_user(&pool, "deadline@test.com").await;
    let gov = docforge::middleware::rate_limit::build_governor(config.rate_limit_per_minute);

    let app = test::init_service(
        App::new()
            .wrap(docforge::middleware::request_id::RequestIdMiddleware)
            .wrap(actix_governor::Governor::new(&gov))
            .app_data(web::Data::new(config))
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(Instant::now()))
            .app_data(web::Data::new(docforge::services::parse_gate::ParseGate::new(8)))
            .app_data(web::Data::new(docforge::services::metrics::Metrics::new()))
            .app_data(web::Data::new(docforge::services::idempotency::IdempotencyCache::with_defaults()))
            .app_data(web::Data::new(dev_clerk_config()))
            .app_data(web::Data::new(
                docforge::auth::clerk::JwksCache::new(String::new()).unwrap(),
            ))
            .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
            .route("/metrics", web::get().to(docforge::api::metrics::metrics))
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
        .set_json(serde_json::json!({"name":"Test"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    let api_key = body["key"].as_str().unwrap().to_string();

    let (boundary, payload_body) = build_multipart_pdf(&common::sample_pdf_bytes());
    let req = test::TestRequest::post()
        .uri("/v1/parse")
        .insert_header(("X-API-Key", api_key.as_str()))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .set_payload(payload_body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        504,
        "zero-second deadline must surface as 504 Gateway Timeout"
    );
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["error"]["code"].as_str(), Some("DEADLINE_EXCEEDED"));
    assert_eq!(
        body["error"]["retryable"].as_bool(),
        Some(true),
        "deadline exceedances must be flagged retryable"
    );

    // Verify the outcome was recorded in metrics with the 504 status.
    let req = test::TestRequest::get().uri("/metrics").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body = test::read_body(resp).await;
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("parse_requests_total{endpoint=\"/v1/parse\",status=\"504\"}"),
        "504 outcome must be recorded in parse_requests counter; got: {text}"
    );
}

#[actix_rt::test]
async fn test_paddle_degraded_metric_increments_after_fallback() {
    // Point Paddle at an unroutable address so the call fails. Run a
    // scanned PDF that requires OCR so the dispatcher actually reaches
    // the Paddle path, then falls back to Tesseract. Both the warning
    // in the response body and the `paddle_degraded_total` metric must
    // be visible to the caller.
    let config = AppConfig {
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
        stripe_success_url: None,
        stripe_cancel_url: None,
        stripe_portal_return_url: None,
        stripe_price_starter: None,
        stripe_price_pro: None,
        tesseract_path: "tesseract".into(),
        pdftoppm_path: "pdftoppm".into(),
        paddleocr_url: Some("http://127.0.0.1:1".into()),
        paddleocr_timeout_secs: 1,
        paddleocr_mode: docforge::config::PaddleOcrMode::Fallback,
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
    let clerk_id = seed_clerk_user(&pool, "degr@test.com").await;
    let gov = docforge::middleware::rate_limit::build_governor(config.rate_limit_per_minute);

    let app = test::init_service(
        App::new()
            .wrap(docforge::middleware::request_id::RequestIdMiddleware)
            .wrap(actix_governor::Governor::new(&gov))
            .app_data(web::Data::new(config))
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(Instant::now()))
            .app_data(web::Data::new(docforge::services::parse_gate::ParseGate::new(8)))
            .app_data(web::Data::new(docforge::services::metrics::Metrics::new()))
            .app_data(web::Data::new(docforge::services::idempotency::IdempotencyCache::with_defaults()))
            .app_data(web::Data::new(dev_clerk_config()))
            .app_data(web::Data::new(
                docforge::auth::clerk::JwksCache::new(String::new()).unwrap(),
            ))
            .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
            .route("/metrics", web::get().to(docforge::api::metrics::metrics))
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
        .set_json(serde_json::json!({"name":"Test"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    let api_key = body["key"].as_str().unwrap().to_string();

    let scanned = include_bytes!("fixtures/scanned_form.pdf").to_vec();
    let (boundary, payload_body) = build_multipart_pdf(&scanned);
    let req = test::TestRequest::post()
        .uri("/v1/parse")
        .insert_header(("X-API-Key", api_key.as_str()))
        .insert_header((
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        ))
        .set_payload(payload_body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        200,
        "tesseract fallback must recover the scanned doc"
    );
    let body: serde_json::Value = test::read_body_json(resp).await;
    let warnings = body["document"]["metadata"]["warnings"]
        .as_array()
        .expect("warnings array must be present after Paddle degradation");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str() == Some("paddle_degraded_to_tesseract")),
        "expected paddle_degraded_to_tesseract warning, got {warnings:?}"
    );
    assert_eq!(
        body["document"]["metadata"]["routed_to"].as_str(),
        Some("tesseract"),
        "fallback must report tesseract as the actual backend"
    );

    // Now scrape /metrics: paddle_degraded_total must be ≥ 1.
    let req = test::TestRequest::get().uri("/metrics").to_request();
    let resp = test::call_service(&app, req).await;
    let body = test::read_body(resp).await;
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("paddle_degraded_total 1"),
        "paddle_degraded_total must increment to 1 after fallback; got: {text}"
    );
}

#[actix_rt::test]
async fn test_parse_rejects_unknown_multipart_field() {
    let (app, api_key, _, _) = test_app_with_key!();
    let (boundary, body) = build_multipart_with_unknown_field(&common::sample_pdf_bytes());

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
    assert_eq!(
        resp.status(),
        400,
        "unexpected multipart field must be rejected with 400"
    );
}
