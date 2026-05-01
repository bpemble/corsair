"""Observer probe: measure incumbent MM response latency.

Subscribes to HG front-month futures + a band of ATM option contracts,
records every bid/ask update with monotonic timestamps. Post-processes to
answer: for each underlying mid tick, how long until each option's BBO
updates? That delta = fastest incumbent MM's response latency (from our
perspective, which is the relevant latency because adverse selection risk
is measured in our wire-time).

Read-only (no order flow). Uses clientId=77 so it coexists with a running
corsair on clientId=0 — safe to run in parallel.

Usage:
    docker compose run --rm --no-deps \\
        -v $(pwd)/scripts:/app/scripts \\
        corsair python3 /app/scripts/mm_response_probe.py \\
        --duration 300 --atm 6.00

Output: per-option (strike, right) distribution of response latency in ms.
"""
import argparse
import asyncio
import os
import statistics
import sys
import time
from collections import defaultdict

import yaml
from ib_insync import IB, Future, FuturesOption

# ── Config ─────────────────────────────────────────────────────────────
# Config loading is optional — fully overridable via CLI args so the
# probe can run against any product (HG / SI / ETH / …) without needing
# a corsair config for that product. Loads hg_v1_4_paper.yaml as a
# default source for the HG product spec.
_cfg_name = os.environ.get("CORSAIR_CONFIG", "config/hg_v1_4_paper.yaml")
_cfg_path = os.path.join(os.path.dirname(__file__), "..", _cfg_name)
try:
    with open(_cfg_path) as f:
        _cfg = yaml.safe_load(f)
        _prod = _cfg["products"][0]["product"] if "products" in _cfg else _cfg["product"]
        DEFAULT_SYMBOL = _prod["underlying_symbol"]
        DEFAULT_OPT_SYMBOL = _prod.get("option_symbol", DEFAULT_SYMBOL)
        DEFAULT_TRADING_CLASS = _prod.get("trading_class", "")
        DEFAULT_EXCHANGE = _prod.get("exchange", "CME")
        DEFAULT_CURRENCY = _prod.get("currency", "USD")
        DEFAULT_MULTIPLIER = str(_prod["multiplier"])
except Exception:
    DEFAULT_SYMBOL = "HG"
    DEFAULT_OPT_SYMBOL = "HXE"
    DEFAULT_TRADING_CLASS = "HXE"
    DEFAULT_EXCHANGE = "COMEX"
    DEFAULT_CURRENCY = "USD"
    DEFAULT_MULTIPLIER = "25000"

HOST = os.environ.get("CORSAIR_GATEWAY_HOST", "127.0.0.1")
PORT = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4002"))
MATCH_WINDOW_MS = 2000          # ignore pairings longer than this


