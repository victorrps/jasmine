#!/usr/bin/env bash
# Smoke test the PaddleOCR sidecar.
#
# Prerequisite: sidecar is running (./scripts/run_paddle_server.sh)
#
# Usage:
#   ./scripts/smoke_paddle.sh                         # default: sample.pdf
#   ./scripts/smoke_paddle.sh path/to/file.pdf
#   PADDLE_URL=http://host:8868 ./scripts/smoke_paddle.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
URL="${PADDLE_URL:-http://127.0.0.1:8868}"
PDF="${1:-$REPO_ROOT/tests/fixtures/sample.pdf}"
OUT_DIR="$REPO_ROOT/output/paddle_smoke"

if [[ ! -f "$PDF" ]]; then
  echo "ERROR: PDF not found: $PDF" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
NAME="$(basename "$PDF" .pdf)"
OUT_JSON="$OUT_DIR/$NAME.json"
OUT_MD="$OUT_DIR/$NAME.md"

echo "[smoke] PDF:      $PDF"
echo "[smoke] sidecar:  $URL"
echo "[smoke] output:   $OUT_MD"

# 1. health
echo "[smoke] GET /health"
if ! curl -fsS "$URL/health" >/dev/null; then
  echo "ERROR: sidecar is not responding at $URL. Start it with ./scripts/run_paddle_server.sh" >&2
  exit 2
fi

# 2. build the payload with python (base64 + json — avoids shell quoting pitfalls)
PAYLOAD="$(mktemp)"
trap 'rm -f "$PAYLOAD"' EXIT
python3 - "$PDF" "$PAYLOAD" <<'PY'
import base64, json, sys
pdf_path, out_path = sys.argv[1], sys.argv[2]
with open(pdf_path, "rb") as f:
    data = base64.b64encode(f.read()).decode("ascii")
with open(out_path, "w") as f:
    json.dump({"file": data, "fileType": 1}, f)
PY

# 3. POST /layout-parsing (first request may take 5-15s while model loads)
echo "[smoke] POST /layout-parsing (this may take 5-15s on first call)"
START=$(date +%s)
HTTP_CODE=$(curl -sS -o "$OUT_JSON" -w "%{http_code}" \
  -H "Content-Type: application/json" \
  --data-binary "@$PAYLOAD" \
  "$URL/layout-parsing")
ELAPSED=$(( $(date +%s) - START ))
echo "[smoke] HTTP $HTTP_CODE in ${ELAPSED}s"

if [[ "$HTTP_CODE" != "200" ]]; then
  echo "ERROR: sidecar returned HTTP $HTTP_CODE. Response body:" >&2
  cat "$OUT_JSON" >&2
  exit 3
fi

# 4. extract markdown, count pages, validate shape
python3 - "$OUT_JSON" "$OUT_MD" <<'PY'
import json, sys
json_path, md_path = sys.argv[1], sys.argv[2]
with open(json_path) as f:
    payload = json.load(f)

pages = (payload.get("result") or {}).get("layoutParsingResults") or []
if not pages:
    print("FAIL: zero pages in response", file=sys.stderr)
    sys.exit(4)

md_parts = []
for i, p in enumerate(pages, 1):
    md = (p.get("markdown") or {}).get("text") or ""
    if len(pages) > 1:
        md_parts.append(f"## Page {i}\n\n{md.strip()}")
    else:
        md_parts.append(md.strip())

combined = "\n\n".join(md_parts) + "\n"
with open(md_path, "w") as f:
    f.write(combined)

char_count = sum(len((p.get("markdown") or {}).get("text") or "") for p in pages)
print(f"[smoke] pages:    {len(pages)}")
print(f"[smoke] md chars: {char_count}")

if char_count < 10:
    print("FAIL: markdown is suspiciously empty (<10 chars)", file=sys.stderr)
    sys.exit(5)

print(f"[smoke] PASS — wrote {md_path}")
PY
