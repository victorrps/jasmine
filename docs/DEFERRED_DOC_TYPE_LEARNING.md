# Deferred: Document-Type Telemetry & Learning Loop

**Status:** deferred until a compliance & data-privacy review is signed off.
**Phases that shipped:** Phase 1 (weighted detector with confidence + alternates) and Phase 2 (`document_type_hint` multipart parameter with hint-wins reconciliation).

## What was deferred

### Phase 3 — Detection telemetry
- Structured per-request logging of `(detected_type, hint, effective, confidence, debug_scores)` to a durable sink.
- A metrics surface (Prometheus / OpenTelemetry) for per-type hit rate, confidence distribution, and hint-vs-detector disagreement rate.
- A per-tenant counter that flags tenants whose hints systematically disagree with the detector, which is the strongest "tune the detector" signal available.

### Phase 4 — Supervised learning loop
- Persistence of `(text sample, detected, hint, effective)` tuples as training data.
- An offline job that re-tunes strong-phrase / positive / negative keyword weights (or replaces them with a learned classifier) against the collected tuples.
- A promotion workflow: shadow new weights against production traffic before flipping the feature flag.

## Why deferred

Both phases involve **retaining document content server-side beyond the lifetime of a single request**. That changes this service's compliance posture:

- The extracted `text` field of a PDF can contain PII, payment data, health information, or export-controlled content. Persisting it — even scoped to "the first N characters for type-tuning" — triggers obligations under GDPR, CCPA/CPRA, HIPAA (if any customer ever uploads a medical record), PCI-DSS (if any customer ever uploads an invoice with a PAN), and several sectoral regimes.
- The controlled vocabulary was deliberately pruned to exclude compliance-sensitive types (`medical_record`, `id_document`, `tax_form`) for exactly this reason. Building a learning loop that ingests arbitrary uploaded text would re-open that door without the governance work that informed the original pruning decision.
- Per-tenant disagreement counters are themselves sensitive: they can reveal the *kinds* of documents a customer is processing, which many enterprise customers treat as confidential.

## Prerequisites before resuming this work

1. **Data-retention policy** — how long samples may be held, on which storage class, with which encryption, and who can read them.
2. **Customer opt-in** — explicit contractual language that permits sample retention for model tuning, defaulting to off.
3. **PII-stripping pipeline** — a pre-ingest scrubber that removes names, emails, phone numbers, card numbers, SSNs/ID numbers, and free-text addresses before any sample is written to disk.
4. **Per-tenant kill switch** — ability to purge all samples for a given tenant on request within the SLA implied by GDPR Art. 17 / CCPA §1798.105.
5. **Audit trail** — every access to retained samples must be logged with actor, reason, and data scope.
6. **Legal sign-off** — security, privacy, and legal all review the above before the first byte of customer text is persisted.

## What we keep in the meantime

Phase 1+2 already give us most of the tuning value without retention:

- `tracing::info!` lines emitted by `pdf_parser::log_detection` and the hint-disagreement log in `reconcile_document_type` contain the detected type, hint, confidence, and debug scores. These live as long as the log retention window — typically short — and can be aggregated into anonymized counters without ever persisting the raw text.
- The feature table and thresholds in `src/services/doc_type_detector.rs` are deliberately small and hand-editable, so operators can tune them from aggregated log metrics rather than raw samples.

When Phase 3/4 resumes, start by designing the counters that can be derived from the existing logs **before** adding any new retention.
