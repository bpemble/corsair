"""Compare a baseline session vs min_edge-reduced experiment session.

Core question: does reducing min_edge from 0.001 → 0.0005 (2 ticks → 1 tick)
produce more fills at roughly the same net edge capture, or does it
expose us to adverse selection that eats the apparent gain?

Measures four things per session:
  1. Fill rate (fills/hour during peak US)
  2. Median edge at fill (SVI-based, as measured 2026-04-21)
  3. Median edge at t+60s post-fill (the adverse-decay cliff point)
  4. Adverse-decay ratio = 1 - (edge@60s / edge@fill)
     — normalized per-fill adverse selection rate

Net-P&L read is the product of #1 and #3. Adverse-selection read is #4
alone. If #1 grows but #3 shrinks proportionally, fill rate is illusory.
If #4 grows, we're catching more informed flow at the tighter edge.

Usage:
    python scripts/min_edge_experiment_comparison.py BASELINE_DATE EXPERIMENT_DATE

e.g.:
    python scripts/min_edge_experiment_comparison.py 2026-04-21 2026-04-22
"""
from __future__ import annotations

import json
import math
import re
import statistics
import sys
from bisect import bisect_right
from datetime import datetime, timedelta
from pathlib import Path
from statistics import NormalDist


N = NormalDist().cdf
TICK = 0.0005
MULT = 25000.0  # HG


def tsm_z(s): return datetime.fromisoformat(s.replace("Z", "+00:00"))


def svi_var(p, k):
    return p["a"] + p["b"] * (p["rho"] * (k - p["m"])
                              + math.sqrt((k - p["m"]) ** 2 + p["sigma"] ** 2))


def bs(F, K, iv, tte, is_call):
    if iv <= 0 or tte <= 0:
        return max(0, F - K) if is_call else max(0, K - F)
    sqt = iv * math.sqrt(tte)
    d1 = (math.log(F / K) + 0.5 * iv * iv * tte) / sqt
    d2 = d1 - sqt
    return F * N(d1) - K * N(d2) if is_call else K * N(-d2) - F * N(-d1)


def theo_from_fit(fit, K, is_call):
    F, tte = fit["forward"], fit["tte_years"]
    w = svi_var(fit["params"], math.log(K / F))
    return bs(F, K, math.sqrt(max(w, 1e-12) / tte), tte, is_call)


def parse_strike(sym: str):
    m = re.search(r"([CP])(\d+)", sym)
    return float(m.group(2)) / 100.0, m.group(1) == "C"


def analyze(date_str: str, expiry: str = "20260427"):
    """Return per-session summary dict."""
    repo = Path(__file__).resolve().parents[1]
    fills_path = repo / f"logs-paper/fills-{date_str}.jsonl"
    sabr_path = repo / f"logs-paper/sabr_fits-{date_str}.jsonl"
    if not fills_path.exists() or not sabr_path.exists():
        return {"date": date_str, "error": "missing data"}

    fills = []
    with open(fills_path) as f:
        for ln in f:
            fills.append(json.loads(ln))

    fits_C, fits_P = [], []
    with open(sabr_path) as f:
        for ln in f:
            r = json.loads(ln)
            if r["expiry"] != expiry:
                continue
            (fits_C if r["side"] == "C" else fits_P).append(
                (tsm_z(r["timestamp_utc"]), r))
    fits_C.sort(key=lambda x: x[0])
    fits_P.sort(key=lambda x: x[0])
    tsC = [t for t, _ in fits_C]
    tsP = [t for t, _ in fits_P]

    def fit_at(t, is_call):
        arr_t = tsC if is_call else tsP
        arr = fits_C if is_call else fits_P
        i = bisect_right(arr_t, t) - 1
        return arr[i][1] if i >= 0 else None

    edges_at_fill: list[float] = []
    edges_at_60s: list[float] = []
    decays_norm: list[float] = []
    peak_fills = 0

    for fl in fills:
        sym = fl["symbol"].split()[-1] if " " in fl["symbol"] else fl["symbol"]
        K, is_call = parse_strike(sym)
        sgn = 1 if fl["side"] == "BUY" else -1
        px = float(fl["price"])
        t0 = tsm_z(fl["ts"])
        if 13 <= t0.hour <= 20:
            peak_fills += 1
        f0 = fit_at(t0, is_call)
        f60 = fit_at(t0 + timedelta(seconds=60), is_call)
        if f0 is None or f60 is None:
            continue
        e0 = sgn * (theo_from_fit(f0, K, is_call) - px) * MULT
        e60 = sgn * (theo_from_fit(f60, K, is_call) - px) * MULT
        if e0 <= 0:
            continue
        edges_at_fill.append(e0)
        edges_at_60s.append(e60)
        decays_norm.append(1.0 - (e60 / e0))

    return {
        "date": date_str,
        "fills_total": len(fills),
        "fills_peak_us": peak_fills,
        "edges_at_fill": edges_at_fill,
        "edges_at_60s": edges_at_60s,
        "decays_norm": decays_norm,
    }


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    return xs[min(len(xs) - 1, max(0, int(round(p / 100 * (len(xs) - 1)))))]


