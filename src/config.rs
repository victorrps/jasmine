use anyhow::{Context, Result};
use std::collections::HashMap;

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub rate_limit_per_minute: u64,
    /// Server-side HMAC key for API key hashing. Prevents offline brute-force if DB is leaked.
    pub api_key_pepper: String,
    /// Anthropic API key for Claude Haiku schema extraction. Optional — falls back to stub.
    pub anthropic_api_key: Option<String>,
    /// Stripe secret key for metered billing. Optional — billing features disabled without it.
    #[allow(dead_code)]
    pub stripe_secret_key: Option<String>,
    /// Stripe webhook signing secret. Optional — signature verification skipped without it.
    pub stripe_webhook_secret: Option<String>,
    /// Path to tesseract binary for local OCR. Defaults to "tesseract" (found via PATH).
    pub tesseract_path: String,
    /// Path to pdftoppm binary for PDF-to-image conversion. Defaults to "pdftoppm" (found via PATH).
    pub pdftoppm_path: String,
    /// PaddleOCR serving URL (e.g. http://localhost:8868). When set, PaddleOCR PP-StructureV3
    /// is used as the primary OCR/layout engine with tesseract as fallback.
    pub paddleocr_url: Option<String>,
    /// Timeout in seconds for PaddleOCR HTTP calls. Defaults to 120s (layout parsing is slow).
    pub paddleocr_timeout_secs: u64,
    /// Backend routing mode when PaddleOCR is configured.
    pub paddleocr_mode: PaddleOcrMode,
    /// Maximum number of in-flight `/v1/parse` + `/v1/extract` requests this
    /// instance will accept concurrently. Excess requests are rejected with
    /// `503 Service Unavailable` and `Retry-After: 5`. Default 8.
    /// Tune per instance: this is the knob between "queues silently" and
    /// "fails fast". Combine with horizontal scaling for production load.
    pub max_concurrent_parses: usize,
    /// Wall-clock budget for a single `/v1/parse` (or `/v1/extract`) call,
    /// in seconds. Requests that exceed this return `504 Gateway Timeout`.
    /// Default 90s. Note: cancelling the future does NOT cancel the
    /// in-flight `spawn_blocking` work; the concurrency cap above is what
    /// prevents stuck tasks from compounding.
    pub parse_deadline_secs: u64,
    /// Maximum length, in chars, of the markdown input passed to Claude
    /// for `/v1/extract`. Hard ceiling — requests above this are rejected
    /// with `413 Payload Too Large` (`code: EXTRACT_INPUT_TOO_LARGE`)
    /// rather than silently truncated. Default 200_000 chars
    /// (~50 K tokens). Set lower to bound your Anthropic spend per call.
    pub extract_max_input_chars: usize,
    /// Clerk JWKS endpoint URL, e.g.
    /// `https://your-app.clerk.accounts.dev/.well-known/jwks.json`.
    /// `None` disables Clerk verification entirely; combine with
    /// `dev_auth_bypass` for local development without a Clerk instance.
    pub clerk_jwks_url: Option<String>,
    /// Clerk issuer (the frontend API URL). Required when `clerk_jwks_url`
    /// is set; ignored otherwise.
    pub clerk_issuer: Option<String>,
    /// JWT clock-skew leeway in seconds for Clerk verification.
    /// Default 30s.
    pub clerk_leeway_secs: u64,
    /// **DEV ONLY**: when `true` AND `clerk_jwks_url` is unset, the
    /// `ClerkAuth` extractor accepts an `X-Dev-User-Id` header in lieu
    /// of a real Clerk JWT, returning that string as the user ID.
    /// This is the local-development escape hatch — it lets you hit
    /// authenticated endpoints with `curl -H 'X-Dev-User-Id: user_test'`
    /// without spinning up a Clerk dev instance.
    ///
    /// **Cannot be enabled in production**: the validation in
    /// `from_vars` rejects the combination `dev_auth_bypass=true` AND
    /// `clerk_jwks_url=Some(_)`. The bypass branch in `ClerkAuth` also
    /// double-checks this at request time so a misconfigured deploy
    /// fails closed instead of silently allowing header impersonation.
    pub dev_auth_bypass: bool,
    /// Clerk webhook signing secret (Svix format: `whsec_<base64>`).
    /// When unset the `/webhooks/clerk` endpoint returns 501 — we
    /// refuse to accept unverified webhooks. Find it in the Clerk
    /// dashboard under **Webhooks → Signing Secret**.
    pub clerk_webhook_secret: Option<String>,
}

