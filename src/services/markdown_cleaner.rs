//! Converts structured PDF/OCR output into well-formatted Markdown.
//!
//! Two pipelines produce raw markdown with HTML artifacts:
//! 1. Native PDFs: `pdftohtml -xml` → XML with font sizes → raw markdown
//! 2. Scanned PDFs: `tesseract hocr` → HTML with layout → raw markdown via htmd
//!
//! Both feed through `postprocess_markdown()` which applies the same intelligence
//! a human editor would: converting HTML tags, merging fragments, fixing headings,
//! detecting lists, and cleaning noise.
//!
//! All processing is local. No customer data leaves the server.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use crate::errors::AppError;

// ═══════════════════════════════════════════════════════════════════════════
//  PUBLIC API
// ═══════════════════════════════════════════════════════════════════════════

/// Convert native PDF bytes to clean Markdown.
pub fn pdf_to_markdown(pdf_bytes: &[u8], pdftoppm_path: &str) -> Result<String, AppError> {
    let pdftohtml_path = derive_pdftohtml_path(pdftoppm_path);

    let tmp_dir = tempfile::TempDir::new()
        .map_err(|e| AppError::Internal(format!("Failed to create temp dir: {e}")))?;
    let pdf_path = tmp_dir.path().join("input.pdf");
    std::fs::write(&pdf_path, pdf_bytes)
        .map_err(|e| AppError::Internal(format!("Failed to write temp PDF: {e}")))?;

    let xml_prefix = tmp_dir.path().join("output");
    let output = Command::new(&pdftohtml_path)
        .arg("-xml")
        .arg("-i")
        .arg("-noframes")
        .arg(&pdf_path)
        .arg(&xml_prefix)
        .output()
        .map_err(|e| AppError::PdfProcessing(format!("Failed to run pdftohtml: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::PdfProcessing(format!("pdftohtml failed: {stderr}")));
    }

    let xml_path = tmp_dir.path().join("output.xml");
    let xml_content = std::fs::read_to_string(&xml_path)
        .map_err(|e| AppError::PdfProcessing(format!("Failed to read pdftohtml XML: {e}")))?;

    let raw_md = xml_to_markdown(&xml_content);
    Ok(postprocess_markdown(&raw_md))
}

/// Convert tesseract hOCR pages to clean Markdown.
pub fn hocr_pages_to_markdown(
    image_dir: &Path,
    tesseract_path: &str,
) -> Result<String, AppError> {
    let mut image_paths: Vec<_> = std::fs::read_dir(image_dir)
        .map_err(|e| AppError::Internal(format!("Failed to read image dir: {e}")))?
        .filter_map(|entry| entry.ok())
        .filter(|e| e.path().extension().map(|ext| ext == "png").unwrap_or(false))
        .map(|e| e.path())
        .collect();
    image_paths.sort();

    if image_paths.is_empty() {
        return Err(AppError::PdfProcessing("No page images found for OCR".into()));
    }

    let mut full_md = String::new();
    let multi_page = image_paths.len() > 1;

    for (i, img_path) in image_paths.iter().enumerate() {
        if multi_page {
            if i > 0 {
                full_md.push('\n');
            }
            full_md.push_str(&format!("## Page {}\n\n", i + 1));
        }
        let hocr = run_tesseract_hocr(img_path, tesseract_path)?;
        let page_md = hocr_to_markdown(&hocr);
        full_md.push_str(&page_md);
        full_md.push('\n');
    }

    Ok(postprocess_markdown(&full_md))
}

/// Backward-compatible entry point for basic text cleanup.
pub fn clean_for_markdown(text: &str) -> String {
    postprocess_markdown(text)
}

// ═══════════════════════════════════════════════════════════════════════════
//  POST-PROCESSOR — the core intelligence
// ═══════════════════════════════════════════════════════════════════════════

/// Post-process raw markdown into clean, human-readable markdown.
///
/// This applies the same transformations a human editor would:
/// 1. Convert HTML tags to markdown syntax (<b> → **, <i> → *, <a> → [](url))
/// 2. Merge fragmented bold spans (syllable-split text from rotated PDF elements)
/// 3. Fix over-promoted headings (demote ### that should be body/list text)
/// 4. Detect and format bullet/list patterns
/// 5. Strip page footers and OCR noise
/// 6. Collapse excessive blank lines
fn postprocess_markdown(raw: &str) -> String {
    let mut text = raw.to_string();

    // Phase 1: Convert HTML tags to markdown
    text = convert_html_to_markdown(&text);

    // Phase 2: Merge fragmented bold spans across lines
    text = merge_bold_fragments(&text);

    // Phase 3: Fix heading levels — demote over-promoted lines
    text = fix_headings(&text);

    // Phase 4: Detect bullet/list patterns
    text = detect_lists(&text);

    // Phase 5: Reflow hard line breaks into paragraphs
    text = reflow_paragraphs(&text);

    // Phase 6: Strip page footers and noise
    text = strip_noise(&text);

    // Phase 7: Clean up inline heading markers that got reflowed into lines
    text = text.replace("  **### **", " ").replace("**### **", " ");
    // Fix orphaned bold markers: "word** " at line start where ** doesn't have a matching opener
    // These come from pdftohtml splitting bold text across elements
    for _ in 0..3 {
        // Repeated to catch nested cases
        text = text.replace("\n** ", "\n**").replace("The** ", "The **");
    }

    // Phase 8: Collapse excessive blank lines and trim
    text = collapse_blanks(&text);

    text
}

/// Convert HTML tags to markdown syntax.
fn convert_html_to_markdown(text: &str) -> String {
    let mut result = text.to_string();

    // <b>text</b> → **text**
    // Handle multiline: <b> on one line, </b> on another
    result = regex_replace(&result, r"<b>(.*?)</b>", "**$1**");
    // Also handle <strong>
    result = regex_replace(&result, r"<strong>(.*?)</strong>", "**$1**");

    // <i>text</i> → *text*
    result = regex_replace(&result, r"<i>(.*?)</i>", "*$1*");
    result = regex_replace(&result, r"<em>(.*?)</em>", "*$1*");

    // <a href="url">text</a> → [text](url)
    result = regex_replace(&result, r#"<a href="([^"]*)"[^>]*>(.*?)</a>"#, "[$2]($1)");

    // Strip any remaining HTML tags
    result = regex_replace(&result, r"<[^>]+>", "");

    // Fix doubled markdown from conversion: ****text**** → **text**
    result = regex_replace(&result, r"\*{4,}", "**");

    // Fix trailing space before closing **: "word **" → "word**"
    // Only match spaces before ** that are followed by end-of-word context
    result = regex_replace(&result, r"(\w)\s+\*\*(\s|$|\n)", "$1**$2");
    // Fix leading space after opening **: "**  word" → "**word"
    // Only match ** at start of bold span (preceded by space/start) followed by spaces
    result = regex_replace(&result, r"(^|\s)\*\*\s+", "$1**");

    result
}

/// Merge fragmented bold spans that were split across lines.
///
/// Detects patterns like:
///   **no**\n**n-**\n**st**\n**ar**\n**ch**\n**y**
/// and merges them into: **non-starchy**
fn merge_bold_fragments(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut bold_buffer = String::new();
    let mut in_fragment_run = false;

    for line in &lines {
        let trimmed = line.trim();

        // Check if this line is a standalone bold fragment (short bold text, < 5 chars)
        if let Some(inner) = extract_bold_content(trimmed) {
            if inner.len() <= 5 || (in_fragment_run && !bold_buffer.is_empty()) {
                // Syllable fragment — concatenate directly
                // Hyphen at end means word continues: "non-" + "starchy" → "non-starchy"
                // No hyphen between short fragments means same word: "ve" + "ge" → "vege"
                if bold_buffer.is_empty() {
                    bold_buffer.push_str(&inner);
                } else if bold_buffer.ends_with('-') || inner.len() <= 3 {
                    // Short syllable or hyphen continuation → concatenate directly
                    bold_buffer.push_str(&inner);
                } else if bold_buffer.ends_with(' ') || inner.starts_with(' ') {
                    bold_buffer.push_str(&inner);
                } else {
                    // Longer word (>3 chars) after fragments → space-separate
                    bold_buffer.push(' ');
                    bold_buffer.push_str(&inner);
                }
                in_fragment_run = true;
                continue;
            }
        }

        // If we were accumulating fragments, flush them
        if in_fragment_run {
            if !bold_buffer.is_empty() {
                result.push(format!("**{}**", bold_buffer));
                bold_buffer.clear();
            }
            in_fragment_run = false;
        }

        result.push(line.to_string());
    }

    // Flush any remaining fragments
    if !bold_buffer.is_empty() {
        result.push(format!("**{}**", bold_buffer));
    }

    result.join("\n")
}

/// Extract the inner text from a line that is just **text** or **text**.
fn extract_bold_content(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("**") && trimmed.ends_with("**") && trimmed.len() > 4 {
        let inner = &trimmed[2..trimmed.len() - 2];
        // Must not contain more ** inside (that would be nested)
        if !inner.contains("**") {
            return Some(inner.to_string());
        }
    }
    None
}

/// Fix over-promoted headings.
///
/// Lines marked as ### that are actually body text, list items,
/// or short descriptive phrases should be demoted.
fn fix_headings(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result: Vec<String> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // Skip ## Page N headers — always keep those
        if trimmed.starts_with("## Page ") {
            result.push(line.to_string());
            continue;
        }

        // Check if this is a # heading (h1) that should be demoted
        // Single words or very short text as h1 in a run → not real headings
        if let Some(content) = trimmed.strip_prefix("# ") {
            if !content.starts_with('#') {
                let clean = content.replace("**", "").replace('*', "").trim().to_string();
                // Short h1 (< 30 chars) in a run of other h1s → demote
                if clean.len() < 30 {
                    let mut consecutive_h1 = 0;
                    for j in (0..i).rev() {
                        let l = lines[j].trim();
                        if l.starts_with("# ") && !l.starts_with("## ") {
                            consecutive_h1 += 1;
                        } else if l.is_empty() {
                            continue;
                        } else {
                            break;
                        }
                    }
                    for l in lines.iter().skip(i + 1).map(|x| x.trim()) {
                        if l.starts_with("# ") && !l.starts_with("## ") {
                            consecutive_h1 += 1;
                        } else if l.is_empty() {
                            continue;
                        } else {
                            break;
                        }
                    }
                    if consecutive_h1 >= 1 {
                        result.push(content.to_string());
                        continue;
                    }
                }
            }
        }

        // Check if this is a ### heading that should be demoted
        if let Some(content) = trimmed.strip_prefix("### ") {
            if should_demote_heading(content, &lines, i) {
                result.push(content.to_string());
                continue;
            }
        }

        // Check if this is a ## heading that is really content
        if let Some(content) = trimmed.strip_prefix("## ") {
            if !trimmed.starts_with("## Page ") {
                let clean = content.replace("**", "").replace('*', "").trim().to_string();
                // Very short content (fractions like ½) — demote
                if clean.len() <= 2 {
                    result.push(content.to_string());
                    continue;
                }
                if should_demote_h2(content) {
                    // Keep as ## only if it looks like a real section title
                    if content.len() > 60 || content.ends_with('.') || content.ends_with(',') {
                        result.push(content.to_string());
                        continue;
                    }
                }
            }
        }

        result.push(line.to_string());
    }

    result.join("\n")
}

/// Determine if a ### heading should be demoted to body text.
fn should_demote_heading(content: &str, lines: &[&str], index: usize) -> bool {
    let clean = content.replace("**", "").replace('*', "").trim().to_string();

    // Very short content (1-2 chars like fractions ½, ¼) — not a heading
    if clean.len() <= 2 {
        return true;
    }

    // Bullet point content — always demote
    if clean.starts_with("• ") || clean.starts_with("- ") || clean.starts_with("* ") {
        return true;
    }

    // Starts with lowercase — not a heading
    if clean.starts_with(|c: char| c.is_lowercase()) {
        return true;
    }

    // Long text (> 80 chars) — body paragraph, not a heading
    if clean.len() > 80 {
        return true;
    }

    // Ends with period or comma — sentence, not a heading
    if clean.ends_with('.') || clean.ends_with(',') || clean.ends_with(';') {
        return true;
    }

    // Part of a consecutive run of ### lines — likely a list, not headings
    let mut consecutive = 0;
    for j in (0..index).rev() {
        if lines[j].trim().starts_with("### ") {
            consecutive += 1;
        } else if lines[j].trim().is_empty() {
            continue;
        } else {
            break;
        }
    }
    for l in lines.iter().skip(index + 1) {
        let trimmed = l.trim();
        if trimmed.starts_with("### ") {
            consecutive += 1;
        } else if trimmed.is_empty() {
            continue;
        } else {
            break;
        }
    }
    // 3+ consecutive ### lines → they're list items, not headings
    if consecutive >= 2 {
        return true;
    }

    false
}

/// Determine if a ## heading should be demoted.
fn should_demote_h2(content: &str) -> bool {
    let clean = content.replace("**", "").replace('*', "");
    clean.len() > 80 || clean.ends_with('.') || clean.ends_with(',')
}

/// Detect bullet/list patterns and format as markdown lists.
fn detect_lists(text: &str) -> String {
    let mut result = String::new();

    for line in text.lines() {
        let trimmed = line.trim();

        // • bullet → - bullet
        if let Some(rest) = trimmed.strip_prefix("• ").or_else(|| trimmed.strip_prefix("· ")) {
            result.push_str("- ");
            result.push_str(rest.trim_start());
            result.push('\n');
            continue;
        }

        // Numbered list: "1. " or "1) "
        // Already valid markdown, keep as-is

        result.push_str(line);
        result.push('\n');
    }

    result
}

/// Reflow hard line breaks from PDF column wrapping into proper paragraphs.
///
/// PDFs break lines at fixed column widths (60-80 chars). This joins
/// consecutive lines that are part of the same sentence/paragraph.
fn reflow_paragraphs(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result: Vec<String> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Blank line: keep it, but check if the NEXT non-blank line should join
        // to the line BEFORE this blank (skip the blank as a soft break)
        if trimmed.is_empty() {
            // Look ahead: is the next non-blank line a continuation?
            if let Some(next_idx) = find_next_nonblank(&lines, i + 1) {
                let next_trimmed = lines[next_idx].trim();
                if let Some(prev) = result.last() {
                    let prev_trimmed = prev.trim().to_string();
                    if should_join_lines(&prev_trimmed, next_trimmed) {
                        // Skip the blank line(s) — the next line will join to prev
                        i += 1;
                        continue;
                    }
                }
            }
            result.push(String::new());
            i += 1;
            continue;
        }

        if trimmed.starts_with('#') || trimmed.starts_with("---") {
            result.push(lines[i].to_string());
            i += 1;
            continue;
        }

        // Check if this line should be joined to the previous one
        if let Some(prev) = result.last_mut() {
            let prev_trimmed = prev.trim().to_string();
            if should_join_lines(&prev_trimmed, trimmed) {
                if !prev.ends_with(' ') {
                    prev.push(' ');
                }
                prev.push_str(trimmed);
                i += 1;
                continue;
            }
        }

        result.push(lines[i].to_string());
        i += 1;
    }

    result.join("\n")
}

fn find_next_nonblank(lines: &[&str], start: usize) -> Option<usize> {
    (start..lines.len()).find(|&i| !lines[i].trim().is_empty())
}

/// Determine if `current` should be joined to `prev` as a continuation.
///
/// Only joins when prev looks like a PDF column-wrapped line cut mid-sentence.
/// Short lines, labeled fields, and bold markers are NOT continuation candidates.
fn should_join_lines(prev: &str, current: &str) -> bool {
    // Don't join to empty/heading/rule lines
    if prev.is_empty() || prev.starts_with('#') || prev.starts_with("---") {
        return false;
    }

    // Don't join if current line is a new structural element
    if current.starts_with('#')
        || current.starts_with("- ")
        || current.starts_with("* ")
        || current.starts_with("---")
        || current.starts_with("| ")
    {
        return false;
    }

    // Don't join if current starts with bold marker (label/heading)
    if current.starts_with("**") {
        return false;
    }

    // Don't join if current looks like a labeled field ("Label:" or "Label Name:")
    if looks_like_label(current) {
        return false;
    }

    // Check if previous line ends a sentence
    let prev_ends_sentence = prev.ends_with('.')
        || prev.ends_with('!')
        || prev.ends_with('?')
        || prev.ends_with(':')
        || prev.ends_with(';');

    let first_char = current.chars().next().unwrap_or(' ');

    // JOIN if: current line starts with lowercase (almost always a continuation)
    if first_char.is_lowercase() {
        return true;
    }

    // Don't join if prev ends a sentence
    if prev_ends_sentence {
        return false;
    }

    // Only reflow when prev is long enough to look like a column-wrapped line.
    // Short lines (< 45 chars) are usually form fields, labels, or addresses,
    // not paragraphs cut by PDF column width.
    let prev_stripped = prev.trim_start_matches("- ");
    if prev_stripped.len() < 45 {
        return false;
    }

    // JOIN: prev is long, doesn't end a sentence → PDF column wrap continuation
    true
}

/// Check if a line looks like a labeled field (e.g., "Employee Name:", "Amount Due: $500")
fn looks_like_label(line: &str) -> bool {
    // Must contain a colon in the first 40 chars
    if let Some(pos) = line.find(':') {
        if pos < 40 {
            // The part before the colon should be mostly words (a label)
            let label_part = &line[..pos];
            let word_count = label_part.split_whitespace().count();
            // Labels are typically 1-4 words before the colon
            return (1..=5).contains(&word_count);
        }
    }
    false
}

/// Strip page footers, OCR noise, and artifacts.
fn strip_noise(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();

        // Empty lines → keep for paragraph breaks
        if trimmed.is_empty() {
            lines.push(String::new());
            continue;
        }

        // Page footer: "Company Page N of M" pattern
        if is_page_footer(trimmed) {
            continue;
        }

        // Single non-alphanumeric char noise (|, ', \)
        if trimmed.len() == 1 && !trimmed.chars().next().unwrap().is_alphanumeric() {
            continue;
        }

        // Backslash escape noise (\\5, \|)
        if trimmed.len() <= 3 && trimmed.contains('\\') {
            continue;
        }

        lines.push(line.to_string());
    }

    lines.join("\n")
}

