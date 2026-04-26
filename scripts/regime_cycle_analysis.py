"""Regime-conditional cycle p99 — does cycle time balloon during bursts?

Aggregate cycle p99 is only informative if the tail is evenly distributed
across time. If cycles stay fast during calm periods but spike during
forward-price bursts, the burst p99 is the number that matters — those
are the exact moments when latency loses us money.

Sources:
  - logs/quotes.csv — cycle boundaries (via (strike, put_call, side)
    key-repeat detection)
  - logs-paper/sabr_fits-DATE.jsonl — 40Hz forward trajectory for
    classifying cycles as BURST vs CALM

Usage:
    python scripts/regime_cycle_analysis.py [YYYY-MM-DD] \
        [--burst-window-sec=2.0] [--burst-threshold-ticks=2]
"""
from __future__ import annotations

import csv
import json
import sys
from bisect import bisect_right
from datetime import date, datetime, timedelta
from pathlib import Path


def tsm(s: str) -> datetime:
    return datetime.fromisoformat(s).replace(tzinfo=None)


def tsm_z(s: str) -> datetime:
    return datetime.fromisoformat(s.replace("Z", "+00:00")).replace(tzinfo=None)


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, max(0, int(round(p / 100 * (len(xs) - 1)))))]


def main():
    repo = Path(__file__).resolve().parents[1]
    args = sys.argv[1:]
    d = date.today().isoformat()
    burst_win = 2.0
    burst_thr = 2
    for a in args:
        if a.startswith("--burst-window-sec="):
            burst_win = float(a.split("=", 1)[1])
        elif a.startswith("--burst-threshold-ticks="):
            burst_thr = int(a.split("=", 1)[1])
        elif not a.startswith("-"):
            d = a

    sabr = repo / f"logs-paper/sabr_fits-{d}.jsonl"
    rotated = repo / f"logs/quotes.{d}.csv"
    quotes = rotated if rotated.exists() else repo / "logs/quotes.csv"
    if not sabr.exists() or not quotes.exists():
        print(f"Missing input: sabr={sabr.exists()} quotes={quotes.exists()}",
              file=sys.stderr)
        sys.exit(1)

    fwd_ts, fwd_val = [], []
    with open(sabr) as f:
        for ln in f:
            r = json.loads(ln)
            # Dedup to one side per fit — we only need the forward trajectory
            if r.get("side") != "C":
                continue
            fwd_ts.append(tsm_z(r["timestamp_utc"]))
            fwd_val.append(r["forward"])

    def forward_delta_ticks(t_start: datetime, t_end: datetime) -> int:
        i0 = bisect_right(fwd_ts, t_start) - 1
        i1 = bisect_right(fwd_ts, t_end)
        if i0 < 0 or i1 <= i0:
            return 0
        window = fwd_val[i0:i1]
        if not window:
            return 0
        return int(round((max(window) - min(window)) / 0.0005))

    burst_spans: list[float] = []
    calm_spans: list[float] = []
    burst_periods: list[float] = []
    calm_periods: list[float] = []

    seen: set = set()
    cycle_start = None
    cycle_end = None
    prev_cycle_end = None

    # Auto-widen: if peak US (13-20 UTC) has no rows yet (session hasn't
    # reached it), fall back to all hours. The burst/calm split is still
    # meaningful during off-hours, just at lower burst frequency.
    hours_with_data: set = set()
    with open(quotes) as f:
        rdr = csv.DictReader(f)
        for row in rdr:
            try:
                ts = tsm(row["timestamp"])
                hours_with_data.add(ts.hour)
            except ValueError:
                continue
    peak_reached = any(13 <= h <= 20 for h in hours_with_data)
    if peak_reached:
        hours_filter = lambda h: 13 <= h <= 20
        window_label = "peak US 13-20 UTC"
    else:
        hours_filter = lambda h: True
        window_label = (f"ALL HOURS — peak US not yet reached; "
                        f"hours with data: {sorted(hours_with_data)}")
    print(f"Regime-conditional cycle metrics ({window_label})")
    print(f"Burst definition: forward moved ≥{burst_thr} ticks within "
          f"{burst_win}s before cycle start")

    with open(quotes) as f:
        rdr = csv.DictReader(f)
        for row in rdr:
            try:
                ts = tsm(row["timestamp"])
            except ValueError:
                continue
            if not hours_filter(ts.hour):
                continue
            key = (row["strike"], row["put_call"], row["side"])
            if cycle_start is None:
                cycle_start, cycle_end, seen = ts, ts, {key}
                continue
            if key in seen:
                span_ms = (cycle_end - cycle_start).total_seconds() * 1000
                ticks = forward_delta_ticks(
                    cycle_start - timedelta(seconds=burst_win), cycle_start)
                is_burst = ticks >= burst_thr
                (burst_spans if is_burst else calm_spans).append(span_ms)
                if prev_cycle_end is not None:
                    period_ms = (cycle_end - prev_cycle_end).total_seconds() * 1000
                    (burst_periods if is_burst else calm_periods).append(period_ms)
                prev_cycle_end = cycle_end
                cycle_start, cycle_end, seen = ts, ts, {key}
            else:
                cycle_end = ts
                seen.add(key)

    n_burst, n_calm = len(burst_spans), len(calm_spans)
    total = n_burst + n_calm
    print(f"Burst frequency: {100*n_burst/max(total,1):.1f}% of cycles "
          f"(n_burst={n_burst:,}, n_calm={n_calm:,})\n")

    print(f"{'Regime':<8} {'span p50':>10} {'p90':>8} {'p99':>8} {'max':>10}"
          f"  {'period p50':>12} {'p90':>8} {'p99':>8}")
    for label, spans, periods in (("BURST", burst_spans, burst_periods),
                                   ("CALM", calm_spans, calm_periods)):
        if not spans:
            continue
        ps_50 = pct(periods, 50) if periods else 0
        ps_90 = pct(periods, 90) if periods else 0
        ps_99 = pct(periods, 99) if periods else 0
        print(f"{label:<8} {pct(spans,50):>8.1f}ms {pct(spans,90):>6.1f}ms "
              f"{pct(spans,99):>6.1f}ms {max(spans):>8.1f}ms  "
              f"{ps_50:>10.1f}ms {ps_90:>6.1f}ms {ps_99:>6.1f}ms")

    if burst_spans and calm_spans:
        ratio = pct(burst_spans, 99) / max(pct(calm_spans, 99), 1e-9)
        print(f"\nBurst/Calm p99 span ratio: {ratio:.2f}×")
        if ratio > 2.0:
            print("  → Latency DOES balloon during bursts. FIX case rescued: "
                  "the moments that matter are latency-bound.")
        elif ratio > 1.3:
            print("  → Modest burst elevation. FIX helps marginally; "
                  "check other sources of variance.")
        else:
            print("  → Cycle time is stable across regimes. FIX case remains "
                  "weak on this diagnostic; the binding constraint is NOT "
                  "cycle latency.")


if __name__ == "__main__":
    main()
