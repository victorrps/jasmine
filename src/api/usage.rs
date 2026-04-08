//! `GET /v1/usage/summary` — dashboard aggregation over `usage_logs`.
//!
//! ClerkAuth-protected. Aggregates the authenticated user's usage over a
//! configurable period (7d / 30d / 90d, default 30d) into per-day buckets
//! and a per-API-key breakdown. Pagination is intentionally absent — the
//! dashboard always asks for one of three fixed windows so the result set
//! is bounded by `period_days * api_key_count`.

use actix_web::{web, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::auth::clerk::{ClerkAuth, ClerkConfig};
use crate::errors::AppError;
use crate::models;

#[derive(Debug, Deserialize)]
pub struct UsageSummaryQuery {
    pub period: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UsageSummaryResponse {
    pub period_days: u32,
    pub total_pages: i64,
    pub total_requests: i64,
    pub by_day: Vec<DayBucket>,
    pub by_api_key: Vec<ApiKeyBucket>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct DayBucket {
    pub date: String,
    pub requests: i64,
    pub pages: i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ApiKeyBucket {
    pub key_id: String,
    pub name: String,
    pub requests: i64,
    pub last_used_at: Option<String>,
}

/// Clamp a `?period=` query value to one of {7, 30, 90}, defaulting to 30
/// for missing or unrecognised values. Returning a 400 here would just
/// pollute the dashboard with errors when a user typoes the URL — the
/// summary itself is always cheap to compute.
fn clamp_period(raw: Option<&str>) -> u32 {
    match raw {
        Some("7d") => 7,
        Some("90d") => 90,
        _ => 30,
    }
}

#[tracing::instrument(skip(auth, pool, clerk_cfg), fields(clerk_user_id = %auth.clerk_user_id))]
pub async fn get_usage_summary(
    auth: ClerkAuth,
    pool: web::Data<SqlitePool>,
    clerk_cfg: web::Data<ClerkConfig>,
    query: web::Query<UsageSummaryQuery>,
) -> Result<HttpResponse, AppError> {
    let user_id = models::user::get_local_id_by_clerk_id(
        &pool,
        &auth.clerk_user_id,
        clerk_cfg.dev_auto_provision(),
    )
        .await?;

    let period_days = clamp_period(query.period.as_deref());
    // SQLite's `datetime('now', '-N days')` accepts a literal modifier; we
    // build it from a validated u32 so there's no injection surface.
    let since_modifier = format!("-{period_days} days");

    let totals: (i64, i64) = sqlx::query_as(
        "SELECT \
             COALESCE(SUM(ul.pages_processed), 0), \
             COUNT(ul.id) \
         FROM usage_logs ul \
         INNER JOIN api_keys ak ON ak.id = ul.api_key_id \
         WHERE ak.user_id = ? \
           AND ul.created_at >= datetime('now', ?)",
    )
    .bind(&user_id)
    .bind(&since_modifier)
    .fetch_one(pool.get_ref())
    .await
    .map_err(AppError::Database)?;

    let by_day: Vec<DayBucket> = sqlx::query_as(
        "SELECT \
             date(ul.created_at) AS date, \
             COUNT(ul.id) AS requests, \
             COALESCE(SUM(ul.pages_processed), 0) AS pages \
         FROM usage_logs ul \
         INNER JOIN api_keys ak ON ak.id = ul.api_key_id \
         WHERE ak.user_id = ? \
           AND ul.created_at >= datetime('now', ?) \
         GROUP BY date(ul.created_at) \
         ORDER BY date(ul.created_at) ASC",
    )
    .bind(&user_id)
    .bind(&since_modifier)
    .fetch_all(pool.get_ref())
    .await
    .map_err(AppError::Database)?;

    let by_api_key: Vec<ApiKeyBucket> = sqlx::query_as(
        "SELECT \
             ak.id AS key_id, \
             ak.name AS name, \
             COALESCE(COUNT(ul.id), 0) AS requests, \
             ak.last_used_at AS last_used_at \
         FROM api_keys ak \
         LEFT JOIN usage_logs ul \
             ON ul.api_key_id = ak.id \
            AND ul.created_at >= datetime('now', ?) \
         WHERE ak.user_id = ? \
         GROUP BY ak.id, ak.name, ak.last_used_at \
         ORDER BY ak.created_at DESC",
    )
    .bind(&since_modifier)
    .bind(&user_id)
    .fetch_all(pool.get_ref())
    .await
    .map_err(AppError::Database)?;

    Ok(HttpResponse::Ok().json(UsageSummaryResponse {
        period_days,
        total_pages: totals.0,
        total_requests: totals.1,
        by_day,
        by_api_key,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::clerk::JwksCache;
    use actix_web::{test as atest, App};

    fn dev_clerk_cfg() -> ClerkConfig {
        ClerkConfig {
            jwks_url: String::new(),
            issuer: String::new(),
            leeway_secs: 30,
            dev_auth_bypass: true,
        }
    }

    async fn fresh_pool() -> SqlitePool {
        let url = format!(
            "sqlite://file:usage_test_{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        crate::db::init_db(&url).await.unwrap()
    }

    #[test]
    fn clamp_period_defaults_to_30() {
        assert_eq!(clamp_period(None), 30);
        assert_eq!(clamp_period(Some("not-a-period")), 30);
        assert_eq!(clamp_period(Some("")), 30);
    }

    #[test]
    fn clamp_period_accepts_7d_and_90d() {
        assert_eq!(clamp_period(Some("7d")), 7);
        assert_eq!(clamp_period(Some("30d")), 30);
        assert_eq!(clamp_period(Some("90d")), 90);
    }

    #[actix_rt::test]
    async fn empty_user_returns_zeros() {
        let pool = fresh_pool().await;
        models::user::upsert_from_clerk(&pool, "user_empty", "e@dev.local", None, None)
            .await
            .unwrap();
        let app = atest::init_service(
            App::new()
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(dev_clerk_cfg()))
                .app_data(web::Data::new(JwksCache::new(String::new()).unwrap()))
                .route("/v1/usage/summary", web::get().to(get_usage_summary)),
        )
        .await;
        let req = atest::TestRequest::get()
            .uri("/v1/usage/summary")
            .insert_header(("X-Dev-User-Id", "user_empty"))
            .to_request();
        let resp = atest::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = atest::read_body_json(resp).await;
        assert_eq!(body["period_days"], 30);
        assert_eq!(body["total_pages"], 0);
        assert_eq!(body["total_requests"], 0);
        assert!(body["by_day"].as_array().unwrap().is_empty());
        assert!(body["by_api_key"].as_array().unwrap().is_empty());
    }

    #[actix_rt::test]
    async fn period_query_param_clamps_to_seven() {
        let pool = fresh_pool().await;
        models::user::upsert_from_clerk(&pool, "user_seven", "s@dev.local", None, None)
            .await
            .unwrap();
        let app = atest::init_service(
            App::new()
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(dev_clerk_cfg()))
                .app_data(web::Data::new(JwksCache::new(String::new()).unwrap()))
                .route("/v1/usage/summary", web::get().to(get_usage_summary)),
        )
        .await;
        let req = atest::TestRequest::get()
            .uri("/v1/usage/summary?period=7d")
            .insert_header(("X-Dev-User-Id", "user_seven"))
            .to_request();
        let resp = atest::call_service(&app, req).await;
        let body: serde_json::Value = atest::read_body_json(resp).await;
        assert_eq!(body["period_days"], 7);
    }

    #[actix_rt::test]
    async fn missing_auth_returns_401() {
        let pool = fresh_pool().await;
        let app = atest::init_service(
            App::new()
                .app_data(web::Data::new(pool))
                .app_data(web::Data::new(dev_clerk_cfg()))
                .app_data(web::Data::new(JwksCache::new(String::new()).unwrap()))
                .route("/v1/usage/summary", web::get().to(get_usage_summary)),
        )
        .await;
        let req = atest::TestRequest::get().uri("/v1/usage/summary").to_request();
        let resp = atest::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);
    }
}
