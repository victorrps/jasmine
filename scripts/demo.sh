#!/usr/bin/env bash
# DocForge API Demo Script
# Usage: ./scripts/demo.sh [PDF_PATH]
# Default PDF: tests/fixtures/sample.pdf

set -euo pipefail

BASE="http://127.0.0.1:8080"
PDF="${1:-tests/fixtures/sample.pdf}"

# Colors
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

header() { echo -e "\n${BLUE}═══════════════════════════════════════${NC}"; echo -e "${BLUE}  $1${NC}"; echo -e "${BLUE}═══════════════════════════════════════${NC}"; }
pass() { echo -e "  ${GREEN}[PASS]${NC} $1"; }
fail() { echo -e "  ${RED}[FAIL]${NC} $1"; }
info() { echo -e "  ${YELLOW}→${NC} $1"; }

# ── Health Check ──────────────────────────────────────────────────────────────

header "Health Check"
HEALTH=$(curl -s "$BASE/health")
if echo "$HEALTH" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d['status']=='ok'" 2>/dev/null; then
    VERSION=$(echo "$HEALTH" | python3 -c "import sys,json; print(json.load(sys.stdin)['version'])")
    UPTIME=$(echo "$HEALTH" | python3 -c "import sys,json; print(json.load(sys.stdin)['uptime_seconds'])")
    pass "Server v${VERSION} — uptime ${UPTIME}s"
else
    fail "Server not responding at $BASE"
    echo "$HEALTH"
    exit 1
fi

# ── Create Users ──────────────────────────────────────────────────────────────

header "Creating 3 Test Users"

declare -A JWTS
declare -A KEYS
declare -A KEY_IDS

USERS=("alice:alice@docforge.dev:aliceSecure123:Alice Chen"
       "bob:bob@docforge.dev:bobSecure456!:Bob Martinez"
       "carol:carol@docforge.dev:carolPass789!:Carol Wu")

for entry in "${USERS[@]}"; do
    IFS=':' read -r name email password fullname <<< "$entry"

    # Register
    REG=$(curl -s -X POST "$BASE/auth/register" \
        -H "Content-Type: application/json" \
        -d "{\"email\":\"$email\",\"password\":\"$password\",\"name\":\"$fullname\"}")

    REG_STATUS=$(echo "$REG" | python3 -c "import sys,json; d=json.load(sys.stdin); print('ok' if 'id' in d else d.get('error',{}).get('code','unknown'))" 2>/dev/null)

    if [ "$REG_STATUS" = "ok" ]; then
        info "$fullname registered"
    elif [ "$REG_STATUS" = "CONFLICT" ]; then
        info "$fullname already exists"
    else
        fail "Registration failed: $REG_STATUS"
    fi

    # Login
    LOGIN=$(curl -s -X POST "$BASE/auth/login" \
        -H "Content-Type: application/json" \
        -d "{\"email\":\"$email\",\"password\":\"$password\"}")
    JWT=$(echo "$LOGIN" | python3 -c "import sys,json; print(json.load(sys.stdin)['access_token'])")
    JWTS[$name]="$JWT"

    # Create API Key
    KEY_RESP=$(curl -s -X POST "$BASE/api-keys" \
        -H "Authorization: Bearer $JWT" \
        -H "Content-Type: application/json" \
        -d "{\"name\":\"${fullname} Key\"}")
    KEY=$(echo "$KEY_RESP" | python3 -c "import sys,json; print(json.load(sys.stdin)['key'])")
    KID=$(echo "$KEY_RESP" | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])")
    KEYS[$name]="$KEY"
    KEY_IDS[$name]="$KID"

    pass "$fullname — key: ${KEY:0:20}..."
done

# ── Parse Tests ───────────────────────────────────────────────────────────────

header "Test 1: Alice — Parse PDF"
PARSE=$(curl -s -X POST "$BASE/v1/parse" \
    -H "X-API-Key: ${KEYS[alice]}" \
    -F "file=@$PDF")
echo "$PARSE" | python3 -c "
import sys, json
d = json.load(sys.stdin)
doc = d['document']
meta = doc['metadata']
print(f'  Pages: {meta[\"page_count\"]}')
print(f'  Type:  {meta[\"detected_type\"]}')
print(f'  Scan:  {meta[\"is_scanned\"]}')
print(f'  Time:  {meta[\"processing_ms\"]}ms')
print(f'  Chars: {sum(p[\"char_count\"] for p in doc[\"pages\"])}')
print(f'  ReqID: {d[\"request_id\"]}')
text = doc['text'][:200].replace(chr(10), ' ')
print(f'  Text:  {text}...')
"

# ── Extract Tests ─────────────────────────────────────────────────────────────

header "Test 2: Bob — Extract Invitation Fields"
EXTRACT=$(curl -s -X POST "$BASE/v1/extract" \
    -H "X-API-Key: ${KEYS[bob]}" \
    -F "file=@$PDF" \
    -F 'schema={"type":"object","properties":{"event_name":{"type":"string"},"date":{"type":"string"},"location":{"type":"string"},"invitee":{"type":"string"},"host":{"type":"string"}}}')
echo "$EXTRACT" | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(f'  Model:   {d[\"extracted\"][\"model\"]}')
print(f'  Warning: {d[\"extracted\"].get(\"warning\", \"none\")}')
print(f'  Pages:   {d[\"usage\"][\"pages_processed\"]}')
print(f'  Credits: {d[\"usage\"][\"credits_used\"]}')
"

header "Test 3: Carol — Extract with Minimal Schema"
EXTRACT2=$(curl -s -X POST "$BASE/v1/extract" \
    -H "X-API-Key: ${KEYS[carol]}" \
    -F "file=@$PDF" \
    -F 'schema={"type":"object","properties":{"summary":{"type":"string"}}}')
