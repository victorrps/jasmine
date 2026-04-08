#!/usr/bin/env bash
# Create the PaddleOCR sidecar venv and install dependencies.
#
# Usage:
#   ./scripts/setup_paddle.sh
#
# Idempotent — safe to re-run. Uses a dedicated venv at .venv-paddle/ so the
# large paddlepaddle deps don't pollute any other Python environment.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENV="$REPO_ROOT/.venv-paddle"
PY="${PYTHON:-python3}"

echo "[setup_paddle] repo root: $REPO_ROOT"
echo "[setup_paddle] venv:      $VENV"

if ! command -v "$PY" >/dev/null 2>&1; then
  echo "ERROR: $PY not found on PATH. Install Python 3.9-3.12 first." >&2
  exit 1
fi

PY_VERSION="$("$PY" -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")')"
echo "[setup_paddle] python:    $PY ($PY_VERSION)"

case "$PY_VERSION" in
  3.9|3.10|3.11|3.12) ;;
  *)
    echo "WARNING: PaddleOCR 3.x supports Python 3.9-3.12; found $PY_VERSION." >&2
    ;;
esac

if [[ ! -f "$VENV/bin/activate" ]]; then
  if [[ -d "$VENV" ]]; then
    echo "[setup_paddle] removing stale venv (no activate script)"
    rm -rf "$VENV"
  fi
  echo "[setup_paddle] creating venv"
  "$PY" -m venv "$VENV"
fi

# shellcheck disable=SC1091
source "$VENV/bin/activate"

python -m pip install --upgrade pip wheel

# Device selection. CPU default keeps a clean checkout buildable on any
# machine; set `PADDLE_DEVICE=gpu` (optionally with `PADDLE_CUDA=cu126`
# or `cu118`) to install the matching paddlepaddle-gpu wheel from the
# official PaddlePaddle index. See PP-StructureV3 docs §1 (Installation).
DEVICE="${PADDLE_DEVICE:-cpu}"
CUDA_TAG="${PADDLE_CUDA:-cu126}"

case "$DEVICE" in
  cpu)
    echo "[setup_paddle] installing paddlepaddle (CPU)"
    python -m pip install \
      "paddlepaddle==3.2.0" \
      -i https://www.paddlepaddle.org.cn/packages/stable/cpu/
    ;;
  gpu)
    echo "[setup_paddle] installing paddlepaddle-gpu (CUDA=$CUDA_TAG)"
    python -m pip install \
      "paddlepaddle-gpu==3.2.0" \
      -i "https://www.paddlepaddle.org.cn/packages/stable/$CUDA_TAG/"
    ;;
  *)
    echo "ERROR: PADDLE_DEVICE must be 'cpu' or 'gpu' (got: $DEVICE)" >&2
    exit 1
    ;;
esac

echo "[setup_paddle] installing paddleocr[doc-parser] + sidecar deps"
python -m pip install -r "$REPO_ROOT/scripts/requirements-paddle.txt"

echo
echo "[setup_paddle] done (device=$DEVICE)."
if [[ "$DEVICE" == "gpu" ]]; then
  echo "Run the sidecar with: PADDLE_DEVICE=gpu ./scripts/run_paddle_server.sh"
else
  echo "Run the sidecar with: ./scripts/run_paddle_server.sh"
fi