fn is_page_footer(line: &str) -> bool {
    let lower = line.to_lowercase();
    if !lower.contains("page") || !lower.contains(" of ") {
        return false;
    }
    if let Some(pos) = lower.find("page") {
        let rest = &lower[pos + 4..];
        return rest.trim_start().starts_with(|c: char| c.is_ascii_digit());
    }
    false
}

/// Collapse runs of 3+ blank lines into max 2, trim trailing whitespace.
fn collapse_blanks(text: &str) -> String {
    let mut result = String::new();
    let mut blank_count = 0;

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(trimmed);
            result.push('\n');
        }
    }

    let trimmed = result.trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  XML → RAW MARKDOWN (pdftohtml pipeline)
// ═══════════════════════════════════════════════════════════════════════════

fn xml_to_markdown(xml: &str) -> String {
    let fonts = parse_font_specs(xml);
    let body_size = detect_body_font_size(&fonts);
    let pages = parse_xml_pages(xml);

    let mut md = String::new();
    let multi_page = pages.len() > 1;

    for (page_idx, page) in pages.iter().enumerate() {
        if multi_page {
            if page_idx > 0 {
                md.push('\n');
            }
            md.push_str(&format!("## Page {}\n\n", page_idx + 1));
        }

        let mut prev_top: i32 = -100;
        let mut current_line = String::new();

        for elem in page {
            let font = fonts.get(&elem.font_id);
            let font_size = font.map(|f| f.size).unwrap_or(body_size);
            let is_bold = font.map(|f| f.is_bold).unwrap_or(false);

            let vert_gap = elem.top - prev_top;
            let is_new_line = vert_gap.abs() > 5;
            let is_new_paragraph = vert_gap > 25;

            if is_new_line && !current_line.is_empty() {
                let line = current_line.trim().to_string();
                if !line.is_empty() {
                    md.push_str(&line);
                    md.push('\n');
                }
                current_line = String::new();
                if is_new_paragraph {
                    md.push('\n');
                }
            }

            let text = elem.text.trim();
            if text.is_empty() {
                prev_top = elem.top;
                continue;
            }

            // Heading detection: only for significantly larger fonts and short text
            let heading_level = if font_size > body_size + 14 && text.len() < 60 {
                Some(1)
            } else if font_size > body_size + 6 && text.len() < 60 {
                Some(2)
            } else if font_size > body_size + 2 && text.len() < 50 {
                Some(3)
            } else {
                None
            };

            if let Some(level) = heading_level {
                if !current_line.is_empty() {
                    md.push_str(current_line.trim());
                    md.push('\n');
                    current_line = String::new();
                }
                if !md.ends_with('\n') {
                    md.push('\n');
                }
                let hashes = "#".repeat(level);
                md.push_str(&format!("{hashes} {text}\n\n"));
                prev_top = elem.top;
                continue;
            }

            if is_bold {
                current_line.push_str(&format!("**{text}** "));
            } else {
                current_line.push_str(text);
                current_line.push(' ');
            }

            prev_top = elem.top;
        }

        let line = current_line.trim().to_string();
        if !line.is_empty() {
            md.push_str(&line);
            md.push('\n');
        }
    }

    md
}

