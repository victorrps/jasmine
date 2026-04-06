"""PaddleOCR PP-StructureV3 HTTP sidecar for DocForge.

A minimal FastAPI service that loads `PPStructureV3()` once at startup and
exposes the PaddleX `/layout-parsing` contract that the Rust client in
`src/services/paddle_ocr.rs` already speaks.

Contract (PaddleX v3, partial — only what DocForge needs):

    POST /layout-parsing
    body: {"file": "<base64>", "fileType": 0|1}   # 0 = image, 1 = PDF
    200:  {"result": {"layoutParsingResults": [
              {"markdown": {"text": "<page markdown>"}},
              ...
          ]}}
    4xx/5xx: {"errorCode": <int>, "errorMsg": "<string>"}

Run:
    ./scripts/run_paddle_server.sh
or:
    .venv-paddle/bin/uvicorn scripts.paddle_server:app \
        --host 127.0.0.1 --port 8868

All processing is local — no customer data leaves the host.
"""
from __future__ import annotations

import base64
import logging
import os
import tempfile
from pathlib import Path
from typing import Any

from fastapi import FastAPI
from fastapi.responses import JSONResponse
from pydantic import BaseModel, Field

logging.basicConfig(
    level=os.environ.get("PADDLE_LOG_LEVEL", "INFO"),
    format="%(asctime)s [paddle-server] %(levelname)s %(message)s",
)
log = logging.getLogger("paddle_server")

# ── Model singleton ─────────────────────────────────────────────────────────
# PPStructureV3 is expensive to construct (model load ~5–15s on CPU). We build
# it lazily on the first request and reuse it for the lifetime of the process.
_pipeline: Any | None = None


def get_pipeline() -> Any:
    global _pipeline
    if _pipeline is None:
        log.info("loading PPStructureV3 pipeline (first request)...")
        from paddleocr import PPStructureV3  # imported lazily so --help is fast

        _pipeline = PPStructureV3()
        log.info("PPStructureV3 ready")
    return _pipeline


# ── Request / response models ───────────────────────────────────────────────
class LayoutRequest(BaseModel):
    file: str = Field(..., description="base64-encoded PDF or image bytes")
    fileType: int = Field(1, description="0 = image, 1 = PDF")


def error(code: int, msg: str, status: int = 400) -> JSONResponse:
    return JSONResponse(
        status_code=status,
        content={"errorCode": code, "errorMsg": msg},
    )


# ── App ─────────────────────────────────────────────────────────────────────
app = FastAPI(title="DocForge PaddleOCR sidecar", version="0.1.0")


@app.get("/health")
def health() -> dict[str, str]:
    return {"status": "ok", "loaded": str(_pipeline is not None).lower()}


@app.post("/layout-parsing")
def layout_parsing(req: LayoutRequest) -> JSONResponse:
    try:
        raw = base64.b64decode(req.file, validate=True)
    except Exception as e:
        return error(400, f"invalid base64: {e}")

    if not raw:
        return error(400, "empty file")

    suffix = ".pdf" if req.fileType == 1 else ".png"
    with tempfile.NamedTemporaryFile(suffix=suffix, delete=False) as tmp:
        tmp.write(raw)
        tmp_path = Path(tmp.name)

    try:
        pipeline = get_pipeline()
        log.info("parsing %s (%d bytes)", tmp_path.name, len(raw))
        results = pipeline.predict(str(tmp_path))

        pages: list[dict[str, Any]] = []
        for res in results:
            md_text = _extract_markdown(res)
            pages.append({"markdown": {"text": md_text}})

        if not pages:
            return error(500, "pipeline returned zero pages", status=500)

        return JSONResponse(
            status_code=200,
            content={
                "result": {"layoutParsingResults": pages},
                "errorCode": 0,
                "errorMsg": "",
            },
        )
    except Exception as e:
        log.exception("layout-parsing failed")
        return error(500, f"pipeline error: {e}", status=500)
    finally:
        try:
            tmp_path.unlink(missing_ok=True)
        except Exception:
            pass


def _extract_markdown(res: Any) -> str:
    """Extract markdown text from a PP-StructureV3 result object.

    The result object exposes `.markdown` in v3.x — either as a dict
    (`{"markdown_texts": "..."}`) or as an object with a `markdown_texts`
    attribute. Fall back to save_to_markdown if those are unavailable.
    """
    md = getattr(res, "markdown", None)
    if md is not None:
        if isinstance(md, dict):
            txt = md.get("markdown_texts") or md.get("text") or ""
            if txt:
                return str(txt)
        for attr in ("markdown_texts", "text"):
            val = getattr(md, attr, None)
            if val:
                return str(val)

    # Fallback: save_to_markdown to a temp dir and read it back.
    with tempfile.TemporaryDirectory() as d:
        try:
            res.save_to_markdown(save_path=d)
            mds = sorted(Path(d).rglob("*.md"))
            if mds:
                return mds[0].read_text(encoding="utf-8", errors="replace")
        except Exception:
            log.exception("save_to_markdown fallback failed")
    return ""


if __name__ == "__main__":
    import uvicorn

    host = os.environ.get("PADDLE_HOST", "127.0.0.1")
    port = int(os.environ.get("PADDLE_PORT", "8868"))
    uvicorn.run(app, host=host, port=port, log_level="info")
