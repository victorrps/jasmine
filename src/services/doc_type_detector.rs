//! Document-type detection.
//!
//! Replaces the old keyword scorer in `pdf_parser.rs` with a weighted
//! multi-feature classifier that:
//!
//! * Tokenizes properly so `"report"` does not match inside `"sharepoint"`
//! * Scores each type via strong phrases + positive/negative keywords
//! * Returns a confidence and top-2 alternates, not just a label
//! * Refuses to guess — if the winner is below the absolute/margin bar,
//!   `type_` is `None`
//!
//! # Design notes
//!
//! * Pure function, deterministic, no I/O, no ML. Same discipline as
//!   `pdf_classifier`.
//! * Thresholds and feature lists are v1 hand-picked values. They will be
//!   tuned from production traffic once the observability phase ships.
//! * **Compliance scope**: the controlled vocabulary intentionally excludes
//!   `medical_record`, `id_document`, and `tax_form`. Adding those categories
//!   changes the compliance classification of this service (HIPAA, KYC,
//!   PII-under-GLBA) and must be a separate decision with legal review.
//! * **Signal privacy**: the raw per-feature scores are logged server-side
//!   (by the dispatcher) but never serialized into the API response — only
//!   the label, confidence, and top-2 alternates ship to clients. Same
//!   reasoning as `ClassifierSignals`: we do not want callers to
//!   reverse-engineer the feature weights and craft inputs that force
//!   expensive routing or misclassification.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Controlled vocabulary of document types.
///
/// Wire format is snake_case. This enum is the API contract for both the
/// response `document_type` field and the request `document_type_hint`
/// parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocType {
    Invoice,
    Receipt,
    Contract,
    Resume,
    BankStatement,
    Letter,
    Invitation,
    Report,
    PurchaseOrder,
    Quote,
    AcademicPaper,
    Article,
    Form,
    /// Escape hatch: customer knows their document is not in the list above.
    Other,
}

impl DocType {
    /// Parse a document-type hint from a free-text field (e.g. a multipart
    /// form value). Case-insensitive, trims whitespace, accepts both
    /// snake_case (`bank_statement`) and natural-language with spaces
    /// (`bank statement`).
    pub fn from_hint_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace(' ', "_").as_str() {
            "invoice" => Some(Self::Invoice),
            "receipt" => Some(Self::Receipt),
            "contract" => Some(Self::Contract),
            "resume" | "cv" => Some(Self::Resume),
            "bank_statement" => Some(Self::BankStatement),
            "letter" => Some(Self::Letter),
            "invitation" => Some(Self::Invitation),
            "report" => Some(Self::Report),
            "purchase_order" | "po" => Some(Self::PurchaseOrder),
            "quote" | "estimate" => Some(Self::Quote),
            "academic_paper" | "paper" => Some(Self::AcademicPaper),
            "article" | "news" => Some(Self::Article),
            "form" => Some(Self::Form),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

/// Where the effective `document_type` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentTypeSource {
    /// Caller supplied a `document_type_hint` — that value is effective.
    Hint,
    /// Auto-detected from extracted text.
    Detector,
}

/// A runner-up classification with its normalized confidence.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct TypeAlternate {
    #[serde(rename = "type")]
    pub type_: DocType,
    pub confidence: f64,
}

/// Detector output — answer, confidence, and top alternates.
#[derive(Debug, Clone, Default)]
pub struct DetectionResult {
    pub type_: Option<DocType>,
    pub confidence: f64,
    pub alternates: Vec<TypeAlternate>,
    /// Full per-type raw scores — **server-side only**, never serialized.
    /// Exposed so the dispatcher can log them via `tracing` for later
    /// tuning. See the module docs for the privacy rationale.
    pub debug_scores: Vec<(DocType, f64)>,
}

// ── Feature table ────────────────────────────────────────────────────────────

struct TypeFeatures {
    type_: DocType,
    /// Multi-word phrases matched as literal substrings in the lowercased,
    /// whitespace-normalized sample. Each hit contributes `STRONG_WEIGHT`.
    strong_phrases: &'static [&'static str],
    /// Single-word tokens matched on word boundaries. Each unique hit
    /// contributes `POSITIVE_WEIGHT` up to `POSITIVE_CAP`.
    positive_keywords: &'static [&'static str],
    /// Tokens that rule the type out. Each unique hit subtracts
    /// `NEGATIVE_WEIGHT`.
    negative_keywords: &'static [&'static str],
}