async def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--duration", type=int, default=300, help="observation seconds")
    ap.add_argument("--expiry", default="20260427", help="option expiry YYYYMMDD")
    ap.add_argument("--atm", type=float, required=True,
                    help="ATM strike (e.g. 6.10 for HG, 32 for SI, 1800 for ETH)")
    ap.add_argument("--n-strikes", type=int, default=2,
                    help="strikes on each side of ATM (total = 2*N+1 strikes × 2 rights)")
    ap.add_argument("--min-ticks", type=int, default=1,
                    help="ignore underlying moves smaller than this many ticks")
    # Product overrides (defaults come from CORSAIR_CONFIG which is HG)
    ap.add_argument("--symbol", default=DEFAULT_SYMBOL)
    ap.add_argument("--opt-symbol", default=DEFAULT_OPT_SYMBOL)
    ap.add_argument("--trading-class", default=DEFAULT_TRADING_CLASS)
    ap.add_argument("--exchange", default=DEFAULT_EXCHANGE)
    ap.add_argument("--currency", default=DEFAULT_CURRENCY)
    ap.add_argument("--multiplier", default=DEFAULT_MULTIPLIER)
    ap.add_argument("--strike-step", type=float, default=0.05,
                    help="nickel step for HG; set larger for SI (e.g. 0.25) or ETH (e.g. 25)")
    ap.add_argument("--underlying-tick", type=float, default=0.0005,
                    help="futures underlying tick (for min-ticks threshold)")
    ap.add_argument("--client-id", type=int, default=77,
                    help="IBKR clientId (use distinct ids for parallel probes)")
    ap.add_argument("--underlying-trading-class", default="",
                    help="filter futures by tradingClass (e.g. SI for standard silver "
                         "to exclude micro SIL when searching symbol=SI)")
    args = ap.parse_args()

    SYMBOL = args.symbol
    OPT_SYMBOL = args.opt_symbol
    TRADING_CLASS = args.trading_class
    EXCHANGE = args.exchange
    CURRENCY = args.currency
    MULTIPLIER = args.multiplier
    UNDERLYING_TICK = args.underlying_tick
    CLIENT_ID = args.client_id

    ib = IB()
    # Lean connect per CLAUDE.md §5 — stock connectAsync times out on FA
    # paper (multi-sub-account bootstrap). Observer clientIds don't
    # need positions/orders/accounts; market data only.
    await ib.client.connectAsync(HOST, PORT, CLIENT_ID, 30)
    print(f"Connected. Server version {ib.client.serverVersion()}  clientId={CLIENT_ID}  symbol={SYMBOL}")

    # Qualify underlying futures — options-expiry != futures-expiry, so
    # let IB pick the front-month contract via reqContractDetails instead
    # of guessing the date. Pick the nearest tradable expiry.
    probe = Future(SYMBOL, exchange=EXCHANGE, currency=CURRENCY)
    det = await ib.reqContractDetailsAsync(probe)
    if not det:
        print(f"Could not resolve {SYMBOL} futures", file=sys.stderr)
        ib.disconnect()
        return 1
    today = time.strftime("%Y%m%d")
    future_expiries = [
        d.contract for d in det
        if d.contract.lastTradeDateOrContractMonth >= today
        and (not args.underlying_trading_class
             or d.contract.tradingClass == args.underlying_trading_class)
    ]
    if not future_expiries:
        print(f"No future-dated {SYMBOL} contracts found", file=sys.stderr)
        ib.disconnect()
        return 1
    underlying = min(
        future_expiries, key=lambda c: c.lastTradeDateOrContractMonth,
    )
    print(f"Underlying: {underlying.localSymbol} conId={underlying.conId}")

    # Qualify N strikes × 2 rights
    contracts = {}
    for k_off in range(-args.n_strikes, args.n_strikes + 1):
        strike = round(args.atm + k_off * args.strike_step, 4)
        for right in ("C", "P"):
            opt = FuturesOption(
                symbol=OPT_SYMBOL, lastTradeDateOrContractMonth=args.expiry,
                strike=strike, right=right, exchange=EXCHANGE,
                multiplier=MULTIPLIER, currency=CURRENCY,
                tradingClass=TRADING_CLASS,
            )
            contracts[(strike, right)] = opt
    await ib.qualifyContractsAsync(*contracts.values())
    n_qualified = sum(1 for c in contracts.values() if c.conId > 0)
    print(f"Options qualified: {n_qualified}/{len(contracts)}")

    # Tick event storage
    und_events = []                                    # [(ts_ns, mid)]
    opt_events = defaultdict(list)                     # key -> [(ts_ns, bid, ask)]
    prev_und_mid = None
    prev_opt = {k: (None, None) for k in contracts}

    import math as _math

    def on_underlying(t):
        nonlocal prev_und_mid
        # IBKR sentinel values (bid/ask <= 0 or NaN) leak through the
        # updateEvent — 2026-04-19 investigation traced "only 1 event"
        # to NaN bid/ask poisoning prev_und_mid. Python's NaN<=0 returns
        # False, so the explicit isnan check is load-bearing.
        if t.bid is None or t.ask is None or t.bid <= 0 or t.ask <= 0:
            return
        if _math.isnan(t.bid) or _math.isnan(t.ask):
            return
        mid = (t.bid + t.ask) / 2
        threshold = UNDERLYING_TICK * args.min_ticks - 1e-9
        if args.min_ticks <= 0:
            threshold = 1e-9
        if prev_und_mid is None or abs(mid - prev_und_mid) >= threshold:
            und_events.append((time.monotonic_ns(), mid))
            prev_und_mid = mid

    def on_option(t, key):
        pb, pa = prev_opt[key]
        if t.bid is not None and t.ask is not None and t.bid > 0 and t.ask > 0:
            if t.bid != pb or t.ask != pa:
                opt_events[key].append((time.monotonic_ns(), t.bid, t.ask))
                prev_opt[key] = (t.bid, t.ask)

    und_ticker = ib.reqMktData(underlying, "", False, False)
    und_ticker.updateEvent += on_underlying
    for key, opt in contracts.items():
        if opt.conId == 0:
            continue
        t = ib.reqMktData(opt, "", False, False)
        t.updateEvent += (lambda tk, k=key: on_option(tk, k))

    print(f"Observing for {args.duration}s...")
    await asyncio.sleep(args.duration)
    ib.disconnect()

    # ── Post-process ────────────────────────────────────────────────────
    print()
    print(f"Underlying mid-change events recorded: {len(und_events)}")
    print(f"Option BBO-change events per key:")
    for key in sorted(contracts):
        print(f"  {key[0]:.2f}{key[1]}: {len(opt_events[key])}")
    print()

    if not und_events:
        print("No underlying ticks observed. Market may be quiet or data feed stale.")
        return 0

    print("=" * 72)
    print(f"{'Strike/Right':<15}{'n':>6}{'mean':>8}{'p10':>8}{'p50':>8}{'p90':>8}{'p99':>8}")
    print("-" * 72)
    WINDOW_NS = MATCH_WINDOW_MS * 1_000_000
    for key in sorted(contracts):
        o_events = opt_events[key]
        if not o_events:
            continue
        # For each underlying tick, find next option BBO event on this key.
        # No dedup on option events — same event may be paired to multiple
        # underlying ticks if the option was slow to respond to a burst.
        # That's correct: it measures how stale the option quote was across
        # each underlying tick, which is the adverse-selection-relevant view.
        latencies_ms = []
        for u_ts, _ in und_events:
            # Find first option event strictly after u_ts via lower_bound scan
            for o_ts, _, _ in o_events:
                if o_ts > u_ts:
                    dt_ns = o_ts - u_ts
                    if dt_ns < WINDOW_NS:
                        latencies_ms.append(dt_ns / 1_000_000.0)
                    break
        if not latencies_ms:
            continue
        s = sorted(latencies_ms)
        n = len(s)
        p10 = s[n // 10] if n >= 10 else s[0]
        p50 = s[n // 2]
        p90 = s[min(n - 1, n * 9 // 10)]
        p99 = s[min(n - 1, n * 99 // 100)]
        mean = statistics.mean(s)
        print(f"{key[0]:.2f}{key[1]:<10}{n:>6}{mean:>8.1f}{p10:>8.1f}{p50:>8.1f}{p90:>8.1f}{p99:>8.1f}")
    print("=" * 72)
    print("Units: milliseconds. Values = wall-clock from underlying mid-tick")
    print("arrival at our process to the next BBO change on that option, also")
    print("measured at our process. Lower = faster incumbent MM response.")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
