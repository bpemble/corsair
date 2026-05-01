"""Post-session analysis for peer-consensus cap (Change 1).

Reads the day's skips JSONL (cap_engaged events + regular skips),
fills JSONL, and quotes CSV, and produces:

  1. Cap fire-rate summary (count, events/hour, by-strike, by-side)
  2. Price-improvement distribution (how many ticks the cap saved per fire)
  3. Fill impact:
       - fills-per-session comparison to prior-session baselines
       - vs-mid distribution on today's fills (are adverse-to-mid fills down?)
       - correlation: did any cap-engaged strike/side subsequently fill?
  4. Behind-incumbent rate comparison (cap pushes us behind more often)
  5. Counterfactual sample for manual review:
       - Adaptive sample size based on cap event count
       - Each sample row carries pre-cap/post-cap, mid-at-fire, theo-at-fire,
         mid-at-T+30s (read from quotes CSV), mid-at-T+60s
       - Human judgment: did market move toward pre-cap (SABR right) or
         away (SABR wrong)?

Usage:
    python3 scripts/peer_cap_session_analysis.py [YYYY-MM-DD]

Defaults to today if date omitted. Designed to be run end-of-session.
"""
from __future__ import annotations

import csv
import json
import random
import statistics
import sys
from collections import Counter, defaultdict
from datetime import date, datetime, timedelta, timezone
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]
TICK = 0.0005  # HG tick


def pct(xs, p):
    if not xs:
        return None
    s = sorted(xs)
    return s[max(0, min(len(s) - 1, int(len(s) * p)))]


def parse_ts(s: str) -> datetime | None:
    """Parse timestamps from either skips.jsonl ('Z' suffix) or quotes.csv
    (no tz). Always return timezone-aware UTC."""
    if not s:
        return None
    t = s.replace("Z", "").replace("T", " ")
    if "+" in t:
        for fmt in ("%Y-%m-%d %H:%M:%S.%f%z", "%Y-%m-%d %H:%M:%S%z"):
            try:
                return datetime.strptime(t, fmt)
            except ValueError:
                continue
    for fmt in ("%Y-%m-%d %H:%M:%S.%f", "%Y-%m-%d %H:%M:%S"):
        try:
            return datetime.strptime(t, fmt).replace(tzinfo=timezone.utc)
        except ValueError:
            continue
    return None


def load_jsonl(p: Path) -> list[dict]:
    if not p.exists():
        return []
    return [json.loads(l) for l in p.read_text().splitlines() if l.strip()]


# ─── Section 1: cap fire-rate summary ──────────────────────────────────

def summarize_cap_events(cap_events: list[dict]) -> None:
    print(f"\n{'='*78}\n1. Cap fire rate\n{'='*78}")
    n = len(cap_events)
    if n == 0:
        print("  No cap events recorded. Cap is disabled or didn't fire.")
        return
    # time span
    tss = [parse_ts(e["timestamp_utc"]) for e in cap_events if "timestamp_utc" in e]
    tss = [t for t in tss if t is not None]
    if tss:
        span = (max(tss) - min(tss)).total_seconds()
        per_hr = n / (span / 3600) if span > 0 else 0
        print(f"  Total events: {n}")
        print(f"  Span: {min(tss).strftime('%H:%M:%S')} → {max(tss).strftime('%H:%M:%S')} UTC "
              f"({span/60:.1f} min)")
        print(f"  Rate: {per_hr:.1f} events/hour")
    else:
        print(f"  Total events: {n}  (no timestamps to compute rate)")

    # Split by side
    by_side = Counter(e.get("side", "?") for e in cap_events)
    print(f"  By side: BUY={by_side.get('BUY', 0)}  SELL={by_side.get('SELL', 0)}")

    # By strike (top 10)
    by_strike = Counter((e.get("strike"), e.get("cp"), e.get("side")) for e in cap_events)
    print(f"  Top 10 (strike, cp, side) by cap engagement:")
    for (k, cp, sd), cnt in by_strike.most_common(10):
        print(f"    {k} {cp} {sd:<4}  {cnt:5d}")


# ─── Section 2: price-improvement distribution ─────────────────────────

