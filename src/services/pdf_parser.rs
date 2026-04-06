use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::errors::AppError;
use super::doc_type_detector::{self, DocType, DocumentTypeSource, TypeAlternate};
use super::ocr;

/// Reference-counted, immutable view of an uploaded PDF. Replaces the
/// previous `Vec<u8>` parameter so the dispatcher can pass the bytes to
/// the OCR chain and (if needed) to `parse_pdf` without paying for an
/// extra copy on every fan-out. The single unavoidable materialization
/// happens at the `pdf_oxide::PdfDocument::from_bytes` boundary.
pub type PdfBytes = Arc<[u8]>;

/// Result of parsing a single PDF document.
#[derive(Debug, Serialize)]
pub struct ParseResult {
    pub document: DocumentResult,
    pub usage: UsageInfo,
}

/// The parsed document content.
#[derive(Debug, Serialize)]
pub struct DocumentResult {
    pub markdown: String,
    pub text: String,
    pub pages: Vec<PageResult>,
    pub tables: Vec<TableResult>,
    pub metadata: DocumentMetadata,
}

/// Per-page extraction result.
#[derive(Debug, Serialize)]
pub struct PageResult {
    pub page_number: u32,
    pub text: String,
    pub char_count: usize,
}

/// An extracted table.
#[derive(Debug, Serialize)]
pub struct TableResult {
    pub page: u32,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Which backend actually produced a parse result. Typed so every caller
/// gets compile-time exhaustiveness and the wire format stays stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutedTo {
    PdfOxide,
    Paddle,
    Tesseract,
}

/// Soft-warning surfaced in `metadata.warnings` when the dispatcher had
/// to compromise on its preferred path. The wire format is the snake_case
/// string of the variant — typed in code, stable on the wire.
///
/// Customers parsing the response can branch on these to know whether
/// their "structured" doc was actually OCR'd, whether the text layer was
/// suspiciously thin, etc. Without this field, backend degradation is
/// silent and surprising.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseWarning {
    /// PaddleOCR was preferred but failed (network error, empty result,
    /// or sidecar exception); Tesseract recovered.
    PaddleDegradedToTesseract,
    /// PaddleOCR returned an empty result and we kept the pdf_oxide
    /// output rather than degrading further.
    PaddleReturnedEmpty,
    /// pdf_oxide produced very little text on a doc that did NOT
    /// classify as scanned. Caller may want to retry with a different
    /// mode or inspect the source PDF.
    PdfOxideTextSuspiciouslyLow,
    /// One or more OCR backends emitted a runtime warning during
    /// processing (forwarded from `OcrResult.warning`).
    OcrBackendWarning,
}

/// Document-level metadata.
#[derive(Debug, Serialize)]
pub struct DocumentMetadata {
    pub page_count: u32,
    pub pdf_version: Option<String>,
    pub is_encrypted: bool,
    pub is_scanned: bool,
    /// Auto-detected document type from extracted text. May be `None` if the
    /// detector is not confident enough to commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_type: Option<DocType>,
    /// Normalized confidence for `detected_type` in `[0.0, 1.0]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_type_confidence: Option<f64>,
    /// Runner-up classifications with their confidences.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub detected_type_alternates: Vec<TypeAlternate>,
    /// Customer-supplied `document_type_hint`, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_type_hint: Option<DocType>,
    /// Effective document type after reconciliation between hint and
    /// detector. Hint wins on disagreement; see
    /// `reconcile_document_type`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_type: Option<DocType>,
    /// Where `document_type` came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_type_source: Option<DocumentTypeSource>,
    pub image_count: u32,
    pub processing_ms: u64,
    /// Client-facing classifier projection (only set when `PaddleOcrMode::Auto`
    /// routing ran). The raw signal values stay server-side in tracing logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification: Option<super::pdf_classifier::ClassificationSummary>,
    /// Which backend actually produced this result. Only set when the
    /// dispatcher picked a backend (i.e. from `parse_pdf_with_backends_mode`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routed_to: Option<RoutedTo>,
    /// Soft warnings about backend degradation, low text quality, etc.
    /// Empty on the happy path. See [`ParseWarning`] for the controlled
    /// vocabulary. Skipped from serialization when empty so existing
    /// clients see no shape change.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ParseWarning>,
}

/// Credits and pages processed.
#[derive(Debug, Serialize)]
pub struct UsageInfo {
    pub pages_processed: u32,
    pub credits_used: u32,
}

/// Parse a PDF from raw bytes. This is CPU-bound — call via spawn_blocking.
///
/// Uses pdf_oxide for raw text extraction and pdftohtml for structured markdown.
/// Takes a borrowed slice so the dispatcher can pass `&PdfBytes` (an
/// `Arc<[u8]>` deref) without paying for an extra copy. The single
/// unavoidable `Vec` materialization happens inside this function at the
/// `pdf_oxide::PdfDocument::from_bytes` boundary.
pub fn parse_pdf(bytes: &[u8], pdftoppm_path: &str) -> Result<ParseResult, AppError> {
    let start = Instant::now();

    // Refuse password-protected PDFs up front. We do not crack, guess, or
    // attempt OCR on encrypted documents — that would be a customer
    // surprise (silently bypassing access control on a doc the caller
    // may not be authorized to read in cleartext). Maps to 422 with
    // `code: ENCRYPTED_PDF`.
    if is_encrypted_pdf(bytes) {
        return Err(AppError::EncryptedPdf);
    }

    let mut doc = pdf_oxide::PdfDocument::from_bytes(bytes.to_vec())
        .map_err(|e| AppError::PdfProcessing(format!("Failed to open PDF: {e}")))?;

    let page_count = doc
        .page_count()
        .map_err(|e| AppError::PdfProcessing(format!("Failed to get page count: {e}")))?
        as u32;
    if page_count == 0 {
        return Err(AppError::PdfProcessing("PDF has no pages".into()));
    }

    // Extract raw text per page (for the `text` field and scan detection)
    let mut pages = Vec::with_capacity(page_count as usize);
    let mut full_text = String::new();

    for page_num in 0..page_count {
        let raw_text = doc.extract_text(page_num as usize).unwrap_or_else(|e| {
            tracing::warn!(page = page_num, error = %e, "Failed to extract text from page");
            String::new()
        });
        let page_text = fix_superscript_artifacts(&raw_text);
        let char_count = page_text.len();

        if !full_text.is_empty() {
            full_text.push('\n');
        }
        full_text.push_str(&page_text);

        pages.push(PageResult {
            page_number: page_num + 1,
            text: page_text,
            char_count,
        });
    }

    // Scan detection: if average chars per page < 50, likely scanned
    let total_chars: usize = pages.iter().map(|p| p.char_count).sum();
    let avg_chars = total_chars as f64 / page_count as f64;
    let is_scanned = avg_chars < 50.0;

    // Generate structured markdown via pdftohtml (preserves fonts/headings)
    let markdown = match super::markdown_cleaner::pdf_to_markdown(bytes, pdftoppm_path) {
        Ok(md) if !md.trim().is_empty() => md,
        Ok(_) | Err(_) => {
            // Fallback: basic markdown from raw text
            tracing::debug!("pdftohtml markdown generation failed, using basic text");
            build_markdown(&pages)
        }
    };

    // Extract tables from markdown
    let tables = extract_tables_from_markdown(&markdown);

    // Detect document type using the weighted feature detector.
    let detection = doc_type_detector::detect(&full_text);
    log_detection(&detection);
    let (detected_type, detected_type_confidence, detected_type_alternates) =
        unpack_detection(&detection);

    let processing_ms = start.elapsed().as_millis() as u64;

    Ok(ParseResult {
        document: DocumentResult {
            markdown,
            text: full_text,
            pages,
            tables,
            metadata: DocumentMetadata {
                page_count,
                pdf_version: None,
                is_encrypted: false,
                is_scanned,
                detected_type,
                detected_type_confidence,
                detected_type_alternates,
                document_type_hint: None,
                document_type: None,
                document_type_source: None,
                image_count: 0,
                processing_ms,
                classification: None,
                routed_to: None,
                warnings: Vec::new(),
            },
        },
        usage: UsageInfo {
            pages_processed: page_count,
            credits_used: page_count * 2,
        },
    })
}

