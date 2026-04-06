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
use crate::services::{ocr, pdf_parser};

const MAX_FILE_SIZE: usize = 50 * 1024 * 1024; // 50 MB
const PDF_MAGIC: &[u8] = b"%PDF-";

/// Full parse response envelope.
#[derive(Debug, Serialize)]
pub struct ParseResponse {
    pub document: pdf_parser::DocumentResult,
    pub usage: pdf_parser::UsageInfo,
    pub request_id: String,
}

/// POST /v1/parse — upload a PDF and receive structured output.
#[tracing::instrument(skip(auth, payload, pool, config, req_id))]
pub async fn parse_pdf(
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

    let bytes = extract_pdf_bytes(&mut payload).await?;

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
    let result = pdf_parser::parse_pdf_with_backends(
        bytes,
        &ocr_config,
        paddle_config.as_ref(),
    )
    .await?;

    // Log usage asynchronously
    let pool_clone = pool.get_ref().clone();
    let key_id = auth.api_key_id.clone();
    let rid = req_id.id.clone();
    let pages = result.usage.pages_processed;
    let credits = result.usage.credits_used;
    let ms = result.document.metadata.processing_ms;
    tokio::spawn(async move {
        if let Err(e) = models::usage_log::log_usage(
            &pool_clone,
            &key_id,
            "/v1/parse",
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
                "failed to write usage log for /v1/parse — billing audit gap"
            );
        }
    });

    Ok(HttpResponse::Ok().json(ParseResponse {
        document: result.document,
        usage: result.usage,
        request_id: req_id.id.clone(),
    }))
}

/// Extract and validate PDF bytes from a multipart upload.
pub async fn extract_pdf_bytes(payload: &mut Multipart) -> Result<Vec<u8>, AppError> {
    let mut bytes = Vec::new();

    if let Some(item) = payload.next().await {
        let mut field = item.map_err(|e| {
            tracing::warn!(error = %e, "Multipart field error");
            AppError::Validation("Invalid file upload".into())
        })?;

        while let Some(chunk) = field.next().await {
            let data = chunk.map_err(|e| {
                tracing::warn!(error = %e, "Multipart chunk read error");
                AppError::Validation("Failed to read uploaded file".into())
            })?;
            if bytes.len() + data.len() > MAX_FILE_SIZE {
                return Err(AppError::FileTooLarge);
            }
            bytes.extend_from_slice(&data);
        }
    }

    if bytes.is_empty() {
        return Err(AppError::Validation("No file uploaded".into()));
    }

    if bytes.len() < 64 || &bytes[..5] != PDF_MAGIC {
        return Err(AppError::InvalidPdf);
    }

    Ok(bytes)
}
