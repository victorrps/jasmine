#!/usr/bin/env python3
"""Generate synthetic PDF fixtures for the DocForge test suite.

All PDFs are fully synthetic — no real personal data. Regenerate with:

    .venv-paddle/bin/python scripts/build_fixtures.py

Fixtures produced under tests/fixtures/:

    multipage_report.pdf   — 3-page native-text report (multi-page stitching)
    scanned_form.pdf       — image-only PDF (triggers is_scanned → OCR path)
    form_with_labels.pdf   — labeled fields (label/heading logic)
    table_document.pdf     — native text with a bordered table (table recognition)
    ordinal_dates.pdf      — dates with ordinal suffixes (superscript artifact tests)
    long_article.pdf       — 10-page prose article (long TextSimple)
    two_column_article.pdf — 2-column newspaper layout (multi-column detection)
    mixed_content.pdf      — prose + table + labels on same pages
    long_scanned.pdf       — 3-page image-only (multi-page OCR path)

`sample.pdf` is intentionally left untouched (1-page invoice already committed).
"""
from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont
from reportlab.lib import colors
from reportlab.lib.pagesizes import LETTER
from reportlab.lib.styles import getSampleStyleSheet
from reportlab.lib.units import inch
from reportlab.pdfgen import canvas
from reportlab.platypus import (
    BaseDocTemplate,
    Frame,
    PageBreak,
    PageTemplate,
    Paragraph,
    SimpleDocTemplate,
    Spacer,
    Table,
    TableStyle,
)

REPO_ROOT = Path(__file__).resolve().parents[1]
OUT = REPO_ROOT / "tests" / "fixtures"


def build_multipage_report() -> Path:
    path = OUT / "multipage_report.pdf"
    doc = SimpleDocTemplate(str(path), pagesize=LETTER, title="Multipage Report")
    styles = getSampleStyleSheet()
    flow = []
    for i in range(1, 4):
        flow.append(Paragraph(f"Section {i}: Quarterly Summary", styles["Heading1"]))
        flow.append(Spacer(1, 0.2 * inch))
        flow.append(
            Paragraph(
                f"This is body text for page {i}. It discusses results, findings, "
                "and next steps for the Acme Widgets project. All numbers below "
                "are synthetic and meant purely for parser testing.",
                styles["BodyText"],
            )
        )
        flow.append(Spacer(1, 0.2 * inch))
        flow.append(
            Paragraph(
                f"Highlight {i}.1: Revenue grew by {i * 7}% over the prior period.",
                styles["BodyText"],
            )
        )
        flow.append(Paragraph("Page break below.", styles["BodyText"]))
        if i < 3:
            flow.append(PageBreak())
    doc.build(flow)
    return path


def build_form_with_labels() -> Path:
    path = OUT / "form_with_labels.pdf"
    c = canvas.Canvas(str(path), pagesize=LETTER)
    width, height = LETTER
    y = height - 1 * inch

    c.setFont("Helvetica-Bold", 18)
    c.drawString(1 * inch, y, "Employee Onboarding Form")
    y -= 0.5 * inch

    c.setFont("Helvetica", 12)
    fields = [
        ("Employee Name", "Alice Sample"),
        ("Employee ID", "E-0001"),
        ("Department", "Sales"),
        ("Start Date", "January 15, 2024"),
        ("Location", "Remote"),
        ("Manager", "Bob Placeholder"),
    ]
    for label, value in fields:
        c.drawString(1 * inch, y, f"{label}:")
        c.drawString(2.75 * inch, y, value)
        y -= 0.35 * inch

    y -= 0.2 * inch
    c.setFont("Helvetica-Bold", 13)
    c.drawString(1 * inch, y, "Acknowledgement")
    y -= 0.3 * inch
    c.setFont("Helvetica", 11)
    c.drawString(
        1 * inch,
        y,
        "I confirm the information above is accurate. This document is synthetic.",
    )
    c.showPage()
    c.save()
    return path