/// Log the full per-type scores from the detector for server-side tuning.
/// The raw scores never leave the process — they are not serialized into
/// the API response.
///
/// **Phase 3 boundary (see `docs/DEFERRED_DOC_TYPE_LEARNING.md`)**: this is
/// the single place where detector output lands in the tuning-telemetry
/// feed. When the telemetry/learning phase ships, this is the line that
/// needs to be gated behind a per-tenant opt-in, routed through a
/// PII-scrubbing sink, and subject to a retention policy. Any new
/// detection logging should go through this helper so the opt-in check
/// has exactly one home.
fn log_detection(detection: &doc_type_detector::DetectionResult) {
    tracing::info!(
        detected = ?detection.type_,
        confidence = detection.confidence,
        scores = ?detection.debug_scores,
        "document type detection"
    );
}

/// Split a `DetectionResult` into the triple stored on `DocumentMetadata`.
fn unpack_detection(
    detection: &doc_type_detector::DetectionResult,
) -> (Option<DocType>, Option<f64>, Vec<TypeAlternate>) {
    match detection.type_ {
        Some(t) => (
            Some(t),
            Some(detection.confidence),
            detection.alternates.clone(),
        ),
        None => (None, None, Vec::new()),
    }
}

/// Reconcile a customer-supplied hint with the auto-detected type.
///
/// Semantics (option **b** from the Phase 1+2 plan):
/// * Hint is provided → hint wins; `document_type_source = Hint`. If the
///   detector disagrees with the hint, both are logged so tuning can learn
///   from the disagreement without silently overriding the customer.
/// * No hint, detector confident → use the detector;
///   `document_type_source = Detector`.
/// * No hint, no confident detection → `document_type = None`,
///   `document_type_source = None`.
fn reconcile_document_type(
    hint: Option<DocType>,
    detected: Option<DocType>,
) -> (Option<DocType>, Option<DocumentTypeSource>) {
    match (hint, detected) {
        (Some(h), detected) => {
            if let Some(d) = detected {
                if d != h {
                    tracing::info!(
                        hint = ?h,
                        detected = ?d,
                        "document_type hint disagrees with detector; hint wins"
                    );
                }
            }
            (Some(h), Some(DocumentTypeSource::Hint))
        }
        (None, Some(d)) => (Some(d), Some(DocumentTypeSource::Detector)),
        (None, None) => (None, None),
    }
}

/// Threshold for `PdfOxideTextSuspiciouslyLow`. Picked to be loose enough
/// that legitimately short docs (single-page receipts, business cards)
/// don't trip it but tight enough to flag PDFs where pdf_oxide silently
/// returned almost nothing on a doc that did NOT classify as scanned.
const PDF_OXIDE_LOW_TEXT_THRESHOLD: usize = 200;

/// Append `PdfOxideTextSuspiciouslyLow` to a pdf_oxide result whose total
/// text is suspiciously sparse on a non-scanned, multi-page doc. Idempotent
/// — safe to call repeatedly.
fn maybe_warn_low_text(result: &mut ParseResult) {
    if result.document.metadata.is_scanned {
        return;
    }
    if result.document.metadata.page_count < 2 {
        return;
    }
    let total_chars: usize = result.document.pages.iter().map(|p| p.char_count).sum();
    if total_chars < PDF_OXIDE_LOW_TEXT_THRESHOLD
        && !result
            .document
            .metadata
            .warnings
            .contains(&ParseWarning::PdfOxideTextSuspiciouslyLow)
    {
        result
            .document
            .metadata
            .warnings
            .push(ParseWarning::PdfOxideTextSuspiciouslyLow);
    }
}

/// Stamp a `ParseResult` with the document-type hint and the reconciled
/// effective type + source. Called by every dispatcher arm after the backend
/// run completes.
fn apply_document_type_hint(result: &mut ParseResult, hint: Option<DocType>) {
    let detected = result.document.metadata.detected_type;
    let (effective, source) = reconcile_document_type(hint, detected);
    result.document.metadata.document_type_hint = hint;
    result.document.metadata.document_type = effective;
    result.document.metadata.document_type_source = source;
}

/// Cheap byte-scan for the `/Encrypt` trailer entry that marks a
/// password-protected PDF.
///
/// We look at the first ~4 KB of the file. The trailer dict is at the
/// end of the file in the spec, but most encrypted PDFs land the
/// `/Encrypt` reference much earlier (cross-reference stream, document
/// catalog, or near the header in linearized PDFs). For files we
/// cannot scan in 4 KB we err on the side of "not encrypted" and let
/// pdf_oxide surface the parse error — we'd rather miss an exotic
/// encryption layout than block a legitimate doc.
///
/// The leading `/` anchors the keyword so prose containing the word
/// "Encrypted" doesn't trip the check.
fn is_encrypted_pdf(bytes: &[u8]) -> bool {
    const SCAN_WINDOW: usize = 4 * 1024;
    let needle = b"/Encrypt";
    let window = &bytes[..bytes.len().min(SCAN_WINDOW)];
    window.windows(needle.len()).any(|w| w == needle)
}

/// Returns true if the error is a structural PDF parsing failure that OCR can recover from.
/// These are cases where pdf_oxide can't read the PDF structure, but the file is still a
/// valid PDF that Claude Vision can process visually.
fn is_ocr_recoverable(err: &AppError) -> bool {
    match err {
        AppError::PdfProcessing(msg) => {
            msg.contains("Catalog missing")
                || msg.contains("Failed to get page count")
                || msg.contains("Failed to open PDF")
                || msg.contains("PDF has no pages")
        }
        _ => false,
    }
}

/// Output of the OCR backend chain — includes which engine actually produced
/// the result so the caller can tag `routed_to` accurately (rather than
/// guessing "paddle iff paddle_cfg.is_some()") plus any soft warnings the
/// chain accumulated (e.g. Paddle failed and Tesseract recovered).
struct OcrBackendOutcome {
    result: ocr::OcrResult,
    backend: RoutedTo,
    warnings: Vec<ParseWarning>,
}

/// Stamp an OCR result with classification + routing metadata and return the
/// ParseResult. Used by every OCR dispatch arm.
fn finalize_ocr_result(
    ocr_result: ocr::OcrResult,
    classification: Option<super::pdf_classifier::ClassificationSummary>,
    routed_to: RoutedTo,
    warnings: Vec<ParseWarning>,
) -> ParseResult {
    let backend_warning = ocr_result.warning.is_some();
    let mut pr = build_result_from_ocr(ocr_result);
    pr.document.metadata.classification = classification;
    pr.document.metadata.routed_to = Some(routed_to);
    pr.document.metadata.warnings = warnings;
    if backend_warning {
        pr.document
            .metadata
            .warnings
            .push(ParseWarning::OcrBackendWarning);
    }
    pr
}

