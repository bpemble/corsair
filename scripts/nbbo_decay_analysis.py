"""NBBO-based edge-decay analysis.

Cross-checks the SVI-based 48s half-life measurement by computing the same
trajectory using raw NBBO midpoints from logs/quotes.csv. Requires the
logging_utils.py precision fix landed 2026-04-21 — prior data was rounded
to 2 decimals (20 ticks) and cannot resolve sub-cent moves.

Usage:
    python scripts/nbbo_decay_analysis.py [YYYY-MM-DD]

Defaults to today. Reads logs-paper/fills-DATE.jsonl and logs/quotes.csv
(current day only — rotated files are named quotes.YYYY-MM-DD.csv).
"""
from __future__ import annotations

import csv
import json
import re
import statistics
import sys
from bisect import bisect_right
from collections import defaultdict
from datetime import datetime, date, timedelta
from pathlib import Path


def parse_iso(s: str) -> datetime:
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def parse_strike_from_symbol(sym: str) -> tuple[float, bool]:
    # "HXEJ6 C605" -> (6.05, is_call=True)
    m = re.search(r"([CP])(\d+)", sym)
    if not m:
        raise ValueError(f"can't parse symbol: {sym}")
    return float(m.group(2)) / 100.0, m.group(1) == "C"


def load_fills(path: Path) -> list[dict]:
    fills = []
    with open(path) as f:
        for line in f:
            fills.append(json.loads(line))
    return fills


def load_quotes_series(
    quotes_path: Path, strikes_of_interest: set[tuple[float, str]],
) -> dict[tuple[float, str, str], list[tuple[datetime, float]]]:
    """Return {(strike, put_call, side): [(ts, incumbent_price), ...]}.
    side is "BUY" (market bid) or "SELL" (market ask). Stream-processed so
    we don't materialize the whole CSV."""
    series: dict = defaultdict(list)
    wanted_strikes = {(round(K, 4), cp) for K, cp in strikes_of_interest}
    with open(quotes_path) as f:
        rdr = csv.DictReader(f)
        for row in rdr:
            try:
                strike = round(float(row["strike"]), 4)
            except (TypeError, ValueError):
                continue
            cp = row["put_call"]
            if (strike, cp) not in wanted_strikes:
                continue
            inc = row.get("incumbent_price")
            if not inc:
                continue
            try:
                inc_f = float(inc)
            except ValueError:
                continue
            ts = parse_iso(row["timestamp"])
            series[(strike, cp, row["side"])].append((ts, inc_f))
    return series


def mid_at(series_buy: list, series_sell: list, tgt_ts: datetime,
           ) -> float | None:
    """Return (bid+ask)/2 using the latest BUY-side incumbent (= best bid)
    and SELL-side incumbent (= best ask) ≤ tgt_ts."""
    def _latest(ser: list[tuple[datetime, float]], t: datetime):
        if not ser:
            return None
        times = [r[0] for r in ser]
        i = bisect_right(times, t) - 1
        return ser[i][1] if i >= 0 else None

    b = _latest(series_buy, tgt_ts)
    a = _latest(series_sell, tgt_ts)
    if b is None or a is None:
        return None
    return (b + a) / 2.0


