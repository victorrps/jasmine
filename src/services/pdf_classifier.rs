//! Heuristic PDF classifier.
//!
//! Takes the `DocumentResult` produced by `pdf_oxide`'s first pass and decides
//! which of four buckets the document falls into. This is step 1 of the
//! "auto routing" feature — it does NOT alter routing behaviour yet. The
//! router (step 2) will consume the `ClassificationReport` and pick a backend.
//!
//! # Design
//!
//! * **Pure & deterministic** — no I/O, no async, no model loading.
//! * **Cheap** — O(n) over the extracted text, target <50ms on any reasonable
//!   document. Intended to run inline in the request path.
//! * **Heuristic** — thresholds are hand-picked for v1 and will be tuned from
//!   production traffic (see the metrics plan in README). No learned weights.
//! * **Signal-transparent** — the report exposes the raw signals so the
//!   router can log them per request for later tuning.
//!
//! # Priority order
//!
//! 1. `ScannedOrEmpty` — no text layer / negligible character density
//! 2. `TextStructured` — tables, forms, or multi-column layouts detected
//! 3. `TextSimple`      — clean running text, no structure
//! 4. `Unknown`         — degenerate / inconclusive input

use serde::Serialize;

use super::pdf_parser::DocumentResult;

/// Coarse classification bucket for a parsed PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfClass {
    /// Clean running text with no detected structure — `pdf_oxide` is enough.
    TextSimple,
    /// Tables, labeled forms, or multi-column layouts — Paddle wins here.
    TextStructured,
    /// No text layer / image-only PDF — must go through OCR.
    ScannedOrEmpty,
    /// Degenerate or inconclusive input — caller should fall back to a safe default.
    Unknown,
}

/// Raw signals the classifier computed. Exposed so the router can log them
/// per request for future threshold tuning. Not serialized — this is a
/// server-internal structure kept out of API responses so callers can't
/// reverse-engineer the routing thresholds.
#[derive(Debug, Clone, Copy)]
pub struct ClassifierSignals {
    pub page_count: u32,
    pub chars_per_page: f64,
    /// Average `|` characters per non-empty line — catches ASCII tables.
    pub pipe_density: f64,
    /// Fraction of lines matching `Label: value`.
    pub label_density: f64,
    /// Fraction of lines with ≥2 mid-line multi-space gaps. Catches both
    /// tabular rows and multi-column layouts without distinguishing them.
    pub column_alignment: f64,
}

/// Full classifier output — class plus the evidence that led to it.
///
/// Do **not** serialize this into an API response. The raw signal values
/// allow a curious caller to reverse-engineer the classifier thresholds and
/// craft documents that reliably force expensive routing decisions. Keep it
/// server-side only (logs, internal metrics). The public projection that
/// ships to clients is [`ClassificationSummary`].
#[derive(Debug, Clone, Copy)]
pub struct ClassificationReport {
    pub class: PdfClass,
    pub signals: ClassifierSignals,
}

/// Client-facing projection of a classification result. Only exposes the
/// coarse class, not the raw numeric signals.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ClassificationSummary {
    pub class: PdfClass,
}

impl From<ClassificationReport> for ClassificationSummary {
    fn from(report: ClassificationReport) -> Self {
        Self { class: report.class }
    }
}

// ── Thresholds (v1, hand-picked) ─────────────────────────────────────────────
// These will be tuned from production traffic — do not treat them as precise.

/// Below this avg character density, treat as scanned/empty even if the
/// `is_scanned` flag was not set upstream.
const MIN_CHARS_PER_PAGE: f64 = 50.0;

/// Average `|` characters per non-empty line above which we assume an ASCII table.
const PIPE_DENSITY_TABLE: f64 = 0.5;

/// Fraction of lines with ≥2 mid-line multi-space gaps above which we assume
/// a table or multi-column layout on its own.
const COLUMN_ALIGNMENT_STRONG: f64 = 0.40;

/// Combined weak-signal threshold: `label_density + column_alignment ≥ this`
/// also counts as structured. Catches mixed documents where neither signal
/// alone is strong but both contribute.
const COMBINED_WEAK_SIGNAL: f64 = 0.25;

/// Upper bound on lines walked during classification. Prevents a pathological
/// PDF with millions of lines from pinning a CPU core. 5 000 non-empty lines
/// is ≈ 80 pages of dense text — plenty for classification confidence.
const MAX_CLASSIFIER_LINES: usize = 5_000;

/// Classify a parsed document. Pure function — safe to call anywhere.
pub fn classify(doc: &DocumentResult) -> ClassificationReport {
    let signals = compute_signals(doc);
    let class = decide(doc, &signals);
    ClassificationReport { class, signals }
}

