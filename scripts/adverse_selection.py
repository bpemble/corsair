"""Adverse selection analyzer for corsair fills.

Phase 1 — Edge capture vs theo_at_fill (offline, uses logs-paper/fills-*.jsonl).
  For each fill, signed edge = (price - theo) for SELL, (theo - price) for BUY.
  Positive = captured spread. Negative = paid through theo, possible adverse.
  Aggregates: overall, by side, by right, by strike bucket.

Phase 2 — Post-fill mid drift (requires Gateway).
  For each fill, queries reqHistoricalTicksAsync (BID_ASK) to reconstruct
  option mid at T+{1,5,60,300}s. Drift signed by side: favorable = mid moved
  our way (we profited after). Negative favorable = adverse selection.

Usage (offline, logs only):
    python3 scripts/adverse_selection.py --offline

Usage (both phases, requires Gateway running):
    docker compose run --rm --no-deps \\
        -v $(pwd)/scripts:/app/scripts \\
        -v $(pwd)/logs-paper:/app/logs-paper \\
        corsair python3 -u /app/scripts/adverse_selection.py

Outputs:
    Summary tables to stdout + optional CSV via --csv-out PATH.
"""
import argparse
import asyncio
import csv
import json
import os
import sys
from collections import defaultdict
from datetime import datetime, timedelta, timezone
from pathlib import Path
from statistics import mean


# ── Constants ──────────────────────────────────────────────────────────
MULTIPLIER = 25000  # HG: 25,000 lbs per contract
HORIZONS_SEC = [1, 5, 60, 300]
MONTH_CODE = {"F": 1, "G": 2, "H": 3, "J": 4, "K": 5, "M": 6,
              "N": 7, "Q": 8, "U": 9, "V": 10, "X": 11, "Z": 12}


