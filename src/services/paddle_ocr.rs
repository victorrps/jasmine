//! PaddleOCR PP-StructureV3 client.
//!
//! Calls a running PaddleOCR serving instance (default port 8868) to perform
//! layout-aware document parsing with native Markdown output.
//!
//! PP-StructureV3 produces structured results with:
//! - Heading hierarchy detection
//! - Table recognition (cells → Markdown tables)
//! - Formula detection
//! - Reading order inference
//!
//! This is vastly superior to raw tesseract for document extraction.
//! All processing is local — no customer data leaves the server.
//!
//! Endpoint contract (PaddleX serving, v3.x):
//!   POST {base_url}/layout-parsing
//!   body: { "file": "<base64>", "fileType": 0|1 }   (0 = image, 1 = PDF)
//!   resp: { "result": { "layoutParsingResults": [ { "markdown": { "text": "..." }, "prunedResult": {...} }, ... ] } }

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

use crate::errors::AppError;
use crate::services::ocr::{OcrPageResult, OcrResult};

/// Configuration for the PaddleOCR client.
#[derive(Debug, Clone)]
pub struct PaddleOcrConfig {
    pub base_url: String,
    pub timeout_secs: u64,
}

impl PaddleOcrConfig {
    pub fn new(base_url: impl Into<String>, timeout_secs: u64) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            timeout_secs,
        }
    }
}

/// PaddleOCR serving request body.
#[derive(Debug, Serialize)]
struct LayoutRequest<'a> {
    file: &'a str,
    #[serde(rename = "fileType")]
    file_type: u8,
}

/// PaddleOCR serving response envelope.
#[derive(Debug, Deserialize)]
struct LayoutResponse {
    #[serde(default)]
    result: Option<LayoutResult>,
    #[serde(default)]
    #[serde(rename = "errorCode")]
    error_code: Option<i64>,
    #[serde(default)]
    #[serde(rename = "errorMsg")]
    error_msg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LayoutResult {
    #[serde(default)]
    #[serde(rename = "layoutParsingResults")]
    layout_parsing_results: Vec<PageResult>,
}

#[derive(Debug, Deserialize)]
struct PageResult {
    #[serde(default)]
    markdown: Option<MarkdownField>,
}

/// PaddleOCR returns markdown as either a string or an object `{ "text": "..." }`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MarkdownField {
    Text(String),
    Object {
        #[serde(default)]
        text: String,
    },
}

impl MarkdownField {
    fn into_string(self) -> String {
        match self {
            MarkdownField::Text(s) => s,
            MarkdownField::Object { text } => text,
        }
    }
}

/// Parse a PDF using PaddleOCR PP-StructureV3.
///
/// Returns an `OcrResult` compatible with the existing tesseract pipeline,
/// so callers can use either backend interchangeably.
pub async fn parse_pdf(
    pdf_bytes: &[u8],
    config: &PaddleOcrConfig,
) -> Result<OcrResult, AppError> {
    let start = Instant::now();

    let encoded = base64::engine::general_purpose::STANDARD.encode(pdf_bytes);
    let body = LayoutRequest {
        file: &encoded,
        file_type: 1, // PDF
    };

    let url = format!("{}/layout-parsing", config.base_url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs))
        .build()
        .map_err(|e| AppError::Internal(format!("Failed to build HTTP client: {e}")))?;

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            AppError::UpstreamApi(format!("PaddleOCR request failed: {e}"))
        })?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::UpstreamApi(format!(
            "PaddleOCR returned {status}: {}",
            truncate(&text, 300)
        )));
    }

    let parsed: LayoutResponse = resp.json().await.map_err(|e| {
        AppError::UpstreamApi(format!("Failed to decode PaddleOCR response: {e}"))
    })?;

    if let Some(code) = parsed.error_code {
        if code != 0 {
            let msg = parsed.error_msg.unwrap_or_else(|| "unknown error".into());
            return Err(AppError::UpstreamApi(format!(
                "PaddleOCR error {code}: {msg}"
            )));
        }
    }

    let page_results = parsed
        .result
        .map(|r| r.layout_parsing_results)
        .unwrap_or_default();

    if page_results.is_empty() {
        return Err(AppError::PdfProcessing(
            "PaddleOCR returned no pages".into(),
        ));
    }

    build_ocr_result(page_results, start.elapsed().as_millis() as u64)
}

fn build_ocr_result(
    pages: Vec<PageResult>,
    processing_ms: u64,
) -> Result<OcrResult, AppError> {
    let total_pages = pages.len();
    let mut page_markdowns: Vec<String> = Vec::with_capacity(total_pages);
    let mut ocr_pages: Vec<OcrPageResult> = Vec::with_capacity(total_pages);

    for (idx, page) in pages.into_iter().enumerate() {
        let md = page
            .markdown
            .map(MarkdownField::into_string)
            .unwrap_or_default();
        let md = md.trim().to_string();

        ocr_pages.push(OcrPageResult {
            page_number: (idx + 1) as u32,
            text: strip_markdown(&md),
        });
        page_markdowns.push(md);
    }

    let multi_page = total_pages > 1;
    let mut combined = String::new();
    for (i, md) in page_markdowns.iter().enumerate() {
        if multi_page {
            if i > 0 {
                combined.push('\n');
            }
            combined.push_str(&format!("## Page {}\n\n", i + 1));
        }
        combined.push_str(md);
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
    }

    let text = ocr_pages
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    Ok(OcrResult {
        text,
        markdown: combined.trim_end().to_string() + "\n",
        pages: ocr_pages,
        processing_ms,
        warning: None,
    })
}

