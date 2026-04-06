//! Live integration test for the PaddleOCR sidecar.
//!
//! Gated by `PADDLEOCR_URL` — skipped silently when the env var is unset so
//! CI doesn't require a running sidecar. Run locally with:
//!
//!   PADDLEOCR_URL=http://127.0.0.1:8868 cargo test --test test_paddle_ocr_live -- --nocapture

use docforge::services::paddle_ocr::{parse_pdf, PaddleOcrConfig};

const SAMPLE_PDF: &[u8] = include_bytes!("fixtures/sample.pdf");

#[tokio::test]
async fn paddle_sidecar_parses_sample_pdf() {
    let Ok(url) = std::env::var("PADDLEOCR_URL") else {
        eprintln!("PADDLEOCR_URL not set — skipping live sidecar test");
        return;
    };

    let cfg = PaddleOcrConfig::new(url, 120);
    let result = parse_pdf(SAMPLE_PDF, &cfg)
        .await
        .expect("paddle sidecar should parse the sample PDF");

    assert!(!result.pages.is_empty(), "expected at least one page");
    assert!(
        !result.markdown.trim().is_empty(),
        "expected non-empty markdown"
    );
    assert!(
        result.markdown.trim().len() >= 10,
        "markdown suspiciously short: {:?}",
        result.markdown
    );
    assert!(result.processing_ms > 0, "processing_ms should be set");
}
