#!/usr/bin/env python3
"""Wire-to-wire latency histogram from logs-paper/wire_timing-YYYY-MM-DD.jsonl.

The broker emits one JSONL row per place_order outcome with five
broker-edge timestamps (CLAUDE.md §16/§17). This script computes the
five stage latencies per order and prints percentile distributions.

Stages (microseconds):
  tick_to_decide   — trader reaction (broker_recv_ns of tick → trader_decide_ts_ns)
  trader_to_broker — outbound IPC (trader_decide_ts_ns → broker_order_recv_ns)
  broker_handle    — broker order construction (broker_order_recv_ns → broker_order_send_ns)
  external_rtt     — gateway + IBKR + CME + reverse (broker_order_send_ns → broker_order_ack_ns)
  total            — full chain (triggering_tick_broker_recv_ns → broker_order_ack_ns)

The "marker" suffix on broker_order_send/ack reflects that they're
captured at the handle_place call boundary, not literally at the TCP
write/read inside place_order. ~30 µs of place_order setup overhead
is lumped into broker_handle.

Usage:
    python3 scripts/wire_to_wire_histogram.py [--date YYYY-MM-DD] [--logs-dir DIR] [--plot]

Without --date, picks the most-recent wire_timing-*.jsonl in the logs dir.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import statistics
import sys
from pathlib import Path


STAGES = [
    "tick_to_decide",
    "trader_to_broker",
    "broker_handle",
    "external_rtt",
    "total",
]


def compute_stages(row: dict) -> dict | None:
    """Return per-stage microseconds, or None if a required ts is missing.

    All inputs are nanoseconds; outputs are microseconds (float).
    Skips rows where the triggering tick wasn't captured (e.g. boot
    orders without a tick context)."""
    tick_ns = row.get("triggering_tick_broker_recv_ns")
    decide_ns = row.get("trader_decide_ts_ns")
    recv_ns = row.get("broker_order_recv_ns")
    send_ns = row.get("broker_order_send_ns")
    ack_ns = row.get("broker_order_ack_ns")

    if None in (decide_ns, recv_ns, send_ns, ack_ns):
        return None

    out = {
        "trader_to_broker": (recv_ns - decide_ns) / 1_000.0,
        "broker_handle": (send_ns - recv_ns) / 1_000.0,
        "external_rtt": (ack_ns - send_ns) / 1_000.0,
    }
    if tick_ns is not None:
        out["tick_to_decide"] = (decide_ns - tick_ns) / 1_000.0
        out["total"] = (ack_ns - tick_ns) / 1_000.0
    else:
        out["tick_to_decide"] = None
        out["total"] = None
    return out


def percentiles(samples: list[float], pcts=(50, 75, 90, 95, 99, 99.9)) -> dict:
    if not samples:
        return {f"p{p}": None for p in pcts}
    samples = sorted(samples)
    n = len(samples)
    out = {}
    for p in pcts:
        idx = max(0, min(n - 1, math.ceil(n * p / 100) - 1))
        out[f"p{p}"] = samples[idx]
    return out


def fmt_us(v: float | None) -> str:
    if v is None:
        return "—"
    if v >= 1_000_000:
        return f"{v / 1_000_000:.2f}s"
    if v >= 1_000:
        return f"{v / 1_000:.2f}ms"
    return f"{v:.1f}µs"


def find_default_log(logs_dir: Path) -> Path | None:
    matches = sorted(logs_dir.glob("wire_timing-*.jsonl"))
    return matches[-1] if matches else None


def load_rows(path: Path) -> list[dict]:
    rows = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--date", help="YYYY-MM-DD; defaults to latest available")
    ap.add_argument(
        "--logs-dir",
        default=os.environ.get("CORSAIR_LOGS_DIR", "logs-paper"),
        help="Where wire_timing-*.jsonl lives (default logs-paper/)",
    )
    ap.add_argument("--plot", action="store_true", help="ASCII histogram per stage")
    ap.add_argument(
        "--outcome",
        choices=["all", "ack", "rejected"],
        default="ack",
        help="Filter by place outcome (default: ack only)",
    )
    args = ap.parse_args()

    logs_dir = Path(args.logs_dir)
    if not logs_dir.is_absolute() and not logs_dir.exists():
        # Fall back to project-rooted path when run from anywhere.
        logs_dir = Path(__file__).resolve().parent.parent / args.logs_dir

    if args.date:
        path = logs_dir / f"wire_timing-{args.date}.jsonl"
    else:
        path = find_default_log(logs_dir)
        if path is None:
            print(f"No wire_timing-*.jsonl in {logs_dir}", file=sys.stderr)
            sys.exit(1)

    if not path.exists():
        print(f"Not found: {path}", file=sys.stderr)
        sys.exit(1)

    rows = load_rows(path)
    if args.outcome != "all":
        rows = [r for r in rows if r.get("outcome") == args.outcome]

    print(f"# wire-to-wire latency from {path}")
    print(f"# total rows: {len(rows)}")

    by_stage: dict[str, list[float]] = {s: [] for s in STAGES}
    rows_with_tick = 0
    rows_without_tick = 0
    for r in rows:
        stages = compute_stages(r)
        if stages is None:
            continue
        for s in STAGES:
            v = stages.get(s)
            if v is not None and v > 0:
                by_stage[s].append(v)
        if stages.get("tick_to_decide") is not None:
            rows_with_tick += 1
        else:
            rows_without_tick += 1

    print(f"# rows with triggering tick: {rows_with_tick}")
    print(f"# rows w/o triggering tick:  {rows_without_tick} (boot/cancel-replace)")
    print()

    # Header
    pcts = (50, 75, 90, 95, 99, 99.9)
    header = f"{'stage':<22}{'n':>7}  {'min':>9}  {'mean':>9}"
    for p in pcts:
        header += f"  {f'p{p}':>9}"
    header += f"  {'max':>9}"
    print(header)
    print("-" * len(header))
    for s in STAGES:
        samples = by_stage[s]
        n = len(samples)
        if n == 0:
            print(f"{s:<22}{n:>7}  {'—':>9}  {'—':>9}" + "  ".join(f"{'—':>9}" for _ in pcts) + f"  {'—':>9}")
            continue
        pct = percentiles(samples, pcts)
        line = f"{s:<22}{n:>7}  {fmt_us(min(samples)):>9}  {fmt_us(statistics.fmean(samples)):>9}"
        for p in pcts:
            line += f"  {fmt_us(pct[f'p{p}']):>9}"
        line += f"  {fmt_us(max(samples)):>9}"
        print(line)

    if args.plot:
        print()
        for s in STAGES:
            samples = by_stage[s]
            if not samples:
                continue
            print(f"\n{s} (µs):")
            ascii_hist(samples)


def ascii_hist(samples: list[float], bins: int = 20, width: int = 60):
    """ASCII histogram. Log-bins so a long tail doesn't crush detail."""
    if not samples:
        return
    smin, smax = min(samples), max(samples)
    if smin <= 0:
        smin = max(0.1, smin)
    if smax <= smin:
        smax = smin * 2
    log_min = math.log10(smin)
    log_max = math.log10(smax)
    edges = [10 ** (log_min + i * (log_max - log_min) / bins) for i in range(bins + 1)]
    counts = [0] * bins
    for v in samples:
        if v <= 0:
            continue
        for i in range(bins):
            if v <= edges[i + 1]:
                counts[i] += 1
                break
    cmax = max(counts) or 1
    for i in range(bins):
        lo = fmt_us(edges[i])
        hi = fmt_us(edges[i + 1])
        bar_len = int(counts[i] * width / cmax)
        bar = "#" * bar_len
        print(f"  [{lo:>9} – {hi:>9}] {counts[i]:>6}  {bar}")


if __name__ == "__main__":
    main()