# ── Fill loading + parsing ─────────────────────────────────────────────
def load_fills(glob_pattern: str):
    fills = []
    for path in sorted(Path(".").glob(glob_pattern)):
        with open(path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    d = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if d.get("event_type") == "fill":
                    fills.append(d)
    fills.sort(key=lambda d: d.get("ts", d.get("timestamp_utc", "")))
    return fills


def parse_symbol(sym: str):
    """Parse 'HXEK6 P555' -> (strike=5.55, right='P', month_code='K', year=2026).
    Returns (None, None, None, None) if unparseable."""
    try:
        prefix, strike_part = sym.split(" ", 1)
        right = strike_part[0]
        if right not in ("C", "P"):
            return None, None, None, None
        strike = int(strike_part[1:]) / 100.0
        # prefix like 'HXEK6' -> month='K', year='6'
        month_ch = prefix[-2]
        year_digit = prefix[-1]
        year = 2020 + int(year_digit)  # TODO: decade rollover beyond 2030
        return strike, right, month_ch, year
    except (ValueError, IndexError):
        return None, None, None, None


def signed_edge(f: dict) -> float:
    """Edge per unit (price, not dollar) — positive = captured spread."""
    price = f["price"]
    theo = f["theo_at_fill"]
    return (price - theo) if f["side"] == "SELL" else (theo - price)


def strike_bucket(strike, forward):
    """ATM=±2 nickels, near=±5, wing=further."""
    if forward is None or forward <= 0:
        return "?"
    diff_nickels = abs(round((strike - forward) / 0.05))
    if diff_nickels <= 2:
        return "ATM"
    if diff_nickels <= 5:
        return "near"
    return "wing"


def percentile(sorted_vals, p):
    n = len(sorted_vals)
    if n == 0:
        return None
    idx = max(0, min(n - 1, int(round(p * (n - 1)))))
    return sorted_vals[idx]


def describe(values):
    if not values:
        return None
    s = sorted(values)
    n = len(s)
    return {
        "n": n,
        "mean": mean(s),
        "p10": percentile(s, 0.10),
        "p50": percentile(s, 0.50),
        "p90": percentile(s, 0.90),
        "min": s[0],
        "max": s[-1],
        "neg_frac": sum(1 for v in s if v < 0) / n,
    }


def print_desc(label: str, values, mult=MULTIPLIER, indent=2):
    d = describe(values)
    pad = " " * indent
    if d is None:
        print(f"{pad}{label}: no data")
        return
    # In dollars per contract
    fmt = lambda v: f"${v * mult:+8.2f}"
    print(f"{pad}{label:<16} n={d['n']:4d}  "
          f"mean={fmt(d['mean'])}  p10={fmt(d['p10'])}  "
          f"p50={fmt(d['p50'])}  p90={fmt(d['p90'])}  "
          f"neg={d['neg_frac']*100:4.0f}%")


# ── Phase 1: edge capture ──────────────────────────────────────────────
def run_phase1(fills):
    print("=" * 80)
    print(f"PHASE 1 — Edge capture vs theo_at_fill  (n={len(fills)} fills)")
    print("=" * 80)
    print("Positive edge = captured spread. Negative edge = paid through theo.")
    print("Dollar values are per contract at multiplier=25000.\n")

    all_edges = [signed_edge(f) for f in fills]
    print_desc("Overall", all_edges, indent=0)

    print("\nBy side:")
    for side in ("BUY", "SELL"):
        vals = [signed_edge(f) for f in fills if f["side"] == side]
        print_desc(side, vals)

    print("\nBy right:")
    for right in ("C", "P"):
        vals = [
            signed_edge(f) for f in fills
            if parse_symbol(f["symbol"])[1] == right
        ]
        print_desc(right, vals)

    print("\nBy strike bucket (ATM=±2 nickels, near=±5, wing=>5):")
    for bucket in ("ATM", "near", "wing", "?"):
        vals = [
            signed_edge(f) for f in fills
            if strike_bucket(parse_symbol(f["symbol"])[0],
                             f.get("forward_at_fill")) == bucket
        ]
        if vals:
            print_desc(bucket, vals)

    print("\nBy side × right:")
    for side in ("BUY", "SELL"):
        for right in ("C", "P"):
            vals = [
                signed_edge(f) for f in fills
                if f["side"] == side
                and parse_symbol(f["symbol"])[1] == right
            ]
            if vals:
                print_desc(f"{side}/{right}", vals)

    # Dollar total captured
    total_usd = sum(signed_edge(f) * MULTIPLIER * f["size"] for f in fills)
    print(f"\nTotal signed edge across all fills: ${total_usd:+.2f}")


# ── Phase 2: post-fill mid drift ───────────────────────────────────────
async def run_phase2(fills, csv_out=None):
    try:
        from ib_insync import IB, FuturesOption
    except ImportError:
        print("\n[Phase 2 skipped] ib_insync not installed.")
        return

    host = os.environ.get("CORSAIR_GATEWAY_HOST", "127.0.0.1")
    port = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4002"))

    print()
    print("=" * 80)
    print(f"PHASE 2 — Post-fill mid drift via IBKR historical ticks")
    print("=" * 80)
    print(f"Connecting to {host}:{port} as clientId=44")

    ib = IB()
    try:
        await asyncio.wait_for(
            ib.connectAsync(host, port, clientId=44, timeout=15),
            timeout=20,
        )
    except (asyncio.TimeoutError, ConnectionRefusedError, OSError) as e:
        print(f"[Phase 2 skipped] gateway unreachable: {e}")
        return
    print(f"Connected. Server version {ib.client.serverVersion()}")

    unique_symbols = sorted({f["symbol"] for f in fills})
    print(f"Qualifying {len(unique_symbols)} unique contracts...")

    contracts: dict = {}
    for sym in unique_symbols:
        strike, right, month_ch, year = parse_symbol(sym)
        if strike is None:
            continue
        stub = FuturesOption(
            symbol="HXE", strike=strike, right=right,
            exchange="COMEX", multiplier=str(MULTIPLIER), currency="USD",
            tradingClass="HXE",
        )
        try:
            details = await ib.reqContractDetailsAsync(stub)
        except Exception as e:
            print(f"  {sym}: reqContractDetails failed: {e}")
            continue
        if not details:
            print(f"  {sym}: no contract details")
            continue
        # Match by localSymbol first
        match = None
        for d in details:
            if d.contract.localSymbol == sym:
                match = d.contract
                break
        if match is None:
            # Fall back to closest-expiry match on the target month
            target = year * 100 + MONTH_CODE.get(month_ch, 0)
            match = min(
                (d.contract for d in details),
                key=lambda c: abs(
                    int((c.lastTradeDateOrContractMonth or "00000000")[:6]) - target
                ),
            )
        contracts[sym] = match
    print(f"Qualified {len(contracts)}/{len(unique_symbols)}")

    # Query historical ticks for each fill × horizon
    drifts_by_horizon: dict = defaultdict(list)
    rows_for_csv = []
    n_queries = n_no_data = 0

    for i, f in enumerate(fills):
        sym = f["symbol"]
        if sym not in contracts:
            continue
        c = contracts[sym]
        fill_time = datetime.fromisoformat(
            f["ts"].replace("Z", "+00:00")
        ).astimezone(timezone.utc)
        mid_at_fill = (f["market_bid"] + f["market_ask"]) / 2
        if mid_at_fill <= 0:
            # Fall back to theo if market_bid/ask are zero
            mid_at_fill = f["theo_at_fill"]

        row = {
            "ts": f["ts"], "symbol": sym, "side": f["side"],
            "size": f["size"], "price": f["price"],
            "theo": f["theo_at_fill"], "mid_fill": mid_at_fill,
            "edge_dollars": signed_edge(f) * MULTIPLIER * f["size"],
        }

        for h in HORIZONS_SEC:
            target = fill_time + timedelta(seconds=h)
            end_str = (target + timedelta(seconds=2)).strftime(
                "%Y%m%d %H:%M:%S UTC"
            )
            try:
                ticks = await asyncio.wait_for(
                    ib.reqHistoricalTicksAsync(
                        c, "", end_str, numberOfTicks=20,
                        whatToShow="BID_ASK", useRth=False,
                    ),
                    timeout=10,
                )
                n_queries += 1
            except Exception as e:
                row[f"mid_t+{h}s"] = None
                row[f"favorable_t+{h}s"] = None
                continue

            if not ticks:
                n_no_data += 1
                row[f"mid_t+{h}s"] = None
                row[f"favorable_t+{h}s"] = None
                continue

            # Pick the tick closest to the target time
            closest = min(
                ticks,
                key=lambda t: abs((t.time.replace(tzinfo=timezone.utc)
                                   - target).total_seconds()),
            )
            if closest.priceBid <= 0 or closest.priceAsk <= 0:
                n_no_data += 1
                row[f"mid_t+{h}s"] = None
                row[f"favorable_t+{h}s"] = None
                continue

            mid_at_h = (closest.priceBid + closest.priceAsk) / 2
            raw_drift = mid_at_h - mid_at_fill
            favorable = -raw_drift if f["side"] == "SELL" else raw_drift
            drifts_by_horizon[h].append(favorable)
            row[f"mid_t+{h}s"] = mid_at_h
            row[f"favorable_t+{h}s"] = favorable

        rows_for_csv.append(row)
        if (i + 1) % 5 == 0:
            print(f"  processed {i+1}/{len(fills)} fills  "
                  f"(queries={n_queries}, empty={n_no_data})")

    ib.disconnect()
    print(f"Done. Total queries={n_queries}, empty={n_no_data}\n")

    # Report aggregate drift per horizon
    for h in HORIZONS_SEC:
        vals = drifts_by_horizon[h]
        if not vals:
            print(f"T+{h}s: no data")
            continue
        print(f"T+{h}s favorable drift (n={len(vals)}):")
        print_desc("favorable", vals)
    print()
    print("Interpretation: 'favorable' > 0 means mid moved our way after fill.")
    print("Negative favorable = adverse selection (they knew; we paid).")
    print("Symmetric distribution around 0 = noise only (no systematic edge).")

    # Optional CSV
    if csv_out and rows_for_csv:
        fields = ["ts", "symbol", "side", "size", "price", "theo",
                  "mid_fill", "edge_dollars"]
        for h in HORIZONS_SEC:
            fields.extend([f"mid_t+{h}s", f"favorable_t+{h}s"])
        with open(csv_out, "w", newline="") as fh:
            w = csv.DictWriter(fh, fieldnames=fields)
            w.writeheader()
            for r in rows_for_csv:
                w.writerow({k: r.get(k, "") for k in fields})
        print(f"\nPer-fill CSV written: {csv_out}")


# ── Main ───────────────────────────────────────────────────────────────
def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--fills-glob", default="logs-paper/fills-*.jsonl",
                    help="glob for fill JSONL files")
    ap.add_argument("--offline", action="store_true",
                    help="skip Phase 2 (no Gateway queries)")
    ap.add_argument("--csv-out", default=None,
                    help="write per-fill Phase 2 data to this CSV path")
    args = ap.parse_args()

    fills = load_fills(args.fills_glob)
    print(f"Loaded {len(fills)} fills from {args.fills_glob}")
    if not fills:
        print("No fills found. Nothing to analyze.")
        return 0

    # First & last fill times
    first_ts = fills[0].get("ts", "?")
    last_ts = fills[-1].get("ts", "?")
    print(f"Time range: {first_ts}  →  {last_ts}\n")

    run_phase1(fills)

    if not args.offline:
        asyncio.run(run_phase2(fills, csv_out=args.csv_out))
    else:
        print("\n[Phase 2 skipped — --offline]")

    return 0


if __name__ == "__main__":
    sys.exit(main())