def summarize_improvements(cap_events: list[dict]) -> None:
    print(f"\n{'='*78}\n2. Price-improvement distribution\n{'='*78}")
    imps = [e.get("improvement_ticks") for e in cap_events
            if e.get("improvement_ticks") is not None]
    if not imps:
        print("  No improvement data (cap didn't engage or field missing)")
        return
    print(f"  n={len(imps)}  min={min(imps):.1f}  p50={pct(imps, .5):.1f}  "
          f"p90={pct(imps, .9):.1f}  p99={pct(imps, .99):.1f}  max={max(imps):.1f}  (ticks)")
    # Distribution buckets
    buckets = Counter()
    for t in imps:
        if t <= 1: buckets["1 tick"] += 1
        elif t <= 2: buckets["2 ticks"] += 1
        elif t <= 3: buckets["3 ticks"] += 1
        elif t <= 5: buckets["4-5 ticks"] += 1
        elif t <= 10: buckets["6-10 ticks"] += 1
        else: buckets[">10 ticks"] += 1
    print(f"\n  Histogram:")
    for label in ["1 tick", "2 ticks", "3 ticks", "4-5 ticks", "6-10 ticks", ">10 ticks"]:
        n = buckets[label]
        bar = "█" * int(40 * n / max(buckets.values())) if n else ""
        print(f"    {label:<12}  {n:6d}  {bar}")


# ─── Section 3: fill impact ────────────────────────────────────────────

def summarize_fills(fills: list[dict], cap_events: list[dict],
                    baseline_path: Path | None) -> None:
    print(f"\n{'='*78}\n3. Fill impact\n{'='*78}")
    print(f"  Fills today: {len(fills)}")
    if not fills:
        print("  No fills to analyze.")
        return

    def parse_sym(s): p = s.split()[-1]; return (int(p[1:])/100.0, p[0])

    # vs-mid distribution. Detect flatten-burst fills (cluster within 2s
    # of each other; crossed-market prints) and report separately.
    def fill_vs_mid_ticks(f):
        mb, ma = f.get("market_bid", 0), f.get("market_ask", 0)
        if mb <= 0 or ma <= 0 or mb >= ma:  # also reject crossed markets
            return None
        mid = (mb + ma) / 2
        favor = (f["price"] - mid) if f["side"] == "SELL" else (mid - f["price"])
        return favor / TICK

    # Identify likely flatten-burst fills: 4+ fills within 3 seconds
    from_ts = [parse_ts(f.get("ts", "")) for f in fills]
    flatten_idx = set()
    for i, t in enumerate(from_ts):
        if not t: continue
        cluster = [j for j, tj in enumerate(from_ts)
                   if tj and abs((tj - t).total_seconds()) <= 3]
        if len(cluster) >= 4:
            flatten_idx.update(cluster)

    organic_vs_mid = []
    flatten_vs_mid = []
    for i, f in enumerate(fills):
        v = fill_vs_mid_ticks(f)
        if v is None:
            continue
        if i in flatten_idx:
            flatten_vs_mid.append(v)
        else:
            organic_vs_mid.append(v)

    print(f"  Organic fills: {len(organic_vs_mid)}  |  Flatten-burst fills: {len(flatten_vs_mid)}")
    if organic_vs_mid:
        print(f"\n  ORGANIC per-fill (fill - mid) ticks, signed to favor us:")
        print(f"    n={len(organic_vs_mid)}  min={min(organic_vs_mid):.1f}  "
              f"p50={pct(organic_vs_mid, .5):.1f}  p90={pct(organic_vs_mid, .9):.1f}  "
              f"max={max(organic_vs_mid):.1f}")
        adverse = sum(1 for v in organic_vs_mid if v < -2)
        print(f"    Adverse (>2 ticks unfavorable): {adverse}/{len(organic_vs_mid)}  "
              f"({100*adverse/len(organic_vs_mid):.1f}%)  "
              f"← KEY METRIC, should trend toward 0 with cap")
    if flatten_vs_mid:
        print(f"\n  FLATTEN-BURST fills (excluded from organic stats — crossed-market prints):")
        print(f"    n={len(flatten_vs_mid)}  min={min(flatten_vs_mid):.1f}  "
              f"max={max(flatten_vs_mid):.1f}  (ignore for cap analysis)")
    vs_mid = organic_vs_mid  # used below

    # Correlation: did cap-engaged (strike, cp, side) subsequently fill?
    if cap_events:
        capped_keys = set()
        for e in cap_events:
            capped_keys.add((e.get("strike"), e.get("cp"), e.get("side")))
        fill_keys = set()
        for f in fills:
            k, cp = parse_sym(f["symbol"])
            fill_keys.add((k, cp, f["side"]))
        overlap = capped_keys & fill_keys
        print(f"\n  Capped (strike,cp,side) keys: {len(capped_keys)}")
        print(f"  Filled (strike,cp,side) keys:  {len(fill_keys)}")
        print(f"  Overlap (capped + filled):     {len(overlap)}")
        if overlap:
            print(f"  Keys that both cap-engaged AND filled today:")
            for k in sorted(overlap):
                print(f"    strike={k[0]} {k[1]} {k[2]}")

    # Baseline comparison if provided (also filter flatten-burst fills)
    if baseline_path and baseline_path.exists():
        base = load_jsonl(baseline_path)
        base_ts = [parse_ts(f.get("ts", "")) for f in base]
        base_flatten_idx = set()
        for i, t in enumerate(base_ts):
            if not t: continue
            cluster = [j for j, tj in enumerate(base_ts)
                       if tj and abs((tj - t).total_seconds()) <= 3]
            if len(cluster) >= 4:
                base_flatten_idx.update(cluster)
        base_vs_mid = []
        for i, f in enumerate(base):
            if i in base_flatten_idx: continue
            v = fill_vs_mid_ticks(f)
            if v is not None:
                base_vs_mid.append(v)
        if base_vs_mid:
            print(f"\n  BASELINE ({baseline_path.name}, organic only):")
            adverse_base = sum(1 for v in base_vs_mid if v < -2)
            print(f"    n={len(base_vs_mid)}  p50={pct(base_vs_mid, .5):.1f}  "
                  f"adverse(>2tk)={adverse_base}/{len(base_vs_mid)}  "
                  f"({100*adverse_base/len(base_vs_mid):.1f}%)")


