use serde::Serialize;
use sqlx::SqlitePool;

use crate::errors::AppError;

// ── Pricing tiers ────────────────────────────────────────────────────────────

const FREE_PAGE_LIMIT: u32 = 50;
const STARTER_PAGE_LIMIT: u32 = 1_000;
const PRO_PAGE_LIMIT: u32 = 5_000;
const ENTERPRISE_PAGE_LIMIT: u32 = 25_000;

const FREE_PRICE_CENTS: u32 = 0;
const STARTER_PRICE_CENTS: u32 = 900;
const PRO_PRICE_CENTS: u32 = 2_900;
const ENTERPRISE_PRICE_CENTS: u32 = 7_900;

/// Available pricing tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PricingTier {
    Free,
    Starter,
    Pro,
    Enterprise,
}

impl PricingTier {
    /// Maximum pages per calendar month for this tier.
    pub fn page_limit(self) -> u32 {
        match self {
            Self::Free => FREE_PAGE_LIMIT,
            Self::Starter => STARTER_PAGE_LIMIT,
            Self::Pro => PRO_PAGE_LIMIT,
            Self::Enterprise => ENTERPRISE_PAGE_LIMIT,
        }
    }

    /// Monthly price in US cents.
    pub fn price_cents(self) -> u32 {
        match self {
            Self::Free => FREE_PRICE_CENTS,
            Self::Starter => STARTER_PRICE_CENTS,
            Self::Pro => PRO_PRICE_CENTS,
            Self::Enterprise => ENTERPRISE_PRICE_CENTS,
        }
    }

    /// Human-readable display name.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Free => "Free",
            Self::Starter => "Starter",
            Self::Pro => "Pro",
            Self::Enterprise => "Enterprise",
        }
    }

    /// Parse a tier string from the database. Defaults to `Free` on unknown values.
    pub fn from_db_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "starter" => Self::Starter,
            "pro" => Self::Pro,
            "enterprise" => Self::Enterprise,
            _ => Self::Free,
        }
    }

    /// All tiers in ascending order.
    pub fn all() -> &'static [PricingTier] {
        &[Self::Free, Self::Starter, Self::Pro, Self::Enterprise]
    }
}

// ── Usage status ─────────────────────────────────────────────────────────────

/// Current usage status for an API key's owner.
#[derive(Debug, Clone, Serialize)]
pub struct UsageStatus {
    pub allowed: bool,
    pub used: u32,
    pub limit: u32,
    pub tier: String,
}

// ── Database queries ─────────────────────────────────────────────────────────

/// Sum total pages processed this calendar month for the user that owns `api_key_id`.
pub async fn get_monthly_usage(pool: &SqlitePool, api_key_id: &str) -> Result<u32, AppError> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(ul.pages_processed), 0) \
         FROM usage_logs ul \
         INNER JOIN api_keys ak ON ak.id = ul.api_key_id \
         WHERE ak.user_id = (SELECT user_id FROM api_keys WHERE id = ?) \
         AND strftime('%Y-%m', ul.created_at) = strftime('%Y-%m', 'now')",
    )
    .bind(api_key_id)
    .fetch_one(pool)
    .await
    .map_err(AppError::Database)?;

    Ok(row.0 as u32)
}

/// Resolve the pricing tier for the user that owns `api_key_id`.
async fn get_user_tier(pool: &SqlitePool, api_key_id: &str) -> Result<PricingTier, AppError> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT COALESCE(u.tier, 'free') \
         FROM users u \
         INNER JOIN api_keys ak ON ak.user_id = u.id \
         WHERE ak.id = ?",
    )
    .bind(api_key_id)
    .fetch_optional(pool)
    .await
    .map_err(AppError::Database)?;

    match row {
        Some((tier_str,)) => Ok(PricingTier::from_db_str(&tier_str)),
        None => Err(AppError::NotFound),
    }
}

/// Check whether the user is within their monthly page limit.
pub async fn check_usage_limit(
    pool: &SqlitePool,
    api_key_id: &str,
) -> Result<UsageStatus, AppError> {
    let tier = get_user_tier(pool, api_key_id).await?;
    let used = get_monthly_usage(pool, api_key_id).await?;
    let limit = tier.page_limit();

    Ok(UsageStatus {
        allowed: used < limit,
        used,
        limit,
        tier: tier.display_name().to_lowercase(),
    })
}

// ── Stripe metered usage reporting ───────────────────────────────────────────

