"""Loop-block detector for the asyncio event loop.

Background thread that pings the loop every PING_INTERVAL_MS via
``loop.call_soon_threadsafe`` and measures the round-trip time. When
the round-trip exceeds THRESHOLD_MS, the main thread's call stack is
captured *mid-block* so we can localize the offending synchronous
handler. The capture happens after the threshold elapses but before
the ping completes — `captured_during_block=True` in the row means we
caught the stack while the loop was actually still blocked.

Output:
  - WARNING log line per block (single-line summary)
  - JSONL row at ``logs-paper/loop_blocks-YYYY-MM-DD.jsonl``

JSONL schema:
    {
      "ts": ISO-8601 UTC,
      "elapsed_ms": float,
      "main_stack": ["file:line in func", ...],  # innermost first
      "captured_during_block": bool
    }

Env knobs (all optional):
  CORSAIR_LOOP_BLOCK_THRESHOLD_MS  default 50.0
  CORSAIR_LOOP_BLOCK_PING_MS       default 10.0
  CORSAIR_LOOP_BLOCK_DISABLED      "1" → disable entirely
"""

import asyncio
import json
import logging
import os
import sys
import threading
import time
from datetime import date, datetime, timezone

logger = logging.getLogger(__name__)

_MAX_FRAMES = 25


def _capture_main_stack() -> list[str]:
    """Snapshot the MainThread's current call stack.

    Returns ``["file:line in func", ...]`` with the innermost frame
    first. Bounded to ``_MAX_FRAMES`` so a runaway recursion can't
    blow the JSONL row.
    """
    main_ident: int | None = None
    for t in threading.enumerate():
        if t.name == "MainThread":
            main_ident = t.ident
            break
    if main_ident is None:
        return []
    frame = sys._current_frames().get(main_ident)
    if frame is None:
        return []
    out: list[str] = []
    while frame is not None and len(out) < _MAX_FRAMES:
        out.append(
            f"{os.path.basename(frame.f_code.co_filename)}:"
            f"{frame.f_lineno} in {frame.f_code.co_name}"
        )
        frame = frame.f_back
    return out


class LoopBlockDetector(threading.Thread):
    """Watches the asyncio loop for synchronous blocks ≥ threshold.

    Daemon thread; running indefinitely is the intended mode. Stop with
    ``stop()`` for clean shutdown (or just let process exit drop it).
    """

    def __init__(
        self,
        loop: asyncio.AbstractEventLoop,
        threshold_ms: float = 50.0,
        ping_interval_ms: float = 10.0,
        jsonl_dir: str = "/app/logs-paper",
    ) -> None:
        super().__init__(name="loop-block-detector", daemon=True)
        self._loop = loop
        self._threshold_s = max(threshold_ms / 1000.0, 0.001)
        self._ping_interval_s = max(ping_interval_ms / 1000.0, 0.001)
        self._jsonl_dir = jsonl_dir
        self._stop_event = threading.Event()
        self._jsonl_lock = threading.Lock()

    def stop(self) -> None:
        self._stop_event.set()

    def run(self) -> None:
        while not self._stop_event.is_set():
            sent_ns = time.monotonic_ns()
            ping_done = threading.Event()
            try:
                self._loop.call_soon_threadsafe(ping_done.set)
            except RuntimeError:
                # Loop has been closed; nothing more to do.
                return
            # Fast path: ping completes within threshold → loop is healthy.
            if ping_done.wait(timeout=self._threshold_s):
                time.sleep(self._ping_interval_s)
                continue
            # Threshold has passed without the ping firing → block in
            # progress. Capture the main-thread stack BEFORE waiting for
            # the ping to complete, so the stack reflects what was
            # actually running when the loop was stuck.
            stack = _capture_main_stack()
            captured_during_block = not ping_done.is_set()
            # Wait for the ping to eventually fire so we can record the
            # actual block duration. Cap at 10s — if we're still blocked
            # past that, log what we have and move on.
            ping_done.wait(timeout=10.0)
            elapsed_ns = time.monotonic_ns() - sent_ns
            self._log_event(elapsed_ns, stack, captured_during_block)
            time.sleep(self._ping_interval_s)

    def _log_event(
        self,
        elapsed_ns: int,
        stack: list[str],
        captured_during_block: bool,
    ) -> None:
        elapsed_ms = elapsed_ns / 1_000_000.0
        leaf = stack[0] if stack else "<no stack>"
        logger.warning(
            "EVENT LOOP BLOCK: %.1fms — main at %s%s",
            elapsed_ms,
            leaf,
            "" if captured_during_block else " (post-block)",
        )
        try:
            os.makedirs(self._jsonl_dir, exist_ok=True)
            today = date.today().strftime("%Y-%m-%d")
            path = os.path.join(self._jsonl_dir, f"loop_blocks-{today}.jsonl")
            rec = {
                "ts": datetime.now(timezone.utc).isoformat(),
                "elapsed_ms": round(elapsed_ms, 2),
                "main_stack": stack,
                "captured_during_block": captured_during_block,
            }
            with self._jsonl_lock:
                with open(path, "a") as f:
                    f.write(json.dumps(rec) + "\n")
        except Exception as e:
            logger.debug("loop_blocks JSONL emit failed: %s", e)


def maybe_start(
    loop: asyncio.AbstractEventLoop,
    jsonl_dir: str = "/app/logs-paper",
) -> LoopBlockDetector | None:
    """Start the detector unless ``CORSAIR_LOOP_BLOCK_DISABLED=1``.

    Threshold and ping interval are read from env at start time; no
    runtime reconfiguration. Returns the running thread or None.
    """
    if os.environ.get("CORSAIR_LOOP_BLOCK_DISABLED", "").strip() == "1":
        logger.info("Loop-block detector disabled by env.")
        return None
    try:
        threshold = float(os.environ.get("CORSAIR_LOOP_BLOCK_THRESHOLD_MS", "50"))
    except ValueError:
        threshold = 50.0
    try:
        interval = float(os.environ.get("CORSAIR_LOOP_BLOCK_PING_MS", "10"))
    except ValueError:
        interval = 10.0
    detector = LoopBlockDetector(
        loop=loop,
        threshold_ms=threshold,
        ping_interval_ms=interval,
        jsonl_dir=jsonl_dir,
    )
    detector.start()
    logger.info(
        "Loop-block detector started: threshold=%.1fms ping=%.1fms",
        threshold,
        interval,
    )
    return detector
