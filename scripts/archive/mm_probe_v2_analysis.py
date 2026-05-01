"""Phase 2 analysis for mm_probe_v2 JSONL captures.

Answers the methodology questions raised on 2026-04-22:

  Q1 (tick-triggered vs periodic): Does the BBO-change rate depend on
     underlying activity, or is it a steady periodic cycle?

  Q2 (fast MMs vs slow): Do fast responses come with meaningful size
     and tight spreads (sophisticated MM signature), or are they small
     retail-size updates?

  Q3 (IBKR latency floor): Is there a systematic minimum inter-arrival
     delay (e.g. ~250ms) in L1 data that sets a floor on any "MM speed"
     measurement? Does unthrottled TBT show ticks below that floor?

  Q4 (hidden same-price updates): What fraction of option updates are
     size-only (no price change) — i.e. invisible to the v1 probe which
     only triggers on BBO price movement?

  Q5 (spread/size characteristics): Per-product per-strike BBO width
     and BBO size distributions for sanity checking.

Usage:
    python3 scripts/mm_probe_v2_analysis.py <probe.jsonl> [<probe.jsonl> ...]

Output: text report per file plus a cross-product comparison table.
"""
from __future__ import annotations

import json
import statistics
import sys
from collections import Counter, defaultdict
from pathlib import Path


def load(path: Path):
    evs = []
    for line in path.read_text().splitlines():
        if not line.strip():
            continue
        try:
            evs.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return evs


def pct(xs, p):
    if not xs:
        return float("nan")
    s = sorted(xs)
    n = len(s)
    idx = max(0, min(n - 1, int(n * p)))
    return s[idx]


def describe(xs, label="", unit="ms"):
    if not xs:
        return f"{label}: n=0"
    s = sorted(xs)
    n = len(s)
    return (f"{label}: n={n}  "
            f"min={s[0]:.2f}{unit}  "
            f"p10={pct(s, 0.10):.2f}  "
            f"p50={pct(s, 0.50):.2f}  "
            f"p90={pct(s, 0.90):.2f}  "
            f"p99={pct(s, 0.99):.2f}  "
            f"max={s[-1]:.2f}")


