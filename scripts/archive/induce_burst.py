#!/usr/bin/env python3
"""Inject a synthetic burst into corsair's FillBurstTracker for Stage 0
verification (Thread 3 deployment runbook Phase 1).

Writes a sentinel JSON file that the running corsair process picks up
on the next main-loop iteration and replays through the existing
`note_layer_c_fill` path. The sentinel auto-deletes after replay.

Patterns:
  c2     5 mixed-side fills 100ms apart on the same strike. Crosses
         K2=3 at fill 3 → C2 trigger fires (+ hedge drain). Default.
  c1     5 same-side (SELL) fills 100ms apart. Crosses K1=2 at fill 2
         → C1 trigger fires (no global cancel; per-side cooldown only).
  custom Reads JSON from --file. Each entry must have keys:
           strike, expiry, right, side
         Optional: ts_offset_ms (default = index * 100).

Default strike/expiry are HG paper sub-account values; override via env
or --file for ETH/different products.

Usage:
    # From inside corsair container:
    docker compose exec corsair python scripts/induce_burst.py
    docker compose exec corsair python scripts/induce_burst.py --pattern c1

    # Custom payload:
    docker compose exec corsair python scripts/induce_burst.py \\
        --pattern custom --file /tmp/my_burst.json

After firing, verify in `logs-paper/`:
  - burst_events-YYYY-MM-DD.jsonl: row(s) with trigger=C1/C2,
    action_fired=true (assuming flags ON), pulled_order_ids populated.
  - hedge_trades-YYYY-MM-DD.jsonl: rows with reason starting "priority_drain"
    in the cooldown window (assuming hedging.mode=execute and book has
    delta to drain).
  - corsair logs: "STAGE 0 BURST INJECTION: replayed N/N synthetic fills"
"""
import argparse
import json
import os
import sys


SENTINEL_NAME = "corsair_inject_burst"
DEFAULT_STRIKE = 6.10
DEFAULT_EXPIRY = "20260526"
DEFAULT_RIGHT = "C"


def _build_pattern(pattern: str) -> list:
    """Return a list of fill records for the named pattern."""
    if pattern == "c2":
        # Mixed sides: alternating BUY/SELL on same strike, 100ms apart.
        # Same-side count maxes at 3 (BUYs), any-side count hits 5.
        return [
            {"strike": DEFAULT_STRIKE, "expiry": DEFAULT_EXPIRY,
             "right": DEFAULT_RIGHT, "side": s, "ts_offset_ms": i * 100}
            for i, s in enumerate(["BUY", "SELL", "BUY", "SELL", "BUY"])
        ]
    if pattern == "c1":
        # All same-side, 100ms apart. Same-side count = any-side count
        # = 5 by the end. K1 crosses at fill 2.
        return [
            {"strike": DEFAULT_STRIKE, "expiry": DEFAULT_EXPIRY,
             "right": DEFAULT_RIGHT, "side": "SELL", "ts_offset_ms": i * 100}
            for i in range(5)
        ]
    raise ValueError(f"unknown pattern: {pattern}")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Inject a synthetic burst for Thread 3 Stage 0 "
                    "hedge-drain verification.")
    parser.add_argument(
        "--pattern", choices=["c1", "c2", "custom"], default="c2",
        help="Burst pattern shape. c2 = mixed sides crosses K2=3 at fill 3 "
             "(default); c1 = same-side crosses K1=2 at fill 2; custom "
             "reads from --file.")
    parser.add_argument(
        "--file", help="JSON file with custom fill records (only with "
                       "--pattern custom). Each record must have strike, "
                       "expiry, right, side; ts_offset_ms is optional.")
    parser.add_argument(
        "--dir", default=os.environ.get("CORSAIR_INDUCE_DIR", "/tmp"),
        help="Sentinel directory. Must match BURST_INJECT_SENTINEL_DIR "
             "in src/quote_engine.py (env: CORSAIR_INDUCE_DIR).")
    args = parser.parse_args()

    if args.pattern == "custom":
        if not args.file:
            print("ERROR: --pattern custom requires --file", file=sys.stderr)
            return 1
        try:
            with open(args.file) as fh:
                payload = json.load(fh)
        except Exception as e:
            print(f"ERROR: failed to read --file {args.file}: {e}",
                  file=sys.stderr)
            return 1
        if not isinstance(payload, list) or not payload:
            print(f"ERROR: --file {args.file} must contain a non-empty "
                  "JSON array", file=sys.stderr)
            return 1
    else:
        payload = _build_pattern(args.pattern)

    path = os.path.join(args.dir, SENTINEL_NAME)
    try:
        with open(path, "w") as fh:
            json.dump(payload, fh)
    except OSError as e:
        print(f"ERROR: failed to write sentinel {path}: {e}", file=sys.stderr)
        return 1

    print(f"Sentinel written: {path}")
    print(f"Pattern: {args.pattern} ({len(payload)} fills)")
    print(f"First record: {payload[0]}")
    print(f"Last record:  {payload[-1]}")
    print("Watch for:")
    print("  - 'STAGE 0 BURST INJECTION: replayed N/N synthetic fills' "
          "in corsair logs")
    print("  - logs-paper/burst_events-<today>.jsonl rows")
    print("  - logs-paper/hedge_trades-<today>.jsonl rows with reason "
          "starting 'priority_drain' (if flags ON + book has delta)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
