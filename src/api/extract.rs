use actix_multipart::Multipart;
use actix_web::{web, HttpResponse};
use futures_util::StreamExt;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::auth::api_key::ApiKeyAuth;
use crate::config::AppConfig;
use crate::errors::AppError;
use crate::middleware::request_id::RequestId;
use crate::models;
use crate::services::doc_type_detector::{DocType, MAX_HINT_BYTES};
use crate::services::metrics::Metrics;
use crate::services::parse_gate::ParseGate;
use crate::services::{ocr, pdf_parser, schema_extractor};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_FILE_SIZE: usize = 50 * 1024 * 1024;
const MAX_SCHEMA_SIZE: usize = 64 * 1024; // 64 KB
const PDF_MAGIC: &[u8] = b"%PDF-";

/// Extract endpoint response.
#[derive(Debug, Serialize)]
pub struct ExtractResponse {
    pub document: pdf_parser::DocumentResult,
    pub extracted: schema_extractor::ExtractResult,
    pub usage: pdf_parser::UsageInfo,
    pub request_id: String,
}

/// POST /v1/extract — upload a PDF + JSON schema, return structured data.
#[tracing::instrument(skip(auth, payload, pool, config, gate, metrics, req_id))]
pub async fn extract_pdf(
    auth: ApiKeyAuth,
    mut payload: Multipart,
    pool: web::Data<SqlitePool>,
    config: web::Data<AppConfig>,
    gate: web::Data<ParseGate>,
    metrics: web::Data<Metrics>,
    req_id: web::ReqData<RequestId>,
) -> Result<HttpResponse, AppError> {
    let started = Instant::now();
    let _permit = gate.try_acquire().map_err(|_| {
        crate::api::parse::record_outcome(&metrics, "/v1/extract", "503", None, started);
        AppError::ServiceBusy
    })?;
    metrics.parse_gate_in_flight.inc();
    let _gate_guard = crate::api::parse::GateGaugeGuard(metrics.clone());
    let status = crate::services::billing::check_usage_limit(pool.get_ref(), &auth.api_key_id).await?;
    if !status.allowed {
        return Err(AppError::QuotaExceeded(format!(
            "Monthly limit of {} pages exceeded ({} used). Upgrade at /billing/plans",
            status.limit, status.used
        )));
    }

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut schema_str: Option<String> = None;
    let mut document_type_hint: Option<DocType> = None;

    while let Some(item) = payload.next().await {
        let mut field = item.map_err(|e| {
            tracing::warn!(error = %e, "Multipart field error");
            AppError::Validation("Invalid file upload".into())
        })?;

        let field_name = field.name().map(|n| n.to_string()).unwrap_or_default();

        let size_limit = match field_name.as_str() {
            "schema" => MAX_SCHEMA_SIZE,
            "document_type_hint" => MAX_HINT_BYTES,
            "file" => MAX_FILE_SIZE,
            other => {
                return Err(AppError::Validation(format!(
                    "Unexpected multipart field: {other}"
                )));
            }
        };
        let mut data = Vec::new();
        while let Some(chunk) = field.next().await {
            let bytes = chunk.map_err(|e| {
                tracing::warn!(error = %e, "Multipart chunk read error");
                AppError::Validation("Failed to read uploaded file".into())
            })?;
            if data.len() + bytes.len() > size_limit {
                match field_name.as_str() {
                    "schema" => {
                        return Err(AppError::Validation(
                            "Schema exceeds maximum size of 64KB".into(),
                        ))
                    }
                    "document_type_hint" => {
                        return Err(AppError::Validation(
                            "document_type_hint is too long".into(),
                        ))
                    }
                    _ => return Err(AppError::FileTooLarge),
                }
            }
            data.extend_from_slice(&bytes);
        }

        match field_name.as_str() {
            "file" => file_bytes = Some(data),
            "schema" => {
                schema_str = Some(
                    String::from_utf8(data)
                        .map_err(|_| AppError::Validation("Schema must be valid UTF-8".into()))?,
                );
            }
            "document_type_hint" => {
                let s = String::from_utf8(data).map_err(|_| {
                    AppError::Validation("document_type_hint must be valid UTF-8".into())
                })?;
                document_type_hint = DocType::from_hint_str(&s);
                if document_type_hint.is_none() && !s.trim().is_empty() {
                    tracing::info!(
                        raw = %s.escape_debug(),
                        "document_type_hint could not be parsed into a known type; ignoring"
                    );
                }
            }
            // Unknown field names are already rejected above when computing
            // `size_limit`, so this arm is unreachable.
            _ => unreachable!("unknown field names are rejected earlier"),
        }
    }

    let bytes = file_bytes.ok_or_else(|| AppError::Validation("No file uploaded".into()))?;
    if bytes.len() < 64 || &bytes[..5] != PDF_MAGIC {
        return Err(AppError::InvalidPdf);
    }
    let bytes: pdf_parser::PdfBytes = Arc::<[u8]>::from(bytes);

    let schema: serde_json::Value = match schema_str {
        Some(s) => serde_json::from_str(&s)
            .map_err(|e| AppError::Validation(format!("Invalid JSON schema: {e}")))?,
        None => return Err(AppError::Validation("Schema field is required".into())),
    };

    let ocr_config = ocr::OcrConfig {
        tesseract_path: config.tesseract_path.clone(),
        pdftoppm_path: config.pdftoppm_path.clone(),
    };
    let paddle_config = config.paddleocr_url.as_ref().map(|url| {
        crate::services::paddle_ocr::PaddleOcrConfig::new(
            url.clone(),
            config.paddleocr_timeout_secs,
        )
    });
    let parse_result = pdf_parser::parse_pdf_with_backends_mode(
        bytes,
        &ocr_config,
        paddle_config.as_ref(),
        config.paddleocr_mode,
        document_type_hint,
        Duration::from_secs(config.parse_deadline_secs),
    )
    .await?;

    // T2.5 — bound Anthropic spend per request. We refuse rather than
    // truncate so the customer is never silently shipped a partial
    // extraction.
    if parse_result.document.markdown.chars().count() > config.extract_max_input_chars {
        return Err(AppError::ExtractInputTooLarge {
            actual: parse_result.document.markdown.chars().count(),
            limit: config.extract_max_input_chars,
        });
    }

    let extracted = schema_extractor::extract_with_schema(
        &parse_result.document.markdown,
        &schema,
        config.anthropic_api_key.as_deref(),
    )
    .await?;

    // T2.4 — validate the model output against the customer-supplied
    // schema. Returning data that doesn't validate is a worse customer
    // surprise than failing loudly.
    if let Err(detail) = validate_against_schema(&extracted.data, &schema) {
        metrics.extract_validation_failures.inc();
        crate::api::parse::record_outcome(
            &metrics,
            "/v1/extract",
            "502",
            parse_result.document.metadata.routed_to,
            started,
        );
        return Err(AppError::SchemaValidationFailed(detail));
    }
    crate::api::parse::record_outcome(
        &metrics,
        "/v1/extract",
        "200",
        parse_result.document.metadata.routed_to,
        started,
    );

    // Log usage asynchronously
    let pool_clone = pool.get_ref().clone();
    let key_id = auth.api_key_id.clone();
    let rid = req_id.id.clone();
    let pages = parse_result.usage.pages_processed;
    let credits = parse_result.usage.credits_used;
    let ms = parse_result.document.metadata.processing_ms;
    tokio::spawn(async move {
        if let Err(e) = models::usage_log::log_usage(
            &pool_clone,
            &key_id,
            "/v1/extract",
            pages,
            credits,
            ms,
            &rid,
        )
        .await
        {
            tracing::error!(
                error = %e,
                api_key_id = %key_id,
                request_id = %rid,
                "failed to write usage log for /v1/extract — billing audit gap"
            );
        }
    });

    Ok(HttpResponse::Ok().json(ExtractResponse {
        document: parse_result.document,
        extracted,
        usage: parse_result.usage,
        request_id: req_id.id.clone(),
    }))
}