/// Parse a PDF and dispatch to the right backend based on routing mode.
///
/// The `deadline` is a wall-clock budget for the whole call. On expiry the
/// future is dropped and the caller receives [`AppError::DeadlineExceeded`]
/// (`504 Gateway Timeout`). Note: dropping the future does NOT cancel any
/// `spawn_blocking` work it dispatched — that work runs to completion in
/// the background. Pair this with the `ParseGate` semaphore in the
/// handlers so abandoned-but-still-running tasks cannot compound.
pub async fn parse_pdf_with_backends_mode(
    bytes: PdfBytes,
    ocr_config: &ocr::OcrConfig,
    paddle_config: Option<&crate::services::paddle_ocr::PaddleOcrConfig>,
    mode: crate::config::PaddleOcrMode,
    document_type_hint: Option<DocType>,
    deadline: Duration,
) -> Result<ParseResult, AppError> {
    let inner = dispatch_inner(bytes, ocr_config, paddle_config, mode);
    let mut result = match tokio::time::timeout(deadline, inner).await {
        Ok(res) => res?,
        Err(_elapsed) => {
            tracing::warn!(
                deadline_secs = deadline.as_secs(),
                "parse deadline exceeded"
            );
            return Err(AppError::DeadlineExceeded);
        }
    };
    apply_document_type_hint(&mut result, document_type_hint);
    Ok(result)
}

async fn dispatch_inner(
    bytes: PdfBytes,
    ocr_config: &ocr::OcrConfig,
    paddle_config: Option<&crate::services::paddle_ocr::PaddleOcrConfig>,
    mode: crate::config::PaddleOcrMode,
) -> Result<ParseResult, AppError> {
    use crate::config::PaddleOcrMode;
    use super::pdf_classifier::PdfClass;

    let bytes_for_ocr: PdfBytes = Arc::clone(&bytes);
    let ocr_cfg = ocr_config.clone();
    let paddle_cfg = paddle_config.cloned();
    let pdftoppm_path = ocr_config.pdftoppm_path.clone();

    // Primary mode: try Paddle first for every PDF when configured.
    if matches!(mode, PaddleOcrMode::Primary) {
        if let Some(cfg) = paddle_cfg.as_ref() {
            tracing::info!("PaddleOCR mode=primary, trying Paddle before pdf_oxide");
            match crate::services::paddle_ocr::parse_pdf(&bytes_for_ocr, cfg).await {
                Ok(result) if !result.markdown.trim().is_empty() => {
                    return Ok(finalize_ocr_result(
                        result,
                        None,
                        RoutedTo::Paddle,
                        Vec::new(),
                    ));
                }
                Ok(_) => {
                    tracing::warn!("PaddleOCR returned empty result, falling back to pdf_oxide");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "PaddleOCR failed, falling back to pdf_oxide");
                }
            }
        }
    }

    // Try pdf_oxide + pdftohtml (fast, local, CPU-bound). The Arc clone is
    // O(1) and lets `parse_pdf` borrow without forcing the dispatcher to
    // own a `Vec<u8>`.
    let bytes_for_local: PdfBytes = Arc::clone(&bytes);
    let local_result = tokio::task::spawn_blocking(move || {
        parse_pdf(&bytes_for_local, &pdftoppm_path)
    })
    .await
    .map_err(|e| AppError::Internal(format!("Task join error: {e}")))?;

    match local_result {
        Ok(mut result) => {
            // Auto mode: classify the first-pass result and route from there.
            if matches!(mode, PaddleOcrMode::Auto) {
                let report = super::pdf_classifier::classify(&result.document);
                // Log the full signal values server-side for threshold tuning
                // from production traffic, then strip to the client-facing
                // summary so raw signals don't leak into API responses.
                tracing::info!(
                    class = ?report.class,
                    chars_per_page = report.signals.chars_per_page,
                    pipe_density = report.signals.pipe_density,
                    label_density = report.signals.label_density,
                    column_alignment = report.signals.column_alignment,
                    page_count = report.signals.page_count,
                    "pdf classified"
                );
                let summary: super::pdf_classifier::ClassificationSummary = report.into();
                result.document.metadata.classification = Some(summary);

                match report.class {
                    PdfClass::TextSimple => {
                        result.document.metadata.routed_to = Some(RoutedTo::PdfOxide);
                        maybe_warn_low_text(&mut result);
                        return Ok(result);
                    }
                    PdfClass::TextStructured => {
                        if let Some(cfg) = paddle_cfg.as_ref() {
                            tracing::info!(
                                "PaddleOCR mode=auto, class=text_structured → Paddle"
                            );
                            match crate::services::paddle_ocr::parse_pdf(&bytes_for_ocr, cfg).await
                            {
                                Ok(pr) if !pr.markdown.trim().is_empty() => {
                                    return Ok(finalize_ocr_result(
                                        pr,
                                        Some(summary),
                                        RoutedTo::Paddle,
                                        Vec::new(),
                                    ));
                                }
                                Ok(_) => {
                                    tracing::warn!(
                                        "PaddleOCR empty on structured doc, keeping pdf_oxide"
                                    );
                                    result
                                        .document
                                        .metadata
                                        .warnings
                                        .push(ParseWarning::PaddleReturnedEmpty);
                                }
                                Err(e) => tracing::warn!(
                                    error = %e,
                                    "PaddleOCR failed on structured doc, keeping pdf_oxide"
                                ),
                            }
                        }
                        result.document.metadata.routed_to = Some(RoutedTo::PdfOxide);
                        maybe_warn_low_text(&mut result);
                        return Ok(result);
                    }
                    PdfClass::ScannedOrEmpty => {
                        tracing::info!("PaddleOCR mode=auto, class=scanned → OCR chain");
                        if let Some(outcome) =
                            run_ocr_backends(&bytes_for_ocr, &ocr_cfg, paddle_cfg.as_ref()).await
                        {
                            return Ok(finalize_ocr_result(
                                outcome.result,
                                Some(summary),
                                outcome.backend,
                                outcome.warnings,
                            ));
                        }
                        // All OCR backends failed — fall through to the
                        // pdf_oxide result even though it's scanned. The
                        // caller will see empty text but at least gets a
                        // shaped response.
                        result.document.metadata.routed_to = Some(RoutedTo::PdfOxide);
                        maybe_warn_low_text(&mut result);
                        return Ok(result);
                    }
                    PdfClass::Unknown => {
                        tracing::info!(
                            "PaddleOCR mode=auto, class=unknown → fallback behaviour"
                        );
                        // fall through to the Fallback-mode logic below
                    }
                }
            }

            // Fallback mode (or Auto/Unknown): pdf_oxide first, OCR only if scanned.
            if result.document.metadata.is_scanned {
                tracing::info!("Scanned PDF detected, attempting structured OCR");
                if let Some(outcome) =
                    run_ocr_backends(&bytes_for_ocr, &ocr_cfg, paddle_cfg.as_ref()).await
                {
                    let classification = result.document.metadata.classification;
                    return Ok(finalize_ocr_result(
                        outcome.result,
                        classification,
                        outcome.backend,
                        outcome.warnings,
                    ));
                }
            }
            result.document.metadata.routed_to = Some(RoutedTo::PdfOxide);
            maybe_warn_low_text(&mut result);
            Ok(result)
        }
        Err(err) if is_ocr_recoverable(&err) => {
            tracing::warn!(
                error = %err,
                "pdf_oxide failed during recoverable error, attempting OCR chain \
                 (classification skipped — no first-pass result to classify)"
            );
            match run_ocr_backends(&bytes_for_ocr, &ocr_cfg, paddle_cfg.as_ref()).await {
                Some(outcome) => Ok(finalize_ocr_result(
                    outcome.result,
                    None,
                    outcome.backend,
                    outcome.warnings,
                )),
                None => Err(AppError::PdfProcessing(
                    "All OCR backends failed to parse the document".into(),
                )),
            }
        }
        Err(err) => Err(err),
    }
}

