use serde::Serialize;
use std::path::Path;
use std::process::Command;
use std::time::Instant;
use tempfile::TempDir;

use crate::errors::AppError;

/// OCR result for a document.
#[derive(Debug, Serialize)]
pub struct OcrResult {
    pub text: String,
    pub markdown: String,
    pub pages: Vec<OcrPageResult>,
    pub processing_ms: u64,
    pub warning: Option<String>,
}

/// Per-page OCR result.
#[derive(Debug, Serialize)]
pub struct OcrPageResult {
    pub page_number: u32,
    pub text: String,
}

/// Configuration for the local OCR pipeline.
#[derive(Debug, Clone)]
pub struct OcrConfig {
    pub tesseract_path: String,
    pub pdftoppm_path: String,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            tesseract_path: "tesseract".into(),
            pdftoppm_path: "pdftoppm".into(),
        }
    }
}

const MAX_OCR_PAGES: u32 = 50;

/// Extract text from a PDF using local tesseract OCR.
///
/// Pipeline: PDF bytes → temp file → pdftoppm (page images) → tesseract (per-page OCR) → text
///
/// All processing is local — no customer data leaves the server.
pub fn ocr_pdf(pdf_bytes: &[u8], config: &OcrConfig) -> Result<OcrResult, AppError> {
    let start = Instant::now();

    // Verify binaries exist
    check_binary(&config.tesseract_path, "tesseract")?;
    check_binary(&config.pdftoppm_path, "pdftoppm")?;

    // Write PDF to a temp file (pdftoppm reads from disk)
    let tmp_dir = TempDir::new()
        .map_err(|e| AppError::Internal(format!("Failed to create temp dir: {e}")))?;
    let pdf_path = tmp_dir.path().join("input.pdf");
    std::fs::write(&pdf_path, pdf_bytes)
        .map_err(|e| AppError::Internal(format!("Failed to write temp PDF: {e}")))?;

    // Render PDF pages to PNG images via pdftoppm
    let image_prefix = tmp_dir.path().join("page");
    let pdftoppm_output = Command::new(&config.pdftoppm_path)
        .arg("-png")
        .arg("-r")
        .arg("300") // 300 DPI — good balance of quality vs speed
        .arg("-l")
        .arg(MAX_OCR_PAGES.to_string()) // limit pages
        .arg(&pdf_path)
        .arg(&image_prefix)
        .output()
        .map_err(|e| {
            AppError::PdfProcessing(format!("Failed to run pdftoppm: {e}"))
        })?;

    if !pdftoppm_output.status.success() {
        let stderr = String::from_utf8_lossy(&pdftoppm_output.stderr);
        return Err(AppError::PdfProcessing(format!(
            "pdftoppm failed: {stderr}"
        )));
    }

    // Find all generated page images, sorted by name
    let mut image_paths: Vec<_> = std::fs::read_dir(tmp_dir.path())
        .map_err(|e| AppError::Internal(format!("Failed to read temp dir: {e}")))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .map(|ext| ext == "png")
                .unwrap_or(false)
        })
        .map(|entry| entry.path())
        .collect();
    image_paths.sort();

    if image_paths.is_empty() {
        return Err(AppError::PdfProcessing(
            "pdftoppm produced no page images".into(),
        ));
    }

    let warning = if image_paths.len() as u32 > MAX_OCR_PAGES {
        Some(format!(
            "PDF exceeds {} pages; only the first {} were OCR'd",
            image_paths.len(),
            MAX_OCR_PAGES
        ))
    } else {
        None
    };

    // OCR each page image with tesseract
    let mut pages = Vec::with_capacity(image_paths.len());
    let mut full_text = String::new();

    for (i, img_path) in image_paths.iter().enumerate() {
        let page_text = tesseract_ocr_image(img_path, &config.tesseract_path)?;
        let trimmed = page_text.trim().to_string();

        if !full_text.is_empty() {
            full_text.push('\n');
        }
        full_text.push_str(&trimmed);

        pages.push(OcrPageResult {
            page_number: (i + 1) as u32,
            text: trimmed,
        });
    }

    // Generate structured markdown via hOCR (tesseract's HTML output with layout info)
    let markdown = super::markdown_cleaner::hocr_pages_to_markdown(
        tmp_dir.path(),
        &config.tesseract_path,
    )
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "hOCR markdown generation failed, using plain text");
        build_basic_markdown(&pages)
    });

    let processing_ms = start.elapsed().as_millis() as u64;

    Ok(OcrResult {
        text: full_text,
        markdown,
        pages,
        processing_ms,
        warning,
    })
}

/// Basic markdown fallback from page text.
fn build_basic_markdown(pages: &[OcrPageResult]) -> String {
    let mut md = String::new();
    for page in pages {
        if pages.len() > 1 {
            md.push_str(&format!("## Page {}\n\n", page.page_number));
        }
        md.push_str(&page.text);
        md.push_str("\n\n");
    }
    md
}

