# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project shape

**DocForge** is a Rust (actix-web 4) PDF extraction API with an optional Python PaddleOCR PP-StructureV3 sidecar for layout-aware parsing. The binary and library share one crate (`docforge`) — `src/lib.rs` re-exports all modules so integration tests can reach into them, and `src/main.rs` is the HTTP server entry point.

Database is SQLite via `sqlx` with migrations in `migrations/` (run automatically at startup via `db::init_db`). All customer PDFs are processed locally — nothing is shipped to external services unless `ANTHROPIC_API_KEY` is set for Claude-Haiku schema extraction.

## Common commands

```bash
# Build + check
cargo build
cargo check
cargo clippy --all-targets -- -D warnings   # must be clean — fixes are enforced

# Run the API
cargo run                                    # → http://127.0.0.1:8080

# Tests
cargo test                                   # full suite (lib + integration)
cargo test --lib                             # unit tests only
cargo test --lib services::pdf_classifier    # one module
cargo test --test test_parse                 # one integration file (tests/test_parse.rs)
cargo test fn_name_substring                 # filter by test name

# Live PaddleOCR integration tests (env-gated — silent skip without the sidecar)
PADDLEOCR_URL=http://127.0.0.1:8868 cargo test --test test_paddle_ocr_live
PADDLEOCR_URL=http://127.0.0.1:8868 cargo test --test test_parse_paddle_e2e
PADDLEOCR_URL=http://127.0.0.1:8868 cargo test --test test_parse_auto_e2e

# PaddleOCR sidecar (optional, ~1.5 GB)
./scripts/setup_paddle.sh                    # one-time venv install into .venv-paddle/
./scripts/run_paddle_server.sh               # listens on 127.0.0.1:8868 (lazy model load on first request)
./scripts/smoke_paddle.sh [path/to.pdf]      # end-to-end curl against the running sidecar

# Regenerate synthetic test PDFs (10 fixtures under tests/fixtures/)
.venv-paddle/bin/python scripts/build_fixtures.py
```

Required env vars (see `.env.example`): `JWT_SECRET`, `API_KEY_PEPPER` (both ≥32 chars), `DATABASE_URL`. Everything else is optional. `AppConfig::from_vars` in `src/config.rs` validates on startup.

## Architecture essentials

### PDF dispatch pipeline (`src/services/pdf_parser.rs::parse_pdf_with_backends_mode`)

This is the single most important function to understand — every request flow for `/v1/parse` and `/v1/extract` goes through it. It dispatches to backends based on `PaddleOcrMode`:

- **`Fallback`** — pdf_oxide first; Paddle/tesseract only if the doc is detected as scanned
- **`Primary`** — Paddle first for every PDF; pdf_oxide as fallback
- **`Auto`** (default when `PADDLEOCR_URL` is set) — runs pdf_oxide first pass (cheap, ~50ms), classifies the extracted text via `pdf_classifier::classify`, then routes: `TextSimple` → keep pdf_oxide result, `TextStructured` → call Paddle, `ScannedOrEmpty` → OCR chain, `Unknown` → behave like Fallback

Every result is tagged with `metadata.routed_to` (typed `RoutedTo` enum: `PdfOxide | Paddle | Tesseract`) so the response envelope records which backend actually produced the output. When `run_ocr_backends` internally falls from Paddle → Tesseract, the outcome struct preserves the real backend — do not infer from `paddle_cfg.is_some()`.

`finalize_ocr_result(ocr, classification, routed_to)` is the helper every OCR dispatch arm uses to stamp the classification summary + routing tag onto the `ParseResult`. Call it, don't duplicate the stamp block.

### Classifier (`src/services/pdf_classifier.rs`)

Pure, deterministic, heuristic — no I/O, no model loading, target <50ms per call. Thresholds are hand-picked v1 constants (`MIN_CHARS_PER_PAGE`, `PIPE_DENSITY_TABLE`, `COLUMN_ALIGNMENT_STRONG`, `COMBINED_WEAK_SIGNAL`) and will be tuned from production traffic — they are intentionally not configurable and not ML-based.

Two public types distinguish server-side vs client-facing data:

- `ClassificationReport { class, signals }` — full data with raw signal values, logged server-side via `tracing::info!`. **Does not derive `Serialize`** — never expose it over the API. The numeric signals would let callers reverse-engineer the routing thresholds.
- `ClassificationSummary { class }` — the `Serialize`-derived projection that lands in the response envelope. Convert via `report.into()`.

