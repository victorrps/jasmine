#!/usr/bin/env bash
# Start the PaddleOCR PP-StructureV3 sidecar on 127.0.0.1:8868.
#
# Usage:
#   ./scripts/run_paddle_server.sh              # default host/port
#   PADDLE_PORT=9000 ./scripts/run_paddle_server.sh
#
# Prerequisite: ./scripts/setup_paddle.sh (creates .venv-paddle/)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENV="$REPO_ROOT/.venv-paddle"

if [[ ! -d "$VENV" ]]; then
  echo "ERROR: $VENV not found. Run ./scripts/setup_paddle.sh first." >&2
  exit 1
fi

# shellcheck disable=SC1091
source "$VENV/bin/activate"

HOST="${PADDLE_HOST:-127.0.0.1}"
PORT="${PADDLE_PORT:-8868}"

echo "[paddle-server] listening on http://$HOST:$PORT"
echo "[paddle-server] first request will load the PPStructureV3 model (5-15s)"

cd "$REPO_ROOT"
exec python -m uvicorn scripts.paddle_server:app \
  --host "$HOST" \
  --port "$PORT" \
  --log-level info
