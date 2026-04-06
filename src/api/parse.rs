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
use crate::services::parse_gate::ParseGate;
use crate::services::{ocr, pdf_parser};
use std::sync::Arc;
use std::time::Duration;

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
#[tracing::instrument(skip(auth, payload, pool, config, gate, req_id))]
pub async fn parse_pdf(
    auth: ApiKeyAuth,
    mut payload: Multipart,
    pool: web::Data<SqlitePool>,
    config: web::Data<AppConfig>,
    gate: web::Data<ParseGate>,
    req_id: web::ReqData<RequestId>,
) -> Result<HttpResponse, AppError> {
    // Acquire a concurrency permit BEFORE the billing check or any
    // expensive work. The permit is held until the dispatcher returns
    // (the `_permit` binding lives for the function scope), so even if
    // the deadline drops the inner future the gate keeps reflecting
    // real in-flight work.
    let _permit = gate.try_acquire().map_err(|_| AppError::ServiceBusy)?;

    let status = crate::services::billing::check_usage_limit(pool.get_ref(), &auth.api_key_id).await?;
    if !status.allowed {
        return Err(AppError::QuotaExceeded(format!(
            "Monthly limit of {} pages exceeded ({} used). Upgrade at /billing/plans",
            status.limit, status.used
        )));
    }

    let (bytes, document_type_hint) = extract_parse_upload(&mut payload).await?;

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
    let result = pdf_parser::parse_pdf_with_backends_mode(
        bytes,
        &ocr_config,
        paddle_config.as_ref(),
        config.paddleocr_mode,
        document_type_hint,
        Duration::from_secs(config.parse_deadline_secs),
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

/// Read a parse upload: PDF bytes plus an optional `document_type_hint`
/// field.
///
/// Accepted field names:
/// * `file` or an unnamed first field → PDF payload
/// * `document_type_hint` → optional type hint (see `DocType::from_hint_str`)
///
/// Any other named field is rejected with `AppError::Validation` so the
/// wire contract stays narrow and consistent with `/v1/extract`.
pub async fn extract_parse_upload(
    payload: &mut Multipart,
) -> Result<(pdf_parser::PdfBytes, Option<DocType>), AppError> {
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut hint: Option<DocType> = None;

    while let Some(item) = payload.next().await {
        let mut field = item.map_err(|e| {
            tracing::warn!(error = %e, "Multipart field error");
            AppError::Validation("Invalid file upload".into())
        })?;

        let field_name = field.name().map(|n| n.to_string()).unwrap_or_default();
        let is_hint = field_name == "document_type_hint";
        let is_file = field_name.is_empty() || field_name == "file";
        if !is_hint && !is_file {
            return Err(AppError::Validation(format!(
                "Unexpected multipart field: {field_name}"
            )));
        }
        let size_limit = if is_hint { MAX_HINT_BYTES } else { MAX_FILE_SIZE };

        let mut data = Vec::new();
        while let Some(chunk) = field.next().await {
            let bytes = chunk.map_err(|e| {
                tracing::warn!(error = %e, "Multipart chunk read error");
                AppError::Validation("Failed to read uploaded file".into())
            })?;
            if data.len() + bytes.len() > size_limit {
                if is_hint {
                    return Err(AppError::Validation(
                        "document_type_hint is too long".into(),
                    ));
                }
                return Err(AppError::FileTooLarge);
            }
            data.extend_from_slice(&bytes);
        }

        if is_hint {
            let s = String::from_utf8(data).map_err(|_| {
                AppError::Validation("document_type_hint must be valid UTF-8".into())
            })?;
            hint = DocType::from_hint_str(&s);
            if hint.is_none() && !s.trim().is_empty() {
                // escape_debug prevents ANSI / control chars in a crafted
                // hint from muddying log viewers.
                tracing::info!(
                    raw = %s.escape_debug(),
                    "document_type_hint could not be parsed into a known type; ignoring"
                );
            }
        } else if file_bytes.is_none() {
            file_bytes = Some(data);
        }
    }

    let bytes = file_bytes.ok_or_else(|| AppError::Validation("No file uploaded".into()))?;
    if bytes.len() < 64 || &bytes[..5] != PDF_MAGIC {
        return Err(AppError::InvalidPdf);
    }
    // Single Vec → Arc materialization at the boundary. Every downstream
    // consumer takes &PdfBytes / &[u8] and pays only Arc-clone cost.
    Ok((Arc::<[u8]>::from(bytes), hint))
}
