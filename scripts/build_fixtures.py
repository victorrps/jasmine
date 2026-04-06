#!/usr/bin/env python3
"""Generate synthetic PDF fixtures for the DocForge test suite.

All PDFs are fully synthetic — no real personal data. Regenerate with:

    .venv-paddle/bin/python scripts/build_fixtures.py

Fixtures produced under tests/fixtures/:

    multipage_report.pdf  — 3-page native-text report (multi-page stitching)
    scanned_form.pdf      — image-only PDF (triggers is_scanned → OCR path)
    form_with_labels.pdf  — labeled fields (label/heading logic)
    table_document.pdf    — native text with a bordered table (table recognition)
    ordinal_dates.pdf     — dates with ordinal suffixes (superscript artifact tests)

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
        from reportlab.platypus import PageBreak

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
    path = OUT / "ordinal_dates.pdf"
    c = canvas.Canvas(str(path), pagesize=LETTER)
    width, height = LETTER
    c.setFont("Helvetica-Bold", 16)
    c.drawString(1 * inch, height - 1 * inch, "Event Schedule")
    c.setFont("Helvetica", 12)
    lines = [
        "The kickoff meeting is scheduled for August 3rd, 2022.",
        "The follow-up review will happen on September 12th, 2022.",
        "Final sign-off is expected by January 1st, 2023.",
        "A retrospective is planned for March 22nd, 2023.",
        "This document contains only synthetic scheduling data.",
    ]
    y = height - 1.6 * inch
    for line in lines:
        c.drawString(1 * inch, y, line)
        y -= 0.35 * inch
    c.showPage()
    c.save()
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
    ]
    for p in built:
        print(f"wrote {p.relative_to(REPO_ROOT)} ({p.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
