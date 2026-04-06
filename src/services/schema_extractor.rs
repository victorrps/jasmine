use serde::{Deserialize, Serialize};
use std::time::Instant;

use crate::errors::AppError;

/// Result of schema-based extraction.
#[derive(Debug, Serialize)]
pub struct ExtractResult {
    pub data: serde_json::Value,
    pub model: String,
    pub processing_ms: u64,
    pub warning: Option<String>,
}

/// Anthropic Messages API request body.
#[derive(Debug, Serialize)]
struct ClaudeRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<ClaudeMessage>,
}

#[derive(Debug, Serialize)]
struct ClaudeMessage {
    role: String,
    content: String,
}

/// Anthropic Messages API response.
#[derive(Debug, Deserialize)]
struct ClaudeResponse {
    content: Vec<ClaudeContentBlock>,
    model: String,
}

#[derive(Debug, Deserialize)]
struct ClaudeContentBlock {
    text: Option<String>,
}

const CLAUDE_API_URL: &str = "https://api.anthropic.com/v1/messages";
const CLAUDE_MODEL: &str = "claude-haiku-4-5-20251001";

/// Strip markdown code fences from a string returned by Claude.
///
/// Handles three cases:
/// - ` ```json\n...\n``` ` — JSON-tagged fence
/// - ` ```\n...\n``` `     — untagged fence
/// - No fences             — returned as-is (trimmed)
pub(crate) fn strip_json_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    let after_prefix = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let after_suffix = after_prefix.strip_suffix("```").unwrap_or(after_prefix);
    after_suffix.trim()
}

/// Classify an upstream Claude API error into a safe user-facing message.
fn classify_upstream_error(msg: Option<&str>, status: reqwest::StatusCode) -> String {
    match msg {
        Some(m) if m.contains("credit balance") => {
            "Extraction service billing error — please check your Anthropic API plan".into()
        }
        Some(m) if m.contains("invalid x-api-key") || m.contains("invalid api key") => {
            "Extraction service authentication failed — check ANTHROPIC_API_KEY".into()
        }
        Some(m) if m.contains("overloaded") => {
            "Extraction service is temporarily overloaded — try again later".into()
        }
        _ if status == reqwest::StatusCode::TOO_MANY_REQUESTS => {
            "Extraction service rate limit exceeded — try again later".into()
        }
        _ => format!("Extraction service returned HTTP {status}"),
    }
}

/// Extract structured data from document markdown using Claude Haiku.
/// Falls back to stub if no API key is configured.
pub async fn extract_with_schema(
    markdown: &str,
    schema: &serde_json::Value,
    api_key: Option<&str>,
) -> Result<ExtractResult, AppError> {
    match api_key {
        Some(key) if !key.is_empty() => extract_with_claude(markdown, schema, key).await,
        _ => extract_stub(),
    }
}

/// Real extraction via Claude Haiku API.
async fn extract_with_claude(
    markdown: &str,
    schema: &serde_json::Value,
    api_key: &str,
) -> Result<ExtractResult, AppError> {
    let start = Instant::now();

    let schema_str = serde_json::to_string_pretty(schema)
        .map_err(|e| AppError::Validation(format!("Invalid schema: {e}")))?;

    let prompt = format!(
        "Extract structured data from this document according to the JSON schema below.\n\
         Return ONLY valid JSON matching the schema — no explanation, no markdown fences.\n\n\
         ## JSON Schema\n```json\n{schema_str}\n```\n\n\
         ## Document\n{markdown}"
    );

    let request_body = ClaudeRequest {
        model: CLAUDE_MODEL.into(),
        max_tokens: 4096,
        messages: vec![ClaudeMessage {
            role: "user".into(),
            content: prompt,
        }],
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| AppError::Internal(format!("HTTP client init failed: {e}")))?;
    let response = client
        .post(CLAUDE_API_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Claude API request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        // Parse error body for user-facing message, but never log the raw body (may contain key info)
        let upstream_msg = match response.json::<serde_json::Value>().await {
            Ok(body) => body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string()),
            Err(_) => None,
        };
        let safe_msg = classify_upstream_error(upstream_msg.as_deref(), status);
        tracing::error!(status = %status, "Claude API error");
        return Err(AppError::UpstreamApi(safe_msg));
    }

    let claude_resp: ClaudeResponse = response
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse Claude response: {e}")))?;

    let raw_text = claude_resp
        .content
        .first()
        .and_then(|b| b.text.as_deref())
        .unwrap_or("{}");

    // Strip markdown fences if Claude included them despite instructions
    let json_text = strip_json_fences(raw_text);

    // Parse the JSON — retry once if invalid
    let data = match serde_json::from_str::<serde_json::Value>(json_text) {
        Ok(v) => v,
        Err(first_err) => {
            tracing::warn!(error = %first_err, "First extraction attempt returned invalid JSON, retrying");
            retry_extraction(markdown, schema, api_key, json_text, &first_err.to_string()).await?
        }
    };

    let processing_ms = start.elapsed().as_millis() as u64;

    Ok(ExtractResult {
        data,
        model: claude_resp.model,
        processing_ms,
        warning: None,
    })
}

