"""Full-book re-quote cycle latency + deferral diagnostics.

Consumes logs-paper/requote_cycles-YYYY-MM-DD.jsonl (emitted by the
instrumentation added 2026-04-21) and reports:
  1. Distribution of cycle_total_us (p50/p90/p99) — the binding metric for
     whether we stay inside the 30s plateau from the 2026-04-21 analysis
  2. Deferral compounding — does deferred_queue_depth grow across cycles?
  3. Skip-reason breakdown — what fraction of orders_evaluated actually amend
     vs. skip for non-latency reasons (already priced, behind_incumbent, stale)

Usage:
    python scripts/requote_cycle_analysis.py [YYYY-MM-DD]
"""
from __future__ import annotations

import json
import statistics
import sys
from collections import Counter
from datetime import date
from pathlib import Path


def main():
    repo = Path(__file__).resolve().parents[1]
    d = sys.argv[1] if len(sys.argv) > 1 else date.today().isoformat()
    path = repo / f"logs-paper/requote_cycles-{d}.jsonl"
    if not path.exists():
        print(f"No requote_cycles log at {path}", file=sys.stderr)
        print("  (Was the instrumentation deployed? docker compose up -d --build corsair)")
        sys.exit(1)

    rows: list[dict] = []
    with open(path) as f:
        for line in f:
            rows.append(json.loads(line))
    if not rows:
        print("Empty log")
        return

    print(f"Loaded {len(rows)} cycle rows from {path.name}\n")

    # ── 1. Cycle-time distribution ───────────────────────────────────
    totals_us = [r["cycle_total_us"] for r in rows if r.get("cycle_total_us") is not None]
    last_ack = [r["last_ack_us"] for r in rows if r.get("last_ack_us") is not None]
    p50_last = [r["p50_ack_us"] for r in rows if r.get("p50_ack_us") is not None]

    def _pct(xs, p):
        if not xs:
            return None
        xs = sorted(xs)
        k = min(len(xs) - 1, max(0, int(round((p / 100.0) * (len(xs) - 1)))))
        return xs[k]

    print("=== Cycle-time distribution (microseconds) ===")
    print(f"{'metric':<22s} {'p50':>10s} {'p90':>10s} {'p99':>10s} {'max':>10s}")
    for label, xs in (("cycle_total_us", totals_us),
                      ("last_ack_us", last_ack),
                      ("per-order p50_ack", p50_last)):
        if not xs:
            continue
        print(f"{label:<22s} {_pct(xs, 50):>10d} {_pct(xs, 90):>10d} "
              f"{_pct(xs, 99):>10d} {max(xs):>10d}")

    # Convert to seconds for the plateau comparison
    p99_s = (_pct(totals_us, 99) or 0) / 1e6
    p90_s = (_pct(totals_us, 90) or 0) / 1e6
    print(f"\n→ p90 full-book cycle: {p90_s:.2f}s")
    print(f"→ p99 full-book cycle: {p99_s:.2f}s")
    print(f"→ 30s plateau threshold (2026-04-21 analysis): "
          f"{'INSIDE' if p99_s < 30 else 'BREACHED'} at p99")
    print(f"→ 10s headroom target: "
          f"{'INSIDE' if p99_s < 10 else 'BREACHED'} at p99")

    # ── 2. Deferral compounding ──────────────────────────────────────
    print("\n=== Deferral compounding ===")
    depths_end = [r.get("deferred_queue_depth_at_end", 0) for r in rows]
    depths_start = [r.get("deferred_queue_depth_at_start", 0) for r in rows]
    n_with_deferrals = sum(1 for d in depths_end if d > 0)
    print(f"Cycles with deferrals:            {n_with_deferrals} / {len(rows)}  "
          f"({100*n_with_deferrals/len(rows):.1f}%)")
    print(f"Max deferred_queue_depth_at_end:  {max(depths_end)}")
    print(f"Mean deferred_queue_depth_at_end: {statistics.mean(depths_end):.2f}")

    # Consecutive-cycle growth: does end-depth[i] exceed end-depth[i-1]?
    growing_runs = 0
    max_run = 0
    cur = 0
    for i in range(1, len(depths_end)):
        if depths_end[i] > depths_end[i - 1]:
            cur += 1
            growing_runs += 1
            max_run = max(max_run, cur)
        else:
            cur = 0
    print(f"Consecutive-growth runs:          {growing_runs} "
          f"(longest run of monotonic growth: {max_run})")
    if max_run >= 3:
        print("  WARNING: deferrals compounded across ≥3 consecutive cycles — "
              "concurrency cap may be too restrictive for peak load.")
    else:
        print("  Deferrals look transient — cap appears sized for observed load.")

    # ── 3. Skip-reason breakdown ─────────────────────────────────────
    print("\n=== Where do 'orders_evaluated' actually go? ===")
    total_reasons: Counter = Counter()
    total_evaluated = 0
    total_amended = 0
    for r in rows:
        for k, v in (r.get("orders_skipped_reasons") or {}).items():
            total_reasons[k] += v
            total_evaluated += v
        total_amended += r.get("orders_amended", 0)
    print(f"Total evaluations across all cycles: {total_evaluated}")
    print(f"Total amends:                        {total_amended} "
          f"({100*total_amended/max(total_evaluated,1):.1f}% of evals)")
    for reason, count in total_reasons.most_common():
        pct = 100 * count / max(total_evaluated, 1)
        print(f"  {reason:<30s} {count:>8d}  ({pct:>5.1f}%)")

    # ── 4. Trigger breakdown ─────────────────────────────────────────
    print("\n=== Trigger breakdown ===")
    triggers = Counter(r.get("trigger", "?") for r in rows)
    for t, c in triggers.most_common():
        print(f"  {t:<20s} {c:>6d}  ({100*c/len(rows):.1f}%)")

    # ── 5. Forward-move distribution at trigger time ─────────────────
    print("\n=== Forward delta at cycle trigger (ticks) ===")
    deltas = [abs(r.get("forward_delta_ticks", 0)) for r in rows]
    print(f"  p50 |Δ_ticks|: {_pct(deltas, 50)}")
    print(f"  p90 |Δ_ticks|: {_pct(deltas, 90)}")
    print(f"  p99 |Δ_ticks|: {_pct(deltas, 99)}")
    print(f"  max           : {max(deltas) if deltas else 0}")


if __name__ == "__main__":
    main()