fn decide(doc: &DocumentResult, s: &ClassifierSignals) -> PdfClass {
    // 1. Scanned / empty text layer — check BEFORE the degenerate-text check
    //    because scanned PDFs legitimately have zero extracted characters.
    if doc.metadata.is_scanned || s.chars_per_page < MIN_CHARS_PER_PAGE {
        return PdfClass::ScannedOrEmpty;
    }

    // 2. Degenerate input with a valid text layer but no content
    if s.page_count == 0 || doc.text.trim().is_empty() {
        return PdfClass::Unknown;
    }

    // 3. Structured signals
    if s.pipe_density >= PIPE_DENSITY_TABLE
        || s.column_alignment >= COLUMN_ALIGNMENT_STRONG
        || (s.label_density + s.column_alignment) >= COMBINED_WEAK_SIGNAL
    {
        return PdfClass::TextStructured;
    }

    // 4. Default: clean text
    PdfClass::TextSimple
}

fn compute_signals(doc: &DocumentResult) -> ClassifierSignals {
    let page_count = doc.metadata.page_count;
    let chars_per_page = if page_count == 0 {
        0.0
    } else {
        let total: usize = doc.pages.iter().map(|p| p.char_count).sum();
        total as f64 / page_count as f64
    };

    let lines: Vec<&str> = doc
        .text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(MAX_CLASSIFIER_LINES)
        .collect();
    let total_lines = lines.len().max(1) as f64;

    let pipe_density = lines
        .iter()
        .map(|l| l.matches('|').count() as f64)
        .sum::<f64>()
        / total_lines;

    let label_count = lines.iter().filter(|l| looks_like_label_line(l)).count() as f64;
    let label_density = label_count / total_lines;

    let column_alignment = compute_column_alignment(&lines);

    ClassifierSignals {
        page_count,
        chars_per_page,
        pipe_density,
        label_density,
        column_alignment,
    }
}

/// Detect `Label: value` lines without a regex engine.
///
/// Rules:
/// * First non-space character is uppercase ASCII
/// * Contains `: ` (colon + space) in the first 40 chars
/// * Left side of the colon is 1..=30 chars, mostly word characters
/// * Right side of the colon is non-empty and short-ish (<120 chars)
fn looks_like_label_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let first = match trimmed.chars().next() {
        Some(c) => c,
        None => return false,
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    // Slice at a UTF-8 char boundary — slicing at byte 80 would panic on
    // multi-byte chars (CJK, emoji, em-dash, etc.) in adversarial PDF text.
    let cut = trimmed
        .char_indices()
        .nth(80)
        .map_or(trimmed.len(), |(i, _)| i);
    let prefix = &trimmed[..cut];
    let colon = match prefix.find(": ") {
        Some(i) if (1..=30).contains(&i) => i,
        _ => return false,
    };
    let label = &trimmed[..colon];
    let rest = trimmed[colon + 2..].trim();
    if rest.is_empty() || rest.len() > 120 {
        return false;
    }
    // Label should be mostly alphanumerics/spaces/hyphens
    let bad = label
        .chars()
        .filter(|c| !(c.is_ascii_alphanumeric() || *c == ' ' || *c == '-' || *c == '_'))
        .count();
    bad == 0
}

/// Compute a column-alignment score in `[0.0, 1.0]`.
///
/// For each line we count "mid-line gaps" — runs of ≥2 consecutive spaces
/// that are neither leading nor trailing. A line with ≥2 such gaps looks
/// like a tabular row (`"Q1  1,200  $120,000  22%"`) or a two-column
/// layout row (`"left text    right text"`). The score is the fraction of
/// lines meeting that bar.
///
/// This collapses tables and multi-column into a single signal — the
/// router doesn't need to tell them apart, it just needs to know the
/// document isn't plain prose.
fn compute_column_alignment(lines: &[&str]) -> f64 {
    if lines.len() < 3 {
        return 0.0;
    }
    let aligned = lines
        .iter()
        .filter(|l| count_mid_line_gaps(l) >= 2)
        .count();
    aligned as f64 / lines.len() as f64
}

