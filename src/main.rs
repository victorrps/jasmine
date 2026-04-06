use actix_cors::Cors;
use actix_governor::Governor;
use actix_web::{middleware::Logger, web, App, HttpServer};
use actix_files as fs;
use std::time::Instant;

mod api;
mod auth;
mod config;
mod db;
mod errors;
mod middleware;
mod models;
mod services;

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    let config = config::AppConfig::from_env()?;

    // Initialize logging — pretty format for dev, structured for readability
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(true)
        .init();

    tracing::info!(
        host = %config.host,
        port = %config.port,
        "Starting DocForge API server"
    );

    let pool = db::init_db(&config.database_url).await?;
    let start_time = Instant::now();
    let governor_cfg = middleware::rate_limit::build_governor(config.rate_limit_per_minute);
    let auth_governor_cfg = middleware::rate_limit::build_auth_governor();
    let parse_gate = services::parse_gate::ParseGate::new(config.max_concurrent_parses);

    let bind_addr = format!("{}:{}", config.host, config.port);
    let config_data = web::Data::new(config);
    let pool_data = web::Data::new(pool);
    let start_data = web::Data::new(start_time);
    let gate_data = web::Data::new(parse_gate);

    HttpServer::new(move || {
        let cors = Cors::default()
            .allowed_origin("http://localhost:3000")
            .allowed_origin("http://localhost:8080")
            .allowed_origin("http://127.0.0.1:3000")
            .allowed_origin("http://127.0.0.1:8080")
            .allowed_methods(vec!["GET", "POST", "DELETE", "OPTIONS"])
            .allowed_headers(vec![
                actix_web::http::header::AUTHORIZATION,
                actix_web::http::header::CONTENT_TYPE,
                actix_web::http::header::HeaderName::from_static("x-api-key"),
            ])
            .max_age(3600);

        App::new()
            .wrap(middleware::request_id::RequestIdMiddleware)
            .wrap(Logger::new("%s %r %Dms"))
            .wrap(cors)
            .wrap(Governor::new(&governor_cfg))
            .app_data(config_data.clone())
            .app_data(pool_data.clone())
            .app_data(start_data.clone())
            .app_data(gate_data.clone())
            .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
            // Health
            .route("/health", web::get().to(api::health::health))
            // Auth (tighter rate limit: 5 req/min per IP)
            .service(
                web::scope("/auth")
                    .wrap(Governor::new(&auth_governor_cfg))
                    .route("/register", web::post().to(auth::handlers::register))
                    .route("/login", web::post().to(auth::handlers::login))
                    .route(
                        "/oauth/{provider}",
                        web::get().to(auth::handlers::oauth_redirect),
                    )
                    .route(
                        "/oauth/{provider}/callback",
                        web::get().to(auth::handlers::oauth_callback),
                    ),
            )
            // API key management (JWT protected)
            .service(
                web::scope("/api-keys")
                    .route("", web::post().to(auth::handlers::create_key))
                    .route("", web::get().to(auth::handlers::list_keys))
                    .route("/{key_id}", web::delete().to(auth::handlers::revoke_key)),
            )
            // PDF endpoints (API key protected)
            .service(
                web::scope("/v1")
                    .route("/parse", web::post().to(api::parse::parse_pdf))
                    .route("/extract", web::post().to(api::extract::extract_pdf))
                    .route("/parse/batch", web::post().to(api::batch::batch_parse))
                    .route("/parse/batch/async", web::post().to(api::batch::batch_parse_async))
                    .route("/parse/batch/{batch_id}", web::get().to(api::batch::batch_status))
                    .route("/usage", web::get().to(api::billing::get_usage)),
            )
            // Billing (public + webhook)
            .service(
                web::scope("/billing")
                    .route("/plans", web::get().to(api::billing::list_plans))
                    .route("/webhook", web::post().to(api::billing::stripe_webhook)),
            )
            // MCP server (Streamable HTTP — no auth for POC, add API key auth for prod)
            .route("/mcp", web::post().to(api::mcp::mcp_handler))
            // Static files (landing page + docs)
            .service(fs::Files::new("/static", "./static").index_file("index.html"))
            .route("/", web::get().to(|| async {
                actix_web::HttpResponse::Found()
                    .insert_header(("Location", "/static/index.html"))
                    .finish()
            }))
    })
    .bind(&bind_addr)?
    .run()
    .await?;

    Ok(())
}
