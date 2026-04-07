use actix_web::{http::StatusCode, HttpResponse};
use serde::Serialize;

/// Structured error response returned to clients.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

/// Error body with a stable code, safe message, and correlation ID.
///
/// `retryable` tells the caller whether retrying the same request is
/// likely to succeed. Transient infra errors (502/503/504/500) are
/// retryable; client-fault errors (400/401/403/404/422) are not. SDKs
/// can use this flag to drive automatic retry-with-backoff without
/// re-encoding the HTTP-status taxonomy.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub request_id: String,
    pub retryable: bool,
}

/// Application error type. Maps to HTTP status codes and stable error codes.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Invalid credentials")]
    InvalidCredentials,

    #[error("Token expired or malformed")]
    InvalidToken,

    #[error("Missing or invalid API key")]
    InvalidApiKey,

    #[error("Validation failed: {0}")]
    Validation(String),

    #[error("Resource already exists: {0}")]
    Conflict(String),

    #[error("Resource not found")]
    NotFound,

    #[error("File exceeds maximum size")]
    FileTooLarge,

    #[error("Uploaded file is not a valid PDF")]
    InvalidPdf,

    #[error("PDF processing failed: {0}")]
    PdfProcessing(String),

    #[error("Upstream API error: {0}")]
    UpstreamApi(String),

    #[error("Monthly page limit exceeded: {0}")]
    QuotaExceeded(String),

    #[error("Not implemented: {0}")]
    NotImplemented(String),

    #[error("Database error")]
    Database(#[from] sqlx::Error),

    #[error("Internal server error")]
    Internal(String),

    /// Request exceeded the configured wall-clock budget. Caller should
    /// retry with a smaller document or wait. The in-flight blocking work
    /// continues to completion in the background — the concurrency cap
    /// (`max_concurrent_parses`) is what prevents stuck tasks from
    /// compounding under load.
    #[error("Request deadline exceeded")]
    DeadlineExceeded,

    /// All concurrency permits are in use. Caller should retry after a
    /// short backoff (we surface `Retry-After: 5`).
    #[error("Service is busy, please retry")]
    ServiceBusy,

    /// PDF is password-protected. We do not attempt to crack or guess.
    #[error("PDF is encrypted")]
    EncryptedPdf,

    /// `/v1/extract`: the model returned data that does not validate
    /// against the customer-supplied JSON Schema. Returning the data
    /// anyway would be a worse customer surprise than failing loudly.
    #[error("Extracted data did not validate against schema: {0}")]
    SchemaValidationFailed(String),

    /// `/v1/extract`: the document markdown exceeded the configured
    /// `extract_max_input_chars` ceiling. Customers tune this to bound
    /// per-call Anthropic spend.
    #[error("Extract input too large: {actual} chars (limit {limit})")]
    ExtractInputTooLarge { actual: usize, limit: usize },
}