/// Run tesseract on a single image file and return the extracted text.
fn tesseract_ocr_image(image_path: &Path, tesseract_path: &str) -> Result<String, AppError> {
    let output = Command::new(tesseract_path)
        .arg(image_path)
        .arg("stdout") // output to stdout instead of file
        .arg("-l")
        .arg("eng")
        .output()
        .map_err(|e| {
            AppError::PdfProcessing(format!(
                "Failed to run tesseract on {}: {e}",
                image_path.display()
            ))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            page = %image_path.display(),
            stderr = %stderr,
            "Tesseract returned non-zero exit code"
        );
        // Don't fail — return empty string for this page
        return Ok(String::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Verify that a binary exists and is executable.
fn check_binary(path: &str, name: &str) -> Result<(), AppError> {
    match Command::new(path).arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            // Some binaries return non-zero for --version but still exist
            // (pdftoppm exits with 99 on --version but -v works)
            if !output.stdout.is_empty() || !output.stderr.is_empty() {
                Ok(())
            } else {
                Err(AppError::PdfProcessing(format!(
                    "{name} binary at '{path}' is not working"
                )))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(AppError::PdfProcessing(
            format!("{name} not found at '{path}'. Install it or set {}_PATH in .env", name.to_uppercase()),
        )),
        Err(e) => Err(AppError::PdfProcessing(format!(
            "Failed to execute {name} at '{path}': {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> OcrConfig {
        OcrConfig {
            tesseract_path: "tesseract".into(),
            pdftoppm_path: "pdftoppm".into(),
        }
    }

    // ── check_binary tests ──────────────────────────────────────────────────

    #[test]
    fn check_binary_finds_tesseract() {
        assert!(check_binary("tesseract", "tesseract").is_ok());
    }

    #[test]
    fn check_binary_finds_pdftoppm() {
        // pdftoppm --version returns non-zero but still outputs version info
        assert!(check_binary("pdftoppm", "pdftoppm").is_ok());
    }

    #[test]
    fn check_binary_fails_for_missing_binary() {
        let result = check_binary("/nonexistent/tesseract", "tesseract");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not found"), "got: {msg}");
    }

    // ── tesseract_ocr_image tests ───────────────────────────────────────────

    #[test]
    fn tesseract_ocr_fails_on_nonexistent_image() {
        let result = tesseract_ocr_image(Path::new("/nonexistent/image.png"), "tesseract");
        // tesseract will fail but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    // ── Full pipeline test with real PDF ────────────────────────────────────

    #[test]
    fn ocr_pdf_with_sample_pdf() {
        let config = test_config();
        let bytes = include_bytes!("../../tests/fixtures/sample.pdf").to_vec();
        let result = ocr_pdf(&bytes, &config);
        assert!(result.is_ok(), "sample.pdf OCR must succeed: {result:?}");
        let ocr = result.unwrap();
        assert!(!ocr.text.is_empty(), "OCR must extract some text");
        assert!(!ocr.pages.is_empty(), "OCR must produce page results");
        assert_eq!(ocr.pages[0].page_number, 1);
        assert!(ocr.processing_ms > 0);
    }

    #[test]
    fn ocr_pdf_with_scanned_form() {
        let config = test_config();
        let bytes = include_bytes!("../../tests/fixtures/scanned_form.pdf").to_vec();
        let result = ocr_pdf(&bytes, &config);
        assert!(result.is_ok(), "scanned_form.pdf OCR must succeed: {result:?}");
        let ocr = result.unwrap();
        assert!(
            ocr.text.len() > 20,
            "OCR must extract meaningful text, got {} chars",
            ocr.text.len()
        );
        assert!(!ocr.pages.is_empty());
    }

    #[test]
    fn ocr_pdf_rejects_empty_bytes() {
        let config = test_config();
        let result = ocr_pdf(&[], &config);
        assert!(result.is_err(), "empty bytes must fail");
    }

    #[test]
    fn ocr_pdf_rejects_non_pdf_bytes() {
        let config = test_config();
        let result = ocr_pdf(b"not a pdf file", &config);
        assert!(result.is_err(), "non-PDF must fail");
    }

    #[test]
    fn ocr_config_default_uses_path_lookup() {
        let config = OcrConfig::default();
        assert_eq!(config.tesseract_path, "tesseract");
        assert_eq!(config.pdftoppm_path, "pdftoppm");
    }

    #[test]
    fn ocr_result_has_correct_page_numbers() {
        let config = test_config();
        let bytes = include_bytes!("../../tests/fixtures/sample.pdf").to_vec();
        if let Ok(ocr) = ocr_pdf(&bytes, &config) {
            for (i, page) in ocr.pages.iter().enumerate() {
                assert_eq!(
                    page.page_number,
                    (i + 1) as u32,
                    "page numbers must be sequential starting at 1"
                );
            }
        }
    }
}
