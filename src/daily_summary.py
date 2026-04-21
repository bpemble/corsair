"""Daily EOD summary writer (paper-trading interface per hg_spec_v1.3.md §17.4).

Reads the session's fill/skip JSONL log and aggregates counts plus edge,
combines with the final portfolio/margin snapshot, and writes a summary
JSON to ``logs-paper/daily-summary-YYYY-MM-DD.json``. Called from
main.py at CME session rollover (17:00 CT).

Kept deliberately simple: the JSONL is read back instead of maintaining
parallel live counters, because at Stage 1 traffic (~20-30 fills/day)
the file is tiny and the alternative adds wiring to every event path.
"""

import json
import logging
import os
from datetime import date

logger = logging.getLogger(__name__)


def _moneyness_bucket(strike_usd: float, forward_usd: float, put_call: str) -> str:
    """Classify a fill for the ``per_moneyness_fills`` breakdown (§17.4).

    Buckets:
      atm            — within $0.02 of forward
      near_otm_put   — put $0.02-$0.15 OTM
      near_otm_call  — call $0.02-$0.15 OTM
      deep_otm_put   — put > $0.15 OTM (or ITM — treat as deep side)
      deep_otm_call  — call > $0.15 OTM
    """
    if forward_usd <= 0:
        return "atm"
    diff = strike_usd - forward_usd
    if abs(diff) <= 0.02:
        return "atm"
    if put_call == "P":
        return "near_otm_put" if -0.15 <= diff < 0 else "deep_otm_put"
    return "near_otm_call" if 0 < diff <= 0.15 else "deep_otm_call"


def _parse_hxe_symbol(symbol: str) -> tuple[str, float]:
    """Parse 'HXEK6 C490' → ('C', 4.90). Returns ('?', 0.0) on parse error."""
    if not symbol or " " not in symbol:
        return "?", 0.0
    tail = symbol.split(" ", 1)[1]
    if len(tail) < 2:
        return "?", 0.0
    pc = tail[0]
    try:
        strike_usd = int(tail[1:]) / 100.0
    except ValueError:
        return "?", 0.0
    return pc, strike_usd


def _aggregate_jsonl(path: str, multiplier: int) -> dict:
    """Read a paper JSONL file and aggregate fills + skips into counters.

    ``multiplier`` is the contract multiplier ($ per point) used to
    convert per-lb edge to dollar edge.
    """
    agg = {
        "n_fills": 0,
        "n_skips": 0,
        "total_size": 0,
        "gross_edge_usd": 0.0,
        "per_moneyness_fills": {
            "atm": 0, "near_otm_put": 0, "near_otm_call": 0,
            "deep_otm_put": 0, "deep_otm_call": 0,
        },
    }
    if not os.path.exists(path):
        return agg
    with open(path, "r") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except ValueError:
                continue
            et = ev.get("event_type")
            if et == "fill":
                agg["n_fills"] += 1
                size = int(ev.get("size") or 0)
                agg["total_size"] += size
                # Gross edge per contract: sign(side)*(theo - price)*multiplier.
                # For a BUY fill, we want price < theo → positive edge.
                theo = ev.get("theo_at_fill")
                price = ev.get("price")
                side = ev.get("side")
                if theo is not None and price is not None and size > 0:
                    sign = 1 if side == "BUY" else -1
                    agg["gross_edge_usd"] += sign * (theo - price) * size * multiplier
                pc, strike_usd = _parse_hxe_symbol(ev.get("symbol", ""))
                forward = float(ev.get("forward_at_fill") or 0)
                bucket = _moneyness_bucket(strike_usd, forward, pc)
                agg["per_moneyness_fills"][bucket] = (
                    agg["per_moneyness_fills"].get(bucket, 0) + 1)
            elif et == "skip":
                agg["n_skips"] += 1
    agg["gross_edge_usd"] = round(agg["gross_edge_usd"], 2)
    return agg


def write_daily_summary(
    session_date: date,
    paper_log_dir: str,
    portfolio,
    margin_checker,
    multiplier: int,
    margin_extremes: tuple[float, float] = (0.0, 0.0),
    kill_switch_trips: int = 0,
    margin_escape_events: int = 0,
) -> str | None:
    """Write the EOD summary JSON for ``session_date`` per spec §17.4.

    Returns the output path on success, None on failure.
    """
    fills_path = os.path.join(
        paper_log_dir, f"fills-{session_date.isoformat()}.jsonl")
    agg = _aggregate_jsonl(fills_path, multiplier)

    summary = {
        "date": session_date.isoformat(),
        "n_fills": agg["n_fills"],
        "n_skips": agg["n_skips"],
        "total_size": agg["total_size"],
        "gross_edge_usd": agg["gross_edge_usd"],
        "realized_pnl_usd": round(
            float(getattr(portfolio, "spread_capture_today", 0.0)), 2),
        "max_margin_usd": round(float(margin_extremes[0]), 2),
        "min_margin_usd": round(float(margin_extremes[1]), 2),
        "kill_switch_trips": kill_switch_trips,
        "margin_escape_events": margin_escape_events,
        "eod_delta": round(float(getattr(portfolio, "net_delta", 0.0)), 3),
        "eod_theta": round(float(getattr(portfolio, "net_theta", 0.0)), 2),
        "eod_vega": round(float(getattr(portfolio, "net_vega", 0.0)), 2),
        "per_moneyness_fills": agg["per_moneyness_fills"],
    }

    out_path = os.path.join(
        paper_log_dir, f"daily-summary-{session_date.isoformat()}.json")
    try:
        with open(out_path, "w") as f:
            json.dump(summary, f, indent=2, default=str)
        logger.info(
            "paper EOD summary: %s (fills=%d skips=%d edge=$%.0f pnl=$%.0f)",
            out_path, summary["n_fills"], summary["n_skips"],
            summary["gross_edge_usd"], summary["realized_pnl_usd"],
        )
        return out_path
    except OSError as e:
        logger.warning("failed to write paper EOD summary %s: %s", out_path, e)
        return None