/// Count runs of ≥2 consecutive spaces that are strictly interior to the
/// line (not leading, not trailing).
fn count_mid_line_gaps(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut gaps = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b' ' {
            let start = i;
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            if i - start >= 2 && start > 0 && i < bytes.len() {
                gaps += 1;
            }
        } else {
            i += 1;
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::pdf_parser::{self, DocumentResult};

    // ── Signal helpers (no PDF needed) ─────────────────────────────────────

    #[test]
    fn label_line_simple_positive() {
        assert!(looks_like_label_line("Employee Name: Alice"));
        assert!(looks_like_label_line("Report ID: RPT-2024-Q3"));
    }

    #[test]
    fn label_line_lowercase_start_rejected() {
        assert!(!looks_like_label_line("notes: this is a note"));
    }

    #[test]
    fn label_line_prose_with_colon_rejected() {
        assert!(!looks_like_label_line(
            "This is a very long sentence that eventually has: a colon in it somewhere"
        ));
    }

    #[test]
    fn count_gaps_tabular_row() {
        let line = "Q1       1,200       $120,000  22%";
        assert!(count_mid_line_gaps(line) >= 2);
    }

    #[test]
    fn count_gaps_single_column_prose() {
        let line = "Just a normal prose line with only single spaces between words";
        assert_eq!(count_mid_line_gaps(line), 0);
    }

    #[test]
    fn count_gaps_ignores_leading_and_trailing_spaces() {
        assert_eq!(count_mid_line_gaps("    hello world    "), 0);
    }

    // ── Fixture-driven integration tests ───────────────────────────────────

    fn parse(bytes: &[u8]) -> DocumentResult {
        pdf_parser::parse_pdf(bytes.to_vec(), "pdftoppm")
            .expect("fixture must parse")
            .document
    }

    #[test]
    fn classifies_sample_invoice_as_text_structured() {
        // The sample invoice has labeled fields ("Bill To:", "Amount Due:",
        // "Payment Terms:") and a tabular layout — that IS structured.
        let doc = parse(include_bytes!("../../tests/fixtures/sample.pdf"));
        let report = classify(&doc);
        assert_eq!(
            report.class,
            PdfClass::TextStructured,
            "signals: {:?}",
            report.signals
        );
    }

    #[test]
    fn classifies_multipage_report_as_text_simple() {
        let doc = parse(include_bytes!("../../tests/fixtures/multipage_report.pdf"));
        assert_eq!(classify(&doc).class, PdfClass::TextSimple);
    }

    #[test]
    fn classifies_ordinal_dates_as_text_simple() {
        let doc = parse(include_bytes!("../../tests/fixtures/ordinal_dates.pdf"));
        let report = classify(&doc);
        assert_eq!(
            report.class,
            PdfClass::TextSimple,
            "signals: {:?}",
            report.signals
        );
    }

    #[test]
    fn classifies_long_article_as_text_simple() {
        let doc = parse(include_bytes!("../../tests/fixtures/long_article.pdf"));
        let report = classify(&doc);
        assert_eq!(
            report.class,
            PdfClass::TextSimple,
            "long prose should not be mistaken for structured; signals: {:?}",
            report.signals
        );
    }

    #[test]
    fn classifies_form_with_labels_as_text_structured() {
        let doc = parse(include_bytes!("../../tests/fixtures/form_with_labels.pdf"));
        let report = classify(&doc);
        assert_eq!(
            report.class,
            PdfClass::TextStructured,
            "signals: {:?}",
            report.signals
        );
    }

    #[test]
    fn classifies_table_document_as_text_structured() {
        let doc = parse(include_bytes!("../../tests/fixtures/table_document.pdf"));
        let report = classify(&doc);
        assert_eq!(
            report.class,
            PdfClass::TextStructured,
            "signals: {:?}",
            report.signals
        );
    }

    #[test]
    fn classifies_two_column_article_as_text_structured() {
        let doc = parse(include_bytes!("../../tests/fixtures/two_column_article.pdf"));
        let report = classify(&doc);
        assert_eq!(
            report.class,
            PdfClass::TextStructured,
            "signals: {:?}",
            report.signals
        );
    }

    #[test]
    fn classifies_mixed_content_as_text_structured() {
        let doc = parse(include_bytes!("../../tests/fixtures/mixed_content.pdf"));
        let report = classify(&doc);
        assert_eq!(
            report.class,
            PdfClass::TextStructured,
            "signals: {:?}",
            report.signals
        );
    }

    #[test]
    fn classifies_scanned_form_as_scanned() {
        let doc = parse(include_bytes!("../../tests/fixtures/scanned_form.pdf"));
        assert_eq!(classify(&doc).class, PdfClass::ScannedOrEmpty);
    }

    #[test]
    fn classifies_long_scanned_as_scanned() {
        let doc = parse(include_bytes!("../../tests/fixtures/long_scanned.pdf"));
        assert_eq!(classify(&doc).class, PdfClass::ScannedOrEmpty);
    }

    // ── Perf smoke ─────────────────────────────────────────────────────────

    #[test]
    fn classifier_is_fast_on_long_document() {
        let doc = parse(include_bytes!("../../tests/fixtures/long_article.pdf"));
        let start = std::time::Instant::now();
        for _ in 0..50 {
            let _ = classify(&doc);
        }
        let per_call = start.elapsed() / 50;
        assert!(
            per_call.as_millis() < 50,
            "classify should be <50ms, got {per_call:?}"
        );
    }
}