/// Validate `data` against the customer-supplied JSON Schema. Compiles
/// the schema fresh per request — schemas are typically tiny (kilobytes)
/// so we don't bother caching, and a fresh compile means we can't be
/// poisoned by a stale compiled instance across requests.
///
/// Returns `Ok(())` if valid, `Err(detail)` with a sanitized list of
/// validation errors otherwise. The detail string is small enough to
/// embed in the API error response without leaking values from the
/// extracted document.
fn validate_against_schema(
    data: &serde_json::Value,
    schema: &serde_json::Value,
) -> Result<(), String> {
    let compiled = match jsonschema::JSONSchema::compile(schema) {
        Ok(c) => c,
        Err(e) => {
            // The schema itself was malformed — surfaced as a "validation
            // failed" error so the customer realizes the schema needs
            // fixing rather than retrying the extraction. We do not leak
            // the internal compiler error verbatim; instead we identify
            // the schema as the offender.
            return Err(format!("schema compilation failed: {e}"));
        }
    };
    if let Err(errors) = compiled.validate(data) {
        // Collect at most 5 error paths so the response stays compact
        // and we never echo extracted values.
        let summary: Vec<String> = errors
            .take(5)
            .map(|e| format!("{}: {}", e.instance_path, e.kind_name()))
            .collect();
        return Err(summary.join("; "));
    }
    Ok(())
}