def build_table_document() -> Path:
    path = OUT / "table_document.pdf"
    doc = SimpleDocTemplate(str(path), pagesize=LETTER, title="Quarterly Sales")
    styles = getSampleStyleSheet()
    flow = [
        Paragraph("Quarterly Sales Report", styles["Heading1"]),
        Spacer(1, 0.2 * inch),
        Paragraph(
            "Synthetic figures for pipeline testing — not real data.",
            styles["BodyText"],
        ),
        Spacer(1, 0.3 * inch),
    ]
    data = [
        ["Quarter", "Units Sold", "Revenue", "Margin"],
        ["Q1", "1,200", "$120,000", "22%"],
        ["Q2", "1,450", "$145,000", "24%"],
        ["Q3", "1,780", "$178,000", "26%"],
        ["Q4", "2,050", "$205,000", "28%"],
    ]
    table = Table(data, hAlign="LEFT", colWidths=[1.2 * inch] * 4)
    table.setStyle(
        TableStyle(
            [
                ("BACKGROUND", (0, 0), (-1, 0), colors.lightgrey),
                ("GRID", (0, 0), (-1, -1), 0.5, colors.black),
                ("FONTNAME", (0, 0), (-1, 0), "Helvetica-Bold"),
                ("ALIGN", (1, 1), (-1, -1), "RIGHT"),
            ]
        )
    )
    flow.append(table)
    doc.build(flow)
    return path


def build_ordinal_dates() -> Path:
    """Plain prose with ordinal dates scattered throughout. Many paragraphs
    of flowing text so the classifier doesn't mistake the short-doc padding
    artifacts of pdf_oxide for structured layout."""
    path = OUT / "ordinal_dates.pdf"
    doc = SimpleDocTemplate(str(path), pagesize=LETTER, title="Event Schedule")
    styles = getSampleStyleSheet()
    flow = [Paragraph("Event Schedule Narrative", styles["Heading1"]), Spacer(1, 0.2 * inch)]
    paragraphs = [
        "The kickoff meeting for the initiative has been scheduled for August 3rd, 2022, "
        "and all interested parties have been notified through the usual distribution list. "
        "This meeting will set the tone for the rest of the programme and give every stakeholder "
        "a chance to raise any blockers they anticipate during the opening phase of the work.",
        "Following the kickoff, the first formal review is currently planned for September 12th, 2022, "
        "at which point the working group will assess progress against the baseline targets that were "
        "agreed during the prior planning round. Any items that are trending off target will be escalated "
        "during that review so that corrective action can be taken without delay.",
        "The second milestone, which concentrates on integration testing across all dependent systems, "
        "is expected to complete on October 21st, 2022. That milestone marks the transition from building "
        "individual components in isolation to verifying that they function correctly when assembled into "
        "the larger solution that end users will eventually interact with.",
        "A steering committee checkpoint is booked for November 4th, 2022, and its purpose is to take "
        "a holistic view of the programme, reconcile the budget against the current burn rate, and decide "
        "whether any scope adjustments are required before the programme enters its final delivery phase "
        "in the new year.",
        "The programme is scheduled to wrap up on January 1st, 2023, with final sign off from all "
        "contributing teams recorded in the shared decision log. A short retrospective will be held shortly "
        "afterwards on January 15th, 2023 to capture lessons learned so that future initiatives can benefit "
        "from the experience accumulated during the effort.",
        "Beyond the close out, there will be a follow up review on March 22nd, 2023 focused on measuring "
        "the long term impact of the work against the original success criteria. This review is essential "
        "because many benefits of programmes like this one only become visible after several months of "
        "normal operation in a production setting.",
        "Everything contained in this document is synthetic and exists purely to exercise the pipeline. "
        "None of the dates, events, or decisions described above correspond to real people, real projects, "
        "or real organisations, and the narrative is deliberately padded to provide enough prose for the "
        "classifier to recognise it as flowing text rather than a structured schedule.",
    ]
    for p in paragraphs:
        flow.append(Paragraph(p, styles["BodyText"]))
        flow.append(Spacer(1, 0.12 * inch))
    doc.build(flow)
    return path


