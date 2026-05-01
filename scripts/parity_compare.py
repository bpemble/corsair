"""Parity comparison: trader_decisions JSONL vs broker order_lifecycle JSONL.

Reads both streams over a window and reports drift per (strike, expiry,
right, side) bucket. Used to validate Phase 3 cut-over readiness — if
the trader's decisions match the broker's actual order placements
≥99% of the time over a multi-hour window, the cut-over is safe.

Usage:
    docker run --rm -v $PWD/logs-paper:/logs-paper:ro \\
                  -v $PWD/scripts:/scripts:ro corsair-corsair \\
        python3 /scripts/parity_compare.py --date 2026-05-01 --minutes 60

Outputs:
    Total trader decisions:       N
    Total broker placements:      M
    Decision-action breakdown:    {place: x, skip: y}
    Skip-reason breakdown:        {would_cross_ask: a, no_vol_surface: b, ...}
    Trader-place ↔ broker-place agreement rate: P%
    Drift by (strike, side):      tabulated

Caveats:
    - Trader decides per tick; broker places at its own cadence. Joining
      by exact timestamp won't work — instead we aggregate by
      (strike, expiry, right, side) over the window and compare.
    - During Phase 2 (trader logs only), drift is meaningless because
      trader doesn't actually place; this report is most useful during
      Phase 3 cut-over.
"""
import argparse
import json
import os
import sys
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone


def _parse_ts(s: str) -> datetime:
    """ISO timestamp → datetime; accepts both Z-suffix and +00:00."""
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    return datetime.fromisoformat(s)


def load_trader_decisions(path: str, since: datetime) -> list[dict]:
    out = []
    if not os.path.exists(path):
        return out
    with open(path) as f:
        for line in f:
            try:
                r = json.loads(line)
                ts = _parse_ts(r["recv_ts"])
                if ts < since:
                    continue
                out.append(r)
            except Exception:
                continue
    return out


def load_order_lifecycle(path: str, since: datetime) -> list[dict]:
    out = []
    if not os.path.exists(path):
        return out
    with open(path) as f:
        for line in f:
            try:
                r = json.loads(line)
                ts = _parse_ts(r["timestamp_utc"])
                if ts < since:
                    continue
                out.append(r)
            except Exception:
                continue
    return out


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--date", default=datetime.now(timezone.utc).strftime("%Y-%m-%d"))
    p.add_argument("--minutes", type=int, default=60,
                   help="Look-back window in minutes (default 60)")
    p.add_argument("--logs-dir", default="/app/logs-paper")
    args = p.parse_args()

    since = datetime.now(timezone.utc) - timedelta(minutes=args.minutes)

    trader_decisions = load_trader_decisions(
        f"{args.logs_dir}/trader_decisions-{args.date}.jsonl", since)
    lifecycle = load_order_lifecycle(
        f"{args.logs_dir}/order_lifecycle-{args.date}.jsonl", since)

    print(f"Window: last {args.minutes}min on {args.date}")
    print(f"  Trader decisions: {len(trader_decisions)}")
    print(f"  Broker order_lifecycle events: {len(lifecycle)}")
    print()

    # Decision action / reason breakdown
    actions = Counter()
    reasons_by_action = defaultdict(Counter)
    for r in trader_decisions:
        d = r["decision"]
        actions[d["action"]] += 1
        reasons_by_action[d["action"]][d["reason"]] += 1
    print("Trader action breakdown:")
    for a, n in actions.most_common():
        pct = 100 * n / len(trader_decisions) if trader_decisions else 0
        print(f"  {a:>5}  {n:>6}  ({pct:5.1f}%)")
    print()
    print("Top skip reasons:")
    for r, n in reasons_by_action["skip"].most_common(10):
        print(f"  {r:<30} {n:>6}")
    print()

    # Broker placement events
    placed = [r for r in lifecycle if r.get("event_type") == "placed"]
    cancelled = [r for r in lifecycle if r.get("event_type") == "cancelled"]
    print(f"Broker placed: {len(placed)}, cancelled: {len(cancelled)}")
    print()

    # Per-(strike, side) cross-tabulation
    trader_keys = Counter()
    broker_keys = Counter()
    for r in trader_decisions:
        d = r["decision"]
        if d["action"] == "place":
            k = (round(d["strike"], 2), d["expiry"], d["right"], d["side"])
            trader_keys[k] += 1
    for r in placed:
        try:
            k = (round(float(r["strike"]), 2), r["expiry"], r["right"], r["side"])
            broker_keys[k] += 1
        except (KeyError, TypeError):
            continue

    all_keys = set(trader_keys) | set(broker_keys)
    if not all_keys:
        print("No keys to compare.")
        return 0

    print(f"Per-key drift ({len(all_keys)} unique keys):")
    print(f"  {'STRIKE':>7} {'EXP':>10} {'R':>1} {'SIDE':>4} "
          f"{'TRADER':>7} {'BROKER':>7} {'AGREE':>6}")
    n_agree = 0
    for k in sorted(all_keys):
        t = trader_keys[k]
        b = broker_keys[k]
        agree = "yes" if (t > 0) == (b > 0) else "no"
        if (t > 0) == (b > 0):
            n_agree += 1
        if t == 0 and b == 0:
            continue
        if t == b == 0:
            continue
        print(f"  {k[0]:>7.2f} {k[1]:>10} {k[2]:>1} {k[3]:>4} "
              f"{t:>7} {b:>7} {agree:>6}")

    print()
    pct = 100 * n_agree / len(all_keys)
    print(f"Trader/broker active-side agreement: {n_agree}/{len(all_keys)} ({pct:.1f}%)")
    print()
    if pct < 99:
        print("⚠ Below 99% — cut-over not yet safe.")
        return 1
    print("✓ ≥99% agreement.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