def analyze_one(evs, label):
    """Return a dict of structured findings for cross-product aggregation."""
    print(f"\n{'='*76}\n{label}\n{'='*76}")

    # Event counts
    kinds = Counter(e.get("event", "?") for e in evs)
    print(f"\nEvent kinds:")
    for k, v in kinds.most_common():
        print(f"  {k:25s} {v:8d}")

    # Connection metadata
    meta = {}
    for e in evs:
        if e.get("event") == "connected":
            meta = e
            break
    duration_sec = meta.get("duration_sec", 0)
    print(f"\nDuration configured: {duration_sec}s")

    # Underlying resolution
    res = next((e for e in evs if e.get("event") == "underlying_resolved"), {})
    print(f"Underlying: {res.get('symbol')} (conId={res.get('con_id')}, "
          f"tradingClass={res.get('trading_class')}, expiry={res.get('expiry')})")

    qual = next((e for e in evs if e.get("event") == "options_qualified"), {})
    print(f"Options qualified: {qual.get('qualified')}/{qual.get('requested')}")

    # ────────────────────────────────────────────────────────────────────
    # Q3: IBKR floor — underlying inter-arrival for L1 vs TBT
    # ────────────────────────────────────────────────────────────────────
    print("\n--- Q3: Underlying tick cadence (L1 vs TBT) ---")
    und_mkt = [e for e in evs if e.get("event") == "und_mkt"]
    und_tbt = [e for e in evs if e.get("event") == "und_tbt"]
    print(f"und_mkt events: {len(und_mkt)} (rate {len(und_mkt)/max(1,duration_sec):.2f}/s)")
    print(f"und_tbt events: {len(und_tbt)} (rate {len(und_tbt)/max(1,duration_sec):.2f}/s)")

    und_mkt_real = [e for e in und_mkt
                    if e.get("change_type") not in ("no_change", "first", None)]
    print(f"und_mkt with real change: {len(und_mkt_real)} (excluding no_change/first)")

    def inter_arrival_ms(events):
        ts = [e["ts_mono_ns"] for e in events]
        if len(ts) < 2:
            return []
        return [(ts[i + 1] - ts[i]) / 1e6 for i in range(len(ts) - 1)]

    ia_mkt = inter_arrival_ms(und_mkt)
    ia_mkt_real = inter_arrival_ms(und_mkt_real)
    ia_tbt = inter_arrival_ms(und_tbt)
    print()
    print(describe(ia_mkt, "und_mkt IA (all events)"))
    print(describe(ia_mkt_real, "und_mkt IA (price/size change only)"))
    print(describe(ia_tbt, "und_tbt IA (reqTickByTickData)"))
    if ia_tbt and ia_mkt:
        tbt_min = min(ia_tbt); mkt_min = min(ia_mkt)
        print(f"\nDiagnostic: TBT min={tbt_min:.1f}ms vs L1 min={mkt_min:.1f}ms.")
        if tbt_min < 50 and mkt_min > 150:
            print("  → TBT CAN deliver sub-50ms ticks while L1 bottoms at >150ms.")
            print("    Consistent with known IBKR L1 throttling (~4 updates/sec).")
        elif tbt_min > 150 and mkt_min > 150:
            print("  → Neither L1 nor TBT deliver sub-150ms ticks.")
            print("    Either (a) the underlying is genuinely slow in this window,")
            print("    (b) both subscriptions share the same bottleneck, or")
            print("    (c) the TBT subscription is being silently throttled too.")
        else:
            print("  → Mixed signal; interpret with care.")

    # ────────────────────────────────────────────────────────────────────
    # Q4: Hidden same-price option updates
    # ────────────────────────────────────────────────────────────────────
    print("\n--- Q4: Option BBO change categorization ---")
    opt_mkt = [e for e in evs if e.get("event") == "opt_mkt"]
    types = Counter(e.get("change_type") for e in opt_mkt)
    total = sum(types.values())
    print(f"Total opt_mkt events: {total}")
    for t, n in types.most_common():
        pct_s = f"{100.0 * n / total:.1f}%" if total else "—"
        print(f"  {str(t):20s} {n:7d}  ({pct_s})")

    px_types = {"bid_px", "ask_px", "both_px"}
    size_types = {"bid_size_only", "ask_size_only", "both_size_only"}
    px_changes = sum(n for t, n in types.items() if t in px_types)
    size_only = sum(n for t, n in types.items() if t in size_types)
    real = px_changes + size_only
    print(f"\nOf real changes (price + size, excluding no_change/first):")
    print(f"  Price changes: {px_changes}  ({100.0*px_changes/real:.1f}% if real else —)"
          if real else f"  Price changes: {px_changes}")
    print(f"  Size-only:     {size_only}  ({100.0*size_only/real:.1f}%)"
          if real else f"  Size-only:     {size_only}")
    if real:
        print(f"→ v1 probe would have been INVISIBLE to {100.0*size_only/real:.1f}% "
              f"of real BBO activity.")

    # ────────────────────────────────────────────────────────────────────
    # Q1: Is BBO change tick-triggered or periodic?
    # ────────────────────────────────────────────────────────────────────
    print("\n--- Q1: Tick-triggered vs periodic BBO activity ---")
    # Build a time-indexed merge of underlying price-change events and
    # option BBO price-change events. For each 1-sec bucket, count each.
    if und_mkt and opt_mkt:
        def bucket(e):
            # Convert mono_ns to seconds-from-first-event
            return int((e["ts_mono_ns"] - evs[0]["ts_mono_ns"]) / 1e9)

        und_px_change = [e for e in und_mkt
                         if e.get("change_type") in ("bid_px", "ask_px", "both_px")]
        opt_px_change = [e for e in opt_mkt
                         if e.get("change_type") in ("bid_px", "ask_px", "both_px")]

        und_buckets = Counter(bucket(e) for e in und_px_change)
        opt_buckets = Counter(bucket(e) for e in opt_px_change)

        # For each underlying-active bucket, count opt activity in that +
        # next 2 buckets (0-3 sec window post-tick).
        tick_window_opt_rate = []
        quiet_window_opt_rate = []
        max_bucket = max(list(und_buckets) + list(opt_buckets) + [0])
        for b in range(max_bucket + 1):
            is_active = und_buckets.get(b, 0) > 0
            local_opt = opt_buckets.get(b, 0)
            if is_active:
                tick_window_opt_rate.append(local_opt)
            else:
                quiet_window_opt_rate.append(local_opt)

        def avg(xs):
            return sum(xs) / len(xs) if xs else 0.0

        print(f"Active 1-sec buckets (und moved): {len(tick_window_opt_rate)}")
        print(f"Quiet 1-sec buckets (no und move): {len(quiet_window_opt_rate)}")
        print(f"  Avg opt px-changes / ACTIVE bucket: {avg(tick_window_opt_rate):.2f}")
        print(f"  Avg opt px-changes / QUIET bucket:  {avg(quiet_window_opt_rate):.2f}")
        if quiet_window_opt_rate:
            ratio = avg(tick_window_opt_rate) / max(avg(quiet_window_opt_rate), 1e-9)
            print(f"  Ratio (active/quiet): {ratio:.2f}×")
            if ratio > 3:
                print("  → Opt BBO activity clearly correlates with und ticks (reactive).")
            elif ratio < 1.5:
                print("  → Opt BBO activity is ~steady regardless of und ticks (periodic).")
            else:
                print("  → Weak correlation; some reactive + some periodic.")

    # ────────────────────────────────────────────────────────────────────
    # Q2: Size distribution of BBO changes (fast MMs quote bigger)
    # ────────────────────────────────────────────────────────────────────
    print("\n--- Q2: Size + spread at BBO changes ---")
    # For each opt price-change event, compute observed size and spread
    size_at_change = []
    spread_at_change = []
    for e in opt_mkt:
        if e.get("change_type") not in ("bid_px", "ask_px", "both_px"):
            continue
        b, a = e.get("bid"), e.get("ask")
        bs, as_ = e.get("bid_size") or 0, e.get("ask_size") or 0
        if b and a and a > b:
            spread_at_change.append(a - b)
        if bs and as_:
            size_at_change.append(min(bs, as_))
    if spread_at_change:
        print(describe([1000*s for s in spread_at_change], "BBO spread at px-change", unit="×1e-3"))
    if size_at_change:
        print(describe([float(s) for s in size_at_change], "BBO min(bid_size,ask_size) at px-change", unit=" lots"))

    # ────────────────────────────────────────────────────────────────────
    # Q5: Per-strike summary
    # ────────────────────────────────────────────────────────────────────
    print("\n--- Q5: Per-strike event density ---")
    by_strike = defaultdict(lambda: {"px": 0, "size": 0, "total": 0})
    for e in opt_mkt:
        k = (e.get("strike"), e.get("right"))
        ct = e.get("change_type")
        by_strike[k]["total"] += 1
        if ct in px_types:
            by_strike[k]["px"] += 1
        elif ct in size_types:
            by_strike[k]["size"] += 1
    print(f"{'strike':<10}{'total':>8}{'px-chg':>10}{'size-only':>12}")
    for k in sorted(by_strike):
        v = by_strike[k]
        print(f"{k[0]}{k[1]:<9}{v['total']:>8}{v['px']:>10}{v['size']:>12}")

    # Underlying price-change tick rate
    und_px = sum(1 for e in und_mkt if e.get("change_type") in px_types)
    print(f"\nUnderlying price-change events (L1): {und_px}")

    # Return structured data for cross-product table
    return {
        "label": label,
        "duration_sec": duration_sec,
        "n_und_mkt": len(und_mkt),
        "n_und_tbt": len(und_tbt),
        "n_opt_mkt": len(opt_mkt),
        "ia_mkt_min": min(ia_mkt) if ia_mkt else None,
        "ia_mkt_p50": pct(ia_mkt, 0.5) if ia_mkt else None,
        "ia_tbt_min": min(ia_tbt) if ia_tbt else None,
        "ia_tbt_p50": pct(ia_tbt, 0.5) if ia_tbt else None,
        "types": dict(types),
        "px_changes": px_changes,
        "size_only": size_only,
        "spread_p50": pct(spread_at_change, 0.5) if spread_at_change else None,
        "size_p50": pct(size_at_change, 0.5) if size_at_change else None,
    }


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)

    results = []
    for p_str in sys.argv[1:]:
        p = Path(p_str)
        if not p.exists():
            print(f"skip {p} (missing)")
            continue
        evs = load(p)
        results.append(analyze_one(evs, p.name))

    if len(results) >= 2:
        print(f"\n{'='*76}\nCROSS-PRODUCT COMPARISON\n{'='*76}")
        print(f"{'file':<50}{'n_und':>8}{'n_tbt':>8}{'L1_min':>10}{'TBT_min':>10}")
        for r in results:
            print(f"{r['label']:<50}"
                  f"{r['n_und_mkt']:>8}"
                  f"{r['n_und_tbt']:>8}"
                  f"{(r['ia_mkt_min'] or 0):>9.1f}ms "
                  f"{(r['ia_tbt_min'] or 0):>9.1f}ms")


if __name__ == "__main__":
    main()