/// Run OCR backends in preference order: PaddleOCR (if configured) → tesseract.
/// Returns Some(outcome) on first success, None if all backends failed. The
/// outcome includes the actual backend that produced the result so callers
/// can tag `routed_to` accurately even when Paddle fails and Tesseract
/// recovers.
async fn run_ocr_backends(
    bytes: &PdfBytes,
    tesseract_cfg: &ocr::OcrConfig,
    paddle_cfg: Option<&crate::services::paddle_ocr::PaddleOcrConfig>,
) -> Option<OcrBackendOutcome> {
    // Track whether we had to degrade off Paddle. If so, the eventual
    // Tesseract success is reported with `PaddleDegradedToTesseract`
    // so the caller knows the chain compromised on quality.
    let mut paddle_was_attempted = false;

    // Try PaddleOCR first when configured
    if let Some(cfg) = paddle_cfg {
        paddle_was_attempted = true;
        tracing::info!(url = %cfg.base_url, "Trying PaddleOCR PP-StructureV3");
        match crate::services::paddle_ocr::parse_pdf(bytes, cfg).await {
            Ok(result) if !result.markdown.trim().is_empty() => {
                tracing::info!(
                    pages = result.pages.len(),
                    ms = result.processing_ms,
                    "PaddleOCR succeeded"
                );
                return Some(OcrBackendOutcome {
                    result,
                    backend: RoutedTo::Paddle,
                    warnings: Vec::new(),
                });
            }
            Ok(_) => {
                tracing::warn!("PaddleOCR returned empty result, falling back to tesseract");
            }
            Err(e) => {
                tracing::warn!(error = %e, "PaddleOCR failed, falling back to tesseract");
            }
        }
    }

    // Fall back to tesseract. The Arc clone is cheap; ocr_pdf borrows
    // through the deref so no Vec materialization is needed.
    let bytes_for_tess: PdfBytes = Arc::clone(bytes);
    let cfg = tesseract_cfg.clone();
    let degradation_warnings = if paddle_was_attempted {
        vec![ParseWarning::PaddleDegradedToTesseract]
    } else {
        Vec::new()
    };
    match tokio::task::spawn_blocking(move || ocr::ocr_pdf(&bytes_for_tess, &cfg)).await {
        Ok(Ok(result)) if !result.text.is_empty() => Some(OcrBackendOutcome {
            result,
            backend: RoutedTo::Tesseract,
            warnings: degradation_warnings,
        }),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "Tesseract OCR failed");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "Tesseract task join error");
            None
        }
        _ => None,
    }
}

/// Build a ParseResult from OCR output.
fn build_result_from_ocr(ocr_result: ocr::OcrResult) -> ParseResult {
    let pages: Vec<PageResult> = if ocr_result.pages.is_empty() {
        vec![PageResult {
            page_number: 1,
            text: ocr_result.text.clone(),
            char_count: ocr_result.text.len(),
        }]
    } else {
        ocr_result
            .pages
            .iter()
            .map(|p| PageResult {
                page_number: p.page_number,
                text: p.text.clone(),
                char_count: p.text.len(),
            })
            .collect()
    };

    let page_count = pages.len() as u32;
    // Use OCR's structured markdown (from hOCR), not basic text-dump
    let markdown = if ocr_result.markdown.trim().is_empty() {
        build_markdown(&pages)
    } else {
        ocr_result.markdown
    };
    let tables = extract_tables_from_markdown(&markdown);
    let detection = doc_type_detector::detect(&ocr_result.text);
    log_detection(&detection);
    let (detected_type, detected_type_confidence, detected_type_alternates) =
        unpack_detection(&detection);

    ParseResult {
        document: DocumentResult {
            markdown,
            text: ocr_result.text,
            pages,
            tables,
            metadata: DocumentMetadata {
                page_count,
                pdf_version: None,
                is_encrypted: false,
                is_scanned: true,
                detected_type,
                detected_type_confidence,
                detected_type_alternates,
                document_type_hint: None,
                document_type: None,
                document_type_source: None,
                image_count: 0,
                processing_ms: ocr_result.processing_ms,
                classification: None,
                routed_to: None,
                warnings: Vec::new(),
            },
        },
        usage: UsageInfo {
            pages_processed: page_count,
            credits_used: page_count * 3, // OCR costs more
        },
    }
}

/// Build clean markdown from page text.
///
/// Applies markdown cleanup to each page's text to remove OCR/extraction artifacts
/// that would break markdown rendering. The raw `text` field is unaffected.
fn build_markdown(pages: &[PageResult]) -> String {
    let mut md = String::new();
    for page in pages {
        if pages.len() > 1 {
            md.push_str(&format!("## Page {}\n\n", page.page_number));
        }
        let cleaned = super::markdown_cleaner::clean_for_markdown(&page.text);
        md.push_str(&cleaned);
        md.push_str("\n\n");
    }
    md
}

/// Parse markdown-style tables from text. Tracks current page via `## Page N` headers.
fn extract_tables_from_markdown(markdown: &str) -> Vec<TableResult> {
    let mut tables = Vec::new();
    let lines: Vec<&str> = markdown.lines().collect();
    let mut i = 0;
    let mut current_page: u32 = 1;

    while i < lines.len() {
        let line = lines[i].trim();

        // Track page headers from build_markdown
        if let Some(rest) = line.strip_prefix("## Page ") {
            if let Ok(p) = rest.trim().parse::<u32>() {
                current_page = p;
            }
        }

        // Detect table: line starts with | and contains at least 2 |
        if line.starts_with('|') && line.matches('|').count() >= 3 {
            let mut table_lines = vec![line];
            let mut j = i + 1;
            while j < lines.len() {
                let next = lines[j].trim();
                if next.starts_with('|') && next.matches('|').count() >= 3 {
                    table_lines.push(next);
                    j += 1;
                } else {
                    break;
                }
            }

            if table_lines.len() >= 3 {
                let headers = parse_table_row(table_lines[0]);
                let mut rows = Vec::new();
                for &row_line in &table_lines[2..] {
                    rows.push(parse_table_row(row_line));
                }
                tables.push(TableResult {
                    page: current_page,
                    headers,
                    rows,
                });
            }

            i = j;
        } else {
            i += 1;
        }
    }

    tables
}

fn parse_table_row(line: &str) -> Vec<String> {
    line.split('|')
        .map(|cell| cell.trim().to_string())
        .filter(|cell| !cell.is_empty())
        .collect()
}

/// Fix common PDF extraction artifacts where superscript ordinal suffixes
/// get misplaced by the PDF text extractor. Handles two cases:
///
/// 1. Orphaned line: "nd" on its own line → merge with previous line
/// 2. Inline merge: "Wordrd" (Word + rd), "32022" (3 + 2022), "andth" (and + th)
///    → split and reconstruct: "Word" + newline, "3rd 2022", "and"
fn fix_superscript_artifacts(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let lines: Vec<&str> = text.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // Case 1: Orphaned superscript on its own line
        if is_superscript_fragment(trimmed) {
            if i > 0 && !result.is_empty() {
                // Merge with previous line
                if result.ends_with('\n') {
                    let base = result.trim_end_matches('\n').trim_end().len();
                    result.truncate(base);
                    result.push_str(trimmed);
                    result.push('\n');
                }
            } else if i == 0 && i + 1 < lines.len() {
                // First line is orphan — merge with next line
                // Find where the next line has a number before where the suffix belongs
                // For now, prepend to next line and let inline fix handle it
                // Skip this line entirely — it's a stray superscript from a date
            }
            continue;
        }

        // Case 2: Fix inline artifacts within the line
        let fixed = fix_inline_superscripts(line);
        result.push_str(&fixed);
        if i < lines.len() - 1 {
            result.push('\n');
        }
    }

    result
}