/// Retry extraction with error feedback.
async fn retry_extraction(
    markdown: &str,
    schema: &serde_json::Value,
    api_key: &str,
    previous_output: &str,
    error_msg: &str,
) -> Result<serde_json::Value, AppError> {
    let schema_str = serde_json::to_string_pretty(schema)
        .map_err(|e| AppError::Validation(format!("Invalid schema: {e}")))?;

    // Truncate previous output to prevent prompt injection and cost doubling
    let max_prev = 500;
    let truncated_output = if previous_output.len() > max_prev {
        &previous_output[..max_prev]
    } else {
        previous_output
    };

    let prompt = format!(
        "Your previous attempt to extract data returned invalid JSON.\n\
         Error: {error_msg}\n\
         Your output was: {truncated_output}\n\n\
         Try again. Return ONLY valid JSON matching this schema — no explanation, no fences.\n\n\
         ## JSON Schema\n```json\n{schema_str}\n```\n\n\
         ## Document\n{markdown}"
    );

    let request_body = ClaudeRequest {
        model: CLAUDE_MODEL.into(),
        max_tokens: 4096,
        messages: vec![ClaudeMessage {
            role: "user".into(),
            content: prompt,
        }],
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| AppError::Internal(format!("HTTP client init failed: {e}")))?;
    let response = client
        .post(CLAUDE_API_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&request_body)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Claude API retry failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let upstream_msg = match response.json::<serde_json::Value>().await {
            Ok(body) => body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string()),
            Err(_) => None,
        };
        let safe_msg = classify_upstream_error(upstream_msg.as_deref(), status);
        return Err(AppError::UpstreamApi(safe_msg));
    }

    let claude_resp: ClaudeResponse = response
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse retry response: {e}")))?;

    let raw_text = claude_resp
        .content
        .first()
        .and_then(|b| b.text.as_deref())
        .unwrap_or("{}");

    let json_text = strip_json_fences(raw_text);

    serde_json::from_str(json_text)
        .map_err(|e| AppError::PdfProcessing(format!("Schema extraction failed after retry: {e}")))
}

/// Stub extraction when no API key is configured.
fn extract_stub() -> Result<ExtractResult, AppError> {
    Ok(ExtractResult {
        data: serde_json::json!({}),
        model: "stub".into(),
        processing_ms: 0,
        warning: Some(
            "Schema extraction requires ANTHROPIC_API_KEY. Set it in .env for real extraction."
                .into(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_extract_stub_when_no_key() {
        let result = extract_with_schema("some markdown", &serde_json::json!({}), None)
            .await
            .unwrap();
        assert_eq!(result.model, "stub");
        assert!(result.warning.is_some());
    }

    #[tokio::test]
    async fn test_extract_stub_when_empty_key() {
        let result = extract_with_schema("some markdown", &serde_json::json!({}), Some(""))
            .await
            .unwrap();
        assert_eq!(result.model, "stub");
    }

    // ── JSON fence stripping tests ────────────────────────────────────────────

    #[test]
    fn strip_json_fences_removes_json_fence() {
        let input = "```json\n{\"key\": \"value\"}\n```";
        let output = strip_json_fences(input);
        assert_eq!(output, "{\"key\": \"value\"}");
    }

    #[test]
    fn strip_json_fences_removes_plain_fence() {
        let input = "```\n{\"key\": \"value\"}\n```";
        let output = strip_json_fences(input);
        assert_eq!(output, "{\"key\": \"value\"}");
    }

    #[test]
    fn strip_json_fences_returns_plain_json_unchanged() {
        let input = "{\"key\": \"value\"}";
        let output = strip_json_fences(input);
        assert_eq!(output, "{\"key\": \"value\"}");
    }

    #[test]
    fn strip_json_fences_handles_leading_and_trailing_whitespace() {
        let input = "  ```json\n{\"a\": 1}\n```  ";
        let output = strip_json_fences(input);
        assert_eq!(output, "{\"a\": 1}");
    }

    #[test]
    fn strip_json_fences_handles_empty_string() {
        let output = strip_json_fences("");
        assert_eq!(output, "");
    }

    #[test]
    fn strip_json_fences_handles_json_fence_without_newline() {
        // Fence prefix immediately followed by content (no newline after opening)
        let input = "```json{\"key\": \"value\"}```";
        let output = strip_json_fences(input);
        // The prefix is stripped; suffix ``` is stripped; result is trimmed inner content
        assert_eq!(output, "{\"key\": \"value\"}");
    }

    // ── Stub fallback — additional coverage ──────────────────────────────────

    #[tokio::test]
    async fn stub_result_data_is_empty_object() {
        let result = extract_with_schema("doc text", &serde_json::json!({}), None)
            .await
            .unwrap();
        assert_eq!(result.data, serde_json::json!({}));
    }

    #[tokio::test]
    async fn stub_result_processing_ms_is_zero() {
        let result = extract_with_schema("doc text", &serde_json::json!({}), None)
            .await
            .unwrap();
        assert_eq!(result.processing_ms, 0);
    }

    #[tokio::test]
    async fn stub_warning_message_mentions_api_key() {
        let result = extract_with_schema("doc text", &serde_json::json!({}), None)
            .await
            .unwrap();
        let warning = result.warning.expect("stub must have a warning");
        assert!(
            warning.contains("ANTHROPIC_API_KEY"),
            "warning should mention ANTHROPIC_API_KEY, got: {warning}"
        );
    }

    #[tokio::test]
    async fn empty_string_key_falls_back_to_stub_with_warning() {
        let result = extract_with_schema("doc text", &serde_json::json!({}), Some(""))
            .await
            .unwrap();
        assert_eq!(result.model, "stub");
        assert!(
            result.warning.is_some(),
            "stub fallback must include warning"
        );
    }
}