#[derive(Debug)]
struct FontSpec {
    size: i32,
    #[allow(dead_code)]
    family: String,
    is_bold: bool,
}

#[derive(Debug)]
struct TextElement {
    top: i32,
    #[allow(dead_code)]
    left: i32,
    font_id: String,
    text: String,
}

fn parse_font_specs(xml: &str) -> HashMap<String, FontSpec> {
    let mut fonts = HashMap::new();
    for line in xml.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("<fontspec") {
            continue;
        }
        let id = extract_attr(trimmed, "id").unwrap_or_default();
        let size: i32 = extract_attr(trimmed, "size")
            .and_then(|s| s.parse().ok())
            .unwrap_or(12);
        let family = extract_attr(trimmed, "family").unwrap_or_default();
        let is_bold = family.contains("Bold")
            || family.contains("bold")
            || family.contains("Heavy")
            || family.contains("Black");
        fonts.insert(id, FontSpec { size, family, is_bold });
    }
    fonts
}

fn detect_body_font_size(fonts: &HashMap<String, FontSpec>) -> i32 {
    let mut size_counts: HashMap<i32, usize> = HashMap::new();
    for font in fonts.values() {
        *size_counts.entry(font.size).or_insert(0) += 1;
    }
    size_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(size, _)| size)
        .unwrap_or(12)
}