def build_long_article() -> Path:
    """10-page prose article — long TextSimple that should not be misclassified as structured."""
    path = OUT / "long_article.pdf"
    doc = SimpleDocTemplate(str(path), pagesize=LETTER, title="Long Article")
    styles = getSampleStyleSheet()
    flow = [Paragraph("A Survey of Synthetic Testing Practices", styles["Title"]), Spacer(1, 0.3 * inch)]

    paragraphs = [
        "Synthetic fixtures are essential when real data cannot be distributed due to privacy, "
        "licensing, or contractual constraints. A well designed synthetic corpus mirrors the "
        "shape, density, and structural variety of real inputs while containing no personal "
        "information whatsoever.",
        "The goal of a classifier is not to produce a perfect answer but to produce a reliable "
        "signal that downstream routing can act upon. Classifiers that are too confident become "
        "brittle, while classifiers that hedge too much provide no value to the system.",
        "Long form articles present a particular challenge. They are often lengthy enough to "
        "tempt engineers into assuming they are complex, when in reality they consist of plain "
        "running text with no tables, no forms, and no multi column layouts.",
        "When designing heuristics it is useful to start with the simplest possible rules and "
        "only add complexity when a concrete failure case demands it. Threshold tuning should be "
        "driven by observed traffic rather than speculation about future inputs.",
        "This document intentionally contains multiple paragraphs of plain prose spanning many "
        "pages. It exercises the classifier under the condition where length alone should not "
        "trigger a structured classification, since nothing in the content is actually structured.",
    ]

    for page_no in range(1, 11):
        flow.append(Paragraph(f"Section {page_no}", styles["Heading2"]))
        flow.append(Spacer(1, 0.15 * inch))
        for para in paragraphs:
            flow.append(Paragraph(para, styles["BodyText"]))
            flow.append(Spacer(1, 0.1 * inch))
        if page_no < 10:
            flow.append(PageBreak())
    doc.build(flow)
    return path


def build_two_column_article() -> Path:
    """2-column newspaper layout — stresses multi-column detection."""
    path = OUT / "two_column_article.pdf"
    doc = BaseDocTemplate(str(path), pagesize=LETTER, title="Two Column Article")
    page_w, page_h = LETTER
    margin = 0.75 * inch
    gutter = 0.3 * inch
    col_w = (page_w - 2 * margin - gutter) / 2
    col_h = page_h - 2 * margin
    frames = [
        Frame(margin, margin, col_w, col_h, id="col1"),
        Frame(margin + col_w + gutter, margin, col_w, col_h, id="col2"),
    ]
    doc.addPageTemplates([PageTemplate(id="TwoCol", frames=frames)])

    styles = getSampleStyleSheet()
    flow = [Paragraph("Synthetic News Bulletin", styles["Title"]), Spacer(1, 0.2 * inch)]
    para = (
        "This article is rendered in a two column layout so that pdf_oxide sees "
        "a consistent horizontal gap between the left and right columns on every "
        "line. A classifier watching for stable gap positions across many lines "
        "should be able to flag this document as structured."
    )
    for i in range(20):
        flow.append(Paragraph(f"Paragraph {i + 1}. {para}", styles["BodyText"]))
        flow.append(Spacer(1, 0.08 * inch))
    doc.build(flow)
    return path


def build_mixed_content() -> Path:
    """Prose + a table + labeled fields on the same page."""
    path = OUT / "mixed_content.pdf"
    doc = SimpleDocTemplate(str(path), pagesize=LETTER, title="Mixed Content")
    styles = getSampleStyleSheet()
    flow = [
        Paragraph("Quarterly Operations Review", styles["Title"]),
        Spacer(1, 0.2 * inch),
        Paragraph(
            "This synthetic document combines multiple structural elements on the "
            "same page to stress the classifier. It contains introductory prose, a "
            "labeled metadata block, a bordered table of figures, and a closing "
            "narrative paragraph. None of this data is real.",
            styles["BodyText"],
        ),
        Spacer(1, 0.25 * inch),
        Paragraph("Metadata", styles["Heading2"]),
    ]
    labels = [
        ("Report ID", "RPT-2024-Q3"),
        ("Prepared By", "Synthetic Reporter"),
        ("Period", "July to September 2024"),
        ("Status", "Final"),
    ]
    for key, val in labels:
        flow.append(Paragraph(f"<b>{key}:</b> {val}", styles["BodyText"]))
    flow.append(Spacer(1, 0.25 * inch))
    flow.append(Paragraph("Figures", styles["Heading2"]))
    data = [
        ["Metric", "Target", "Actual", "Delta"],
        ["Uptime", "99.9%", "99.95%", "+0.05%"],
        ["Incidents", "<5", "3", "-2"],
        ["MTTR", "30m", "22m", "-8m"],
        ["Deploys", "20", "27", "+7"],
    ]
    t = Table(data, hAlign="LEFT", colWidths=[1.3 * inch] * 4)
    t.setStyle(
        TableStyle(
            [
                ("BACKGROUND", (0, 0), (-1, 0), colors.lightgrey),
                ("GRID", (0, 0), (-1, -1), 0.5, colors.black),
                ("FONTNAME", (0, 0), (-1, 0), "Helvetica-Bold"),
            ]
        )
    )
    flow.append(t)
    flow.append(Spacer(1, 0.25 * inch))
    flow.append(
        Paragraph(
            "Overall the synthetic operations team performed above plan during the "
            "reporting window. All figures above are invented and exist solely to "
            "exercise the structured content classifier.",
            styles["BodyText"],
        )
    )
    doc.build(flow)
    return path