/// Backend routing mode for PaddleOCR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaddleOcrMode {
    /// Use pdf_oxide first; fall back to Paddle only for scanned / broken PDFs.
    Fallback,
    /// Use Paddle first for every PDF; fall back to pdf_oxide if Paddle fails.
    Primary,
    /// Classify the document (via `pdf_classifier`) and pick a backend per
    /// document: plain text → pdf_oxide, structured → Paddle, scanned → OCR.
    /// This is the default when `PADDLEOCR_URL` is configured.
    Auto,
}

impl PaddleOcrMode {
    fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "fallback" => Ok(Self::Fallback),
            "primary" => Ok(Self::Primary),
            "auto" => Ok(Self::Auto),
            other => anyhow::bail!(
                "PADDLEOCR_MODE must be 'auto', 'primary', or 'fallback', got: {other}"
            ),
        }
    }
}

impl AppConfig {
    /// Load configuration from environment variables.
    /// Fails fast if any required variable is missing or invalid.
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let vars: HashMap<String, String> = std::env::vars().collect();
        Self::from_vars(&vars)
    }

    /// Build configuration from an explicit variable map.
    /// Used directly in tests to avoid global environment mutation.
    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Self> {
        let get = |key: &str| -> Option<String> { vars.get(key).cloned() };

        let port_str = get("PORT").unwrap_or_else(|| "8080".into());
        let port: u16 = port_str.parse().context("PORT must be a valid u16")?;

        let database_url = get("DATABASE_URL").with_context(|| "DATABASE_URL must be set")?;

        let rate_limit_per_minute: u64 = get("RATE_LIMIT_PER_MINUTE")
            .unwrap_or_else(|| "60".into())
            .parse()
            .context("RATE_LIMIT_PER_MINUTE must be a valid u64")?;

        let api_key_pepper =
            get("API_KEY_PEPPER").with_context(|| "API_KEY_PEPPER must be set")?;
        if api_key_pepper.len() < 32 {
            anyhow::bail!("API_KEY_PEPPER must be at least 32 characters");
        }

        let anthropic_api_key = get("ANTHROPIC_API_KEY").filter(|k| !k.is_empty());
        let stripe_secret_key = get("STRIPE_SECRET_KEY").filter(|k| !k.is_empty());
        let stripe_webhook_secret = get("STRIPE_WEBHOOK_SECRET").filter(|k| !k.is_empty());

        let tesseract_path =
            get("TESSERACT_PATH").unwrap_or_else(|| "tesseract".into());
        let pdftoppm_path =
            get("PDFTOPPM_PATH").unwrap_or_else(|| "pdftoppm".into());

        let paddleocr_url = get("PADDLEOCR_URL")
            .filter(|u| !u.is_empty())
            .map(|u| u.trim_end_matches('/').to_string());
        let paddleocr_timeout_secs: u64 = get("PADDLEOCR_TIMEOUT_SECS")
            .unwrap_or_else(|| "120".into())
            .parse()
            .context("PADDLEOCR_TIMEOUT_SECS must be a valid u64")?;
        // Default: `auto` when a sidecar URL is configured, `fallback` otherwise.
        // This keeps existing deployments without a sidecar unchanged while
        // making classifier-based routing the out-of-the-box behaviour for
        // anyone who opts in to running PaddleOCR.
        let default_mode = if paddleocr_url.is_some() {
            "auto"
        } else {
            "fallback"
        };
        let paddleocr_mode = PaddleOcrMode::parse(
            &get("PADDLEOCR_MODE").unwrap_or_else(|| default_mode.into()),
        )?;

        let max_concurrent_parses: usize = get("MAX_CONCURRENT_PARSES")
            .unwrap_or_else(|| "8".into())
            .parse()
            .context("MAX_CONCURRENT_PARSES must be a valid usize")?;
        if max_concurrent_parses == 0 {
            anyhow::bail!("MAX_CONCURRENT_PARSES must be greater than 0");
        }

        let parse_deadline_secs: u64 = get("PARSE_DEADLINE_SECS")
            .unwrap_or_else(|| "90".into())
            .parse()
            .context("PARSE_DEADLINE_SECS must be a valid u64")?;
        if parse_deadline_secs == 0 {
            anyhow::bail!("PARSE_DEADLINE_SECS must be greater than 0");
        }

        let extract_max_input_chars: usize = get("EXTRACT_MAX_INPUT_CHARS")
            .unwrap_or_else(|| "200000".into())
            .parse()
            .context("EXTRACT_MAX_INPUT_CHARS must be a valid usize")?;
        if extract_max_input_chars == 0 {
            anyhow::bail!("EXTRACT_MAX_INPUT_CHARS must be greater than 0");
        }

        let clerk_jwks_url = get("CLERK_JWKS_URL").filter(|s| !s.is_empty());
        let clerk_issuer = get("CLERK_ISSUER").filter(|s| !s.is_empty());
        if clerk_jwks_url.is_some() && clerk_issuer.is_none() {
            anyhow::bail!("CLERK_ISSUER must be set when CLERK_JWKS_URL is configured");
        }
        if clerk_issuer.is_some() && clerk_jwks_url.is_none() {
            // Warn rather than bail: refusing to start would break
            // any deploy that has the issuer set as a "preparation"
            // step before wiring the JWKS URL. The orphaned issuer
            // is harmless at runtime (the verifier never reaches the
            // `iss` check unless `jwks_url` is set), but the warning
            // ensures the operator knows their configuration is
            // incomplete.
            tracing::warn!(
                "CLERK_ISSUER is set but CLERK_JWKS_URL is not — \
                 CLERK_ISSUER will be ignored until the JWKS URL is configured."
            );
        }
        let clerk_leeway_secs: u64 = get("CLERK_LEEWAY_SECS")
            .unwrap_or_else(|| "30".into())
            .parse()
            .context("CLERK_LEEWAY_SECS must be a valid u64")?;

        let clerk_webhook_secret = get("CLERK_WEBHOOK_SECRET").filter(|s| !s.is_empty());
        if let Some(ref s) = clerk_webhook_secret {
            if !s.starts_with("whsec_") {
                anyhow::bail!(
                    "CLERK_WEBHOOK_SECRET must start with 'whsec_' (Svix format)"
                );
            }
        }

        let dev_auth_bypass = get("DEV_AUTH_BYPASS")
            .map(|s| matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false);
        // Hard safety: dev bypass and real Clerk are mutually exclusive.
        // Refusing this combination at startup means a deploy that
        // accidentally leaves DEV_AUTH_BYPASS=true in its env cannot
        // also be wired to real Clerk and silently grant header
        // impersonation in production.
        if dev_auth_bypass && clerk_jwks_url.is_some() {
            anyhow::bail!(
                "DEV_AUTH_BYPASS=true is mutually exclusive with CLERK_JWKS_URL — \
                 unset one of them. The bypass exists only for local development \
                 against a system not yet wired to a Clerk instance."
            );
        }

        Ok(Self {
            host: get("HOST").unwrap_or_else(|| "127.0.0.1".into()),
            port,
            database_url,
            rate_limit_per_minute,
            api_key_pepper,
            anthropic_api_key,
            stripe_secret_key,
            stripe_webhook_secret,
            tesseract_path,
            pdftoppm_path,
            paddleocr_url,
            paddleocr_timeout_secs,
            paddleocr_mode,
            max_concurrent_parses,
            parse_deadline_secs,
            extract_max_input_chars,
            clerk_jwks_url,
            clerk_issuer,
            clerk_leeway_secs,
            dev_auth_bypass,
            clerk_webhook_secret,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_PEPPER: &str = "a_valid_pepper_that_is_at_least_32_chars";
    const VALID_DB: &str = "sqlite://:memory:";

    /// Build a minimal valid vars map for `from_vars`.
    fn base_vars() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("API_KEY_PEPPER".into(), VALID_PEPPER.into());
        m.insert("DATABASE_URL".into(), VALID_DB.into());
        m
    }

    #[test]
    fn from_vars_fails_when_pepper_missing() {
        let mut vars = base_vars();
        vars.remove("API_KEY_PEPPER");
        let err = AppConfig::from_vars(&vars).unwrap_err().to_string();
        assert!(err.contains("API_KEY_PEPPER"), "got: {err}");
    }

    #[test]
    fn from_vars_fails_when_pepper_too_short() {
        let mut vars = base_vars();
        vars.insert("API_KEY_PEPPER".into(), "short".into());
        let err = AppConfig::from_vars(&vars).unwrap_err().to_string();
        assert!(err.contains("32"), "got: {err}");
    }

    #[test]
    fn from_vars_succeeds_with_required_vars() {
        let vars = base_vars();
        let cfg = AppConfig::from_vars(&vars).expect("must succeed with required vars");
        assert_eq!(cfg.database_url, VALID_DB);
    }

    #[test]
    fn from_vars_uses_default_host_when_absent() {
        let vars = base_vars();
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.host, "127.0.0.1");
    }

    #[test]
    fn from_vars_uses_custom_host_when_set() {
        let mut vars = base_vars();
        vars.insert("HOST".into(), "0.0.0.0".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.host, "0.0.0.0");
    }

    #[test]
    fn from_vars_uses_default_port_when_absent() {
        let vars = base_vars();
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.port, 8080);
    }

    #[test]
    fn from_vars_uses_custom_port_when_set() {
        let mut vars = base_vars();
        vars.insert("PORT".into(), "3000".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.port, 3000);
    }

    #[test]
    fn from_vars_uses_default_rate_limit_when_absent() {
        let vars = base_vars();
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.rate_limit_per_minute, 60);
    }

    #[test]
    fn from_vars_uses_custom_rate_limit() {
        let mut vars = base_vars();
        vars.insert("RATE_LIMIT_PER_MINUTE".into(), "120".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.rate_limit_per_minute, 120);
    }

    #[test]
    fn from_vars_fails_when_database_url_missing() {
        let mut vars = base_vars();
        vars.remove("DATABASE_URL");
        let result = AppConfig::from_vars(&vars);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("DATABASE_URL"),
            "error should mention DATABASE_URL, got: {msg}"
        );
    }

    #[test]
    fn from_vars_fails_on_invalid_port() {
        let mut vars = base_vars();
        vars.insert("PORT".into(), "not_a_number".into());
        let result = AppConfig::from_vars(&vars);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("PORT"),
            "error should mention PORT, got: {msg}"
        );
    }

    #[test]
    fn from_vars_fails_on_port_out_of_u16_range() {
        let mut vars = base_vars();
        vars.insert("PORT".into(), "99999".into());
        let result = AppConfig::from_vars(&vars);
        assert!(result.is_err(), "port 99999 must fail u16 range check");
    }

    #[test]
    fn from_vars_accepts_port_boundary_values() {
        let mut vars = base_vars();
        vars.insert("PORT".into(), "1".into());
        assert!(AppConfig::from_vars(&vars).is_ok(), "port 1 must be valid");

        vars.insert("PORT".into(), "65535".into());
        assert!(
            AppConfig::from_vars(&vars).is_ok(),
            "port 65535 must be valid"
        );
    }

    #[test]
    fn from_vars_fails_on_invalid_rate_limit() {
        let mut vars = base_vars();
        vars.insert("RATE_LIMIT_PER_MINUTE".into(), "abc".into());
        let result = AppConfig::from_vars(&vars);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("RATE_LIMIT_PER_MINUTE"));
    }

    // ── anthropic_api_key ────────────────────────────────────────────────────

    // ── concurrency + deadline ───────────────────────────────────────────────

    #[test]
    fn max_concurrent_parses_defaults_to_8() {
        let cfg = AppConfig::from_vars(&base_vars()).unwrap();
        assert_eq!(cfg.max_concurrent_parses, 8);
    }

    #[test]
    fn max_concurrent_parses_parses_custom_value() {
        let mut vars = base_vars();
        vars.insert("MAX_CONCURRENT_PARSES".into(), "32".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.max_concurrent_parses, 32);
    }

    #[test]
    fn max_concurrent_parses_rejects_zero() {
        let mut vars = base_vars();
        vars.insert("MAX_CONCURRENT_PARSES".into(), "0".into());
        let err = AppConfig::from_vars(&vars).unwrap_err().to_string();
        assert!(err.contains("MAX_CONCURRENT_PARSES"), "got: {err}");
    }

    #[test]
    fn parse_deadline_secs_defaults_to_90() {
        let cfg = AppConfig::from_vars(&base_vars()).unwrap();
        assert_eq!(cfg.parse_deadline_secs, 90);
    }

    #[test]
    fn parse_deadline_secs_parses_custom_value() {
        let mut vars = base_vars();
        vars.insert("PARSE_DEADLINE_SECS".into(), "30".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.parse_deadline_secs, 30);
    }

    #[test]
    fn extract_max_input_chars_defaults_to_200_000() {
        let cfg = AppConfig::from_vars(&base_vars()).unwrap();
        assert_eq!(cfg.extract_max_input_chars, 200_000);
    }

    #[test]
    fn extract_max_input_chars_parses_custom_value() {
        let mut vars = base_vars();
        vars.insert("EXTRACT_MAX_INPUT_CHARS".into(), "50000".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.extract_max_input_chars, 50_000);
    }

    #[test]
    fn extract_max_input_chars_rejects_zero() {
        let mut vars = base_vars();
        vars.insert("EXTRACT_MAX_INPUT_CHARS".into(), "0".into());
        assert!(AppConfig::from_vars(&vars).is_err());
    }

    #[test]
    fn parse_deadline_secs_rejects_zero() {
        let mut vars = base_vars();
        vars.insert("PARSE_DEADLINE_SECS".into(), "0".into());
        let err = AppConfig::from_vars(&vars).unwrap_err().to_string();
        assert!(err.contains("PARSE_DEADLINE_SECS"), "got: {err}");
    }

    #[test]
    fn paddleocr_mode_defaults_to_fallback_when_url_absent() {
        let vars = base_vars();
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.paddleocr_mode, PaddleOcrMode::Fallback);
    }

    #[test]
    fn paddleocr_mode_defaults_to_auto_when_url_set() {
        let mut vars = base_vars();
        vars.insert("PADDLEOCR_URL".into(), "http://localhost:8868".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.paddleocr_mode, PaddleOcrMode::Auto);
    }

    #[test]
    fn paddleocr_mode_parses_primary() {
        let mut vars = base_vars();
        vars.insert("PADDLEOCR_MODE".into(), "primary".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.paddleocr_mode, PaddleOcrMode::Primary);
    }

    #[test]
    fn paddleocr_mode_parses_auto_explicit_even_without_url() {
        let mut vars = base_vars();
        vars.insert("PADDLEOCR_MODE".into(), "auto".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.paddleocr_mode, PaddleOcrMode::Auto);
    }

    #[test]
    fn paddleocr_mode_parses_fallback_explicit_with_url() {
        // Explicit override beats the URL-driven default.
        let mut vars = base_vars();
        vars.insert("PADDLEOCR_URL".into(), "http://localhost:8868".into());
        vars.insert("PADDLEOCR_MODE".into(), "fallback".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.paddleocr_mode, PaddleOcrMode::Fallback);
    }

    #[test]
    fn paddleocr_mode_parses_primary_case_insensitive() {
        let mut vars = base_vars();
        vars.insert("PADDLEOCR_MODE".into(), "PRIMARY".into());
        assert_eq!(
            AppConfig::from_vars(&vars).unwrap().paddleocr_mode,
            PaddleOcrMode::Primary
        );
    }

    #[test]
    fn paddleocr_mode_rejects_invalid_value() {
        let mut vars = base_vars();
        vars.insert("PADDLEOCR_MODE".into(), "maybe".into());
        let err = AppConfig::from_vars(&vars).unwrap_err().to_string();
        assert!(err.contains("PADDLEOCR_MODE"), "got: {err}");
    }

    #[test]
    fn anthropic_api_key_is_none_when_var_absent() {
        let vars = base_vars(); // no ANTHROPIC_API_KEY entry
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert!(
            cfg.anthropic_api_key.is_none(),
            "missing ANTHROPIC_API_KEY must produce None"
        );
    }

    #[test]
    fn anthropic_api_key_is_none_when_var_is_empty_string() {
        let mut vars = base_vars();
        vars.insert("ANTHROPIC_API_KEY".into(), "".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert!(
            cfg.anthropic_api_key.is_none(),
            "empty ANTHROPIC_API_KEY must produce None, not Some(\"\")"
        );
    }

    #[test]
    fn anthropic_api_key_is_some_when_var_is_valid_key() {
        let mut vars = base_vars();
        vars.insert("ANTHROPIC_API_KEY".into(), "sk-ant-api03-testkey123".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(
            cfg.anthropic_api_key.as_deref(),
            Some("sk-ant-api03-testkey123"),
            "valid ANTHROPIC_API_KEY must be stored as Some"
        );
    }

    #[test]
    fn anthropic_api_key_preserves_key_value_exactly() {
        let key = "sk-ant-api03-abcdefghijklmnop";
        let mut vars = base_vars();
        vars.insert("ANTHROPIC_API_KEY".into(), key.into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.anthropic_api_key.as_deref(), Some(key));
    }

    #[test]
    fn anthropic_api_key_whitespace_only_is_treated_as_non_empty() {
        // The filter only checks `is_empty()`, not whitespace — document the actual behaviour.
        // A key of "   " is unusual but not our concern to reject here (that's the API's job).
        let mut vars = base_vars();
        vars.insert("ANTHROPIC_API_KEY".into(), "   ".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        // "   " is non-empty, so it should be Some("   ")
        assert_eq!(
            cfg.anthropic_api_key.as_deref(),
            Some("   "),
            "whitespace-only key is non-empty and must be preserved as Some"
        );
    }

    // ── Clerk + dev-bypass parsing ─────────────────────────────────────────

    #[test]
    fn clerk_defaults_to_disabled() {
        let cfg = AppConfig::from_vars(&base_vars()).unwrap();
        assert_eq!(cfg.clerk_jwks_url, None);
        assert_eq!(cfg.clerk_issuer, None);
        assert_eq!(cfg.clerk_leeway_secs, 30);
        assert!(!cfg.dev_auth_bypass);
    }

    #[test]
    fn clerk_url_requires_issuer() {
        let mut vars = base_vars();
        vars.insert(
            "CLERK_JWKS_URL".into(),
            "https://test.clerk.accounts.dev/.well-known/jwks.json".into(),
        );
        let err = AppConfig::from_vars(&vars).unwrap_err().to_string();
        assert!(err.contains("CLERK_ISSUER"), "got: {err}");
    }

    #[test]
    fn clerk_url_with_issuer_succeeds() {
        let mut vars = base_vars();
        vars.insert(
            "CLERK_JWKS_URL".into(),
            "https://test.clerk.accounts.dev/.well-known/jwks.json".into(),
        );
        vars.insert("CLERK_ISSUER".into(), "https://test.clerk.accounts.dev".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert!(cfg.clerk_jwks_url.is_some());
        assert_eq!(
            cfg.clerk_issuer.as_deref(),
            Some("https://test.clerk.accounts.dev")
        );
    }

    #[test]
    fn clerk_leeway_parses_custom_value() {
        let mut vars = base_vars();
        vars.insert("CLERK_LEEWAY_SECS".into(), "60".into());
        let cfg = AppConfig::from_vars(&vars).unwrap();
        assert_eq!(cfg.clerk_leeway_secs, 60);
    }

    #[test]
    fn dev_auth_bypass_accepts_truthy_strings() {
        for v in &["true", "TRUE", "True", "1", "yes", "YES"] {
            let mut vars = base_vars();
            vars.insert("DEV_AUTH_BYPASS".into(), (*v).into());
            let cfg = AppConfig::from_vars(&vars).unwrap();
            assert!(cfg.dev_auth_bypass, "value {v:?} should enable bypass");
        }
    }

    #[test]
    fn dev_auth_bypass_rejects_falsy_strings() {
        for v in &["false", "0", "no", "off", ""] {
            let mut vars = base_vars();
            vars.insert("DEV_AUTH_BYPASS".into(), (*v).into());
            let cfg = AppConfig::from_vars(&vars).unwrap();
            assert!(!cfg.dev_auth_bypass, "value {v:?} must NOT enable bypass");
        }
    }

    #[test]
    fn dev_auth_bypass_rejected_when_combined_with_clerk_url() {
        // Hard safety: a deploy that accidentally leaves
        // DEV_AUTH_BYPASS=true in its env and ALSO wires Clerk must
        // refuse to start. This is the production guardrail that
        // makes header impersonation impossible by config.
        let mut vars = base_vars();
        vars.insert("DEV_AUTH_BYPASS".into(), "true".into());
        vars.insert(
            "CLERK_JWKS_URL".into(),
            "https://prod.clerk.accounts.dev/.well-known/jwks.json".into(),
        );
        vars.insert("CLERK_ISSUER".into(), "https://prod.clerk.accounts.dev".into());
        let err = AppConfig::from_vars(&vars).unwrap_err().to_string();
        assert!(
            err.contains("DEV_AUTH_BYPASS") && err.contains("mutually exclusive"),
            "expected mutually-exclusive error, got: {err}"
        );
    }
}
