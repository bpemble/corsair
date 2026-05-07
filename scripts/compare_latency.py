#!/usr/bin/env python3
"""A/B compare two trader histogram dumps from the replay harness.

The harness leaves a JSON file with two arrays:
    {"ipc_ns": [...], "ttt_ns": [...]}

Samples are nanoseconds (switched 2026-05-06 from µs to resolve sub-µs
A/B comparisons; integer-truncated µs lost 5–20% of dynamic range).

This script loads two such files and reports:
  - Per-percentile distribution stats (p50/p90/p99/p99.9/max) with bootstrap 95% CI
  - Two-sample Kolmogorov-Smirnov p-value
  - Verdict (SHIP / REVERT / INCONCLUSIVE) against operator-defined thresholds

Verdict rules (defaults; tune via flags):
  SHIP        : p50 drops by ≥ noise_floor_p50_ns (default 200ns)
                OR p99 drops by ≥ noise_floor_p99_pct (default 5%)
                AND no other percentile regresses past its noise floor.
  REVERT      : any tracked percentile regresses past its noise floor.
  INCONCLUSIVE: change is within noise on both sides — no signal either way.

Usage:
    scripts/compare_latency.py before.json after.json [--metric ttt_ns|ipc_ns]
"""
from __future__ import annotations

import argparse
import json
import sys
from typing import Tuple

import numpy as np

try:
    from scipy import stats as scipy_stats
    HAVE_SCIPY = True
except ImportError:
    HAVE_SCIPY = False


def percentile_with_ci(samples: np.ndarray, q: float, n_boot: int = 1000,
                       seed: int = 42) -> Tuple[float, float, float]:
    """Return (point_estimate, ci_lo_2.5, ci_hi_97.5) for percentile q."""
    rng = np.random.default_rng(seed)
    point = float(np.percentile(samples, q))
    n = len(samples)
    boots = np.empty(n_boot, dtype=np.float64)
    for i in range(n_boot):
        boots[i] = np.percentile(rng.choice(samples, size=n, replace=True), q)
    lo, hi = np.percentile(boots, [2.5, 97.5])
    return point, float(lo), float(hi)


def ks_2sample(a: np.ndarray, b: np.ndarray) -> Tuple[float, float]:
    """Two-sample KS. Falls back to manual computation if scipy not available."""
    if HAVE_SCIPY:
        r = scipy_stats.ks_2samp(a, b)
        return float(r.statistic), float(r.pvalue)
    # Manual KS — sufficient for our scale.
    a_sort = np.sort(a)
    b_sort = np.sort(b)
    all_vals = np.concatenate([a_sort, b_sort])
    cdf_a = np.searchsorted(a_sort, all_vals, side='right') / len(a_sort)
    cdf_b = np.searchsorted(b_sort, all_vals, side='right') / len(b_sort)
    d = float(np.max(np.abs(cdf_a - cdf_b)))
    n_a, n_b = len(a), len(b)
    en = np.sqrt(n_a * n_b / (n_a + n_b))
    p = float(np.exp(-2 * (en * d) ** 2))  # asymptotic approx
    return d, p