# ─── Section 4: behind_incumbent delta ─────────────────────────────────

def summarize_behind_incumbent(skips: list[dict], baseline_skips: list[dict] | None) -> None:
    print(f"\n{'='*78}\n4. Behind-incumbent rate (cap is expected to push this UP)\n{'='*78}")
    bi = sum(1 for s in skips if s.get("skip_reason") == "behind_incumbent")
    total_skips = len(skips)
    print(f"  Today: behind_incumbent skips = {bi} / {total_skips}  "
          f"({100*bi/total_skips:.1f}% of skip stream)" if total_skips else "  No skip data")
    if baseline_skips:
        base_bi = sum(1 for s in baseline_skips if s.get("skip_reason") == "behind_incumbent")
        base_total = len(baseline_skips)
        print(f"  Baseline: behind_incumbent = {base_bi} / {base_total}  "
              f"({100*base_bi/base_total:.1f}%)" if base_total else "")


# ─── Section 5: counterfactual sample ──────────────────────────────────

def index_quotes_csv(quotes_path: Path, target_date: str) -> dict:
    """Build (strike, cp, side) → sorted list of (ts, incumbent_bid, incumbent_ask)
    from quotes.csv for the target date. Used for T+N mid lookup."""
    index: dict = defaultdict(list)
    if not quotes_path.exists():
        return index
    print(f"  Indexing {quotes_path.name}...")
    with open(quotes_path) as f:
        rdr = csv.DictReader(f)
        for r in rdr:
            if not r.get("timestamp", "").startswith(target_date):
                continue
            try:
                strike = float(r["strike"])
                cp = r.get("put_call", "")
                side = r.get("side", "")
                ts = parse_ts(r["timestamp"])
                ib = r.get("incumbent_price")
                bw = r.get("bbo_width")
                if ts and ib and bw:
                    # We don't have bid/ask directly; infer from incumbent + width/2
                    # For this purpose, use mid = incumbent_price (approx) for speed.
                    # Cleaner: if the telemetry row is BUY side, incumbent_price ≈ best bid;
                    # if SELL, incumbent_price ≈ best ask. So mid ≈ incumbent ± width/2.
                    ip = float(ib)
                    w = float(bw)
                    mid = ip + w/2 if side == "BUY" else ip - w/2
                    index[(strike, cp, side)].append((ts, mid))
            except (ValueError, KeyError):
                continue
    # sort each list
    for k in index:
        index[k].sort(key=lambda x: x[0])
    return index


def mid_at(index: dict, key, at_ts: datetime) -> float | None:
    """Binary-search-ish lookup. Return the closest quote-csv mid ≤ at_ts."""
    lst = index.get(key, [])
    if not lst:
        return None
    # simple linear (can optimize if needed)
    best = None
    for ts, mid in lst:
        if ts <= at_ts:
            best = mid
        else:
            break
    return best


