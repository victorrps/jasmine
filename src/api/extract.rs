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
use crate::services::{ocr, pdf_parser, schema_extractor};

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
#[tracing::instrument(skip(auth, payload, pool, config, req_id))]
pub async fn extract_pdf(
    auth: ApiKeyAuth,
    mut payload: Multipart,
    pool: web::Data<SqlitePool>,
    config: web::Data<AppConfig>,
    req_id: web::ReqData<RequestId>,
) -> Result<HttpResponse, AppError> {
    let status = crate::services::billing::check_usage_limit(pool.get_ref(), &auth.api_key_id).await?;
    if !status.allowed {
        return Err(AppError::QuotaExceeded(format!(
            "Monthly limit of {} pages exceeded ({} used). Upgrade at /billing/plans",
            status.limit, status.used
        )));
    }

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut schema_str: Option<String> = None;

    while let Some(item) = payload.next().await {
        let mut field = item.map_err(|e| {
            tracing::warn!(error = %e, "Multipart field error");
            AppError::Validation("Invalid file upload".into())
        })?;

        let field_name = field.name().map(|n| n.to_string()).unwrap_or_default();

        let size_limit = if field_name == "schema" {
            MAX_SCHEMA_SIZE
        } else {
            MAX_FILE_SIZE
        };
        let mut data = Vec::new();
        while let Some(chunk) = field.next().await {
            let bytes = chunk.map_err(|e| {
                tracing::warn!(error = %e, "Multipart chunk read error");
                AppError::Validation("Failed to read uploaded file".into())
            })?;
            if data.len() + bytes.len() > size_limit {
                if field_name == "schema" {
                    return Err(AppError::Validation(
                        "Schema exceeds maximum size of 64KB".into(),
                    ));
                }
                return Err(AppError::FileTooLarge);
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
            _ => {}
        }
    }

    let bytes = file_bytes.ok_or_else(|| AppError::Validation("No file uploaded".into()))?;
    if bytes.len() < 64 || &bytes[..5] != PDF_MAGIC {
        return Err(AppError::InvalidPdf);
    }

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
    let parse_result = pdf_parser::parse_pdf_with_backends(
        bytes,
        &ocr_config,
        paddle_config.as_ref(),
    )
    .await?;

    let extracted = schema_extractor::extract_with_schema(
        &parse_result.document.markdown,
        &schema,
        config.anthropic_api_key.as_deref(),
    )
    .await?;

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