fn parse_xml_pages(xml: &str) -> Vec<Vec<TextElement>> {
    let mut pages: Vec<Vec<TextElement>> = Vec::new();
    let mut current_page: Vec<TextElement> = Vec::new();
    let mut in_page = false;

    for line in xml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<page ") {
            if in_page && !current_page.is_empty() {
                pages.push(std::mem::take(&mut current_page));
            }
            in_page = true;
        } else if trimmed == "</page>" {
            if !current_page.is_empty() {
                pages.push(std::mem::take(&mut current_page));
            }
            in_page = false;
        } else if in_page && trimmed.starts_with("<text ") {
            if let Some(elem) = parse_text_element(trimmed) {
                current_page.push(elem);
            }
        }
    }
    if !current_page.is_empty() {
        pages.push(current_page);
    }
    pages
}

fn parse_text_element(line: &str) -> Option<TextElement> {
    let top: i32 = extract_attr(line, "top")?.parse().ok()?;
    let left: i32 = extract_attr(line, "left")?.parse().ok()?;
    let font_id = extract_attr(line, "font")?;
    let start = line.find('>')? + 1;
    let end = line.rfind("</text>")?;
    if start >= end {
        return None;
    }
    let text = html_decode(&line[start..end]);
    if text.trim().is_empty() {
        return None;
    }
    Some(TextElement { top, left, font_id, text })
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let search = format!("{name}=\"");
    let start = tag.find(&search)? + search.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn html_decode(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

// ═══════════════════════════════════════════════════════════════════════════
//  hOCR → RAW MARKDOWN (tesseract pipeline)
// ═══════════════════════════════════════════════════════════════════════════

fn hocr_to_markdown(hocr: &str) -> String {
    let cleaned_html = preprocess_hocr(hocr);
    match htmd::convert(&cleaned_html) {
        Ok(md) => md,
        Err(_) => strip_html_tags(hocr),
    }
}

fn preprocess_hocr(hocr: &str) -> String {
    let mut result = String::new();
    let mut skip_depth = 0;

    for line in hocr.lines() {
        let trimmed = line.trim();

        let is_skip_block = trimmed.contains("ocr_photo")
            || trimmed.contains("ocr_separator")
            || trimmed.contains("ocr_caption");
        if is_skip_block {
            let has_open = trimmed.contains("<div") || trimmed.contains("<span");
            let has_close = trimmed.contains("</div>") || trimmed.contains("</span>");
            if has_open && !has_close {
                skip_depth += 1;
            }
            continue;
        }
        if skip_depth > 0 {
            if trimmed.contains("</span>") || trimmed.contains("</div>") {
                skip_depth -= 1;
            }
            continue;
        }

        // Filter low-confidence words and pipe noise
        if trimmed.contains("ocrx_word") {
            let word_text = extract_span_text(trimmed).unwrap_or_default();
            let conf = extract_wconf(trimmed).unwrap_or(96);
            if conf < 75 && word_text.len() <= 2 {
                continue;
            }
            if word_text == "|" || word_text == "\\|" {
                continue;
            }
        }

        let processed = trimmed.to_string();
        // Strip hOCR class/title attributes
        let processed = processed
            .replace("<p class='ocr_par'", "<p")
            .replace("<span class='ocr_line'", "<span")
            .replace("<span class='ocrx_word'", "<span");

        result.push_str(&processed);
        result.push('\n');
    }

    // Remove title attributes (bbox data)
    let re_title = regex_lite::Regex::new(r#" title='[^']*'"#).unwrap();
    re_title.replace_all(&result, "").to_string()
}

fn extract_wconf(line: &str) -> Option<u32> {
    let pos = line.find("x_wconf ")?;
    let rest = &line[pos + 8..];
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

fn extract_span_text(line: &str) -> Option<String> {
    let end = line.rfind("</span>")?;
    let before = &line[..end];
    let start = before.rfind('>')? + 1;
    let text = line[start..end].trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn strip_html_tags(html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
}

fn run_tesseract_hocr(image_path: &Path, tesseract_path: &str) -> Result<String, AppError> {
    let output = Command::new(tesseract_path)
        .arg(image_path)
        .arg("stdout")
        .arg("-l")
        .arg("eng")
        .arg("hocr")
        .output()
        .map_err(|e| {
            AppError::PdfProcessing(format!("Failed to run tesseract hocr: {e}"))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(page = %image_path.display(), "Tesseract hocr failed: {stderr}");
        return Ok(String::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn derive_pdftohtml_path(pdftoppm_path: &str) -> String {
    if let Some(dir) = Path::new(pdftoppm_path).parent() {
        let candidate = dir.join("pdftohtml");
        if candidate.exists() {
            return candidate.to_string_lossy().to_string();
        }
    }
    "pdftohtml".to_string()
}

// ═══════════════════════════════════════════════════════════════════════════
//  REGEX HELPER
// ═══════════════════════════════════════════════════════════════════════════

fn regex_replace(text: &str, pattern: &str, replacement: &str) -> String {
    let re = regex_lite::Regex::new(pattern).unwrap();
    re.replace_all(text, replacement).to_string()
}

// ═══════════════════════════════════════════════════════════════════════════
//  TESTS
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── HTML conversion ─────────────────────────────────────────────────

    #[test]
    fn converts_bold_html() {
        assert_eq!(convert_html_to_markdown("<b>hello</b>"), "**hello**");
    }

    #[test]
    fn converts_italic_html() {
        assert_eq!(convert_html_to_markdown("<i>hello</i>"), "*hello*");
    }

    #[test]
    fn converts_link_html() {
        let input = r#"<a href="https://example.com">click here</a>"#;
        let result = convert_html_to_markdown(input);
        assert_eq!(result, "[click here](https://example.com)");
    }

    #[test]
    fn strips_remaining_html() {
        assert_eq!(convert_html_to_markdown("<div>text</div>"), "text");
    }

    #[test]
    fn mixed_html() {
        let input = "<b>Bold</b> and <i>italic</i> text";
        assert_eq!(
            convert_html_to_markdown(input),
            "**Bold** and *italic* text"
        );
    }

    // ── Bold fragment merging ───────────────────────────────────────────

    #[test]
    fn merges_short_bold_fragments() {
        let input = "**no**\n**n-**\n**st**\n**ar**\n**ch**\n**y**";
        let result = merge_bold_fragments(input);
        assert!(
            result.contains("**non-starchy**"),
            "should merge fragments, got: {result}"
        );
    }

    #[test]
    fn merges_bold_words_with_spaces() {
        // Short syllable fragments concatenate directly;
        // longer bold words (>4 chars) get space-joined to the buffer
        let input = "**non-**\n**st**\n**ar**\n**chy**\n**vegetables**";
        let result = merge_bold_fragments(input);
        assert!(
            result.contains("**non-starchy vegetables**"),
            "should space-join word-level bold fragments, got: {result}"
        );
    }

    #[test]
    fn keeps_long_bold_lines() {
        let input = "**This is a full bold line**\n**Another full bold line**";
        let result = merge_bold_fragments(input);
        assert!(result.contains("**This is a full bold line**"));
        assert!(result.contains("**Another full bold line**"));
    }

    #[test]
    fn extract_bold_content_works() {
        assert_eq!(extract_bold_content("**hi**"), Some("hi".into()));
        assert_eq!(extract_bold_content("normal text"), None);
        assert_eq!(extract_bold_content("**"), None);
    }

    // ── Heading demotion ────────────────────────────────────────────────

    #[test]
    fn demotes_bullet_headings() {
        let input = "### • Decrease risk of diabetes.";
        let result = fix_headings(input);
        assert!(
            !result.contains("###"),
            "bullet should be demoted, got: {result}"
        );
        assert!(result.contains("• Decrease risk"));
    }

    #[test]
    fn demotes_lowercase_headings() {
        let input = "### and stroke.";
        let result = fix_headings(input);
        assert!(!result.contains("###"), "lowercase should be demoted");
    }

    #[test]
    fn demotes_long_headings() {
        let input =
            "### This is a very long line that should not be a heading because it is clearly a paragraph of body text and not a section title.";
        let result = fix_headings(input);
        assert!(!result.contains("###"), "long line should be demoted");
    }

    #[test]
    fn keeps_real_headings() {
        let input = "## GRAINS & STARCHES\n\nSome body text here.";
        let result = fix_headings(input);
        assert!(result.contains("## GRAINS & STARCHES"));
    }

    #[test]
    fn keeps_page_headers() {
        let input = "## Page 1\n\nContent";
        let result = fix_headings(input);
        assert!(result.contains("## Page 1"));
    }

    #[test]
    fn demotes_consecutive_h1_short_lines() {
        let input = "Some text\n\n# Alpha\n\n# Beta\n\n# Gamma da\n\n# Delta";
        let result = fix_headings(input);
        assert!(
            !result.contains("# Alpha"),
            "consecutive short h1 should be demoted, got: {result}"
        );
        assert!(result.contains("Alpha"));
        assert!(result.contains("Delta"));
    }

    #[test]
    fn keeps_single_h1_heading() {
        let input = "# Main Title\n\nBody text here.";
        let result = fix_headings(input);
        assert!(result.contains("# Main Title"));
    }

    #[test]
    fn demotes_consecutive_h3_lines() {
        let input = "### Apple\n\n### Banana\n\n### Cherry\n\n### Date";
        let result = fix_headings(input);
        // 4 consecutive ### lines → all demoted
        assert!(
            !result.contains("### Apple"),
            "consecutive items should be demoted, got: {result}"
        );
    }

    // ── List detection ──────────────────────────────────────────────────

    #[test]
    fn converts_bullet_to_dash() {
        let input = "• First item\n• Second item";
        let result = detect_lists(input);
        assert!(result.contains("- First item"));
        assert!(result.contains("- Second item"));
    }

    // ── Page footer stripping ───────────────────────────────────────────

    #[test]
    fn strips_page_footer() {
        assert!(is_page_footer(
            "Example Corp Notice and Consent | Page 2 of 4"
        ));
        assert!(is_page_footer("Page 1 of 4"));
    }

    #[test]
    fn keeps_normal_text() {
        assert!(!is_page_footer("The page was full of text"));
    }

    // ── Label detection ───────────────────────────────────────────────

    #[test]
    fn detects_labeled_fields() {
        assert!(looks_like_label("Employee Name: Alice"));
        assert!(looks_like_label("Amount Due: $500.00"));
        assert!(looks_like_label("Bill To: Acme Corp"));
        assert!(looks_like_label("Location: Canada"));
    }

    #[test]
    fn does_not_detect_label_in_prose() {
        // Colon deep in a long sentence is not a label
        assert!(!looks_like_label(
            "This is a very long sentence that happens to have a colon somewhere"
        ));
        // No colon at all
        assert!(!looks_like_label("Normal text without colon"));
    }

    #[test]
    fn does_not_join_short_lines_without_punctuation() {
        // Short lines like form fields should stay separate
        assert!(!should_join_lines("Bill To: Acme Corp", "Amount Due: $500.00"));
        assert!(!should_join_lines("Employee Name: Alice", "Department: Sales"));
    }

    #[test]
    fn does_not_join_bold_labels() {
        assert!(!should_join_lines("Some text", "**Label** value"));
    }

    #[test]
    fn joins_long_line_continuations() {
        // Long line (>45 chars) without sentence-ending punctuation → join
        let long_prev = "This is a long enough previous line that was clearly cut by PDF";
        assert!(should_join_lines(long_prev, "Column width and should continue here"));
    }

    // ── Noise stripping ─────────────────────────────────────────────────

    #[test]
    fn strips_single_char_noise() {
        let input = "Hello\n\n|\n\nWorld";
        let result = strip_noise(input);
        assert!(!result.contains("\n|\n"));
    }

    #[test]
    fn strips_backslash_noise() {
        let input = "Hello\n\\5\nWorld";
        let result = strip_noise(input);
        assert!(!result.contains("\\5"), "got: {result}");
    }

    // ── Blank collapsing ────────────────────────────────────────────────

    #[test]
    fn collapses_excessive_blanks() {
        let input = "Hello\n\n\n\n\nWorld";
        let result = collapse_blanks(input);
        assert_eq!(result.matches("\n\n\n\n").count(), 0);
    }

    #[test]
    fn empty_input() {
        assert_eq!(postprocess_markdown(""), "");
    }

    // ── Full pipeline integration ───────────────────────────────────────

    #[test]
    fn full_html_in_markdown_gets_cleaned() {
        let input = "## Title\n\n<b>Bold text</b> and <i>italic</i>\n\n<a href=\"https://example.com\">Link</a>";
        let result = postprocess_markdown(input);
        assert!(result.contains("**Bold text**"), "got:\n{result}");
        assert!(result.contains("*italic*"), "got:\n{result}");
        assert!(result.contains("[Link](https://example.com)"), "got:\n{result}");
        assert!(!result.contains("<b>"), "no HTML tags should remain");
    }

    #[test]
    fn fragmented_syllables_get_merged() {
        let input = "## Page 1\n\n**wa**\n**ter**\n\nSome body text.";
        let result = postprocess_markdown(input);
        assert!(
            result.contains("**water**"),
            "fragments should merge, got:\n{result}"
        );
    }

    #[test]
    fn fragmented_words_get_spaced() {
        // Short syllable fragments + longer word fragments get space-joined
        let input = "## Page 1\n\n**non-**\n**st**\n**ar**\n**chy**\n**vegetables**\n\nSome body text.";
        let result = postprocess_markdown(input);
        assert!(
            result.contains("non-starchy vegetables"),
            "word-level fragments should get spaces, got:\n{result}"
        );
    }

    #[test]
    fn sample_invoice_produces_clean_markdown() {
        let bytes = include_bytes!("../../tests/fixtures/sample.pdf");
        let result = pdf_to_markdown(bytes, "pdftoppm");
        assert!(result.is_ok());
        let md = result.unwrap();
        assert!(md.contains("Invoice"), "got:\n{md}");
        assert!(!md.contains("<b>"), "no HTML tags");
    }

    // ── hOCR tests ──────────────────────────────────────────────────────

    #[test]
    fn extracts_wconf() {
        let line = "<span class='ocrx_word' title='bbox 1 2 3 4; x_wconf 62'>test</span>";
        assert_eq!(extract_wconf(line), Some(62));
    }

    #[test]
    fn extracts_span_text() {
        let line = "<span class='ocrx_word' title='bbox 1 2 3 4; x_wconf 95'>SAMPLE</span>";
        assert_eq!(extract_span_text(line), Some("SAMPLE".into()));
    }

    #[test]
    fn filters_pipe_characters() {
        let hocr = "<span class='ocrx_word' title='bbox 1 2 3 4; x_wconf 92'>|</span>";
        let cleaned = preprocess_hocr(hocr);
        assert!(!cleaned.contains("|"), "pipe should be filtered");
    }

    #[test]
    fn strips_caption_blocks() {
        let hocr = "<span class='ocr_caption' title='bbox 1 2 3 4'>\n\
            <span class='ocrx_word' title='bbox 1 2 3 4; x_wconf 95'>SAMPLE</span>\n\
            </span>";
        let cleaned = preprocess_hocr(hocr);
        assert!(!cleaned.contains("SAMPLE"), "caption should be stripped");
    }

    #[test]
    fn strips_photo_blocks() {
        let hocr = "<div class='ocr_photo' id='b1' title=\"bbox 1 2 3 4\"></div>\n<p>Real</p>";
        let cleaned = preprocess_hocr(hocr);
        assert!(!cleaned.contains("ocr_photo"));
        assert!(cleaned.contains("Real"));
    }

    // ── Reflow paragraphs ──────────────────────────────────────────────

    #[test]
    fn reflows_continuation_lines() {
        let input = "This is a long line that wraps at the column\nboundary and continues here.";
        let result = reflow_paragraphs(input);
        assert!(
            result.contains("column boundary"),
            "should join continuation, got: {result}"
        );
    }

    #[test]
    fn reflow_preserves_headings() {
        let input = "## Section Title\n\nBody text here.";
        let result = reflow_paragraphs(input);
        assert!(result.contains("## Section Title\n"), "heading preserved, got: {result}");
    }

    #[test]
    fn reflow_preserves_list_items() {
        let input = "- First item\n- Second item\n- Third item";
        let result = reflow_paragraphs(input);
        assert!(result.contains("- First item\n"), "list items preserved, got: {result}");
        assert!(result.contains("- Second item\n"), "list items preserved, got: {result}");
    }

    #[test]
    fn reflow_does_not_join_after_sentence_end() {
        let input = "End of sentence.\nNew sentence starts here.";
        let result = reflow_paragraphs(input);
        assert!(
            result.contains("sentence.\n"),
            "should not join after period, got: {result}"
        );
    }

    #[test]
    fn reflow_joins_lowercase_continuation() {
        let input = "The quick brown fox jumps over\nthe lazy dog.";
        let result = reflow_paragraphs(input);
        assert!(
            result.contains("over the lazy"),
            "should join lowercase continuation, got: {result}"
        );
    }

    #[test]
    fn reflow_preserves_blank_line_paragraphs() {
        let input = "First paragraph.\n\nSecond paragraph.";
        let result = reflow_paragraphs(input);
        assert!(
            result.contains("First paragraph.\n\nSecond paragraph."),
            "blank lines preserved, got: {result}"
        );
    }

    // ── Bold trailing-space cleanup ────────────────────────────────────

    #[test]
    fn trims_trailing_space_in_bold() {
        let input = "**Benefits of a low GI diet **";
        let result = convert_html_to_markdown(input);
        assert_eq!(result, "**Benefits of a low GI diet**");
    }

    #[test]
    fn trims_leading_space_in_bold() {
        let input = "** Benefits of a low GI diet**";
        let result = convert_html_to_markdown(input);
        assert_eq!(result, "**Benefits of a low GI diet**");
    }

    // ── Fraction heading demotion ──────────────────────────────────────

    #[test]
    fn demotes_fraction_h3() {
        let input = "### **\u{00BD}**";
        let result = fix_headings(input);
        assert!(!result.contains("###"), "fraction heading should be demoted, got: {result}");
    }

    #[test]
    fn demotes_fraction_h2() {
        let input = "## **\u{00BD}**";
        let result = fix_headings(input);
        assert!(!result.contains("##"), "fraction h2 should be demoted, got: {result}");
    }

    #[test]
    fn keeps_real_short_h2() {
        // "FAQ" is 3 chars — should NOT be demoted
        let input = "## FAQ";
        let result = fix_headings(input);
        assert!(result.contains("## FAQ"), "real heading preserved, got: {result}");
    }
}
