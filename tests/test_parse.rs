mod common;

use actix_web::{test, web, App};
use docforge::config::AppConfig;
use docforge::db;
use std::time::Instant;

macro_rules! test_app_with_key {
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
                .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
                .route("/health", web::get().to(docforge::api::health::health))
                .service(
                    web::scope("/auth")
                        .wrap(actix_governor::Governor::new(&auth_gov))
                        .route("/register", web::post().to(docforge::auth::handlers::register))
                        .route("/login", web::post().to(docforge::auth::handlers::login)),
                )
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

        // Register + login
        let req = test::TestRequest::post()
            .uri("/auth/register")
            .set_json(serde_json::json!({"email":"p@test.com","password":"password123"}))
            .to_request();
        test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri("/auth/login")
            .set_json(serde_json::json!({"email":"p@test.com","password":"password123"}))
            .to_request();
        let resp = test::call_service(&app, req).await;
        let body: serde_json::Value = test::read_body_json(resp).await;
        let jwt = body["access_token"].as_str().unwrap().to_string();

        // Create API key
        let req = test::TestRequest::post()
            .uri("/api-keys")
            .insert_header(("Authorization", format!("Bearer {jwt}")))
            .set_json(serde_json::json!({"name":"Test"}))
            .to_request();
        let resp = test::call_service(&app, req).await;
        let body: serde_json::Value = test::read_body_json(resp).await;
        let api_key = body["key"].as_str().unwrap().to_string();
        let key_id = body["id"].as_str().unwrap().to_string();

        (app, api_key, jwt, key_id)
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
    let (app, api_key, jwt, key_id) = test_app_with_key!();

    // Revoke
    let req = test::TestRequest::delete()
        .uri(&format!("/api-keys/{key_id}"))
        .insert_header(("Authorization", format!("Bearer {jwt}")))
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
        extract_max_input_chars: 32, // tiny ceiling
    };
    let pool = db::init_db(&config.database_url).await.unwrap();
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
            .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
            .service(
                web::scope("/auth")
                    .route("/register", web::post().to(docforge::auth::handlers::register))
                    .route("/login", web::post().to(docforge::auth::handlers::login)),
            )
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
        .uri("/auth/register")
        .set_json(serde_json::json!({"email":"big@e.com","password":"test_password_long"}))
        .to_request();
    let _ = test::call_service(&app, req).await;

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(serde_json::json!({"email":"big@e.com","password":"test_password_long"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    let jwt = body["access_token"].as_str().unwrap().to_string();

    let req = test::TestRequest::post()
        .uri("/api-keys")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
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
        max_concurrent_parses: 1,
        parse_deadline_secs: 90,
            extract_max_input_chars: 200_000,
    };
    let pool = db::init_db(&config.database_url).await.unwrap();
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
            .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
            .service(
                web::scope("/auth")
                    .route("/register", web::post().to(docforge::auth::handlers::register))
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

    // Register + login + create API key inline (can't reuse the macro
    // because we need a custom gate). Stripped down: just enough to
    // authenticate against the saturated handler.
    let req = test::TestRequest::post()
        .uri("/auth/register")
        .set_json(serde_json::json!({
            "email": "gate@example.com",
            "password": "test_password_long"
        }))
        .to_request();
    let _ = test::call_service(&app, req).await;

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(serde_json::json!({
            "email": "gate@example.com",
            "password": "test_password_long"
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    let jwt = body["access_token"].as_str().unwrap().to_string();

    let req = test::TestRequest::post()
        .uri("/api-keys")
        .insert_header(("Authorization", format!("Bearer {jwt}")))
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