def main():
    repo = Path(__file__).resolve().parents[1]
    d = sys.argv[1] if len(sys.argv) > 1 else date.today().isoformat()

    fills_path = repo / f"logs-paper/fills-{d}.jsonl"
    quotes_path = repo / "logs/quotes.csv"  # current-day is un-rotated
    rotated = repo / f"logs/quotes.{d}.csv"
    if rotated.exists():
        quotes_path = rotated

    if not fills_path.exists():
        print(f"No fills file at {fills_path}", file=sys.stderr)
        sys.exit(1)
    if not quotes_path.exists():
        print(f"No quotes file at {quotes_path}", file=sys.stderr)
        sys.exit(1)

    fills = load_fills(fills_path)
    if not fills:
        print("Empty fills file"); return

    # Parse strikes we care about (and confirm they look like 4-decimal prices
    # post logging_utils fix)
    strikes_of_interest: set[tuple[float, str]] = set()
    for f in fills:
        sym = f["symbol"].split()[-1] if " " in f["symbol"] else f["symbol"]
        K, is_call = parse_strike_from_symbol(sym)
        strikes_of_interest.add((K, "C" if is_call else "P"))

    print(f"Loading quotes.csv (strikes of interest: {len(strikes_of_interest)})...")
    series = load_quotes_series(quotes_path, strikes_of_interest)
    if not series:
        print("No rows for any fill strike in quotes.csv — check precision/format")
        return
    samples = next(iter(series.values()))[:5]
    print(f"Sample incumbent_price values: {[round(s[1], 6) for s in samples]}")
    if all(s[1] == round(s[1], 2) for s in samples):
        print("WARNING: quotes.csv still appears 2-decimal rounded. "
              "Did logging_utils.py precision fix land in the running image?")
        print("(Continuing — but sub-cent moves will be invisible.)")

    # Base configs
    MULT = 25000.0  # HG contract multiplier
    HORIZONS_SEC = [0, 1, 2, 5, 10, 15, 20, 30, 45, 60, 90, 120, 180, 300]

    # Per-fill: compute edge(t) = sgn * (mid(t) - px) for each horizon
    per_fill_curve: list[dict] = []
    for i, f in enumerate(fills, 1):
        sym = f["symbol"].split()[-1]
        K, is_call = parse_strike_from_symbol(sym)
        cp = "C" if is_call else "P"
        sgn = 1 if f["side"] == "BUY" else -1
        px = float(f["price"])
        t0 = parse_iso(f["ts"])
        sb = series.get((round(K, 4), cp, "BUY"), [])
        ss = series.get((round(K, 4), cp, "SELL"), [])
        row = {"idx": i, "sym": sym, "side": f["side"], "px": px, "K": K,
               "cp": cp, "edge": {}}
        m0 = mid_at(sb, ss, t0)
        if m0 is None:
            continue
        edge0 = sgn * (m0 - px) * MULT
        row["edge0"] = edge0
        for h in HORIZONS_SEC:
            mh = mid_at(sb, ss, t0 + timedelta(seconds=h))
            if mh is None:
                continue
            row["edge"][h] = sgn * (mh - px) * MULT
        per_fill_curve.append(row)

    # Aggregate: average $ edge at each horizon, and normalized decay curve
    agg_dollars: dict[int, list[float]] = defaultdict(list)
    agg_norm: dict[int, list[float]] = defaultdict(list)
    for row in per_fill_curve:
        e0 = row.get("edge0")
        if e0 is None or e0 <= 0:
            continue
        for h, e in row["edge"].items():
            agg_dollars[h].append(e)
            agg_norm[h].append(e / e0)

    print(f"\n=== NBBO-based edge trajectory (N={len(per_fill_curve)} fills, "
          f"{sum(1 for r in per_fill_curve if r.get('edge0', 0) > 0)} with edge0>0) ===")
    print(f"{'t':>6s}  {'mean_$':>9s}  {'median_$':>9s}  {'mean_norm':>10s}  {'N':>4s}")
    for h in HORIZONS_SEC:
        xs = agg_dollars.get(h, [])
        ns = agg_norm.get(h, [])
        if not xs:
            continue
        print(f"{h:>5d}s  ${statistics.mean(xs):>+7.2f}  "
              f"${statistics.median(xs):>+7.2f}  {statistics.mean(ns):>+10.3f}  "
              f"{len(xs):>4d}")

    # Find half-life of mean normalized edge
    means_norm = [(h, statistics.mean(agg_norm.get(h, []) or [float("nan")]))
                  for h in HORIZONS_SEC]
    half_life = None
    for i in range(1, len(means_norm)):
        h0, v0 = means_norm[i - 1]
        h1, v1 = means_norm[i]
        if v0 > 0.5 >= v1:
            # linear interp
            frac = (v0 - 0.5) / (v0 - v1) if v0 != v1 else 0.0
            half_life = h0 + frac * (h1 - h0)
            break
    print(f"\nNBBO-based half-life: "
          f"{half_life:.1f}s" if half_life else "\nNBBO half-life: did not cross 0.5 within window")

    print("\nCompare this number to the 48s SVI-based half-life measured 2026-04-21.")
    print("  - If NBBO half-life << 48s  → SVI lag was masking real decay; drift is faster than we thought")
    print("  - If NBBO half-life ~= 48s  → SVI is tracking NBBO in real time; 48s is a true property of the market")
    print("  - If NBBO half-life >> 48s  → SVI is overshooting drift (unlikely)")


if __name__ == "__main__":
    main()