def counterfactual_sample(cap_events: list[dict], quotes_index: dict,
                          out_path: Path) -> None:
    print(f"\n{'='*78}\n5. Counterfactual sample for manual review\n{'='*78}")
    n = len(cap_events)
    # Adaptive sample size
    if n == 0:
        print("  No cap events to sample")
        return
    if n <= 50:
        sample = cap_events
        print(f"  {n} events total — reviewing all")
    elif n <= 200:
        sample = random.sample(cap_events, 50)
        print(f"  {n} events total — random sample of 50")
    else:
        sample = random.sample(cap_events, 100)
        print(f"  {n} events total — random sample of 100")

    # Enrich with T+30s and T+60s mid lookups
    enriched = []
    for e in sample:
        ts = parse_ts(e.get("timestamp_utc", ""))
        if not ts:
            continue
        key = (e.get("strike"), e.get("cp"), e.get("side"))
        mid_30 = mid_at(quotes_index, key, ts + timedelta(seconds=30))
        mid_60 = mid_at(quotes_index, key, ts + timedelta(seconds=60))
        mid_at_fire = e.get("mid")
        pre_cap = e.get("pre_cap_price")
        post_cap = e.get("post_cap_price")
        side = e.get("side")
        # Judgment: did mid move toward pre_cap (SABR right) or away?
        judgment = "?"
        if mid_at_fire is not None and mid_60 is not None and pre_cap is not None:
            d_fire = pre_cap - mid_at_fire
            d_60 = pre_cap - mid_60
            # SABR was right if |d_60| < |d_fire| (market moved toward SABR)
            if abs(d_60) < abs(d_fire) - TICK/2:
                judgment = "SABR→right"
            elif abs(d_60) > abs(d_fire) + TICK/2:
                judgment = "SABR→wrong"
            else:
                judgment = "stable"
        enriched.append({
            "ts": e.get("timestamp_utc"),
            "strike": e.get("strike"),
            "cp": e.get("cp"),
            "side": side,
            "pre_cap": pre_cap,
            "post_cap": post_cap,
            "improvement_ticks": e.get("improvement_ticks"),
            "theo_at_fire": e.get("theo"),
            "mid_at_fire": mid_at_fire,
            "bbo_bid": e.get("bbo_bid"),
            "bbo_ask": e.get("bbo_ask"),
            "mid_plus_30s": mid_30,
            "mid_plus_60s": mid_60,
            "forward_at_fire": e.get("forward"),
            "sabr_judgment": judgment,
        })

    # Write CSV
    if enriched:
        fieldnames = list(enriched[0].keys())
        with open(out_path, "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=fieldnames)
            w.writeheader()
            w.writerows(enriched)
        print(f"  Wrote sample → {out_path}")

        # Summary of SABR judgment
        j = Counter(r["sabr_judgment"] for r in enriched)
        print(f"\n  SABR-direction breakdown in sample (T+60s):")
        for k, v in j.most_common():
            pct_s = f"{100*v/len(enriched):.1f}%"
            print(f"    {k:<15}  {v:4d}  ({pct_s})")
        print(f"\n  Interpret:")
        print(f"    'SABR→right': subsequent mid moved TOWARD our pre-cap price "
              f"(SABR disagreement proved correct)")
        print(f"    'SABR→wrong': subsequent mid moved AWAY from pre-cap (cap saved us)")
        print(f"    'stable':     no meaningful move in 60s")


# ─── Main ──────────────────────────────────────────────────────────────

def main():
    target_date = sys.argv[1] if len(sys.argv) > 1 else date.today().isoformat()
    prev_date = (date.fromisoformat(target_date) - timedelta(days=1)).isoformat()
    print(f"Peer-consensus cap post-session analysis — date: {target_date}")
    print(f"Baseline (previous session): {prev_date}")

    skips_p = REPO / f"logs-paper/skips-{target_date}.jsonl"
    fills_p = REPO / f"logs-paper/fills-{target_date}.jsonl"
    base_fills_p = REPO / f"logs-paper/fills-{prev_date}.jsonl"
    quotes_dated = REPO / f"logs/quotes.{target_date}.csv"
    quotes_default = REPO / "logs/quotes.csv"
    quotes_p = quotes_dated if quotes_dated.exists() else quotes_default

    skips_all = load_jsonl(skips_p)
    base_skips = load_jsonl(REPO / f"logs-paper/skips-{prev_date}.jsonl")
    cap_events = [s for s in skips_all if s.get("event_type") == "cap_engaged"]
    regular_skips = [s for s in skips_all if s.get("event_type") != "cap_engaged"]

    fills = load_jsonl(fills_p)

    summarize_cap_events(cap_events)
    summarize_improvements(cap_events)
    summarize_fills(fills, cap_events, base_fills_p)
    summarize_behind_incumbent(regular_skips, base_skips)

    # Counterfactual sample — only if cap events exist
    out_sample = REPO / f"logs-paper/peer_cap_sample_{target_date}.csv"
    quotes_index = index_quotes_csv(quotes_p, target_date) if cap_events else {}
    counterfactual_sample(cap_events, quotes_index, out_sample)

    print(f"\n{'='*78}")
    print(f"Done. Sample CSV (if generated): {out_sample}")
    print(f"Next step: eyeball the sample CSV. If SABR→right dominates, cap is")
    print(f"too aggressive (consider ticks=3 or 4). If SABR→wrong dominates,")
    print(f"cap is doing its job. If 'stable' dominates, most of these are noise.")


if __name__ == "__main__":
    random.seed(42)  # reproducible sampling
    main()