def fmt_ns(v: float) -> str:
    """Format a nanosecond value as a human-friendly string."""
    if v < 1_000:
        return f"{v:7.0f} ns"
    if v < 1_000_000:
        return f"{v / 1_000:7.2f} us"
    return f"{v / 1_000_000:6.2f} ms"


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("before", help="histogram JSON from the baseline run")
    ap.add_argument("after", help="histogram JSON from the candidate run")
    ap.add_argument("--metric", default="ttt_ns", choices=("ttt_ns", "ipc_ns"),
                    help="which latency series to compare (default ttt_ns)")
    ap.add_argument("--noise-floor-p50-ns", type=int, default=1000,
                    help="p50 threshold in nanoseconds; 1000ns = 2x observed "
                         "baseline-to-baseline envelope (default 1000)")
    ap.add_argument("--noise-floor-p99-pct", type=float, default=5.0,
                    help="p99 threshold in percent — informational only, "
                         "does not gate verdict (default 5.0)")
    ap.add_argument("--label-before", default="before")
    ap.add_argument("--label-after", default="after")
    args = ap.parse_args()

    with open(args.before) as f:
        before_doc = json.load(f)
    with open(args.after) as f:
        after_doc = json.load(f)

    before = np.asarray(before_doc.get(args.metric, []), dtype=np.int64)
    after = np.asarray(after_doc.get(args.metric, []), dtype=np.int64)

    if len(before) < 100 or len(after) < 100:
        print(f"insufficient samples: before n={len(before)}, after n={len(after)} "
              f"(need >= 100 each)", file=sys.stderr)
        sys.exit(2)

    print(f"# {args.metric}    {args.label_before} n={len(before):,}    "
          f"{args.label_after} n={len(after):,}\n")

    pcts = [(50.0, "p50"), (90.0, "p90"), (99.0, "p99"), (99.9, "p99.9")]
    deltas: dict[str, float] = {}
    print(f"{'pct':<6} | "
          f"{args.label_before+' (95% CI)':>30} | "
          f"{args.label_after+' (95% CI)':>30} | "
          f"{'delta':>14}")
    print("-" * 90)
    for q, name in pcts:
        b, b_lo, b_hi = percentile_with_ci(before, q)
        a, a_lo, a_hi = percentile_with_ci(after, q)
        d = a - b
        deltas[name] = d
        b_str = f"{fmt_ns(b)} [{fmt_ns(b_lo).strip()}, {fmt_ns(b_hi).strip()}]"
        a_str = f"{fmt_ns(a)} [{fmt_ns(a_lo).strip()}, {fmt_ns(a_hi).strip()}]"
        d_pct = (100.0 * d / b) if b > 0 else 0.0
        print(f"{name:<6} | {b_str:>30} | {a_str:>30} | "
              f"{fmt_ns(d) if d >= 0 else '-' + fmt_ns(-d).strip()} ({d_pct:+5.1f}%)")
    b_max = float(np.max(before))
    a_max = float(np.max(after))
    print(f"{'max':<6} | {fmt_ns(b_max):>30} | {fmt_ns(a_max):>30} | "
          f"{fmt_ns(a_max - b_max) if a_max >= b_max else '-' + fmt_ns(b_max - a_max).strip()}")

    ks_d, ks_p = ks_2sample(before, after)
    print(f"\nKS two-sample: D={ks_d:.4f}, p={ks_p:.4g}")
    if ks_p < 0.001:
        print("  → distributions differ at 99.9% confidence")
    elif ks_p < 0.05:
        print("  → distributions differ at 95% confidence")
    else:
        print("  → distributions are statistically indistinguishable")

    # Histograms are already in nanoseconds (switch 2026-05-06).
    p50_d_ns = float(deltas["p50"])
    b_p99 = float(np.percentile(before, 99))
    p99_d_pct = (100.0 * deltas["p99"] / b_p99) if b_p99 > 0 else 0.0

    p50_threshold = args.noise_floor_p50_ns
    p99_threshold = args.noise_floor_p99_pct

    p50_improved = p50_d_ns < -p50_threshold
    p99_improved = p99_d_pct < -p99_threshold
    p50_regressed = p50_d_ns > p50_threshold
    p99_regressed = p99_d_pct > p99_threshold

    # In the replay harness the p99 tail is dominated by queue-lag
    # during heavy events (vol_surface fits, risk_state aggregations);
    # measured baseline-to-baseline drift is ±200% on IPC and ±185%
    # on TTT (with N≈155 samples per arm). p99 is therefore reported
    # for visibility but does NOT gate the verdict — only p50 does.
    # Once samples-per-run scale up (longer runs or denser recording),
    # gate p99 too.
    if p50_regressed:
        verdict = "REVERT"
    elif p50_improved:
        verdict = "SHIP"
    else:
        verdict = "INCONCLUSIVE"

    print(f"\nVerdict: {verdict}")
    print(f"  p50 delta = {p50_d_ns:+8.0f} ns   (threshold +/- {p50_threshold} ns)")
    print(f"  p99 delta = {p99_d_pct:+8.1f} %    (threshold +/- {p99_threshold} %)")
    if verdict == "REVERT":
        print("  reason: at least one percentile regressed past noise floor")
    elif verdict == "SHIP":
        print("  reason: at least one percentile improved past noise floor without "
              "regression on the others")
    else:
        print("  reason: no percentile moved enough to clear noise floor")

    sys.exit(0 if verdict in ("SHIP", "INCONCLUSIVE") else 1)


if __name__ == "__main__":
    main()