const FEATURES: &[TypeFeatures] = &[
    TypeFeatures {
        type_: DocType::Invoice,
        strong_phrases: &[
            "bill to",
            "amount due",
            "payment terms",
            "invoice number",
            "invoice #",
            "sold to",
            "total due",
        ],
        positive_keywords: &[
            "invoice", "subtotal", "tax", "due", "balance", "payable", "net",
        ],
        negative_keywords: &["dear", "sincerely", "objective", "whereas", "abstract"],
    },
    TypeFeatures {
        type_: DocType::Receipt,
        strong_phrases: &[
            "total paid",
            "payment received",
            "thank you for your",
            "cash tendered",
            "change due",
            "merchant copy",
        ],
        positive_keywords: &["receipt", "paid", "tendered", "cashier", "tip"],
        negative_keywords: &["bill to", "amount due", "whereas"],
    },
    TypeFeatures {
        type_: DocType::Contract,
        strong_phrases: &[
            "this agreement",
            "party of the first part",
            "party of the second part",
            "effective date",
            "terms and conditions",
            "the parties agree",
            "hereby agree",
            "shall not",
            "in witness whereof",
        ],
        positive_keywords: &[
            "agreement", "whereas", "hereby", "parties", "clause", "herein",
        ],
        negative_keywords: &["invoice", "receipt", "dear"],
    },
    TypeFeatures {
        type_: DocType::Resume,
        strong_phrases: &[
            "work experience",
            "professional experience",
            "education",
            "technical skills",
            "career objective",
        ],
        positive_keywords: &[
            "experience",
            "education",
            "skills",
            "objective",
            "references",
            "certifications",
        ],
        negative_keywords: &["whereas", "amount due", "invoice", "bill to"],
    },
    TypeFeatures {
        type_: DocType::BankStatement,
        strong_phrases: &[
            "account number",
            "statement period",
            "opening balance",
            "closing balance",
            "available balance",
            "transaction history",
        ],
        positive_keywords: &[
            "balance",
            "withdrawal",
            "deposit",
            "statement",
            "ending",
            "beginning",
        ],
        negative_keywords: &["invoice", "receipt", "dear", "objective"],
    },
    TypeFeatures {
        type_: DocType::Letter,
        strong_phrases: &[
            "to whom it may concern",
            "kind regards",
            "kindest regards",
            "best regards",
            "yours sincerely",
            "yours truly",
        ],
        positive_keywords: &["dear", "sincerely", "regards"],
        negative_keywords: &[
            "amount due",
            "bill to",
            "whereas",
            "invoice",
            "abstract",
        ],
    },
    TypeFeatures {
        type_: DocType::Invitation,
        strong_phrases: &[
            "you are invited",
            "cordially invite",
            "request the pleasure",
            "please join us",
            "rsvp",
        ],
        positive_keywords: &["invitation", "invited", "celebrate", "rsvp", "host"],
        negative_keywords: &["invoice", "whereas", "objective"],
    },
    TypeFeatures {
        type_: DocType::Report,
        strong_phrases: &[
            "executive summary",
            "quarterly report",
            "quarterly summary",
            "annual report",
            "operations review",
            "findings and recommendations",
            "key findings",
            "in summary",
            "quarterly sales",
            "sales report",
        ],
        positive_keywords: &[
            "report",
            "findings",
            "conclusion",
            "summary",
            "analysis",
            "recommendation",
            "quarterly",
            "metrics",
            "section",
        ],
        negative_keywords: &["dear", "sincerely", "invoice", "bill to"],
    },
    TypeFeatures {
        type_: DocType::PurchaseOrder,
        strong_phrases: &[
            "purchase order",
            "po number",
            "ship to",
            "deliver to",
            "requested by",
        ],
        positive_keywords: &["quantity", "unit", "supplier", "vendor", "ordered"],
        negative_keywords: &["dear", "whereas", "abstract"],
    },
    TypeFeatures {
        type_: DocType::Quote,
        strong_phrases: &[
            "quotation",
            "price quote",
            "valid until",
            "quote number",
            "estimated cost",
            "this quote",
        ],
        positive_keywords: &["quote", "estimate", "pricing", "valid", "expires"],
        negative_keywords: &["paid", "received", "whereas"],
    },
    TypeFeatures {
        type_: DocType::AcademicPaper,
        strong_phrases: &[
            "abstract",
            "introduction",
            "related work",
            "methodology",
            "experimental results",
            "we propose",
            "prior work",
            "bibliography",
        ],
        positive_keywords: &[
            "abstract",
            "et al",
            "dataset",
            "experiment",
            "figure",
            "citation",
        ],
        negative_keywords: &["invoice", "dear", "amount due"],
    },
    TypeFeatures {
        type_: DocType::Article,
        strong_phrases: &[
            "news bulletin",
            "breaking news",
            "a survey of",
            "blog post",
            "published in",
            "by the author",
        ],
        positive_keywords: &["article", "author", "news", "bulletin", "column", "published"],
        negative_keywords: &["invoice", "amount due", "whereas", "abstract"],
    },
    TypeFeatures {
        type_: DocType::Form,
        strong_phrases: &[
            "onboarding form",
            "registration form",
            "application form",
            "request form",
            "please complete",
            "applicant name",
            "employee name",
            "date of birth",
        ],
        positive_keywords: &[
            "form",
            "applicant",
            "signature",
            "submit",
            "department",
        ],
        negative_keywords: &["dear", "sincerely", "whereas", "abstract", "invoice"],
    },
];

