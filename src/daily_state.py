"""Persistence for daily counters that must survive process restarts.

Spread capture (theo + mid), fill count, daily P&L, and realized P&L
should reflect a running daily total, not just the slice since the most
recent restart. We persist them to data/daily_state.json keyed by the
CME session day. On startup we restore iff the session day still
matches; otherwise we discard and start fresh. The daily reset path
(5pm CT rollover) clears the file as well.
"""

import json
import logging
import os
from datetime import date

logger = logging.getLogger(__name__)

PATH = "data/daily_state.json"
TMP = "data/daily_state.json.tmp"


def save(session_day: date, *, fills_today: int,
         spread_capture_today: float, spread_capture_mid_today: float,
         daily_pnl: float, realized_pnl: float,
         seen_exec_ids: list = None) -> None:
    payload = {
        "session_day": session_day.isoformat(),
        "fills_today": int(fills_today),
        "spread_capture_today": float(spread_capture_today),
        "spread_capture_mid_today": float(spread_capture_mid_today),
        "daily_pnl": float(daily_pnl),
        "realized_pnl": float(realized_pnl),
        # Persist the dedup set so the replay path on the next restart
        # doesn't double-count fills already attributed in a prior process.
        "seen_exec_ids": list(seen_exec_ids) if seen_exec_ids else [],
    }
    try:
        os.makedirs(os.path.dirname(TMP), exist_ok=True)
        with open(TMP, "w") as f:
            json.dump(payload, f)
        os.replace(TMP, PATH)
    except Exception as e:
        logger.warning("daily_state save failed: %s", e)


def load(session_day: date) -> dict | None:
    """Return persisted state iff it matches `session_day`, else None."""
    if not os.path.exists(PATH):
        return None
    try:
        with open(PATH) as f:
            data = json.load(f)
    except Exception as e:
        logger.warning("daily_state load failed: %s", e)
        return None
    if data.get("session_day") != session_day.isoformat():
        logger.info(
            "daily_state: session day mismatch (persisted=%s current=%s); discarding",
            data.get("session_day"), session_day.isoformat(),
        )
        return None
    return data


def clear() -> None:
    try:
        if os.path.exists(PATH):
            os.remove(PATH)
    except Exception as e:
        logger.warning("daily_state clear failed: %s", e)
