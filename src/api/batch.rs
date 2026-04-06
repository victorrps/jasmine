use actix_multipart::Multipart;
use actix_web::{web, HttpResponse};
use futures_util::StreamExt;
use serde::Serialize;
use sqlx::SqlitePool;
use std::time::Instant;

use crate::auth::api_key::ApiKeyAuth;
use crate::errors::AppError;
use crate::middleware::request_id::RequestId;
use crate::models;
use crate::services::pdf_parser;

const MAX_FILE_SIZE: usize = 50 * 1024 * 1024; // 50 MB per file
const MAX_SYNC_FILES: usize = 10;
const MAX_ASYNC_FILES: usize = 50;
const PDF_MAGIC: &[u8] = b"%PDF-";

// ── Response types ────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct BatchParseResponse {
    pub batch_id: String,
    pub results: Vec<BatchItemResult>,
    pub summary: BatchSummary,
    pub request_id: String,
}

#[derive(Debug, Serialize)]
pub struct BatchItemResult {
    pub index: usize,
    pub file_name: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<pdf_parser::DocumentResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<pdf_parser::UsageInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BatchSummary {
    pub total: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub total_pages: u32,
    pub total_credits: u32,
    pub processing_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct AsyncBatchResponse {
    pub batch_id: String,
    pub status: String,
    pub total_files: u32,
    pub request_id: String,
}

#[derive(Debug, Serialize)]
pub struct BatchStatusResponse {
    pub batch_id: String,
    pub status: String,
    pub total_files: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub results: Vec<BatchStatusItem>,
    pub request_id: String,
}

#[derive(Debug, Serialize)]
pub struct BatchStatusItem {
    pub index: i32,
    pub file_name: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Multipart extraction ──────────────────────────────────────────────────

/// A single file extracted from the multipart payload.
struct ExtractedFile {
    index: usize,
    name: Option<String>,
    bytes: Vec<u8>,
}

/// Extract all PDF files from the multipart payload, up to `max_files`.
async fn extract_all_pdfs(
    payload: &mut Multipart,
    max_files: usize,
) -> Result<Vec<ExtractedFile>, AppError> {
    let mut files = Vec::new();
    let mut index = 0usize;

    while let Some(item) = payload.next().await {
        if index >= max_files {
            return Err(AppError::Validation(format!(
                "Too many files: maximum is {max_files}"
            )));
        }

        let mut field = item.map_err(|e| {
            tracing::warn!(error = %e, "Multipart field error");
            AppError::Validation("Invalid file upload".into())
        })?;

        let file_name = field
            .content_disposition()
            .and_then(|cd| cd.get_filename().map(|s| s.to_string()));

        let mut bytes = Vec::new();
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

        if bytes.is_empty() {
            continue; // skip empty fields
        }

        files.push(ExtractedFile {
            index,
            name: file_name,
            bytes,
        });
        index += 1;
    }

    if files.is_empty() {
        return Err(AppError::Validation("No files uploaded".into()));
    }

    Ok(files)
}

/// Validate that the given bytes are a valid PDF (magic bytes check).
fn validate_pdf_bytes(bytes: &[u8]) -> Result<(), AppError> {
    if bytes.len() < 64 || &bytes[..5] != PDF_MAGIC {
        return Err(AppError::InvalidPdf);
    }
    Ok(())
}

// ── POST /v1/parse/batch — synchronous batch ─────────────────────────────

/// Parse up to 10 PDFs synchronously and return all results.
#[tracing::instrument(skip(auth, payload, pool, req_id))]
pub async fn batch_parse(
    auth: ApiKeyAuth,
    mut payload: Multipart,
    pool: web::Data<SqlitePool>,
    req_id: web::ReqData<RequestId>,
) -> Result<HttpResponse, AppError> {
    let status = crate::services::billing::check_usage_limit(pool.get_ref(), &auth.api_key_id).await?;
    if !status.allowed {
        return Err(AppError::QuotaExceeded(format!(
            "Monthly limit of {} pages exceeded ({} used). Upgrade at /billing/plans",
            status.limit, status.used
        )));
    }

    let start = Instant::now();
    let files = extract_all_pdfs(&mut payload, MAX_SYNC_FILES).await?;
    let batch_id = format!("batch_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);

    // Validate all files before processing
    for f in &files {
        validate_pdf_bytes(&f.bytes).map_err(|_| {
            AppError::Validation(format!(
                "File at index {} is not a valid PDF",
                f.index
            ))
        })?;
    }

    // Process all files in parallel via spawn_blocking
    let handles: Vec<_> = files
        .into_iter()
        .map(|f| {
            let idx = f.index;
            let name = f.name.clone();
            let bytes = f.bytes;
            tokio::task::spawn_blocking(move || (idx, name, pdf_parser::parse_pdf(bytes, "pdftoppm")))
        })
        .collect();

    let joined = futures_util::future::join_all(handles).await;

    let mut results = Vec::with_capacity(joined.len());
    let mut succeeded = 0u32;
    let mut failed = 0u32;
    let mut total_pages = 0u32;
    let mut total_credits = 0u32;

    for join_result in joined {
        let (idx, name, parse_result) = join_result
            .map_err(|e| AppError::Internal(format!("Task join error: {e}")))?;

        match parse_result {
            Ok(parsed) => {
                total_pages += parsed.usage.pages_processed;
                total_credits += parsed.usage.credits_used;
                succeeded += 1;
                results.push(BatchItemResult {
                    index: idx,
                    file_name: name,
                    status: "success".into(),
                    document: Some(parsed.document),
                    usage: Some(parsed.usage),
                    error: None,
                });
            }
            Err(e) => {
                failed += 1;
                results.push(BatchItemResult {
                    index: idx,
                    file_name: name,
                    status: "error".into(),
                    document: None,
                    usage: None,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    // Sort by index to maintain deterministic order
    results.sort_by_key(|r| r.index);

    let processing_ms = start.elapsed().as_millis() as u64;
    let total = succeeded + failed;

    // Log aggregate usage asynchronously
    let pool_clone = pool.get_ref().clone();
    let key_id = auth.api_key_id.clone();
    let rid = req_id.id.clone();
    tokio::spawn(async move {
        if let Err(e) = models::usage_log::log_usage(
            &pool_clone,
            &key_id,
            "/v1/parse/batch",
            total_pages,
            total_credits,
            processing_ms,
            &rid,
        )
        .await
        {
            tracing::error!(
                error = %e,
                api_key_id = %key_id,
                request_id = %rid,
                "failed to write usage log for /v1/parse/batch — billing audit gap"
            );
        }
    });

    Ok(HttpResponse::Ok().json(BatchParseResponse {
        batch_id,
        results,
        summary: BatchSummary {
            total,
            succeeded,
            failed,
            total_pages,
            total_credits,
            processing_ms,
        },
        request_id: req_id.id.clone(),
    }))
}

// ── POST /v1/parse/batch/async — asynchronous batch ──────────────────────

/// Accept up to 50 PDFs, store a batch job, process in background.
#[tracing::instrument(skip(auth, payload, pool, req_id))]
pub async fn batch_parse_async(
    auth: ApiKeyAuth,
    mut payload: Multipart,
    pool: web::Data<SqlitePool>,
    req_id: web::ReqData<RequestId>,
) -> Result<HttpResponse, AppError> {
    let status = crate::services::billing::check_usage_limit(pool.get_ref(), &auth.api_key_id).await?;
    if !status.allowed {
        return Err(AppError::QuotaExceeded(format!(
            "Monthly limit of {} pages exceeded ({} used). Upgrade at /billing/plans",
            status.limit, status.used
        )));
    }

    let files = extract_all_pdfs(&mut payload, MAX_ASYNC_FILES).await?;

    // Validate all files before accepting the job
    for f in &files {
        validate_pdf_bytes(&f.bytes).map_err(|_| {
            AppError::Validation(format!(
                "File at index {} is not a valid PDF",
                f.index
            ))
        })?;
    }

    let batch_id = format!("batch_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let total_files = files.len() as i32;

    // Create the batch job record
    models::batch_job::create_batch_job(pool.get_ref(), &batch_id, &auth.api_key_id, total_files)
        .await?;

    // Spawn background processing
    let pool_clone = pool.get_ref().clone();
    let batch_id_clone = batch_id.clone();
    let key_id = auth.api_key_id.clone();
    let rid = req_id.id.clone();

    tokio::spawn(async move {
        process_async_batch(pool_clone, batch_id_clone, key_id, rid, files).await;
    });

    Ok(HttpResponse::Accepted().json(AsyncBatchResponse {
        batch_id,
        status: "processing".into(),
        total_files: total_files as u32,
        request_id: req_id.id.clone(),
    }))
}

/// Background worker that processes each file and updates the DB.
async fn process_async_batch(
    pool: SqlitePool,
    batch_id: String,
    api_key_id: String,
    request_id: String,
    files: Vec<ExtractedFile>,
) {
    let start = Instant::now();
    let mut succeeded = 0i32;
    let mut failed = 0i32;
    let mut total_pages = 0u32;
    let mut total_credits = 0u32;

    // Process files in parallel
    let handles: Vec<_> = files
        .into_iter()
        .map(|f| {
            let idx = f.index;
            let name = f.name.clone();
            let bytes = f.bytes;
            tokio::task::spawn_blocking(move || (idx, name, pdf_parser::parse_pdf(bytes, "pdftoppm")))
        })
        .collect();

    let joined = futures_util::future::join_all(handles).await;

    for join_result in joined {
        let (idx, name, parse_result) = match join_result {
            Ok(val) => val,
            Err(e) => {
                tracing::error!(batch_id = %batch_id, error = %e, "Task join error in async batch");
                failed += 1;
                let result_id = uuid::Uuid::new_v4().to_string();
                if let Err(save_err) = models::batch_job::save_batch_result(
                    &pool,
                    &result_id,
                    &batch_id,
                    idx_from_failed(failed),
                    None,
                    "error",
                    None,
                    Some(&format!("Task join error: {e}")),
                )
                .await
                {
                    tracing::error!(batch_id = %batch_id, error = %save_err, "failed to persist batch error result");
                }
                continue;
            }
        };

        let result_id = uuid::Uuid::new_v4().to_string();

        match parse_result {
            Ok(parsed) => {
                total_pages += parsed.usage.pages_processed;
                total_credits += parsed.usage.credits_used;
                succeeded += 1;

                let result_json = serde_json::json!({
                    "document": parsed.document,
                    "usage": parsed.usage,
                });

                if let Err(save_err) = models::batch_job::save_batch_result(
                    &pool,
                    &result_id,
                    &batch_id,
                    idx as i32,
                    name.as_deref(),
                    "success",
                    Some(&result_json.to_string()),
                    None,
                )
                .await
                {
                    tracing::error!(batch_id = %batch_id, error = %save_err, "failed to persist batch success result");
                }
            }
            Err(e) => {
                failed += 1;
                if let Err(save_err) = models::batch_job::save_batch_result(
                    &pool,
                    &result_id,
                    &batch_id,
                    idx as i32,
                    name.as_deref(),
                    "error",
                    None,
                    Some(&e.to_string()),
                )
                .await
                {
                    tracing::error!(batch_id = %batch_id, error = %save_err, "failed to persist batch error result");
                }
            }
        }
    }

    let status = if failed > 0 && succeeded > 0 {
        "completed"
    } else if failed > 0 {
        "failed"
    } else {
        "completed"
    };

    if let Err(e) = models::batch_job::update_batch_job_status(
        &pool, &batch_id, status, succeeded, failed,
    )
    .await
    {
        tracing::error!(batch_id = %batch_id, error = %e, "failed to update batch job status");
    }

    let processing_ms = start.elapsed().as_millis() as u64;

    // Log aggregate usage
    if let Err(e) = models::usage_log::log_usage(
        &pool,
        &api_key_id,
        "/v1/parse/batch/async",
        total_pages,
        total_credits,
        processing_ms,
        &request_id,
    )
    .await
    {
        tracing::error!(
            batch_id = %batch_id,
            api_key_id = %api_key_id,
            error = %e,
            "failed to write usage log for /v1/parse/batch/async — billing audit gap"
        );
    }

    tracing::info!(
        batch_id = %batch_id,
        succeeded = succeeded,
        failed = failed,
        processing_ms = processing_ms,
        "Async batch processing completed"
    );
}

/// Helper to derive a file index for join errors (best-effort).
fn idx_from_failed(failed_count: i32) -> i32 {
    // We lost the original index; use -failed_count as a sentinel
    -failed_count
}

// ── GET /v1/parse/batch/{batch_id} — status polling ──────────────────────

/// Retrieve the status and results of an async batch job.
#[tracing::instrument(skip(auth, pool, req_id))]
pub async fn batch_status(
    auth: ApiKeyAuth,
    path: web::Path<String>,
    pool: web::Data<SqlitePool>,
    req_id: web::ReqData<RequestId>,
) -> Result<HttpResponse, AppError> {
    let batch_id = path.into_inner();

    let job = models::batch_job::get_batch_job_for_key(
        pool.get_ref(),
        &batch_id,
        &auth.api_key_id,
    )
    .await?
    .ok_or(AppError::NotFound)?;

    let raw_results = models::batch_job::get_batch_results(pool.get_ref(), &batch_id).await?;

    let results: Vec<BatchStatusItem> = raw_results
        .into_iter()
        .map(|r| BatchStatusItem {
            index: r.file_index,
            file_name: r.file_name,
            status: r.status,
            result: r
                .result_json
                .and_then(|j| serde_json::from_str(&j).ok()),
            error: r.error_message,
        })
        .collect();

    Ok(HttpResponse::Ok().json(BatchStatusResponse {
        batch_id: job.id,
        status: job.status,
        total_files: job.total_files as u32,
        succeeded: job.succeeded as u32,
        failed: job.failed as u32,
        results,
        request_id: req_id.id.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_pdf_bytes ───────────────────────────────────────────────

    #[test]
    fn rejects_empty_bytes() {
        assert!(validate_pdf_bytes(&[]).is_err());
    }

    #[test]
    fn rejects_non_pdf_bytes() {
        let garbage = b"This is definitely not a PDF file at all!!!!!!!!!!!!!!!!!!!!!!!!!!!!";
        assert!(validate_pdf_bytes(garbage).is_err());
    }

    #[test]
    fn rejects_too_short_bytes() {
        let short = b"%PDF-1.4 too short";
        assert!(validate_pdf_bytes(short).is_err());
    }

    #[test]
    fn accepts_valid_pdf_header() {
        let mut bytes = b"%PDF-1.4".to_vec();
        bytes.extend_from_slice(&[0u8; 60]); // pad to 68 bytes
        assert!(validate_pdf_bytes(&bytes).is_ok());
    }

    // ── idx_from_failed ──────────────────────────────────────────────────

    #[test]
    fn idx_from_failed_returns_negative_sentinel() {
        assert_eq!(idx_from_failed(1), -1);
        assert_eq!(idx_from_failed(3), -3);
    }

    // ── BatchSummary serialization ───────────────────────────────────────

    #[test]
    fn batch_summary_serializes_correctly() {
        let summary = BatchSummary {
            total: 5,
            succeeded: 3,
            failed: 2,
            total_pages: 10,
            total_credits: 20,
            processing_ms: 1500,
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["total"], 5);
        assert_eq!(json["succeeded"], 3);
        assert_eq!(json["failed"], 2);
        assert_eq!(json["total_pages"], 10);
        assert_eq!(json["total_credits"], 20);
        assert_eq!(json["processing_ms"], 1500);
    }

    // ── BatchItemResult serialization ────────────────────────────────────

    #[test]
    fn success_item_omits_error_field() {
        let item = BatchItemResult {
            index: 0,
            file_name: Some("test.pdf".into()),
            status: "success".into(),
            document: None, // simplified for test
            usage: None,
            error: None,
        };
        let json = serde_json::to_value(&item).unwrap();
        assert!(!json.as_object().unwrap().contains_key("error"));
    }

    #[test]
    fn error_item_omits_document_and_usage_fields() {
        let item = BatchItemResult {
            index: 1,
            file_name: None,
            status: "error".into(),
            document: None,
            usage: None,
            error: Some("Invalid PDF".into()),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert!(!json.as_object().unwrap().contains_key("document"));
        assert!(!json.as_object().unwrap().contains_key("usage"));
        assert_eq!(json["error"], "Invalid PDF");
    }

    // ── AsyncBatchResponse serialization ─────────────────────────────────

    #[test]
    fn async_response_serializes_correctly() {
        let resp = AsyncBatchResponse {
            batch_id: "batch_abc123".into(),
            status: "processing".into(),
            total_files: 5,
            request_id: "req_abc123456789".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["batch_id"], "batch_abc123");
        assert_eq!(json["status"], "processing");
        assert_eq!(json["total_files"], 5);
    }

    // ── BatchStatusItem serialization ────────────────────────────────────

    #[test]
    fn status_item_omits_null_fields() {
        let item = BatchStatusItem {
            index: 0,
            file_name: None,
            status: "success".into(),
            result: Some(serde_json::json!({"pages": 1})),
            error: None,
        };
        let json = serde_json::to_value(&item).unwrap();
        assert!(!json.as_object().unwrap().contains_key("error"));
        assert!(json.as_object().unwrap().contains_key("result"));
    }

    // ── MAX constants ────────────────────────────────────────────────────

    #[test]
    fn sync_limit_is_ten() {
        assert_eq!(MAX_SYNC_FILES, 10);
    }

    #[test]
    fn async_limit_is_fifty() {
        assert_eq!(MAX_ASYNC_FILES, 50);
    }

    #[test]
    fn max_file_size_is_50mb() {
        assert_eq!(MAX_FILE_SIZE, 50 * 1024 * 1024);
    }
}