impl AppError {
    /// Stable error code string for the API response.
    fn code(&self) -> &str {
        match self {
            Self::InvalidCredentials => "INVALID_CREDENTIALS",
            Self::InvalidToken => "INVALID_TOKEN",
            Self::InvalidApiKey => "INVALID_API_KEY",
            Self::Validation(_) => "VALIDATION_ERROR",
            Self::Conflict(_) => "CONFLICT",
            Self::NotFound => "NOT_FOUND",
            Self::FileTooLarge => "PAYLOAD_TOO_LARGE",
            Self::InvalidPdf => "INVALID_FILE",
            Self::PdfProcessing(_) => "PDF_PROCESSING_ERROR",
            Self::UpstreamApi(_) => "UPSTREAM_API_ERROR",
            Self::QuotaExceeded(_) => "QUOTA_EXCEEDED",
            Self::NotImplemented(_) => "NOT_IMPLEMENTED",
            Self::Database(_) => "INTERNAL_ERROR",
            Self::Internal(_) => "INTERNAL_ERROR",
            Self::DeadlineExceeded => "DEADLINE_EXCEEDED",
            Self::ServiceBusy => "SERVICE_BUSY",
            Self::EncryptedPdf => "ENCRYPTED_PDF",
            Self::SchemaValidationFailed(_) => "SCHEMA_VALIDATION_FAILED",
            Self::ExtractInputTooLarge { .. } => "EXTRACT_INPUT_TOO_LARGE",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidCredentials => StatusCode::UNAUTHORIZED,
            Self::InvalidToken => StatusCode::UNAUTHORIZED,
            Self::InvalidApiKey => StatusCode::UNAUTHORIZED,
            Self::Validation(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::FileTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::InvalidPdf => StatusCode::BAD_REQUEST,
            Self::PdfProcessing(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::UpstreamApi(_) => StatusCode::BAD_GATEWAY,
            Self::QuotaExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
            Self::ServiceBusy => StatusCode::SERVICE_UNAVAILABLE,
            Self::EncryptedPdf => StatusCode::UNPROCESSABLE_ENTITY,
            Self::SchemaValidationFailed(_) => StatusCode::BAD_GATEWAY,
            Self::ExtractInputTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        }
    }

    /// Safe message for external consumption. Never leaks internals.
    fn safe_message(&self) -> String {
        match self {
            Self::Database(_) | Self::Internal(_) => "An internal error occurred".into(),
            // PdfProcessing carries subprocess stderr, tool paths, and
            // exit codes. Surface a generic message so an attacker who
            // can reliably trigger tesseract/pdftoppm failures cannot
            // enumerate the host filesystem or toolchain versions.
            Self::PdfProcessing(_) => {
                "PDF processing failed; the document may be corrupt or unsupported".into()
            }
            Self::QuotaExceeded(msg) => msg.clone(),
            Self::SchemaValidationFailed(detail) => {
                format!("Extracted data did not validate against schema: {detail}")
            }
            Self::ExtractInputTooLarge { actual, limit } => {
                format!("Extract input too large: {actual} chars (limit {limit})")
            }
            other => other.to_string(),
        }
    }

    /// Whether the error is transient — i.e. retrying the same request
    /// is likely to succeed eventually. Used to populate `retryable` in
    /// the response envelope so SDKs can drive retry-with-backoff.
    pub fn retryable(&self) -> bool {
        match self {
            // Transient infrastructure errors. `PdfProcessing` lives here
            // because it carries subprocess failures (tesseract crash,
            // pdftoppm exit code, missing OCR pages) — the same input may
            // succeed on a retry once load drops or the helper recovers.
            Self::DeadlineExceeded
            | Self::ServiceBusy
            | Self::Internal(_)
            | Self::Database(_)
            | Self::UpstreamApi(_)
            | Self::PdfProcessing(_) => true,

            // Permanent client-fault errors
            Self::InvalidCredentials
            | Self::InvalidToken
            | Self::InvalidApiKey
            | Self::Validation(_)
            | Self::Conflict(_)
            | Self::NotFound
            | Self::FileTooLarge
            | Self::InvalidPdf
            | Self::QuotaExceeded(_)
            | Self::NotImplemented(_)
            | Self::EncryptedPdf
            | Self::SchemaValidationFailed(_)
            | Self::ExtractInputTooLarge { .. } => false,
        }
    }

    /// Build the HTTP response for this error with the given request ID.
    pub fn to_response(&self, request_id: &str) -> HttpResponse {
        let body = ErrorResponse {
            error: ErrorBody {
                code: self.code().into(),
                message: self.safe_message(),
                request_id: request_id.into(),
                retryable: self.retryable(),
            },
        };
        let mut builder = HttpResponse::build(self.status());
        if matches!(self, Self::ServiceBusy) {
            builder.insert_header(("Retry-After", "5"));
        }
        builder.json(body)
    }
}

impl actix_web::ResponseError for AppError {
    fn status_code(&self) -> StatusCode {
        self.status()
    }

    fn error_response(&self) -> HttpResponse {
        // When we don't have a request ID (middleware not yet run), use empty string
        self.to_response("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Error code mapping ────────────────────────────────────────────────────

    #[test]
    fn invalid_credentials_maps_to_correct_code() {
        assert_eq!(AppError::InvalidCredentials.code(), "INVALID_CREDENTIALS");
    }

    #[test]
    fn invalid_token_maps_to_correct_code() {
        assert_eq!(AppError::InvalidToken.code(), "INVALID_TOKEN");
    }

    #[test]
    fn invalid_api_key_maps_to_correct_code() {
        assert_eq!(AppError::InvalidApiKey.code(), "INVALID_API_KEY");
    }

    #[test]
    fn validation_error_maps_to_correct_code() {
        assert_eq!(
            AppError::Validation("bad input".into()).code(),
            "VALIDATION_ERROR"
        );
    }

    #[test]
    fn conflict_maps_to_correct_code() {
        assert_eq!(AppError::Conflict("dup".into()).code(), "CONFLICT");
    }

    #[test]
    fn not_found_maps_to_correct_code() {
        assert_eq!(AppError::NotFound.code(), "NOT_FOUND");
    }

    #[test]
    fn file_too_large_maps_to_correct_code() {
        assert_eq!(AppError::FileTooLarge.code(), "PAYLOAD_TOO_LARGE");
    }

    #[test]
    fn invalid_pdf_maps_to_correct_code() {
        assert_eq!(AppError::InvalidPdf.code(), "INVALID_FILE");
    }

    #[test]
    fn pdf_processing_maps_to_correct_code() {
        assert_eq!(
            AppError::PdfProcessing("oops".into()).code(),
            "PDF_PROCESSING_ERROR"
        );
    }

    #[test]
    fn quota_exceeded_maps_to_correct_code() {
        assert_eq!(
            AppError::QuotaExceeded("limit reached".into()).code(),
            "QUOTA_EXCEEDED"
        );
    }

    #[test]
    fn not_implemented_maps_to_correct_code() {
        assert_eq!(
            AppError::NotImplemented("todo".into()).code(),
            "NOT_IMPLEMENTED"
        );
    }

    #[test]
    fn internal_maps_to_correct_code() {
        assert_eq!(AppError::Internal("boom".into()).code(), "INTERNAL_ERROR");
    }

    // ── New variants for Tier 1 hardening ──────────────────────────────

    #[test]
    fn deadline_exceeded_maps_to_504() {
        let err = AppError::DeadlineExceeded;
        assert_eq!(err.code(), "DEADLINE_EXCEEDED");
        assert_eq!(err.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn service_busy_maps_to_503() {
        let err = AppError::ServiceBusy;
        assert_eq!(err.code(), "SERVICE_BUSY");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn encrypted_pdf_maps_to_422() {
        let err = AppError::EncryptedPdf;
        assert_eq!(err.code(), "ENCRYPTED_PDF");
        assert_eq!(err.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn schema_validation_failed_maps_to_502() {
        let err = AppError::SchemaValidationFailed("path /foo: required".into());
        assert_eq!(err.code(), "SCHEMA_VALIDATION_FAILED");
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        assert!(
            err.safe_message().contains("path /foo"),
            "validation detail must be forwarded to caller"
        );
    }

    #[test]
    fn retryable_flag_true_for_transient_errors() {
        assert!(AppError::DeadlineExceeded.retryable());
        assert!(AppError::ServiceBusy.retryable());
        assert!(AppError::Internal("x".into()).retryable());
        assert!(AppError::UpstreamApi("x".into()).retryable());
        assert!(
            AppError::PdfProcessing("tesseract exited with code 1".into()).retryable(),
            "OCR subprocess failures are transient — clients should retry"
        );
    }

    #[test]
    fn retryable_flag_false_for_client_errors() {
        assert!(!AppError::InvalidPdf.retryable());
        assert!(!AppError::EncryptedPdf.retryable());
        assert!(!AppError::Validation("x".into()).retryable());
        assert!(!AppError::QuotaExceeded("x".into()).retryable());
        assert!(!AppError::ExtractInputTooLarge { actual: 1, limit: 0 }.retryable());
        assert!(!AppError::SchemaValidationFailed("x".into()).retryable());
    }

    #[actix_rt::test]
    async fn error_response_body_includes_retryable_flag() {
        let resp = AppError::ServiceBusy.to_response("req_test");
        let bytes = actix_web::body::to_bytes(resp.into_body()).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["retryable"], true);
        assert_eq!(json["error"]["code"], "SERVICE_BUSY");
    }

    #[actix_rt::test]
    async fn error_response_body_marks_client_error_non_retryable() {
        let resp = AppError::InvalidPdf.to_response("req_test");
        let bytes = actix_web::body::to_bytes(resp.into_body()).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["retryable"], false);
    }

    #[test]
    fn extract_input_too_large_maps_to_413() {
        let err = AppError::ExtractInputTooLarge { actual: 100, limit: 80 };
        assert_eq!(err.code(), "EXTRACT_INPUT_TOO_LARGE");
        assert_eq!(err.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn service_busy_response_includes_retry_after_header() {
        let resp = AppError::ServiceBusy.to_response("req_test");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let h = resp
            .headers()
            .get("retry-after")
            .expect("ServiceBusy must include Retry-After header");
        assert_eq!(h.to_str().unwrap(), "5");
    }

    // ── HTTP status code mapping ──────────────────────────────────────────────

    #[test]
    fn invalid_credentials_returns_401() {
        assert_eq!(
            AppError::InvalidCredentials.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn invalid_token_returns_401() {
        assert_eq!(AppError::InvalidToken.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn invalid_api_key_returns_401() {
        assert_eq!(AppError::InvalidApiKey.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn validation_error_returns_400() {
        assert_eq!(
            AppError::Validation("x".into()).status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn conflict_returns_409() {
        assert_eq!(
            AppError::Conflict("x".into()).status(),
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn not_found_returns_404() {
        assert_eq!(AppError::NotFound.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn file_too_large_returns_413() {
        assert_eq!(
            AppError::FileTooLarge.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn invalid_pdf_returns_400() {
        assert_eq!(AppError::InvalidPdf.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn pdf_processing_returns_422() {
        assert_eq!(
            AppError::PdfProcessing("x".into()).status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn quota_exceeded_returns_429() {
        assert_eq!(
            AppError::QuotaExceeded("x".into()).status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test]
    fn not_implemented_returns_501() {
        assert_eq!(
            AppError::NotImplemented("x".into()).status(),
            StatusCode::NOT_IMPLEMENTED
        );
    }

    #[test]
    fn internal_returns_500() {
        assert_eq!(
            AppError::Internal("x".into()).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    // ── safe_message() ────────────────────────────────────────────────────────

    #[test]
    fn internal_error_safe_message_does_not_leak_details() {
        let err = AppError::Internal("secret DB path /var/db/prod".into());
        let msg = err.safe_message();
        assert_eq!(msg, "An internal error occurred");
        assert!(
            !msg.contains("DB path"),
            "safe_message must not leak internal details"
        );
    }

    #[test]
    fn validation_error_safe_message_includes_description() {
        let err = AppError::Validation("email is required".into());
        let msg = err.safe_message();
        assert!(
            msg.contains("email is required"),
            "validation message should be forwarded: {msg}"
        );
    }

    #[test]
    fn pdf_processing_safe_message_does_not_leak_subprocess_details() {
        // PdfProcessing carries subprocess stderr/paths like
        // "tesseract exited with code 1: /var/tmp/page-3.png not found".
        // We must not forward that to callers — scrub it to a generic
        // user-facing message instead.
        let err = AppError::PdfProcessing(
            "tesseract exited with code 1: /var/tmp/page-3.png not found".into(),
        );
        let msg = err.safe_message();
        assert!(!msg.contains("tesseract"), "leaked tool name: {msg}");
        assert!(!msg.contains("/var/tmp"), "leaked filesystem path: {msg}");
        assert!(!msg.contains("exit"), "leaked subprocess detail: {msg}");
    }

    #[test]
    fn not_found_safe_message_is_stable() {
        let msg = AppError::NotFound.safe_message();
        assert_eq!(msg, "Resource not found");
    }

    #[test]
    fn quota_exceeded_safe_message_forwards_text() {
        let err = AppError::QuotaExceeded(
            "Monthly limit of 50 pages exceeded (50 used). Upgrade at /billing/plans".into(),
        );
        let msg = err.safe_message();
        assert!(msg.contains("Monthly limit of 50 pages exceeded"), "got: {msg}");
        assert!(msg.contains("/billing/plans"), "got: {msg}");
    }

    #[test]
    fn conflict_safe_message_includes_reason() {
        let msg = AppError::Conflict("email already registered".into()).safe_message();
        assert!(msg.contains("email already registered"), "got: {msg}");
    }

    // ── to_response() ─────────────────────────────────────────────────────────

    #[test]
    fn to_response_embeds_request_id() {
        let err = AppError::NotFound;
        let resp = err.to_response("req_abc123456789");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn to_response_uses_empty_request_id_when_not_available() {
        // error_response() (via ResponseError) passes "" — must not panic
        let err = AppError::InvalidCredentials;
        let resp = err.to_response("");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