// ── Tunable thresholds (v1, hand-picked) ─────────────────────────────────────

/// Weight applied per matching strong phrase.
const STRONG_WEIGHT: f64 = 3.0;
/// Weight applied per matching positive keyword (word-boundary match).
const POSITIVE_WEIGHT: f64 = 1.0;
/// Cap on positive-keyword contribution so keyword-stuffed docs don't dominate.
const POSITIVE_CAP: f64 = 5.0;
/// Penalty per matching negative keyword.
const NEGATIVE_WEIGHT: f64 = 2.0;

/// Maximum byte length accepted for the `document_type_hint` multipart
/// field. The longest valid hint string is ~20 bytes; 64 is a generous
/// cap that tolerates trailing whitespace without rejecting the request.
pub const MAX_HINT_BYTES: usize = 64;

/// Minimum absolute winning score to declare a detection at all. Set
/// equal to `STRONG_WEIGHT` on purpose — one matching strong phrase is
/// sufficient on its own, because the strong-phrase list is curated to
/// be highly specific. Positive keywords alone will never pass the
/// floor without at least one strong phrase or three positive hits.
const MIN_ABSOLUTE_SCORE: f64 = 3.0;
/// Minimum gap between the top score and the runner-up to avoid ties.
const MIN_MARGIN: f64 = 1.0;

/// Upper bound on characters examined by the detector. Same spirit as
/// `pdf_classifier::MAX_CLASSIFIER_LINES` — prevents pathological docs
/// from blowing the latency budget.
const MAX_SAMPLE_CHARS: usize = 4_000;

// ── Public entry point ───────────────────────────────────────────────────────

/// Detect the document type from extracted text. Deterministic, pure.
pub fn detect(text: &str) -> DetectionResult {
    if text.trim().is_empty() {
        return DetectionResult::default();
    }

    let sample: String = text.chars().take(MAX_SAMPLE_CHARS).collect();
    let lower = sample.to_lowercase();
    let normalized = normalize_whitespace(&lower);
    // Build the token set once — keyword lookup is O(1) per feature
    // entry instead of O(tokens). See `score_type`.
    let tokens: HashSet<&str> = tokenize(&normalized).into_iter().collect();

    // Score every type.
    let mut scores: Vec<(DocType, f64)> = FEATURES
        .iter()
        .map(|f| (f.type_, score_type(&normalized, &tokens, f)))
        .collect();

    // Sort descending by score.
    scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
    });

    let top = scores.first().copied();
    let second = scores.get(1).copied();

    let top_score = top.map(|(_, s)| s).unwrap_or(0.0);
    let second_score = second.map(|(_, s)| s).unwrap_or(0.0);

    // Require both an absolute floor and a gap over the runner-up.
    let detected = if top_score >= MIN_ABSOLUTE_SCORE
        && (top_score - second_score) >= MIN_MARGIN
    {
        top.map(|(t, _)| t)
    } else {
        None
    };

    // Confidence: normalized share of the total non-negative score mass.
    let total_mass: f64 = scores.iter().map(|(_, s)| s.max(0.0)).sum();
    let confidence = if detected.is_some() && total_mass > 0.0 {
        (top_score / total_mass).clamp(0.0, 1.0)
    } else {
        0.0
    };

    // Top-2 alternates (skipping the winner if we declared one).
    let skip = if detected.is_some() { 1 } else { 0 };
    let alternates: Vec<TypeAlternate> = scores
        .iter()
        .skip(skip)
        .take(2)
        .filter(|(_, s)| *s > 0.0)
        .map(|(t, s)| TypeAlternate {
            type_: *t,
            confidence: if total_mass > 0.0 {
                (*s / total_mass).clamp(0.0, 1.0)
            } else {
                0.0
            },
        })
        .collect();

    DetectionResult {
        type_: detected,
        confidence,
        alternates,
        debug_scores: scores,
    }
}

