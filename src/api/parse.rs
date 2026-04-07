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
use crate::services::idempotency::{CachedResponse, IdempotencyCache, MAX_KEY_LEN};
use crate::services::metrics::{BackendLabels, Metrics, ParseLabels};
use crate::services::parse_gate::ParseGate;
use crate::services::{ocr, pdf_parser};
use actix_web::HttpRequest;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip(auth, http_req, payload, pool, config, gate, metrics, idem, req_id))]
pub async fn parse_pdf(
    auth: ApiKeyAuth,
    http_req: HttpRequest,
    mut payload: Multipart,
    pool: web::Data<SqlitePool>,
    config: web::Data<AppConfig>,
    gate: web::Data<ParseGate>,
    metrics: web::Data<Metrics>,
    idem: web::Data<IdempotencyCache>,
    req_id: web::ReqData<RequestId>,
) -> Result<HttpResponse, AppError> {
    let started = Instant::now();

    // Idempotency replay short-circuit. Skipped entirely if the header
    // is absent. Replays do NOT consume a gate permit, do NOT log
    // usage, and do NOT bill — they just return the cached body.
    let idem_key = read_idempotency_key(&http_req)?;
    let idem_cache_key = idem_key
        .as_ref()
        .map(|k| IdempotencyCache::make_key(&auth.api_key_id, k));
    if let Some(ref cache_key) = idem_cache_key {
        if let Some(cached) = idem.get(cache_key) {
            return Ok(replay_cached(cached));
        }
    }
    // Acquire a concurrency permit BEFORE the billing check or any
    // expensive work. The permit is held until the dispatcher returns
    // (the `_permit` binding lives for the function scope), so even if
    // the deadline drops the inner future the gate keeps reflecting
    // real in-flight work.
    let _permit = gate.try_acquire().map_err(|_| {
        record_outcome(&metrics, "/v1/parse", "503", None, started);
        AppError::ServiceBusy
    })?;
    // Construct the guard atomically with the inc — never inc separately,
    // see GateGaugeGuard::new docs for the cancellation rationale.
    let _gate_guard = GateGaugeGuard::new(metrics.clone());

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
    let result = match pdf_parser::parse_pdf_with_backends_mode(
        bytes,
        &ocr_config,
        paddle_config.as_ref(),
        config.paddleocr_mode,
        document_type_hint,
        Duration::from_secs(config.parse_deadline_secs),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let status = match &e {
                AppError::DeadlineExceeded => "504",
                AppError::EncryptedPdf => "422",
                AppError::InvalidPdf => "400",
                AppError::QuotaExceeded(_) => "429",
                _ => "500",
            };
            record_outcome(&metrics, "/v1/parse", status, None, started);
            return Err(e);
        }
    };
    record_outcome(
        &metrics,
        "/v1/parse",
        "200",
        result.document.metadata.routed_to,
        started,
    );
    if result
        .document
        .metadata
        .warnings
        .contains(&pdf_parser::ParseWarning::PaddleDegradedToTesseract)
    {
        metrics.paddle_degraded.inc();
    }
    if let Some(ref class) = result.document.metadata.classification {
        // Stable label strings — never derive from `Debug`, which would
        // silently rename a metric series the day a new variant is added.
        let label = match class.class {
            crate::services::pdf_classifier::PdfClass::TextSimple => "text_simple",
            crate::services::pdf_classifier::PdfClass::TextStructured => "text_structured",
            crate::services::pdf_classifier::PdfClass::ScannedOrEmpty => "scanned_or_empty",
            crate::services::pdf_classifier::PdfClass::Unknown => "unknown",
        };
        metrics
            .classifier_class
            .get_or_create(&crate::services::metrics::ClassifierLabels {
                class: label.into(),
            })
            .inc();
    }

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

    let body = ParseResponse {
        document: result.document,
        usage: result.usage,
        request_id: req_id.id.clone(),
    };
    let body_bytes = serde_json::to_vec(&body).map_err(|e| {
        AppError::Internal(format!("failed to serialize parse response: {e}"))
    })?;

    if let Some(cache_key) = idem_cache_key {
        if idem.should_cache(body_bytes.len()) {
            idem.put(
                cache_key,
                CachedResponse {
                    status: actix_web::http::StatusCode::OK,
                    body: body_bytes.clone(),
                    content_type: "application/json".into(),
                    stored_at: Instant::now(),
                },
            );
        }
    }

    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(body_bytes))
}

