"""Price-distance distribution on behind_incumbent events.

Requires logging_utils.py 4-decimal precision fix (landed 2026-04-21) to
have been deployed. Prior data is at 2-decimal precision and cannot
resolve 1-tick vs multi-tick gaps.

Reads logs/quotes.csv for the target date. For each row with
skip_reason="behind_incumbent", computes our intended price vs incumbent
price. Distribution (in ticks of 0.0005) decides config vs model:
  - concentrated at 1 tick → lower min_edge_points by 1 tick (config fix)
  - spread across 2-5 ticks → theo is systematically conservative vs
    market (model fix — re-examine SVI wing fits)
  - heavy tail beyond 5 ticks → real edge arbitrage, no fix is free

Usage:
    python scripts/behind_incumbent_distance.py [YYYY-MM-DD]
"""
from __future__ import annotations

import csv
import statistics
import sys
from collections import Counter, defaultdict
from datetime import date
from pathlib import Path


def main():
    repo = Path(__file__).resolve().parents[1]
    d = sys.argv[1] if len(sys.argv) > 1 else date.today().isoformat()
    rotated = repo / f"logs/quotes.{d}.csv"
    path = rotated if rotated.exists() else repo / "logs/quotes.csv"
    if not path.exists():
        print(f"No quotes file at {path}", file=sys.stderr)
        sys.exit(1)

    TICK = 0.0005
    MIN_EDGE = 0.001  # v1.4 §3.4 — min_edge_points on HG; 2 ticks
    rows_total = 0
    rows_behind = 0
    samples_valid = 0
    samples_missing_theo = 0
    samples_precision_2dp = 0
    ticks_all = []
    ticks_by_strike: dict = defaultdict(list)
    ticks_by_hour: dict = defaultdict(list)
    distance_hist = Counter()

    # Key insight (2026-04-22): on behind_incumbent skip rows, `our_price`
    # is empty — the engine doesn't compute our target when it's going to
    # skip. Reconstruct the target from `theo ± min_edge`:
    #   target_BUY  = theo - min_edge  (we'd post below theo to be a buyer)
    #   target_SELL = theo + min_edge  (we'd post above theo to be a seller)
    # `gap = incumbent - target_BUY`  for BUY (positive = incumbent is
    # better bid than our would-be price)
    # `gap = target_SELL - incumbent` for SELL (positive = incumbent is
    # a better ask than our would-be price)
    # Theo comes from the quotes.csv row at 4dp precision (post-fix).
    with open(path) as f:
        rdr = csv.DictReader(f)
        for row in rdr:
            rows_total += 1
            if row.get("skip_reason") != "behind_incumbent":
                continue
            rows_behind += 1
            try:
                theo = float(row["theo"]) if row["theo"] else None
                inc = float(row["incumbent_price"]) if row["incumbent_price"] else None
            except ValueError:
                continue
            if theo is None or inc is None:
                samples_missing_theo += 1
                continue
            # Precision-fix detector: if BOTH theo and inc look 2dp-rounded
            # across the whole column, the writer wasn't updated. Per-row
            # heuristic is noisy (many natural prices land at 2dp by
            # chance), so we count and report.
            if theo == round(theo, 2) and inc == round(inc, 2):
                samples_precision_2dp += 1

            # Target reconstruction
            if row["side"] == "BUY":
                target = theo - MIN_EDGE
                distance = inc - target       # how much better-than-our-target incumbent's BID is
            else:
                target = theo + MIN_EDGE
                distance = target - inc       # how much better-than-our-target incumbent's ASK is
            if distance <= 0:
                # Nonsensical for a behind_incumbent — likely 2dp rounding
                # swamping a sub-tick true gap. Skip (happens when theo
                # ~= incumbent to 2dp precision).
                continue
            t = int(round(distance / TICK))
            samples_valid += 1
            ticks_all.append(t)
            distance_hist[t] += 1
            try:
                strike = round(float(row["strike"]), 2)
                ticks_by_strike[(strike, row["put_call"])].append(t)
            except (TypeError, ValueError):
                pass
            try:
                hour = int(row["timestamp"][11:13])
                ticks_by_hour[hour].append(t)
            except (TypeError, ValueError):
                pass

    print(f"Total rows: {rows_total:,}")
    print(f"Behind-incumbent rows: {rows_behind:,}")
    print(f"Valid reconstructed samples: {samples_valid:,}")
    if samples_missing_theo:
        print(f"  (skipped {samples_missing_theo:,} rows with missing theo/incumbent)")
    if samples_precision_2dp:
        pct = 100 * samples_precision_2dp / max(rows_behind, 1)
        msg = f"  ({samples_precision_2dp:,} rows = {pct:.1f}% have 2dp-aligned theo+inc"
        if pct > 80:
            msg += " — precision fix may not be live!)"
        else:
            msg += " — normal; many natural prices are 2dp-aligned)"
        print(msg)
    if samples_valid == 0:
        print("\nNo valid samples — cannot measure. Check deployment.")
        sys.exit(0)

    print(f"\n=== Distance distribution (ticks, 1 tick = 0.0005 = $12.50) ===")
    total = samples_valid
    for t in sorted(distance_hist.keys()):
        count = distance_hist[t]
        pct = 100 * count / total
        bar = "█" * int(pct / 2)
        print(f"  {t:>3} tick(s): {count:>8,} ({pct:>5.1f}%)  {bar}")

    print(f"\n{'p50':>5}: {statistics.median(ticks_all)} ticks")
    ticks_sorted = sorted(ticks_all)
    def pct_fn(p): return ticks_sorted[min(len(ticks_sorted)-1, int(round(p/100*(len(ticks_sorted)-1))))]
    print(f"{'p90':>5}: {pct_fn(90)} ticks")
    print(f"{'p99':>5}: {pct_fn(99)} ticks")
    print(f"{'max':>5}: {max(ticks_all)} ticks")

    print(f"\n=== Diagnostic read ===")
    p50_t = statistics.median(ticks_all)
    pct_1tick = 100 * distance_hist.get(1, 0) / total
    if p50_t == 1 and pct_1tick >= 60:
        print(f"  Median distance is 1 tick ({pct_1tick:.0f}% of events).")
        print("  → PROBABLE FIX: reduce min_edge_points by 1 tick in "
              "config/hg_v1_4_paper.yaml (0.001 → 0.0005).")
        print("    Expected: ~60% reduction in behind_incumbent rate; "
              "~50% reduction in per-fill edge. Net P&L depends on fill-rate gain.")
    elif p50_t <= 3:
        print(f"  Median distance is {p50_t} ticks — moderate gap.")
        print("  → Likely MODEL issue: our theo is systematically conservative. "
              "Inspect SVI wing fits vs incumbent implied vol.")
    else:
        print(f"  Median distance is {p50_t} ticks — large gap.")
        print("  → STRUCTURAL: incumbents are pricing materially different risk "
              "from our model. Low-hanging config changes won't close this.")

    print(f"\n=== By strike — which strikes have the tightest gap? ===")
    rows_out = []
    for key, ts in ticks_by_strike.items():
        if len(ts) < 50:
            continue
        rows_out.append((key, len(ts), statistics.median(ts),
                         sorted(ts)[int(0.9 * len(ts))]))
    for (strike, cp), n, med, p90 in sorted(rows_out, key=lambda r: r[2]):
        print(f"  {strike:>5.2f}{cp}  N={n:>7,}  p50={med} ticks  p90={p90} ticks")


if __name__ == "__main__":
    main()