// ── Scoring helpers ──────────────────────────────────────────────────────────

fn score_type(normalized: &str, tokens: &HashSet<&str>, f: &TypeFeatures) -> f64 {
    let mut score = 0.0;

    // Strong phrases: literal substring match on the whitespace-normalized
    // sample. These are multi-word anchors, substring match is intentional.
    for phrase in f.strong_phrases {
        if normalized.contains(phrase) {
            score += STRONG_WEIGHT;
        }
    }

    // Positive keywords: unique word-boundary hits, capped. O(1) per
    // keyword thanks to the HashSet built by `detect`.
    let positive_hits = f
        .positive_keywords
        .iter()
        .filter(|kw| tokens.contains(*kw))
        .count() as f64;
    score += (positive_hits * POSITIVE_WEIGHT).min(POSITIVE_CAP);

    // Negative keywords: flat penalty per unique hit (no cap — ruling out
    // should be decisive).
    let negative_hits = f
        .negative_keywords
        .iter()
        .filter(|kw| tokens.contains(*kw))
        .count() as f64;
    score -= negative_hits * NEGATIVE_WEIGHT;

    score
}

/// Collapse all runs of whitespace to a single space.
fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split on anything that's not alphanumeric or `'`. Returns lower-case
/// tokens suitable for word-boundary matching against `positive_keywords`.
fn tokenize(s: &str) -> Vec<&str> {
    s.split(|c: char| !c.is_ascii_alphanumeric() && c != '\'')
        .filter(|t| !t.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tokenizer ─────────────────────────────────────────────────────────

    #[test]
    fn tokenize_splits_on_punctuation() {
        let tokens = tokenize("hello, world! foo-bar baz.");
        assert_eq!(tokens, vec!["hello", "world", "foo", "bar", "baz"]);
    }

    #[test]
    fn tokenize_keeps_apostrophes() {
        let tokens = tokenize("it's john's report");
        assert_eq!(tokens, vec!["it's", "john's", "report"]);
    }

    // ── Word boundary matching prevents false positives ──────────────────

    #[test]
    fn report_keyword_does_not_match_inside_sharepoint() {
        let text = "Visit our sharepoint for updates about reported issues.";
        let result = detect(text);
        // "reported" should not match "report" keyword on word boundary,
        // and "sharepoint" obviously doesn't contain the word "report".
        // With only one incidental signal this should not pass the floor.
        assert!(
            result.type_.is_none() || result.confidence < 0.5,
            "false positive: {:?}",
            result
        );
    }

    // ── DocType parsing (hints) ──────────────────────────────────────────

    #[test]
    fn hint_str_accepts_snake_and_space() {
        assert_eq!(
            DocType::from_hint_str("bank_statement"),
            Some(DocType::BankStatement)
        );
        assert_eq!(
            DocType::from_hint_str("Bank Statement"),
            Some(DocType::BankStatement)
        );
        assert_eq!(
            DocType::from_hint_str("  PURCHASE_ORDER  "),
            Some(DocType::PurchaseOrder)
        );
    }

    #[test]
    fn hint_str_accepts_common_synonyms() {
        assert_eq!(DocType::from_hint_str("cv"), Some(DocType::Resume));
        assert_eq!(DocType::from_hint_str("po"), Some(DocType::PurchaseOrder));
        assert_eq!(DocType::from_hint_str("estimate"), Some(DocType::Quote));
        assert_eq!(DocType::from_hint_str("paper"), Some(DocType::AcademicPaper));
    }

    #[test]
    fn hint_str_rejects_unknown() {
        assert_eq!(DocType::from_hint_str("nonsense"), None);
        assert_eq!(DocType::from_hint_str(""), None);
    }

    // ── Detection for each type ──────────────────────────────────────────

    #[test]
    fn detects_invoice() {
        let text = "Invoice #1234\nBill To: Acme Corp\nAmount Due: $500.00\nPayment Terms: Net 30";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::Invoice));
        assert!(r.confidence > 0.3);
    }

    #[test]
    fn detects_contract() {
        let text = "This Agreement is entered into on the effective date by the parties.\nWhereas the parties hereby agree to the terms and conditions.";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::Contract));
    }

    #[test]
    fn detects_resume() {
        let text = "Career Objective\nProfessional Experience\nEducation\nTechnical Skills\nReferences available upon request";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::Resume));
    }

    #[test]
    fn detects_bank_statement() {
        let text = "Statement Period: Jan 1 - Jan 31\nAccount Number: ****1234\nOpening Balance: $1,000\nClosing Balance: $1,250\nDeposit $500\nWithdrawal $250";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::BankStatement));
    }

    #[test]
    fn detects_letter() {
        let text = "Dear Sir,\n\nThank you for your consideration.\n\nYours sincerely,\nJane";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::Letter));
    }

    #[test]
    fn detects_report_quarterly() {
        let text = "Quarterly Report\n\nExecutive Summary\n\nKey findings and recommendations follow. Our analysis of the quarterly metrics shows growth.";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::Report));
    }

    #[test]
    fn detects_purchase_order() {
        let text = "Purchase Order\nPO Number: 98765\nShip To: Warehouse 3\nQuantity: 100 units\nVendor: Acme Supplies";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::PurchaseOrder));
    }

    #[test]
    fn detects_quote() {
        let text = "Price Quote\nQuote Number: Q-001\nValid until March 31\nEstimated cost: $1,200.\nThis quote expires in 30 days.";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::Quote));
    }

    #[test]
    fn detects_academic_paper() {
        let text = "Abstract\n\nWe propose a novel method. Prior work has shown... \n\nIntroduction\n\nRelated work and methodology follow. Our experimental results and dataset demonstrate...";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::AcademicPaper));
    }

    #[test]
    fn detects_article() {
        let text = "News Bulletin\n\nBy the author Jane. Published in the daily column. This article covers breaking news in the industry.";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::Article));
    }

    #[test]
    fn detects_form() {
        let text = "Employee Onboarding Form\n\nEmployee Name: ________\nDepartment: ________\nSignature: ________\nPlease complete and submit.";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::Form));
    }

    // ── Ambiguity & null cases ───────────────────────────────────────────

    #[test]
    fn ambiguous_text_returns_none() {
        let text = "The quick brown fox jumps over the lazy dog. This sentence is neutral.";
        let r = detect(text);
        assert_eq!(r.type_, None);
    }

    #[test]
    fn empty_text_returns_none() {
        let r = detect("");
        assert_eq!(r.type_, None);
        assert_eq!(r.confidence, 0.0);
        assert!(r.alternates.is_empty());
    }

    #[test]
    fn confidence_is_present_when_detected() {
        let text = "Invoice #1234\nBill To: Acme Corp\nAmount Due: $500.00\nPayment Terms: Net 30";
        let r = detect(text);
        assert!(r.type_.is_some());
        assert!(r.confidence > 0.0);
        assert!(r.confidence <= 1.0);
    }

    #[test]
    fn alternates_do_not_include_winner() {
        let text = "Invoice #1234\nBill To: Acme Corp\nAmount Due: $500.00\nPayment Terms: Net 30";
        let r = detect(text);
        for alt in &r.alternates {
            assert_ne!(Some(alt.type_), r.type_);
        }
    }

    // ── Negative keyword sanity ──────────────────────────────────────────

    #[test]
    fn negative_keyword_rules_out_wrong_type() {
        // Letter signals ("Dear", "sincerely", "Best regards") dominate even
        // though "agreement" and "counterparties" are present. The negative
        // keywords on Letter do not fire because none of the strong anti-
        // signals ("whereas", "amount due", etc.) appear.
        let text = "Dear Sir,\n\nWe reached an agreement with our counterparties yesterday. Best regards, Jane. Sincerely.";
        let r = detect(text);
        assert_eq!(r.type_, Some(DocType::Letter));
    }
}