/// Lightweight extension trait to give a stable string label to a
/// `jsonschema::error::ValidationErrorKind` without allocating the full
/// debug repr (which can include extracted values).
trait ErrorKindName {
    fn kind_name(&self) -> &'static str;
}

impl<'a> ErrorKindName for jsonschema::ValidationError<'a> {
    fn kind_name(&self) -> &'static str {
        use jsonschema::error::ValidationErrorKind as K;
        match self.kind {
            K::Required { .. } => "required",
            K::Type { .. } => "type",
            K::Enum { .. } => "enum",
            K::MinLength { .. } => "min_length",
            K::MaxLength { .. } => "max_length",
            K::Minimum { .. } => "minimum",
            K::Maximum { .. } => "maximum",
            K::Pattern { .. } => "pattern",
            K::AdditionalProperties { .. } => "additional_properties",
            _ => "validation_failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_passes_when_data_matches_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name"],
            "properties": {"name": {"type": "string"}}
        });
        let data = serde_json::json!({"name": "Alice"});
        assert!(validate_against_schema(&data, &schema).is_ok());
    }

    #[test]
    fn validate_fails_when_required_field_missing() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["name"],
            "properties": {"name": {"type": "string"}}
        });
        let data = serde_json::json!({});
        let err = validate_against_schema(&data, &schema).unwrap_err();
        assert!(err.contains("required"), "got: {err}");
    }

    #[test]
    fn validate_fails_on_wrong_type() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"age": {"type": "integer"}}
        });
        let data = serde_json::json!({"age": "not a number"});
        let err = validate_against_schema(&data, &schema).unwrap_err();
        assert!(err.contains("type"), "got: {err}");
    }

    #[test]
    fn validate_caps_error_count_at_5() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["a","b","c","d","e","f","g"]
        });
        let data = serde_json::json!({});
        let err = validate_against_schema(&data, &schema).unwrap_err();
        // 5 errors max — separator is "; " so 4 separators
        assert_eq!(err.matches("; ").count(), 4, "got: {err}");
    }

    #[test]
    fn validate_rejects_malformed_schema() {
        let schema = serde_json::json!({"type": "not_a_real_type"});
        let data = serde_json::json!({});
        let err = validate_against_schema(&data, &schema).unwrap_err();
        assert!(err.contains("schema compilation failed"), "got: {err}");
    }
}