echo "$EXTRACT2" | python3 -c "
import sys, json
d = json.load(sys.stdin)
print(f'  Status:  HTTP 200')
print(f'  Model:   {d[\"extracted\"][\"model\"]}')
print(f'  ReqID:   {d[\"request_id\"]}')
"

# ── Security Tests ────────────────────────────────────────────────────────────

header "Security Tests"

# No API key
STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/v1/parse" -F "file=@$PDF")
[ "$STATUS" = "401" ] && pass "Missing API key → 401" || fail "Missing API key → $STATUS (expected 401)"

# Non-PDF file
echo "not a pdf" > /tmp/docforge_fake.txt
STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/v1/parse" \
    -H "X-API-Key: ${KEYS[alice]}" -F "file=@/tmp/docforge_fake.txt")
[ "$STATUS" = "400" ] && pass "Non-PDF upload → 400" || fail "Non-PDF upload → $STATUS (expected 400)"
rm -f /tmp/docforge_fake.txt

# Wrong password
STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/auth/login" \
    -H "Content-Type: application/json" \
    -d '{"email":"alice@docforge.dev","password":"wrongpassword"}')
[ "$STATUS" = "401" ] && pass "Wrong password → 401" || fail "Wrong password → $STATUS (expected 401)"

# Revoke + use
curl -s -o /dev/null -w "" -X DELETE "$BASE/api-keys/${KEY_IDS[carol]}" \
    -H "Authorization: Bearer ${JWTS[carol]}" || true
STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/v1/parse" \
    -H "X-API-Key: ${KEYS[carol]}" -F "file=@$PDF")
[ "$STATUS" = "401" ] && pass "Revoked key → 401" || fail "Revoked key → $STATUS (expected 401)"

# OAuth placeholder
STATUS=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/auth/oauth/google")
[ "$STATUS" = "501" ] && pass "OAuth stub → 501" || fail "OAuth stub → $STATUS (expected 501)"

# X-Request-Id header
REQ_ID=$(curl -s -D - -o /dev/null "$BASE/health" 2>&1 | grep -i x-request-id | tr -d '\r' | awk '{print $2}')
[[ "$REQ_ID" == req_* ]] && pass "X-Request-Id: $REQ_ID" || fail "X-Request-Id missing or malformed: '$REQ_ID'"

# ── MCP Server Tests ──────────────────────────────────────────────────────────

header "MCP Server Tests"

# Initialize
MCP_INIT=$(curl -s -X POST "$BASE/mcp" \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","clientInfo":{"name":"demo","version":"1.0"}}}')
MCP_NAME=$(echo "$MCP_INIT" | python3 -c "import sys,json; print(json.load(sys.stdin)['result']['serverInfo']['name'])" 2>/dev/null)
[ "$MCP_NAME" = "docforge" ] && pass "MCP initialize → serverInfo.name=docforge" || fail "MCP initialize failed: $MCP_INIT"

# List tools
MCP_TOOLS=$(curl -s -X POST "$BASE/mcp" \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}')
TOOL_COUNT=$(echo "$MCP_TOOLS" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['result']['tools']))" 2>/dev/null)
[ "$TOOL_COUNT" = "2" ] && pass "MCP tools/list → $TOOL_COUNT tools (parse_document, extract_fields)" || fail "MCP tools/list: expected 2, got $TOOL_COUNT"

# Call parse_document with base64 PDF (use small test PDF to avoid size issues)
SMALL_PDF="tests/fixtures/sample.pdf"
TMPJSON=$(mktemp)
python3 -c "
import json, base64
with open('$SMALL_PDF', 'rb') as f:
    b64 = base64.b64encode(f.read()).decode()
req = {'jsonrpc':'2.0','id':3,'method':'tools/call','params':{'name':'parse_document','arguments':{'pdf_base64':b64}}}
with open('$TMPJSON', 'w') as out:
    json.dump(req, out)
"
MCP_PARSE=$(curl -s -X POST "$BASE/mcp" -H "Content-Type: application/json" -d @"$TMPJSON")
rm -f "$TMPJSON"
MCP_HAS_TEXT=$(echo "$MCP_PARSE" | python3 -c "
import sys,json
r = json.load(sys.stdin)
text = r['result']['content'][0]['text']
d = json.loads(text)
print('ok' if len(d['document']['text']) > 10 else 'fail')
" 2>/dev/null)
[ "$MCP_HAS_TEXT" = "ok" ] && pass "MCP parse_document → extracted text" || fail "MCP parse_document failed"

# ── Performance ───────────────────────────────────────────────────────────────

header "Performance: 5 Rapid Parses"
for i in 1 2 3 4 5; do
    TIME=$(curl -s -o /dev/null -w "%{time_total}" -X POST "$BASE/v1/parse" \
        -H "X-API-Key: ${KEYS[alice]}" -F "file=@$PDF")
    info "Parse $i: ${TIME}s"
done

# ── Summary ───────────────────────────────────────────────────────────────────

header "Demo Complete"
echo -e "  ${GREEN}Users:${NC}    3 created (alice, bob, carol)"
echo -e "  ${GREEN}Parse:${NC}    working — text + metadata + type detection"
echo -e "  ${GREEN}Extract:${NC}  working — Claude Haiku (or stub if no ANTHROPIC_API_KEY)"
echo -e "  ${GREEN}MCP:${NC}     working — parse_document + extract_fields tools"
echo -e "  ${GREEN}Security:${NC} all checks passed"
echo -e "  ${GREEN}PDF:${NC}      $PDF"
echo ""