/// Strip markdown syntax to produce a plain-text version of the content.
fn strip_markdown(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut in_code = false;
    for line in md.lines() {
        let trimmed = line.trim_start();

        // Skip code fences but keep their content
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        // Strip heading markers
        let without_heading = trimmed.trim_start_matches('#').trim_start();
        // Strip bullet markers
        let without_bullet = without_heading
            .strip_prefix("- ")
            .or_else(|| without_heading.strip_prefix("* "))
            .unwrap_or(without_heading);
        // Strip basic bold/italic markers
        let cleaned = without_bullet
            .replace("**", "")
            .replace("__", "");

        out.push_str(&cleaned);
        out.push('\n');
    }
    out.trim().to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Find a UTF-8 char boundary at or before `max` so we never split a
    // multi-byte sequence (panic-safe for non-ASCII upstream errors).
    let cut = (0..=max).rev().find(|i| s.is_char_boundary(*i)).unwrap_or(0);
    format!("{}…", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_trims_trailing_slash() {
        let cfg = PaddleOcrConfig::new("http://localhost:8868/", 120);
        assert_eq!(cfg.base_url, "http://localhost:8868");
    }

    #[test]
    fn config_preserves_clean_url() {
        let cfg = PaddleOcrConfig::new("http://localhost:8868", 120);
        assert_eq!(cfg.base_url, "http://localhost:8868");
    }

    #[test]
    fn markdown_field_string_variant() {
        let json = r#""hello world""#;
        let field: MarkdownField = serde_json::from_str(json).unwrap();
        assert_eq!(field.into_string(), "hello world");
    }

    #[test]
    fn markdown_field_object_variant() {
        let json = r#"{"text": "hello world"}"#;
        let field: MarkdownField = serde_json::from_str(json).unwrap();
        assert_eq!(field.into_string(), "hello world");
    }

    #[test]
    fn parses_valid_single_page_response() {
        let json = "{\"result\":{\"layoutParsingResults\":[{\"markdown\":{\"text\":\"# Hello\\n\\nBody text.\"}}]}}";
        let parsed: LayoutResponse = serde_json::from_str(json).unwrap();
        let pages = parsed.result.unwrap().layout_parsing_results;
        assert_eq!(pages.len(), 1);
    }

    #[test]
    fn parses_multi_page_response() {
        let json = r#"{
            "result": {
                "layoutParsingResults": [
                    {"markdown": {"text": "Page one"}},
                    {"markdown": {"text": "Page two"}},
                    {"markdown": {"text": "Page three"}}
                ]
            }
        }"#;
        let parsed: LayoutResponse = serde_json::from_str(json).unwrap();
        let pages = parsed.result.unwrap().layout_parsing_results;
        assert_eq!(pages.len(), 3);
    }

    #[test]
    fn build_result_single_page_has_no_page_header() {
        let pages = vec![PageResult {
            markdown: Some(MarkdownField::Text("# Title\n\nBody".into())),
        }];
        let result = build_ocr_result(pages, 42).unwrap();
        assert!(!result.markdown.contains("## Page 1"));
        assert!(result.markdown.contains("# Title"));
        assert_eq!(result.pages.len(), 1);
        assert_eq!(result.processing_ms, 42);
    }

    #[test]
    fn build_result_multi_page_adds_page_headers() {
        let pages = vec![
            PageResult {
                markdown: Some(MarkdownField::Text("First page content".into())),
            },
            PageResult {
                markdown: Some(MarkdownField::Text("Second page content".into())),
            },
        ];
        let result = build_ocr_result(pages, 0).unwrap();
        assert!(result.markdown.contains("## Page 1"));
        assert!(result.markdown.contains("## Page 2"));
        assert!(result.markdown.contains("First page content"));
        assert!(result.markdown.contains("Second page content"));
        assert_eq!(result.pages.len(), 2);
    }

    #[test]
    fn build_result_empty_pages_errors() {
        // Empty pages via the public API would hit the empty check earlier,
        // but ensure build_ocr_result handles an empty markdown field gracefully.
        let pages = vec![PageResult { markdown: None }];
        let result = build_ocr_result(pages, 0).unwrap();
        assert_eq!(result.pages.len(), 1);
    }

    #[test]
    fn strip_markdown_removes_headings() {
        let md = "# Title\n\n## Subtitle\n\nBody";
        let text = strip_markdown(md);
        assert!(!text.contains('#'));
        assert!(text.contains("Title"));
        assert!(text.contains("Subtitle"));
        assert!(text.contains("Body"));
    }

    #[test]
    fn strip_markdown_removes_bullets() {
        let md = "- item one\n- item two";
        let text = strip_markdown(md);
        assert!(!text.contains("- "));
        assert!(text.contains("item one"));
    }

    #[test]
    fn strip_markdown_removes_bold() {
        let text = strip_markdown("**bold** and __also bold__");
        assert_eq!(text, "bold and also bold");
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let s = "a".repeat(500);
        let out = truncate(&s, 10);
        assert_eq!(out.chars().count(), 11); // 10 chars + ellipsis
    }

    #[test]
    fn parses_error_response() {
        let json = r#"{"errorCode": 500, "errorMsg": "model failed"}"#;
        let parsed: LayoutResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.error_code, Some(500));
        assert_eq!(parsed.error_msg.as_deref(), Some("model failed"));
    }
}
