# DocForge

PDF data extraction API — structured, LLM-ready output from any PDF in a single API call. Written in Rust (actix-web 4) with an optional PaddleOCR PP-StructureV3 sidecar for layout-aware parsing of scanned documents.

## Features

- **PDF parsing** — text PDFs via `pdf_oxide`, scanned PDFs via OCR fallback
- **Layout-aware OCR** — optional [PaddleOCR PP-StructureV3](https://www.paddleocr.ai/) sidecar returns native Markdown with heading hierarchy, tables, and reading order; tesseract as secondary fallback
- **Document type detection** — invoice, receipt, contract, resume classification
- **Schema extraction** — `POST /v1/extract` with a JSON schema, returns typed fields (Claude Haiku backend, stub fallback)
- **Batch parsing** — sync and async batch endpoints
- **MCP server** — Streamable HTTP endpoint at `/mcp`
- **Auth** — JWT for humans, API keys for services (`df_live_` prefix, HMAC-SHA256 with server-side pepper)
- **Metered billing** — Stripe webhook with HMAC-SHA256 signature verification, per-tier page limits
- **Rate limiting** — per-API-key and per-IP (tighter on auth endpoints)
- **Request tracing** — UUID request IDs in headers and structured JSON logs
- **Local processing** — no customer PDFs leave the server
- **Production hardening** — request deadline, per-instance concurrency cap, encrypted-PDF detection, structured errors with `retryable` flag, Prometheus `/metrics`, idempotency-key replay

## Quickstart

```bash
# 1. Configure
cp .env.example .env
# Edit .env — set JWT_SECRET and API_KEY_PEPPER to random 64-char strings

# 2. Run the Rust API
cargo run
# → http://127.0.0.1:8080
```

## Optional: PaddleOCR sidecar (layout-aware OCR)

For scanned PDFs or documents with complex layout, run the Python sidecar alongside the Rust API. The sidecar loads `PPStructureV3()` once and serves a local HTTP endpoint the Rust server talks to.

```bash
# One-time install (creates .venv-paddle/, ~1.5 GB)
./scripts/setup_paddle.sh

# Start the sidecar (listens on 127.0.0.1:8868)
./scripts/run_paddle_server.sh
```

Then in `.env`:

```
PADDLEOCR_URL=http://localhost:8868
PADDLEOCR_TIMEOUT_SECS=120
# PADDLEOCR_MODE defaults to "auto" whenever PADDLEOCR_URL is set.
# Explicit override: "auto" | "primary" | "fallback"
```

**Routing modes:**

- `auto` (default when `PADDLEOCR_URL` is set) — classify the document first, then route: plain prose → `pdf_oxide` only, structured (tables, forms, multi-column) → Paddle, scanned/image-only → OCR chain. The response envelope exposes the routing decision under `document.metadata.classification` and `document.metadata.routed_to` so operators can audit every request.
- `fallback` (default when `PADDLEOCR_URL` is unset) — `pdf_oxide` handles every PDF first; PaddleOCR is called only when the document is detected as scanned. Tesseract is the final fallback.
- `primary` — PaddleOCR PP-StructureV3 runs on every PDF regardless of content; `pdf_oxide` is used only if the sidecar fails. Highest quality for structured docs, slowest for plain text.

Multi-page PDFs are stitched on the sidecar via PP-StructureV3's own `concatenate_markdown_pages` (§2.2 of the official docs), which preserves image references and cross-page tables.

**Smoke test:**

```bash
./scripts/smoke_paddle.sh                        # defaults to tests/fixtures/sample.pdf
./scripts/smoke_paddle.sh path/to/document.pdf   # any PDF
```

Output markdown is written to `output/paddle_smoke/<name>.md`.

Requirements: Python 3.9–3.12 with `python3-venv` installed.

## API Reference

### Health & metrics
```bash
curl http://localhost:8080/health
curl http://localhost:8080/metrics    # Prometheus text format — protect via network policy
```

`/metrics` exposes `parse_requests_total{endpoint,status}`,
`parse_duration_seconds{backend}`, `classifier_class_total{class}`,
`paddle_degraded_total`, `parse_gate_in_flight`, and
`extract_validation_failures_total`. Both `/health` and `/metrics` are
unauthenticated by design — bind them to a private interface or
firewall them off in production.

### Register / Login
```bash
curl -X POST http://localhost:8080/auth/register \
  -H "Content-Type: application/json" \
  -d '{"email":"you@example.com","password":"securepass123","name":"Your Name"}'

curl -X POST http://localhost:8080/auth/login \
  -H "Content-Type: application/json" \
  -d '{"email":"you@example.com","password":"securepass123"}'
# → {"access_token":"eyJ...","token_type":"bearer","expires_in":900}
```

### API Keys
```bash
# Create (key returned ONCE)
curl -X POST http://localhost:8080/api-keys \
  -H "Authorization: Bearer <JWT>" \
  -H "Content-Type: application/json" \
  -d '{"name":"My App"}'

curl http://localhost:8080/api-keys -H "Authorization: Bearer <JWT>"
curl -X DELETE http://localhost:8080/api-keys/<key_id> -H "Authorization: Bearer <JWT>"
```

### Parse PDF
```bash
curl -X POST http://localhost:8080/v1/parse \
  -H "X-API-Key: df_live_..." \
  -F "file=@document.pdf"

# Optional: tell the server what you think the document is. The hint wins
# on disagreement with the auto-detector (see doc_type_detector.rs).
curl -X POST http://localhost:8080/v1/parse \
  -H "X-API-Key: df_live_..." \
  -F "file=@document.pdf" \
  -F "document_type_hint=invoice"
```

**Document-type fields in the response** (all optional, omitted when absent):

| Field | Meaning |
|---|---|
| `detected_type` | Auto-detected type, e.g. `invoice`, `report`, `article`, `form`. `None` when the detector is not confident. |
| `detected_type_confidence` | Normalized confidence in `[0, 1]`. |
| `detected_type_alternates` | Up to 2 runner-up types with their confidences. |
| `document_type_hint` | Echo of the caller-supplied hint after parsing. |
| `document_type` | Effective type after reconciliation (hint wins). |
| `document_type_source` | `"hint"` or `"detector"`. |

Accepted hint values: `invoice`, `receipt`, `contract`, `resume` (or `cv`),
`bank_statement`, `letter`, `invitation`, `report`, `purchase_order` (or
`po`), `quote` (or `estimate`), `academic_paper` (or `paper`), `article`
(or `news`), `form`, `other`. Unknown values are logged and ignored.
Compliance-sensitive types (`medical_record`, `id_document`, `tax_form`)
are intentionally excluded — see `docs/DEFERRED_DOC_TYPE_LEARNING.md`.

### Extract (schema-driven)
```bash
curl -X POST http://localhost:8080/v1/extract \
  -H "X-API-Key: df_live_..." \
  -F "file=@invoice.pdf" \
  -F "document_type_hint=invoice" \
  -F 'schema={"type":"object","properties":{"invoice_number":{"type":"string"},"amount":{"type":"number"}}}'
```

### Idempotent retries

Both `/v1/parse` and `/v1/extract` accept an `Idempotency-Key` header
(max 128 chars). The first call runs normally; subsequent calls within
24 h with the same key from the same API key replay the cached body
verbatim and add an `X-Idempotent-Replay: true` response header. Replays
**do not** consume gate permits, log usage, or charge billing.

```bash
curl -X POST http://localhost:8080/v1/parse \
  -H "X-API-Key: df_live_..." \
  -H "Idempotency-Key: invoice-2026-04-06-001" \
  -F "file=@invoice.pdf"
```

The cache is in-process and per-instance. See
`docs/DEFERRED_INFRA.md` for the multi-instance (Redis) follow-up.

### Error envelope

All errors return a structured body so SDKs can drive retry logic:

```json
{
  "error": "service unavailable",
  "code": "service_unavailable",
  "request_id": "req_...",
  "retryable": true
}
```

`retryable: true` means the client should back off and retry (503
busy, 504 deadline, 429 quota). `retryable: false` means the request
will fail the same way every time (400, 401, 403, 413, 422). 503
responses also include a `Retry-After: 5` header.

### Batch
```bash
# Sync
curl -X POST http://localhost:8080/v1/parse/batch \
  -H "X-API-Key: df_live_..." \
  -F "files=@a.pdf" -F "files=@b.pdf"

# Async — returns batch_id, poll /v1/parse/batch/<id>
```

### Billing
```bash
curl http://localhost:8080/v1/usage -H "X-API-Key: df_live_..."
curl http://localhost:8080/billing/plans
```

## Configuration

| Variable | Required | Notes |
|---|---|---|
| `JWT_SECRET` | ✅ | ≥32 chars |
| `API_KEY_PEPPER` | ✅ | ≥32 chars, HMAC key for API key hashing — rotate ⇒ invalidates all keys |
| `DATABASE_URL` | ✅ | `sqlite://docforge.db?mode=rwc` for dev |
| `HOST` / `PORT` | optional | defaults `127.0.0.1:8080` |
| `RATE_LIMIT_PER_MINUTE` | optional | default `60` |
| `MAX_CONCURRENT_PARSES` | optional | default `8` — per-instance concurrency cap; saturates → 503 + Retry-After:5 |
| `PARSE_DEADLINE_SECS` | optional | default `90` — wall-clock budget for the dispatcher; exceeded → 504 |
| `EXTRACT_MAX_INPUT_CHARS` | optional | default `200000` — max markdown chars sent to schema extractor; exceeded → 413 |
| `JWT_EXPIRY_MINUTES` | optional | default `15` |
| `PADDLEOCR_URL` | optional | e.g. `http://localhost:8868` |
| `PADDLEOCR_TIMEOUT_SECS` | optional | default `120` |
| `PADDLEOCR_MODE` | optional | `auto` \| `primary` \| `fallback` — defaults to `auto` when `PADDLEOCR_URL` is set, else `fallback`. See PaddleOCR section. |
| `ANTHROPIC_API_KEY` | optional | enables Claude Haiku schema extraction |
| `STRIPE_SECRET_KEY` | optional | enables Stripe billing |
| `STRIPE_WEBHOOK_SECRET` | optional | required to accept `/billing/webhook` calls |
| `TESSERACT_PATH` / `PDFTOPPM_PATH` | optional | default `tesseract` / `pdftoppm` on PATH |

See `.env.example` for a full template.

## Architecture

```
┌──────────────┐   HTTP    ┌─────────────────┐
│  Rust API    │◄─────────►│  PaddleOCR       │  ← optional sidecar (Python)
│  actix-web 4 │           │  PP-StructureV3  │     .venv-paddle/
└──────┬───────┘           └─────────────────┘
       │
       ├── pdf_oxide (fast text extraction)
       ├── tesseract + pdftoppm (fallback OCR)
       ├── SQLite (users, api_keys, usage_log)
       └── Claude Haiku (optional, schema extraction)
```

| Decision | Choice | Rationale |
|---|---|---|
| Language | Rust | Sub-ms PDF parsing, low memory, no AGPL |
| Web framework | actix-web 4 | Fastest, mature |
| PDF engine | `pdf_oxide` | MIT, native text extraction |
| OCR (layout-aware) | PaddleOCR PP-StructureV3 | Tables + headings + reading order, native Markdown |
| OCR (fallback) | tesseract via pdftoppm | Zero-config, ubiquitous |
| DB | SQLite (dev) | Swap to Postgres for prod |
| Password hash | Argon2id | OWASP recommended |
| API key hash | HMAC-SHA256 + pepper | Prevents offline brute-force if DB leaks |
| Auth | JWT (users) + API keys (services) | Humans vs. machines |

## Markdown dialect per backend

The `markdown` field in the response varies by which backend produced
it. We document the dialects rather than normalizing them so callers
know what to expect — a normalization pass is deferred.

| Backend | Headings | Tables | Images | Page breaks | Notes |
|---|---|---|---|---|---|
| `pdf_oxide` | ATX (`# Heading`) inferred from font weight/size | GitHub pipe tables when borders are detected; otherwise plain text | not extracted | `## Page N` separators on multi-page docs | Plain prose comes through cleanly; complex layouts collapse to linear text |
| `paddle` (PP-StructureV3) | ATX, hierarchy preserved by the layout model | Pipe tables with header row | Referenced as `![](images/...)` (paths are sidecar-local) | `concatenate_markdown_pages` handles cross-page tables natively | Highest fidelity for structured docs |
| `tesseract` | none (line breaks only) | none | none | `## Page N` separators | Lowest fidelity; used only when both pdf_oxide and Paddle fail or are unavailable |

**Stable subset across all backends:** plain paragraphs separated by
blank lines, line breaks within paragraphs, UTF-8 text. Anything beyond
that (headings, tables, images, page markers) is best-effort and
backend-specific. If you need a guaranteed shape, use `/v1/extract`
with a JSON schema rather than parsing the markdown directly.

## Security

- Argon2id password hashing
- API keys: HMAC-SHA256 with server-side pepper (`API_KEY_PEPPER`), plaintext shown once
- JWT: 15-minute expiry, HS256
- Stripe webhook: HMAC-SHA256 signature verification with 5-minute replay tolerance, constant-time comparison
- Rate limiting uses `peer_addr()` only — forwarded-IP headers are NOT trusted (no spoofing)
- PDF magic-byte validation, 50 MB upload cap
- Structured errors — no internal details leaked
- All secrets from environment; fail-fast on missing required vars

## Development

```bash
cargo check
cargo clippy --all-targets -- -D warnings
cargo test

# Live sidecar integration tests (env-gated — skipped without PADDLEOCR_URL)
PADDLEOCR_URL=http://127.0.0.1:8868 cargo test --test test_paddle_ocr_live
PADDLEOCR_URL=http://127.0.0.1:8868 cargo test --test test_parse_paddle_e2e
```

### Test fixtures

All PDFs under `tests/fixtures/` are fully synthetic. Regenerate with:

```bash
.venv-paddle/bin/python scripts/build_fixtures.py
```

| Fixture | Covers |
|---|---|
| `sample.pdf` | 1-page labeled invoice (baseline) |
| `multipage_report.pdf` | 3-page native text (multi-page stitching) |
| `long_article.pdf` | 10-page prose article (long `TextSimple`) |
| `ordinal_dates.pdf` | Prose with ordinal date suffixes |
| `form_with_labels.pdf` | Labeled fields (label/heading logic) |
| `table_document.pdf` | Native text with a bordered table |
| `two_column_article.pdf` | Two-column newspaper layout (column alignment) |
| `mixed_content.pdf` | Prose + labeled metadata + table on same page |
| `scanned_form.pdf` | Single-page image-only PDF (`is_scanned` → OCR path) |
| `long_scanned.pdf` | 3-page image-only PDF (multi-page OCR path) |

## License

Proprietary. All rights reserved.
