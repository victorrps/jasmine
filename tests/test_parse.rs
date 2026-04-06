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
