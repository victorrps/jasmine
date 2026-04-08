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

from contextlib import asynccontextmanager

from fastapi import FastAPI
from fastapi.responses import JSONResponse
from pydantic import BaseModel, Field

def _write_prewarm_image(path: Path) -> None:
    """Write a small valid PNG for pre-warming. Uses Pillow (already a
    paddleocr transitive dep) instead of a hard-coded byte blob so we
    cannot trip a CRC mismatch on a hand-typed hex literal.

    The image is intentionally a solid 64x64 white square — large
    enough for the layout detector to run its preprocessing path
    without triggering size-guard branches on 1x1 inputs.
    """
    from PIL import Image  # lazy import

    Image.new("RGB", (64, 64), color=(255, 255, 255)).save(path, format="PNG")

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
        # `PADDLE_DEVICE` selects the inference backend per the
        # PP-StructureV3 docs (§2.2). Accepted values: "cpu",
        # "gpu", "gpu:0", "gpu:1", "npu", "xpu", "mlu". Default
        # stays "cpu" so a fresh checkout works without CUDA.
        device = os.environ.get("PADDLE_DEVICE", "cpu").strip() or "cpu"
        log.info("loading PPStructureV3 pipeline on device=%s (first request)...", device)
        from paddleocr import PPStructureV3  # imported lazily so --help is fast

        _pipeline = PPStructureV3(device=device)
        log.info("PPStructureV3 ready (device=%s)", device)
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


def _prewarm() -> None:
    """Run a single throwaway predict() to compile JIT kernels and load
    GPU/CPU model weights into RAM. Skipped if `PADDLE_PREWARM=0`.

    Cold-start tax for PP-StructureV3 is dominated by CUDA kernel
    compilation and model deserialization on the first predict() call,
    not by `PPStructureV3()` construction. Pre-warming with a 1x1 PNG
    pays that cost during boot instead of on the first user request,
    where it would burn most of the request deadline budget.
    """
    # Milestones go through print(flush=True) instead of the logging
    # stack. Uvicorn installs its own dictConfig on startup which can
    # silently drop INFO-level records routed through the root logger
    # set up by our module-level basicConfig, so the user sees no
    # prewarm output at all. Printing to stdout is unconditional and
    # lines up with uvicorn's own startup banner.
    import sys

    def say(msg: str) -> None:
        print(f"[paddle-server] {msg}", file=sys.stdout, flush=True)

    if os.environ.get("PADDLE_PREWARM", "1") == "0":
        say("prewarm: skipped (PADDLE_PREWARM=0)")
        return
    try:
        import time

        t0 = time.time()
        say("prewarm: loading PPStructureV3 pipeline (this may take 40-80s on GPU cold)...")
        pipeline = get_pipeline()
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as tmp:
            tmp_path = Path(tmp.name)
        _write_prewarm_image(tmp_path)
        try:
            # Materialize the generator so kernels actually run, not
            # just get scheduled.
            say("prewarm: running throwaway predict() to compile kernels...")
            list(pipeline.predict(input=str(tmp_path)))
        finally:
            tmp_path.unlink(missing_ok=True)
        say(f"prewarm: complete in {time.time() - t0:.2f}s")
    except Exception:
        # Pre-warm failures must not block startup — the first real
        # request will simply pay the tax instead.
        log.exception("prewarm: failed (sidecar will still serve, first request pays cold start)")


@asynccontextmanager
async def lifespan(_: FastAPI):
    _prewarm()
    yield


# ── App ─────────────────────────────────────────────────────────────────────
app = FastAPI(title="DocForge PaddleOCR sidecar", version="0.1.0", lifespan=lifespan)


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
        results = pipeline.predict(input=str(tmp_path))

        # Per the official docs (§2.2 Python script integration), collect each
        # page's markdown dict and use the pipeline's own concatenator for the
        # stitched output — it handles image references and cross-page tables
        # that naive string-joining would break.
        markdown_list: list[Any] = []
        pages: list[dict[str, Any]] = []
        for res in results:
            md_info = getattr(res, "markdown", None)
            markdown_list.append(md_info)
            pages.append({"markdown": {"text": _page_markdown_text(md_info, res)}})

        if not pages:
            return error(500, "pipeline returned zero pages", status=500)

        combined_md = ""
        if len(markdown_list) > 0 and hasattr(pipeline, "concatenate_markdown_pages"):
            try:
                raw = pipeline.concatenate_markdown_pages(markdown_list)
                # PP-StructureV3 returns either a str or a dict like
                # {"markdown_texts": "...", "markdown_images": {...}} — the
                # Rust client only wants the joined text, so unwrap it.
                if isinstance(raw, dict):
                    combined_md = str(raw.get("markdown_texts") or raw.get("text") or "")
                elif raw is not None:
                    combined_md = str(raw)
            except Exception:
                log.exception("concatenate_markdown_pages failed; falling back to per-page join")

        return JSONResponse(
            status_code=200,
            content={
                "result": {
                    "layoutParsingResults": pages,
                    "combinedMarkdown": combined_md,
                },
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


def _page_markdown_text(md_info: Any, res: Any) -> str:
    """Extract a single page's markdown text from a PP-StructureV3 md_info dict/obj."""
    if md_info is not None:
        if isinstance(md_info, dict):
            txt = md_info.get("markdown_texts") or md_info.get("text") or ""
            if txt:
                return str(txt)
        for attr in ("markdown_texts", "text"):
            val = getattr(md_info, attr, None)
            if val:
                return str(val)
    return _extract_markdown(res)


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