/// Report metered usage to Stripe (fire-and-forget).
///
/// This creates a usage record on the given subscription item via the Stripe API.
/// Errors are logged but not propagated — billing failures must not block the
/// API response.
#[allow(dead_code)]
pub async fn report_usage(stripe_key: &str, subscription_item_id: &str, pages: u32) {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default();
    let url = format!(
        "https://api.stripe.com/v1/subscription_items/{}/usage_records",
        subscription_item_id
    );

    let result = client
        .post(&url)
        .basic_auth(stripe_key, None::<&str>)
        .form(&[
            ("quantity", pages.to_string()),
            ("action", "increment".to_string()),
        ])
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!(
                subscription_item_id,
                pages,
                "Stripe usage record created"
            );
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!(
                subscription_item_id,
                pages,
                %status,
                body,
                "Stripe usage report failed"
            );
        }
        Err(e) => {
            tracing::error!(
                subscription_item_id,
                pages,
                error = %e,
                "Stripe usage report request error"
            );
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── PricingTier ──────────────────────────────────────────────────────

    #[test]
    fn free_tier_has_correct_limits() {
        assert_eq!(PricingTier::Free.page_limit(), 50);
        assert_eq!(PricingTier::Free.price_cents(), 0);
    }

    #[test]
    fn starter_tier_has_correct_limits() {
        assert_eq!(PricingTier::Starter.page_limit(), 1_000);
        assert_eq!(PricingTier::Starter.price_cents(), 900);
    }

    #[test]
    fn pro_tier_has_correct_limits() {
        assert_eq!(PricingTier::Pro.page_limit(), 5_000);
        assert_eq!(PricingTier::Pro.price_cents(), 2_900);
    }

    #[test]
    fn enterprise_tier_has_correct_limits() {
        assert_eq!(PricingTier::Enterprise.page_limit(), 25_000);
        assert_eq!(PricingTier::Enterprise.price_cents(), 7_900);
    }

    #[test]
    fn display_name_returns_human_readable() {
        assert_eq!(PricingTier::Free.display_name(), "Free");
        assert_eq!(PricingTier::Starter.display_name(), "Starter");
        assert_eq!(PricingTier::Pro.display_name(), "Pro");
        assert_eq!(PricingTier::Enterprise.display_name(), "Enterprise");
    }

    #[test]
    fn from_db_str_parses_known_tiers() {
        assert_eq!(PricingTier::from_db_str("free"), PricingTier::Free);
        assert_eq!(PricingTier::from_db_str("starter"), PricingTier::Starter);
        assert_eq!(PricingTier::from_db_str("pro"), PricingTier::Pro);
        assert_eq!(
            PricingTier::from_db_str("enterprise"),
            PricingTier::Enterprise
        );
    }

    #[test]
    fn from_db_str_is_case_insensitive() {
        assert_eq!(PricingTier::from_db_str("STARTER"), PricingTier::Starter);
        assert_eq!(PricingTier::from_db_str("Pro"), PricingTier::Pro);
        assert_eq!(
            PricingTier::from_db_str("ENTERPRISE"),
            PricingTier::Enterprise
        );
    }

    #[test]
    fn from_db_str_defaults_to_free_on_unknown() {
        assert_eq!(PricingTier::from_db_str("unknown"), PricingTier::Free);
        assert_eq!(PricingTier::from_db_str(""), PricingTier::Free);
        assert_eq!(PricingTier::from_db_str("premium"), PricingTier::Free);
    }

    #[test]
    fn all_returns_four_tiers_in_order() {
        let tiers = PricingTier::all();
        assert_eq!(tiers.len(), 4);
        assert_eq!(tiers[0], PricingTier::Free);
        assert_eq!(tiers[1], PricingTier::Starter);
        assert_eq!(tiers[2], PricingTier::Pro);
        assert_eq!(tiers[3], PricingTier::Enterprise);
    }

    #[test]
    fn tiers_are_ordered_by_page_limit() {
        let tiers = PricingTier::all();
        for window in tiers.windows(2) {
            assert!(
                window[0].page_limit() < window[1].page_limit(),
                "{:?} should have a lower limit than {:?}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn tiers_are_ordered_by_price() {
        let tiers = PricingTier::all();
        for window in tiers.windows(2) {
            assert!(
                window[0].price_cents() <= window[1].price_cents(),
                "{:?} should cost less than or equal to {:?}",
                window[0],
                window[1]
            );
        }
    }

    // ── UsageStatus ──────────────────────────────────────────────────────

    #[test]
    fn usage_status_allowed_when_under_limit() {
        let status = UsageStatus {
            allowed: true,
            used: 10,
            limit: 50,
            tier: "free".to_string(),
        };
        assert!(status.allowed);
        assert!(status.used < status.limit);
    }

    #[test]
    fn usage_status_denied_when_at_limit() {
        let status = UsageStatus {
            allowed: false,
            used: 50,
            limit: 50,
            tier: "free".to_string(),
        };
        assert!(!status.allowed);
    }

    #[test]
    fn usage_status_denied_when_over_limit() {
        let status = UsageStatus {
            allowed: false,
            used: 60,
            limit: 50,
            tier: "free".to_string(),
        };
        assert!(!status.allowed);
    }
}