Line walking is capped at `MAX_CLASSIFIER_LINES = 5_000` to prevent CPU exhaustion from pathological PDFs. Any new label/line-level heuristic must respect UTF-8 char boundaries — slicing a `&str` by byte index will panic on multi-byte input from adversarial PDF text.

### PaddleOCR sidecar contract

`scripts/paddle_server.py` is a FastAPI service that loads `PPStructureV3()` once (lazy, ~5-15s on first request) and exposes:

```
POST /layout-parsing
body: { "file": "<base64>", "fileType": 0|1 }   // 0 = image, 1 = PDF
resp: { "result": { "layoutParsingResults": [...], "combinedMarkdown": "..." } }
```

The Rust client (`src/services/paddle_ocr.rs`) prefers `combinedMarkdown` when present — the sidecar produces it via `pipeline.concatenate_markdown_pages(...)` per PP-StructureV3 §2.2, which correctly handles cross-page tables and image references. Naive per-page `## Page N` stitching is the fallback only.

### Auth & request flow

Humans authenticate via JWT (`/auth/register`, `/auth/login`, HS256, 15-min expiry). Services authenticate via API keys (`X-API-Key: df_live_...`), stored as HMAC-SHA256 with a server-side pepper (`API_KEY_PEPPER` env var). Rotating the pepper invalidates all existing keys — this is intentional and documented.

Every request goes through `RequestIdMiddleware` (`src/middleware/request_id.rs`) which assigns a `req_...` UUID exposed as `x-request-id` header and included in every structured log line. Rate limiting is per-API-key and per-IP via `actix-governor`, with a tighter bucket on `/auth/*` endpoints. Rate limiting uses `peer_addr()` only — forwarded-IP headers are NOT trusted.

### Billing (optional)

`src/services/billing.rs` + `src/api/billing.rs` implement Stripe metered billing with HMAC-SHA256 webhook signature verification (5-min replay tolerance, constant-time comparison). Per-tier page limits are enforced via `check_usage_limit` before every parse. Billing is disabled if `STRIPE_SECRET_KEY` / `STRIPE_WEBHOOK_SECRET` are unset — the app still runs without them.

## Test fixtures

All PDFs under `tests/fixtures/` are fully synthetic and regenerable via `scripts/build_fixtures.py` (reportlab + Pillow). No real PII, ever. If a new heuristic or backend needs a new shape of document, add a generator function to `build_fixtures.py` and regenerate — do not commit PDFs produced by unknown tools.

| Fixture | Purpose |
|---|---|
| `sample.pdf` | 1-page labeled invoice (classifies as `TextStructured`) |
| `multipage_report.pdf` | 3-page native text (multi-page stitching) |
| `long_article.pdf` | 10-page prose (long `TextSimple`) |
| `ordinal_dates.pdf` | Prose with ordinal date suffixes |
| `form_with_labels.pdf` | Labeled fields (label/heading logic) |
| `table_document.pdf` | Native text with a bordered table |
| `two_column_article.pdf` | Two-column newspaper layout |
| `mixed_content.pdf` | Prose + labels + table on one page |
| `scanned_form.pdf` | Single-page image-only (triggers OCR) |
| `long_scanned.pdf` | 3-page image-only (multi-page OCR) |

## Conventions specific to this repo

- **Integration tests construct `AppConfig` as a struct literal** (see `tests/test_parse.rs`, `tests/test_auth.rs`, `tests/test_parse_paddle_e2e.rs`). When adding a field to `AppConfig`, every test harness must be updated or builds break.
- **`DocumentMetadata` is a serialized public type.** New fields must be `Option<T>` with `#[serde(skip_serializing_if = "Option::is_none")]` so existing clients don't see nulls for modes that don't populate them.
- **Clippy is enforced as `-D warnings`.** Even style lints like `manual_range_contains` will block the build.
- **No personal data in tests or fixtures.** The grep seed list to verify after any fixture regeneration is documented in historical commits — the short version is "use neutral synthetic names (Alice, Bob, Example Corp) and run the generator fresh".

## Key entry points for exploration

- `src/main.rs` — actix-web server setup, route wiring, middleware stack
- `src/config.rs` — `AppConfig::from_vars` — all env var parsing + defaults lives here
- `src/services/pdf_parser.rs` — `parse_pdf_with_backends_mode` is the dispatcher
- `src/services/pdf_classifier.rs` — heuristic classifier + thresholds
- `src/api/parse.rs` — `/v1/parse` handler (multipart → dispatcher → usage log)
- `src/api/extract.rs` — `/v1/extract` handler (parse + schema extraction via Claude Haiku or stub)