/// Read and validate the optional `Idempotency-Key` header. Returns
/// `Ok(None)` when absent, `Ok(Some(key))` when present and valid,
/// `Err(Validation)` when present but malformed (over-length).
fn read_idempotency_key(req: &HttpRequest) -> Result<Option<String>, AppError> {
    let Some(raw) = req.headers().get("idempotency-key") else {
        return Ok(None);
    };
    let raw_str = raw
        .to_str()
        .map_err(|_| AppError::Validation("Idempotency-Key must be ASCII".into()))?;
    // Check the *raw* length first so a client can't smuggle a key past
    // the cap by surrounding it with whitespace.
    if raw_str.len() > MAX_KEY_LEN {
        return Err(AppError::Validation(format!(
            "Idempotency-Key too long ({} > {} chars)",
            raw_str.len(),
            MAX_KEY_LEN
        )));
    }
    let s = raw_str.trim();
    if s.is_empty() {
        return Ok(None);
    }
    Ok(Some(s.to_string()))
}

/// Build an `HttpResponse` from a cached entry, marking the replay
/// with the `X-Idempotent-Replay: true` header so callers can tell.
fn replay_cached(cached: CachedResponse) -> HttpResponse {
    let mut builder = HttpResponse::build(cached.status);
    builder
        .insert_header(("X-Idempotent-Replay", "true"))
        .content_type(cached.content_type);
    builder.body(cached.body)
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

/// RAII helper that increments `parse_gate_in_flight` on construction
/// and decrements it on drop. Mirrors the lifetime of the semaphore
/// permit so the gauge stays in sync with the actual in-flight permit
/// count, even on early-return error paths and async cancellations.
///
/// **Always construct via `GateGaugeGuard::new(metrics)`** so the
/// increment and the drop guard are paired atomically — never call
/// `inc()` separately, since async cancellation between the inc and
/// the guard binding would leak the gauge by +1 forever.
pub(crate) struct GateGaugeGuard(web::Data<Metrics>);
impl GateGaugeGuard {
    pub(crate) fn new(metrics: web::Data<Metrics>) -> Self {
        metrics.parse_gate_in_flight.inc();
        Self(metrics)
    }
}
impl Drop for GateGaugeGuard {
    fn drop(&mut self) {
        self.0.parse_gate_in_flight.dec();
    }
}

/// Record a parse-request outcome in the metrics surface. Always called
/// once per request — including on the early-return error paths — so
/// the `parse_requests_total` counter matches reality.
pub(crate) fn record_outcome(
    metrics: &Metrics,
    endpoint: &str,
    status: &str,
    backend: Option<pdf_parser::RoutedTo>,
    started: Instant,
) {
    metrics
        .parse_requests
        .get_or_create(&ParseLabels {
            endpoint: endpoint.into(),
            status: status.into(),
        })
        .inc();
    // Always observe latency, even when no backend was selected (early
    // 503/504/4xx error paths). Without this the histogram only reflects
    // successes and the tail latency that matters most — deadline
    // exceedances and gate saturation — disappears from the dashboard.
    let backend_label = match backend {
        Some(pdf_parser::RoutedTo::PdfOxide) => "pdf_oxide",
        Some(pdf_parser::RoutedTo::Paddle) => "paddle",
        Some(pdf_parser::RoutedTo::Tesseract) => "tesseract",
        None => "none",
    };
    metrics
        .parse_duration
        .get_or_create(&BackendLabels {
            backend: backend_label.into(),
        })
        .observe(started.elapsed().as_secs_f64());
}
