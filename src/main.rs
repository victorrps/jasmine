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
    let metrics = services::metrics::Metrics::new();
    let idempotency_cache = services::idempotency::IdempotencyCache::with_defaults();

    // Clerk config + JWKS cache. The cache is constructed
    // unconditionally so handler `web::Data<JwksCache>` extraction
    // never fails — but it only does network refresh when a JWKS URL
    // is actually configured. Dev-bypass mode (no Clerk URL +
    // `DEV_AUTH_BYPASS=true`) skips JWKS entirely.
    let clerk_config = auth::clerk::ClerkConfig {
        jwks_url: config.clerk_jwks_url.clone().unwrap_or_default(),
        issuer: config.clerk_issuer.clone().unwrap_or_default(),
        leeway_secs: config.clerk_leeway_secs,
        dev_auth_bypass: config.dev_auth_bypass,
    };
    // Log the chosen Clerk auth mode once at startup. The cache
    // itself is constructed unconditionally below so handlers can
    // always extract `web::Data<JwksCache>` without an Option dance.
    let jwks_url_for_cache = match (&config.clerk_jwks_url, config.dev_auth_bypass) {
        (Some(url), _) => {
            tracing::info!(
                jwks_url = %url,
                issuer = ?config.clerk_issuer,
                "Clerk auth: enabled"
            );
            url.clone()
        }
        (None, true) => {
            tracing::warn!(
                "DEV_AUTH_BYPASS is ENABLED — `X-Dev-User-Id` header will be \
                 trusted as the user ID on all Clerk-protected endpoints. \
                 This MUST be off in production. Combine with CLERK_JWKS_URL \
                 to disable the bypass."
            );
            String::new()
        }
        (None, false) => {
            tracing::info!(
                "Clerk auth: disabled (no CLERK_JWKS_URL set, no DEV_AUTH_BYPASS). \
                 Endpoints that require ClerkAuth will return 401."
            );
            String::new()
        }
    };
    let jwks_cache = auth::clerk::JwksCache::new(jwks_url_for_cache)?;

    let bind_addr = format!("{}:{}", config.host, config.port);
    let config_data = web::Data::new(config);
    let pool_data = web::Data::new(pool);
    let start_data = web::Data::new(start_time);
    let gate_data = web::Data::new(parse_gate);
    let metrics_data = web::Data::new(metrics);
    let idem_data = web::Data::new(idempotency_cache);
    let clerk_config_data = web::Data::new(clerk_config);
    let jwks_data = web::Data::new(jwks_cache);

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
            .app_data(metrics_data.clone())
            .app_data(idem_data.clone())
            .app_data(clerk_config_data.clone())
            .app_data(jwks_data.clone())
            .app_data(web::PayloadConfig::default().limit(50 * 1024 * 1024))
            // Health + metrics (unauthenticated; protect via network policy)
            .route("/health", web::get().to(api::health::health))
            .route("/metrics", web::get().to(api::metrics::metrics))
            // Authenticated user mirror (Clerk JWT or dev-bypass header)
            .route("/me", web::get().to(api::me::get_me))
            // API key management — TODO(piece-6): swap JwtAuth for ClerkAuth.
            // The /auth/{register,login,oauth/*} endpoints were removed in
            // piece-4: identity is now owned by Clerk and provisioned via
            // POST /webhooks/clerk. The auth_governor_cfg lives on for the
            // tighter rate-limit bucket once the user-facing endpoints
            // (e.g. /me, /billing/checkout-session) are wired in.
            .service(
                web::scope("/api-keys")
                    .wrap(Governor::new(&auth_governor_cfg))
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
            // Clerk webhooks (Svix-signed; no auth middleware — the
            // signature IS the auth)
            .route(
                "/webhooks/clerk",
                web::post().to(api::webhooks::clerk_webhook),
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