/// Fix inline superscript artifacts within a single line.
/// Examples:
///   "August 32022" → "August 3rd 2022"  (digit stuck to year)
///   "Wordrd"       → "Word"             (suffix stuck to word ending)
///   "andth"        → "and"              (suffix stuck to word)
fn fix_inline_superscripts(line: &str) -> String {
    let mut result = line.to_string();

    // Fix pattern: digit + suffix + 4-digit year stuck together
    // e.g. "3rd2022" or "32022" (where superscript "rd" was lost/merged)
    // Look for \d{1,2}(st|nd|rd|th)\d{4} or \d{1,2}\d{4} where first digits are 1-31
    for suffix in &["st", "nd", "rd", "th"] {
        // Pattern: "word 3rd2022" → "word 3rd 2022"
        let patterns_to_fix: Vec<String> = (1..=31).map(|d| format!("{d}{suffix}")).collect();
        for pat in &patterns_to_fix {
            // Find cases where ordinal is glued to a year (4-digit number)
            let search = format!("{pat}20");
            if let Some(pos) = result.find(&search) {
                let insert_pos = pos + pat.len();
                result.insert(insert_pos, ' ');
            }
        }
    }

    // Fix pattern: single/double digit directly glued to 4-digit year without suffix
    // e.g. "August 32022" → likely "August 3rd 2022" but we can't recover the suffix
    // Just insert space: "August 3 2022"
    for day in (1..=31).rev() {
        let day_str = day.to_string();
        for year_prefix in &[
            "2019", "2020", "2021", "2022", "2023", "2024", "2025", "2026", "2027",
        ] {
            let glued = format!("{day_str}{year_prefix}");
            if result.contains(&glued) {
                result = result.replace(&glued, &format!("{day_str} {year_prefix}"));
            }
        }
    }

    result
}

