#!/usr/bin/env python3
"""Sample chain_snapshot.json for 120s, bucket each SIDE per tick.

Buckets per (expiry, strike, right, side=BUY/SELL):
  leading  — live on book AND our price is at/inside market best
  behind   — live on book AND our price is worse than market best
  stale    — live on book AND our price differs from theo by > 1 tick
             (proxy: quote hasn't caught up to current theoretical)
  pending  — order sent, not yet live (status == 'pending', no live side)
  idle     — no order resting or pending on this side

A strike contributes TWO side-observations per tick (BUY side on bid, SELL
side on ask).
"""
import json, time, sys
from collections import Counter, defaultdict

SNAPS = [
    ("ETH", "/home/ethereal/corsair/data/chain_snapshot.json"),
    ("HG",  "/home/ethereal/corsair/data/hg_chain_snapshot.json"),
]
DURATION_S = 120
INTERVAL_S = 1.0
# Proxy for "stale": our resting price differs from current theo by >= this.
# ETH options price in 0.25 ticks, so 0.50 = 2 ticks of drift.
STALE_THRESHOLD = 0.50

# Track per-side price history to detect "stale" (price unchanged for N ticks)
_price_history = defaultdict(list)  # (expiry, strike, right, side) -> [last N prices]
STALE_UNCHANGED_TICKS = 10  # ~10s of no price movement while live = stale

def classify(live: bool, our: float, mkt: float, side: str, status: str, key):
    if not live:
        if status == "pending":
            return "pending"
        return "idle"
    if our is None:
        return "idle"
    # Record price history
    hist = _price_history[key]
    hist.append(our)
    if len(hist) > STALE_UNCHANGED_TICKS:
        hist.pop(0)
    # Leading vs behind: compare our resting price to clean market best (excludes us)
    if mkt is None or mkt == 0.0:
        bucket = "leading"  # nothing else quoting on this side
    elif side == "BUY":
        bucket = "leading" if our >= mkt else "behind"
    else:  # SELL
        bucket = "leading" if our <= mkt else "behind"
    # Override to "stale" if price has been unchanged for STALE_UNCHANGED_TICKS
    # AND we're behind (price stuck while market moved past us)
    if bucket == "behind" and len(hist) == STALE_UNCHANGED_TICKS and len(set(hist)) == 1:
        return "stale"
    return bucket

buckets = Counter()
per_side_totals = {"BUY": Counter(), "SELL": Counter()}
per_product = {p: Counter() for p, _ in SNAPS}
ticks = 0
seen = {p: set() for p, _ in SNAPS}

end = time.time() + DURATION_S
while time.time() < end:
    for prod, path in SNAPS:
        try:
            with open(path) as f:
                s = json.load(f)
        except Exception:
            continue
        ts = s.get("timestamp")
        if ts in seen[prod]:
            continue
        seen[prod].add(ts)
        if prod == "ETH":
            ticks += 1  # only count ETH ticks for the global N
        for expiry, exp_data in s.get("chains", {}).items():
            strikes = exp_data.get("strikes", {})
            for strike, sd in strikes.items():
                for right in ("call", "put"):
                    leg = sd.get(right)
                    if not leg:
                        continue
                    kb = (prod, expiry, strike, right, "BUY")
                    ka = (prod, expiry, strike, right, "SELL")
                    b = classify(leg.get("bid_live", False), leg.get("our_bid"),
                                 leg.get("market_bid"), "BUY", leg.get("status", "idle"), kb)
                    buckets[b] += 1; per_side_totals["BUY"][b] += 1
                    per_product[prod][b] += 1
                    a = classify(leg.get("ask_live", False), leg.get("our_ask"),
                                 leg.get("market_ask"), "SELL", leg.get("status", "idle"), ka)
                    buckets[a] += 1; per_side_totals["SELL"][a] += 1
                    per_product[prod][a] += 1
    time.sleep(INTERVAL_S)

total = sum(buckets.values())
print(f"\n=== {ticks} snapshot samples, {total} side-observations ===")
order = ["leading", "behind", "stale", "pending", "idle"]
print(f"{'state':<10} {'count':>10} {'pct':>8}   {'BUY':>8} {'SELL':>8}")
for k in order:
    c = buckets.get(k, 0)
    pct = 100.0 * c / total if total else 0
    print(f"{k:<10} {c:>10} {pct:>7.2f}%   "
          f"{per_side_totals['BUY'].get(k,0):>8} {per_side_totals['SELL'].get(k,0):>8}")
# Also show active-only (exclude idle) for a clearer picture of live behavior
active_total = sum(buckets[k] for k in order if k != "idle")
print(f"\n--- active sides only (excludes idle = {buckets.get('idle',0)}) ---")
for k in ["leading", "behind", "stale", "pending"]:
    c = buckets.get(k, 0)
    pct = 100.0 * c / active_total if active_total else 0
    print(f"{k:<10} {c:>10} {pct:>7.2f}%")
print()
for prod, _ in SNAPS:
    pt = sum(per_product[prod].values())
    if pt == 0: continue
    print(f"--- {prod} only ({pt} side-obs) ---")
    for k in order:
        c = per_product[prod].get(k, 0)
        pct = 100.0 * c / pt
        print(f"{k:<10} {c:>10} {pct:>7.2f}%")