def build_long_scanned() -> Path:
    """3-page image-only PDF — multi-page OCR path."""
    path = OUT / "long_scanned.pdf"
    img_w, img_h = 1275, 1650
    try:
        font_big = ImageFont.truetype("DejaVuSans-Bold.ttf", 48)
        font = ImageFont.truetype("DejaVuSans.ttf", 30)
    except OSError:
        font_big = ImageFont.load_default()
        font = ImageFont.load_default()

    pages: list[Image.Image] = []
    for page_no in range(1, 4):
        img = Image.new("RGB", (img_w, img_h), "white")
        draw = ImageDraw.Draw(img)
        draw.text((100, 100), f"SCANNED DOCUMENT PAGE {page_no}", fill="black", font=font_big)
        draw.text((100, 220), f"Section {page_no} of 3", fill="black", font=font)
        draw.text(
            (100, 320),
            "This page is an image only. It contains no selectable\n"
            "text layer and exists purely to exercise the multi page\n"
            "OCR recovery path in the pdf_parser pipeline.",
            fill="black",
            font=font,
        )
        draw.text(
            (100, 540),
            f"Key {page_no}: synthetic value for classifier testing.",
            fill="black",
            font=font,
        )
        pages.append(img)

    pages[0].save(path, "PDF", resolution=150.0, save_all=True, append_images=pages[1:])
    return path


def build_scanned_form() -> Path:
    """Produce an image-only PDF so pdf_oxide reports is_scanned=True.

    We render text onto a PNG, then embed that PNG as the sole page content —
    no selectable text layer, so the pipeline must fall through to OCR.
    """
    path = OUT / "scanned_form.pdf"

    # Render a page-sized image with legible text
    img_w, img_h = 1275, 1650  # ~150 DPI letter
    img = Image.new("RGB", (img_w, img_h), "white")
    draw = ImageDraw.Draw(img)
    try:
        font_big = ImageFont.truetype("DejaVuSans-Bold.ttf", 48)
        font = ImageFont.truetype("DejaVuSans.ttf", 32)
    except OSError:
        font_big = ImageFont.load_default()
        font = ImageFont.load_default()

    draw.text((100, 100), "SCANNED REQUEST FORM", fill="black", font=font_big)
    draw.text((100, 220), "Request ID: R-4242", fill="black", font=font)
    draw.text((100, 290), "Subject: Synthetic scanned fixture", fill="black", font=font)
    draw.text((100, 360), "Status: Pending review", fill="black", font=font)
    draw.text(
        (100, 460),
        "This page is an image only. It contains no real data\n"
        "and exists solely to test the OCR recovery path.",
        fill="black",
        font=font,
    )

    # Save the image as a single-page PDF (no selectable text layer).
    img.save(path, "PDF", resolution=150.0)
    return path


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    built = [
        build_multipage_report(),
        build_form_with_labels(),
        build_table_document(),
        build_ordinal_dates(),
        build_scanned_form(),
        build_long_article(),
        build_two_column_article(),
        build_mixed_content(),
        build_long_scanned(),
    ]
    for p in built:
        print(f"wrote {p.relative_to(REPO_ROOT)} ({p.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
