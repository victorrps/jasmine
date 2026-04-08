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
# Forwarded to paddle_server.py → PPStructureV3(device=...). Default
# stays "cpu"; set PADDLE_DEVICE=gpu (or gpu:0) after running
# `PADDLE_DEVICE=gpu ./scripts/setup_paddle.sh`.
export PADDLE_DEVICE="${PADDLE_DEVICE:-cpu}"

echo "[paddle-server] listening on http://$HOST:$PORT (device=$PADDLE_DEVICE)"
echo "[paddle-server] first request will load the PPStructureV3 model"
if [[ "$PADDLE_DEVICE" == cpu ]]; then
  echo "[paddle-server] (CPU load: 5-15s)"
else
  echo "[paddle-server] (GPU load: faster steady-state, ~10-20s warm-up)"
fi

cd "$REPO_ROOT"
exec python -m uvicorn scripts.paddle_server:app \
  --host "$HOST" \
  --port "$PORT" \
  --log-level info