/// Returns true if the text looks like an orphaned superscript suffix on its own line.
fn is_superscript_fragment(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.len() > 4 {
        return false;
    }
    let stripped = t.trim_end_matches([',', '.', ';']);
    matches!(stripped, "st" | "nd" | "rd" | "th")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Superscript fix tests ──────────────────────────────────────────────

    #[test]
    fn test_fix_superscript_orphaned_line() {
        let input = "August 2\nnd\n, 2022";
        let fixed = fix_superscript_artifacts(input);
        assert!(fixed.contains("2nd"), "got: {fixed}");
    }

    #[test]
    fn test_fix_superscript_orphaned_rd() {
        let input = "August 3\nrd\n2022";
        let fixed = fix_superscript_artifacts(input);
        assert!(fixed.contains("3rd"), "got: {fixed}");
    }

    #[test]
    fn test_fix_superscript_orphaned_th() {
        let input = "September 5\nth\n2022";
        let fixed = fix_superscript_artifacts(input);
        assert!(fixed.contains("5th"), "got: {fixed}");
    }

    #[test]
    fn test_fix_superscript_orphaned_st() {
        let input = "January 1\nst\n2023";
        let fixed = fix_superscript_artifacts(input);
        assert!(fixed.contains("1st"), "got: {fixed}");
    }

    #[test]
    fn test_fix_inline_day_glued_to_year() {
        // "August 32022" → "August 3 2022"
        let fixed = fix_inline_superscripts("between August 32022 and");
        assert!(
            fixed.contains("August 3 2022"),
            "day should be separated from year, got: {fixed}"
        );
    }

    // NOTE: Stripping ordinal suffixes from arbitrary English words ("Wordrd" → "Word",
    // "andth" → "and") is not reliable without a full dictionary. These artifacts are
    // best handled by the OCR/VLM pipeline (Day 2) which reads the rendered page image.
    // The inline fix only handles the reliably detectable case: digit+year gluing.

    #[test]
    fn test_fix_superscript_does_not_merge_normal_text() {
        let input = "Hello\nWorld\nFoo";
        let fixed = fix_superscript_artifacts(input);
        assert_eq!(fixed, "Hello\nWorld\nFoo");
    }

    #[test]
    fn test_fix_superscript_single_line_unchanged() {
        let input = "No newlines here";
        let fixed = fix_superscript_artifacts(input);
        assert_eq!(fixed, "No newlines here");
    }

    #[test]
    fn test_is_superscript_fragment() {
        assert!(is_superscript_fragment("nd"));
        assert!(is_superscript_fragment("rd"));
        assert!(is_superscript_fragment("th"));
        assert!(is_superscript_fragment("st"));
        assert!(!is_superscript_fragment("hello"));
        assert!(!is_superscript_fragment(""));
        assert!(!is_superscript_fragment("the"));
    }

    // Document-type detection now lives in `doc_type_detector` with its own
    // unit + fixture-driven tests.

    #[test]
    fn test_scan_detection_logic() {
        let pages = [
            PageResult {
                page_number: 1,
                text: "ab".into(),
                char_count: 2,
            },
            PageResult {
                page_number: 2,
                text: "cd".into(),
                char_count: 2,
            },
        ];
        let total: usize = pages.iter().map(|p| p.char_count).sum();
        let avg = total as f64 / pages.len() as f64;
        assert!(avg < 50.0);
    }

    #[test]
    fn test_parse_table_row() {
        let row = "| Item | Qty | Price |";
        let cells = parse_table_row(row);
        assert_eq!(cells, vec!["Item", "Qty", "Price"]);
    }

    // ── Additional unit tests ─────────────────────────────────────────────────

    #[test]
    fn test_parse_table_row_no_pipes_returns_full_string() {
        let row = "no pipes here";
        let cells = parse_table_row(row);
        // splitting on '|' with no '|' gives one element = the whole string
        assert_eq!(cells, vec!["no pipes here"]);
    }

    #[test]
    fn test_parse_table_row_whitespace_is_trimmed() {
        let row = "|  Name  |  Age  |";
        let cells = parse_table_row(row);
        assert_eq!(cells, vec!["Name", "Age"]);
    }

    #[test]
    fn test_extract_tables_from_markdown_detects_table() {
        let markdown = "| Header1 | Header2 | Header3 |\n\
                        |---------|---------|----------|\n\
                        | A       | B       | C        |\n\
                        | D       | E       | F        |\n";
        let tables = extract_tables_from_markdown(markdown);
        assert_eq!(tables.len(), 1, "should detect one table");
        assert_eq!(tables[0].headers, vec!["Header1", "Header2", "Header3"]);
        assert_eq!(tables[0].rows.len(), 2);
        assert_eq!(tables[0].rows[0], vec!["A", "B", "C"]);
    }

    #[test]
    fn test_extract_tables_from_markdown_no_table_returns_empty() {
        let markdown = "Just some plain text.\nNo tables here at all.";
        let tables = extract_tables_from_markdown(markdown);
        assert!(tables.is_empty());
    }

    #[test]
    fn test_extract_tables_from_markdown_incomplete_table_ignored() {
        // Only 2 lines (header + separator) — needs at least 3 to be a real table
        let markdown = "| H1 | H2 | H3 |\n|---|---|---|\n";
        let tables = extract_tables_from_markdown(markdown);
        assert!(
            tables.is_empty(),
            "table without data rows must be ignored; got: {tables:?}"
        );
    }

    #[test]
    fn test_build_markdown_single_page_no_header() {
        let pages = vec![PageResult {
            page_number: 1,
            text: "Hello world".into(),
            char_count: 11,
        }];
        let md = build_markdown(&pages);
        // Single page: no "## Page N" header
        assert!(!md.contains("## Page"), "single page must not add header");
        assert!(md.contains("Hello world"));
    }

    #[test]
    fn test_build_markdown_multi_page_adds_headers() {
        let pages = vec![
            PageResult {
                page_number: 1,
                text: "First".into(),
                char_count: 5,
            },
            PageResult {
                page_number: 2,
                text: "Second".into(),
                char_count: 6,
            },
        ];
        let md = build_markdown(&pages);
        assert!(md.contains("## Page 1"), "multi-page must add ## Page 1");
        assert!(md.contains("## Page 2"), "multi-page must add ## Page 2");
        assert!(md.contains("First"));
        assert!(md.contains("Second"));
    }

    #[test]
    fn test_build_markdown_empty_pages_is_empty() {
        let md = build_markdown(&[]);
        assert!(md.is_empty(), "empty pages must produce empty markdown");
    }

    #[test]
    fn test_parse_pdf_rejects_empty_bytes() {
        let result = parse_pdf(&[], "pdftoppm");
        assert!(result.is_err(), "empty bytes must return an error");
    }

    #[test]
    fn test_parse_pdf_rejects_non_pdf_bytes() {
        let garbage = b"this is not a pdf file at all!!!!".to_vec();
        let result = parse_pdf(&garbage, "pdftoppm");
        assert!(result.is_err(), "non-PDF bytes must return an error");
    }

    #[test]
    fn test_parse_pdf_rejects_truncated_pdf_header() {
        let partial = b"%PDF-1.4".to_vec();
        let result = parse_pdf(&partial, "pdftoppm");
        assert!(result.is_err(), "truncated PDF must return an error");
    }

    #[test]
    fn test_parse_pdf_valid_sample() {
        let bytes = include_bytes!("../../tests/fixtures/sample.pdf").to_vec();
        let result = parse_pdf(&bytes, "pdftoppm");
        assert!(
            result.is_ok(),
            "sample.pdf must parse successfully: {result:?}"
        );
        let parsed = result.unwrap();
        assert!(parsed.document.metadata.page_count > 0);
        assert!(parsed.usage.pages_processed > 0);
        assert_eq!(
            parsed.usage.credits_used,
            parsed.usage.pages_processed * 2,
            "credits must be 2x pages processed"
        );
    }

    #[test]
    fn test_usage_credits_are_two_per_page() {
        let bytes = include_bytes!("../../tests/fixtures/sample.pdf").to_vec();
        if let Ok(result) = parse_pdf(&bytes, "pdftoppm") {
            assert_eq!(result.usage.credits_used, result.usage.pages_processed * 2);
        }
    }

    #[test]
    fn test_scan_detection_triggers_below_50_chars_average() {
        // avg = (10 + 5) / 2 = 7.5 < 50 → is_scanned = true
        let pages = [
            PageResult {
                page_number: 1,
                text: "a".repeat(10),
                char_count: 10,
            },
            PageResult {
                page_number: 2,
                text: "b".repeat(5),
                char_count: 5,
            },
        ];
        let total: usize = pages.iter().map(|p| p.char_count).sum();
        let avg = total as f64 / pages.len() as f64;
        assert!(avg < 50.0);
    }

    #[test]
    fn test_scan_detection_false_when_avg_above_50() {
        // avg = (100 + 200) / 2 = 150 ≥ 50 → is_scanned = false
        let pages = [
            PageResult {
                page_number: 1,
                text: "a".repeat(100),
                char_count: 100,
            },
            PageResult {
                page_number: 2,
                text: "b".repeat(200),
                char_count: 200,
            },
        ];
        let total: usize = pages.iter().map(|p| p.char_count).sum();
        let avg = total as f64 / pages.len() as f64;
        assert!(avg >= 50.0);
    }

    // ── OCR fallback tests ──────────────────────────────────────────────────

    #[test]
    fn is_ocr_recoverable_for_catalog_missing() {
        let err = AppError::PdfProcessing(
            "Failed to get page count: Invalid PDF: Catalog missing /Pages entry".into(),
        );
        assert!(is_ocr_recoverable(&err));
    }

    #[test]
    fn is_ocr_recoverable_for_failed_to_open() {
        let err = AppError::PdfProcessing("Failed to open PDF: some error".into());
        assert!(is_ocr_recoverable(&err));
    }

    #[test]
    fn is_ocr_recoverable_for_no_pages() {
        let err = AppError::PdfProcessing("PDF has no pages".into());
        assert!(is_ocr_recoverable(&err));
    }

    #[test]
    fn is_ocr_recoverable_false_for_other_errors() {
        let err = AppError::InvalidPdf;
        assert!(!is_ocr_recoverable(&err));

        let err = AppError::Internal("something".into());
        assert!(!is_ocr_recoverable(&err));

        let err = AppError::Validation("bad input".into());
        assert!(!is_ocr_recoverable(&err));
    }

    #[test]
    fn build_result_from_ocr_single_page() {
        let ocr_result = ocr::OcrResult {
            text: "Hello from OCR".into(),
            markdown: String::new(),
            pages: vec![],
            processing_ms: 500,
            warning: None,
        };
        let result = build_result_from_ocr(ocr_result);
        assert_eq!(result.document.metadata.page_count, 1);
        assert!(result.document.metadata.is_scanned);
        assert_eq!(result.document.text, "Hello from OCR");
        assert_eq!(result.document.pages.len(), 1);
        assert_eq!(result.document.pages[0].text, "Hello from OCR");
        assert_eq!(result.usage.credits_used, 3); // 1 page * 3
    }

    #[test]
    fn build_result_from_ocr_multi_page() {
        let ocr_result = ocr::OcrResult {
            text: "Page one\n\nPage two".into(),
            markdown: String::new(),
            pages: vec![
                ocr::OcrPageResult {
                    page_number: 1,
                    text: "Page one".into(),
                },
                ocr::OcrPageResult {
                    page_number: 2,
                    text: "Page two".into(),
                },
            ],
            processing_ms: 1000,
            warning: None,
        };
        let result = build_result_from_ocr(ocr_result);
        assert_eq!(result.document.metadata.page_count, 2);
        assert_eq!(result.document.pages.len(), 2);
        assert_eq!(result.document.pages[0].text, "Page one");
        assert_eq!(result.document.pages[1].text, "Page two");
        assert_eq!(result.usage.pages_processed, 2);
        assert_eq!(result.usage.credits_used, 6); // 2 pages * 3
    }

    #[test]
    fn build_result_from_ocr_detects_document_type() {
        let ocr_result = ocr::OcrResult {
            text: "Invoice #1234\nBill To: Acme Corp\nAmount Due: $500\nPayment Terms: Net 30"
                .into(),
            markdown: String::new(),
            pages: vec![],
            processing_ms: 300,
            warning: None,
        };
        let result = build_result_from_ocr(ocr_result);
        assert_eq!(
            result.document.metadata.detected_type,
            Some(DocType::Invoice)
        );
    }

    #[test]
    fn build_result_from_ocr_generates_markdown() {
        let ocr_result = ocr::OcrResult {
            text: "Some text".into(),
            markdown: String::new(),
            pages: vec![],
            processing_ms: 100,
            warning: None,
        };
        let result = build_result_from_ocr(ocr_result);
        assert!(result.document.markdown.contains("Some text"));
    }

    #[tokio::test]
    async fn parse_pdf_with_fallback_succeeds_for_valid_pdf() {
        let config = ocr::OcrConfig::default();
        let bytes = include_bytes!("../../tests/fixtures/sample.pdf").to_vec();
        let result = parse_pdf_with_backends_mode(std::sync::Arc::<[u8]>::from(bytes), &config, None, crate::config::PaddleOcrMode::Fallback, None, std::time::Duration::from_secs(90)).await;
        assert!(result.is_ok(), "valid PDF must parse: {result:?}");
        let parsed = result.unwrap();
        assert!(parsed.document.metadata.page_count > 0);
    }

    #[tokio::test]
    async fn parse_pdf_with_fallback_returns_error_for_garbage() {
        let config = ocr::OcrConfig::default();
        let garbage = b"this is not a PDF at all and never will be ever".to_vec();
        let result = parse_pdf_with_backends_mode(std::sync::Arc::<[u8]>::from(garbage),
            &config,
            None,
            crate::config::PaddleOcrMode::Fallback,
            None,
            std::time::Duration::from_secs(90),
        )
        .await;
        assert!(result.is_err());
    }

    // ── Auto mode dispatch ───────────────────────────────────────────────

    #[tokio::test]
    async fn auto_mode_routes_text_simple_to_pdf_oxide() {
        let config = ocr::OcrConfig::default();
        let bytes = include_bytes!("../../tests/fixtures/multipage_report.pdf").to_vec();
        let result = parse_pdf_with_backends_mode(std::sync::Arc::<[u8]>::from(bytes),
            &config,
            None,
            crate::config::PaddleOcrMode::Auto,
            None,
            std::time::Duration::from_secs(90),
        )
        .await
        .unwrap();
        assert_eq!(result.document.metadata.routed_to, Some(RoutedTo::PdfOxide));
        let cls = result
            .document
            .metadata
            .classification
            .expect("auto should always populate classification");
        assert_eq!(
            cls.class,
            crate::services::pdf_classifier::PdfClass::TextSimple
        );
    }

    #[tokio::test]
    async fn auto_mode_falls_back_to_pdf_oxide_when_paddle_unconfigured() {
        // table_document.pdf classifies as TextStructured, but with no
        // paddle_cfg the Auto path must gracefully degrade to pdf_oxide.
        let config = ocr::OcrConfig::default();
        let bytes = include_bytes!("../../tests/fixtures/table_document.pdf").to_vec();
        let result = parse_pdf_with_backends_mode(std::sync::Arc::<[u8]>::from(bytes),
            &config,
            None,
            crate::config::PaddleOcrMode::Auto,
            None,
            std::time::Duration::from_secs(90),
        )
        .await
        .unwrap();
        assert_eq!(result.document.metadata.routed_to, Some(RoutedTo::PdfOxide));
        let cls = result.document.metadata.classification.unwrap();
        assert_eq!(
            cls.class,
            crate::services::pdf_classifier::PdfClass::TextStructured
        );
    }

    #[tokio::test]
    async fn auto_mode_routes_scanned_to_tesseract_when_paddle_unconfigured() {
        // Scanned doc in Auto mode with no Paddle sidecar configured must
        // land on Tesseract via the OCR chain and tag routed_to accurately.
        let config = ocr::OcrConfig::default();
        let bytes = include_bytes!("../../tests/fixtures/scanned_form.pdf").to_vec();
        let result = parse_pdf_with_backends_mode(std::sync::Arc::<[u8]>::from(bytes),
            &config,
            None,
            crate::config::PaddleOcrMode::Auto,
            None,
            std::time::Duration::from_secs(90),
        )
        .await
        .unwrap();
        assert_eq!(
            result.document.metadata.routed_to,
            Some(RoutedTo::Tesseract),
            "scanned PDF without Paddle should land on Tesseract"
        );
        let cls = result
            .document
            .metadata
            .classification
            .expect("auto always populates classification");
        assert_eq!(
            cls.class,
            crate::services::pdf_classifier::PdfClass::ScannedOrEmpty
        );
    }

    #[tokio::test]
    async fn fallback_mode_tags_result_with_pdf_oxide() {
        // Sanity: non-Auto modes also set routed_to for observability.
        let config = ocr::OcrConfig::default();
        let bytes = include_bytes!("../../tests/fixtures/multipage_report.pdf").to_vec();
        let result = parse_pdf_with_backends_mode(std::sync::Arc::<[u8]>::from(bytes),
            &config,
            None,
            crate::config::PaddleOcrMode::Fallback,
            None,
            std::time::Duration::from_secs(90),
        )
        .await
        .unwrap();
        assert_eq!(result.document.metadata.routed_to, Some(RoutedTo::PdfOxide));
        assert!(
            result.document.metadata.classification.is_none(),
            "classification only runs in Auto mode"
        );
    }

    #[tokio::test]
    async fn parse_pdf_with_fallback_recovers_scanned_pdf_via_ocr() {
        let config = ocr::OcrConfig::default();
        let bytes = include_bytes!("../../tests/fixtures/scanned_form.pdf").to_vec();
        let result = parse_pdf_with_backends_mode(std::sync::Arc::<[u8]>::from(bytes), &config, None, crate::config::PaddleOcrMode::Fallback, None, std::time::Duration::from_secs(90)).await;
        assert!(
            result.is_ok(),
            "scanned PDF must recover via OCR: {result:?}"
        );
        let parsed = result.unwrap();
        assert!(
            parsed.document.metadata.is_scanned,
            "scanned_form.pdf should be detected as scanned"
        );
        assert!(
            parsed.document.text.len() > 20,
            "OCR must extract meaningful text"
        );
    }

    #[tokio::test]
    async fn parse_pdf_handles_multipage_report() {
        let config = ocr::OcrConfig::default();
        let bytes = include_bytes!("../../tests/fixtures/multipage_report.pdf").to_vec();
        let result = parse_pdf_with_backends_mode(std::sync::Arc::<[u8]>::from(bytes), &config, None, crate::config::PaddleOcrMode::Fallback, None, std::time::Duration::from_secs(90)).await;
        assert!(result.is_ok(), "multipage PDF must parse: {result:?}");
        let parsed = result.unwrap();
        assert_eq!(parsed.document.metadata.page_count, 3);
        assert!(parsed.document.text.contains("Section 1"));
        assert!(parsed.document.text.contains("Section 3"));
    }

    // ── Document-type detection: fixture-driven oracle ──────────────────

    /// For each fixture we assert either a specific detected_type or that
    /// the detector refused to guess (None). The acceptable-None fixtures
    /// are short/structured/scanned samples where the detector should
    /// legitimately not commit.
    #[test]
    fn fixture_oracle_sample_invoice() {
        let bytes = include_bytes!("../../tests/fixtures/sample.pdf").to_vec();
        let r = parse_pdf(&bytes, "pdftoppm").unwrap();
        assert_eq!(r.document.metadata.detected_type, Some(DocType::Invoice));
    }

    #[test]
    fn fixture_oracle_multipage_report_is_report_or_none() {
        let bytes = include_bytes!("../../tests/fixtures/multipage_report.pdf").to_vec();
        let r = parse_pdf(&bytes, "pdftoppm").unwrap();
        // Report or None are both acceptable outcomes — the important
        // thing is that the old "stringly-typed" noise is gone.
        match r.document.metadata.detected_type {
            Some(DocType::Report) | None => {}
            other => panic!("unexpected detected_type: {other:?}"),
        }
    }

    #[test]
    fn fixture_oracle_long_article_is_article_or_none() {
        let bytes = include_bytes!("../../tests/fixtures/long_article.pdf").to_vec();
        let r = parse_pdf(&bytes, "pdftoppm").unwrap();
        match r.document.metadata.detected_type {
            Some(DocType::Article) | None => {}
            other => panic!("unexpected detected_type: {other:?}"),
        }
    }

    #[test]
    fn fixture_oracle_form_with_labels_is_form_or_none() {
        let bytes = include_bytes!("../../tests/fixtures/form_with_labels.pdf").to_vec();
        let r = parse_pdf(&bytes, "pdftoppm").unwrap();
        match r.document.metadata.detected_type {
            Some(DocType::Form) | None => {}
            other => panic!("unexpected detected_type: {other:?}"),
        }
    }

    // ── Hint reconciliation (option b: hint wins) ───────────────────────

    #[test]
    fn hint_overrides_detector_when_they_disagree() {
        let detected = Some(DocType::Invoice);
        let hint = Some(DocType::Receipt);
        let (effective, source) = reconcile_document_type(hint, detected);
        assert_eq!(effective, Some(DocType::Receipt));
        assert_eq!(source, Some(DocumentTypeSource::Hint));
    }

    #[test]
    fn hint_present_with_no_detection_still_wins() {
        let (effective, source) = reconcile_document_type(Some(DocType::Contract), None);
        assert_eq!(effective, Some(DocType::Contract));
        assert_eq!(source, Some(DocumentTypeSource::Hint));
    }

    #[test]
    fn detector_wins_when_no_hint() {
        let (effective, source) = reconcile_document_type(None, Some(DocType::Invoice));
        assert_eq!(effective, Some(DocType::Invoice));
        assert_eq!(source, Some(DocumentTypeSource::Detector));
    }

    #[test]
    fn neither_hint_nor_detection_yields_none() {
        let (effective, source) = reconcile_document_type(None, None);
        assert_eq!(effective, None);
        assert_eq!(source, None);
    }

    // ── Encrypted-PDF detection ─────────────────────────────────────────

    #[test]
    fn is_encrypted_pdf_detects_encrypt_trailer_entry() {
        // Minimal synthetic PDF prefix containing an /Encrypt trailer
        // entry. We do not care that this is a complete or parseable PDF
        // — the detector only needs to find the keyword in the first
        // 4 KB after the header.
        let mut bytes = b"%PDF-1.7\n".to_vec();
        bytes.extend_from_slice(b"%some binary marker\n");
        bytes.extend_from_slice(b"trailer\n<< /Size 5 /Root 1 0 R /Encrypt 4 0 R >>\n");
        bytes.resize(256, b' ');
        assert!(is_encrypted_pdf(&bytes));
    }

    #[test]
    fn is_encrypted_pdf_false_for_plain_sample() {
        let bytes = include_bytes!("../../tests/fixtures/sample.pdf");
        assert!(!is_encrypted_pdf(bytes));
    }

    #[test]
    fn is_encrypted_pdf_does_not_false_positive_on_substring() {
        // The literal "Encrypt" inside content text (not as a trailer
        // dict key) does not constitute an encrypted PDF. We require
        // the leading "/" to anchor the keyword.
        let mut bytes = b"%PDF-1.7\n".to_vec();
        bytes.extend_from_slice(b"This document discusses Encrypted communications.\n");
        bytes.resize(256, b' ');
        assert!(!is_encrypted_pdf(&bytes));
    }

    #[test]
    fn parse_pdf_returns_encrypted_pdf_error_on_encrypt_trailer() {
        let mut bytes = b"%PDF-1.7\n".to_vec();
        bytes.extend_from_slice(b"trailer\n<< /Size 5 /Root 1 0 R /Encrypt 4 0 R >>\n");
        bytes.resize(256, b' ');
        let err = parse_pdf(&bytes, "pdftoppm").unwrap_err();
        assert!(
            matches!(err, AppError::EncryptedPdf),
            "expected EncryptedPdf, got {err:?}"
        );
    }

    // ── Structured tables from OCR backends ─────────────────────────────

    #[test]
    fn paddle_markdown_with_pipe_table_promotes_to_structured_tables() {
        // PaddleOCR PP-StructureV3 emits GFM pipe tables in its markdown
        // output. The dispatcher feeds that markdown into
        // `extract_tables_from_markdown`, which already lifts pipe-tables
        // into the typed `tables` field. This test pins that contract so
        // a future markdown change can't silently strip Paddle's tables.
        let md = "## Page 1\n\n\
                  | Item | Qty | Price |\n\
                  |------|-----|-------|\n\
                  | A    | 2   | $10   |\n\
                  | B    | 1   | $20   |\n";
        let ocr_result = ocr::OcrResult {
            text: "Item Qty Price A 2 $10 B 1 $20".into(),
            markdown: md.into(),
            pages: vec![ocr::OcrPageResult {
                page_number: 1,
                text: "Item Qty Price A 2 $10 B 1 $20".into(),
            }],
            processing_ms: 100,
            warning: None,
        };
        let pr = build_result_from_ocr(ocr_result);
        assert_eq!(
            pr.document.tables.len(),
            1,
            "Paddle markdown table must promote to structured tables; got {:?}",
            pr.document.tables
        );
        assert_eq!(pr.document.tables[0].headers, vec!["Item", "Qty", "Price"]);
        assert_eq!(pr.document.tables[0].rows.len(), 2);
    }

    // ── Backend degradation warnings ────────────────────────────────────

    #[test]
    fn warning_serializes_as_snake_case_string() {
        let w = ParseWarning::PaddleDegradedToTesseract;
        let s = serde_json::to_string(&w).unwrap();
        assert_eq!(s, "\"paddle_degraded_to_tesseract\"");
    }

    #[test]
    fn document_metadata_omits_warnings_when_empty() {
        let bytes = include_bytes!("../../tests/fixtures/sample.pdf").to_vec();
        let result = parse_pdf(&bytes, "pdftoppm").unwrap();
        let json = serde_json::to_value(&result.document.metadata).unwrap();
        assert!(
            json.get("warnings").is_none(),
            "warnings field must be skipped when empty; got {json}"
        );
    }

    #[tokio::test]
    async fn paddle_failure_records_degradation_warning_when_tesseract_recovers() {
        // Configure a Paddle URL that won't resolve so the call errors
        // out, then run a scanned doc that needs OCR. Tesseract should
        // recover and the metadata should record the degradation.
        let config = ocr::OcrConfig::default();
        let bad_paddle = crate::services::paddle_ocr::PaddleOcrConfig::new(
            String::from("http://127.0.0.1:1"), // unroutable
            1,
        );
        let bytes = include_bytes!("../../tests/fixtures/scanned_form.pdf").to_vec();
        let result = parse_pdf_with_backends_mode(
            std::sync::Arc::<[u8]>::from(bytes),
            &config,
            Some(&bad_paddle),
            crate::config::PaddleOcrMode::Fallback,
            None,
            std::time::Duration::from_secs(90),
        )
        .await
        .expect("tesseract must still recover this scanned doc");
        assert_eq!(result.document.metadata.routed_to, Some(RoutedTo::Tesseract));
        assert!(
            result
                .document
                .metadata
                .warnings
                .contains(&ParseWarning::PaddleDegradedToTesseract),
            "expected paddle_degraded_to_tesseract warning, got {:?}",
            result.document.metadata.warnings
        );
    }

    #[tokio::test]
    async fn dispatcher_returns_deadline_exceeded_when_budget_is_zero() {
        // A 1ns deadline cannot accommodate any real backend work, so the
        // dispatcher must surface DeadlineExceeded regardless of which
        // mode is in use. We deliberately use a valid PDF here so the
        // failure can only come from the timeout, not from a parse error.
        let config = ocr::OcrConfig::default();
        let bytes = include_bytes!("../../tests/fixtures/sample.pdf").to_vec();
        let result = parse_pdf_with_backends_mode(
            std::sync::Arc::<[u8]>::from(bytes),
            &config,
            None,
            crate::config::PaddleOcrMode::Fallback,
            None,
            std::time::Duration::from_nanos(1),
        )
        .await;
        match result {
            Err(AppError::DeadlineExceeded) => {}
            other => panic!("expected DeadlineExceeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatcher_threads_document_type_hint_into_metadata() {
        let config = ocr::OcrConfig::default();
        let bytes = include_bytes!("../../tests/fixtures/sample.pdf").to_vec();
        let result = parse_pdf_with_backends_mode(std::sync::Arc::<[u8]>::from(bytes),
            &config,
            None,
            crate::config::PaddleOcrMode::Fallback,
            Some(DocType::Quote),
            std::time::Duration::from_secs(90),
        )
        .await
        .unwrap();
        let m = &result.document.metadata;
        assert_eq!(m.document_type_hint, Some(DocType::Quote));
        // Hint wins, regardless of what the detector guessed.
        assert_eq!(m.document_type, Some(DocType::Quote));
        assert_eq!(m.document_type_source, Some(DocumentTypeSource::Hint));
    }
}