def summarize(row: dict) -> None:
    if row.get("error"):
        print(f"  {row['date']}: {row['error']}")
        return
    n_fill = row["fills_total"]
    n_peak = row["fills_peak_us"]
    e0 = row["edges_at_fill"]
    e60 = row["edges_at_60s"]
    dn = row["decays_norm"]
    if not e0:
        print(f"  {row['date']}: no scorable fills")
        return
    print(f"  {row['date']}:")
    print(f"    fills total / peak:        {n_fill} / {n_peak}")
    print(f"    median edge at fill:       ${statistics.median(e0):+6.2f}")
    print(f"    median edge at t+60s:      ${statistics.median(e60):+6.2f}")
    print(f"    median adverse decay:      {statistics.median(dn):+6.3f}  "
          f"(1.0 = full edge gone)")
    print(f"    mean  adverse decay:       {statistics.mean(dn):+6.3f}")
    pos_survivors = sum(1 for v in e60 if v > 0) / len(e60)
    print(f"    fraction still +edge@60s:  {pos_survivors:.1%}")


def main():
    if len(sys.argv) < 3:
        print("Usage: min_edge_experiment_comparison.py BASELINE_DATE EXPERIMENT_DATE")
        sys.exit(1)
    baseline = analyze(sys.argv[1])
    experiment = analyze(sys.argv[2])
    print("\n=== Session summary ===\n")
    summarize(baseline)
    summarize(experiment)

    if baseline.get("edges_at_fill") and experiment.get("edges_at_fill"):
        print("\n=== Delta (experiment vs baseline) ===")
        bf, ef = baseline["fills_peak_us"], experiment["fills_peak_us"]
        be0, ee0 = (statistics.median(baseline["edges_at_fill"]),
                    statistics.median(experiment["edges_at_fill"]))
        bd, ed = (statistics.median(baseline["decays_norm"]),
                  statistics.median(experiment["decays_norm"]))
        print(f"  peak fills:   {bf} → {ef}   ({(ef/max(bf,1)-1)*100:+.1f}%)")
        print(f"  edge@fill:    ${be0:+.2f} → ${ee0:+.2f}   "
              f"({(ee0/max(be0,1e-9)-1)*100:+.1f}%)")
        print(f"  adverse decay: {bd:.3f} → {ed:.3f}   (absolute change {ed-bd:+.3f})")

        # Decision guidance
        print("\n=== Read ===")
        if ef > bf * 1.3 and (ed - bd) < 0.15 and ee0 > be0 * 0.4:
            print("  → Experiment validates 1-tick min_edge. Config change worth keeping.")
            print("    Amend spec §3.4 with the empirical justification.")
        elif (ed - bd) >= 0.15:
            print("  → Adverse-selection rate worsened meaningfully.")
            print("    Lower min_edge catches more informed flow. Revert to 0.001.")
            print("    Look instead at model-side improvements (SVI wing calibration).")
        elif ef < bf * 1.2:
            print("  → Fill rate barely moved. The behind_incumbent gap is wider than 1 tick.")
            print("    Revert. Price-distance diagnostic should show median >1 tick.")
        else:
            print("  → Mixed signal. Run another experiment session to build sample size.")


if __name__ == "__main__":
    main()
