"""Multi-order amend-latency probe.

Like amend_latency_probe.py but places N concurrent resting orders and
runs a round-robin modify loop across them. Simulates corsair's loaded
pipeline conditions (many orders being modified frequently) from a
standalone script — lets us isolate gateway vs corsair without turning
corsair on.

All orders are SELL limit asks on DEEP-OTM strikes at a price far above
any realistic market bid (starts at $1.00, steps down by one tick per
modify). For HG OTM calls worth ~$0.001-$0.05, a $1.00 ask is 20-1000×
above fair — guaranteed unfillable.

Uses clientId=0 (FA master); DO NOT run while corsair is up. Stop first:
    docker compose stop corsair

Usage:
    docker compose run --rm --no-deps \\
        -v $(pwd)/scripts:/app/scripts \\
        corsair python3 /app/scripts/multi_amend_probe.py \\
        --n-orders 15 --rounds 10
"""
import argparse
import asyncio
import os
import statistics
import sys
import time

import yaml
from ib_insync import IB, FuturesOption, LimitOrder

_cfg_name = os.environ.get("CORSAIR_CONFIG", "config/hg_v1_4_paper.yaml")
_cfg_path = os.path.join(os.path.dirname(__file__), "..", _cfg_name)
with open(_cfg_path) as f:
    _cfg = yaml.safe_load(f)
    _prod = _cfg["products"][0]["product"] if "products" in _cfg else _cfg["product"]
    SYMBOL = _prod["underlying_symbol"]
    OPT_SYMBOL = _prod.get("option_symbol", SYMBOL)
    TRADING_CLASS = _prod.get("trading_class", "")
    EXCHANGE = _prod.get("exchange", "CME")
    CURRENCY = _prod.get("currency", "USD")
    MULTIPLIER = str(_prod["multiplier"])

HOST = os.environ.get("CORSAIR_GATEWAY_HOST", "127.0.0.1")
PORT = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4002"))
ACCOUNT = os.environ.get("CORSAIR_ACCOUNT_ID") or os.environ.get("IBKR_ACCOUNT")
if not ACCOUNT:
    print("ERROR: CORSAIR_ACCOUNT_ID/IBKR_ACCOUNT env var must be set")
    sys.exit(1)

# Deep-OTM targets: 7 OTM puts + 8 OTM calls, all worth pennies at F~6.02.
OTM_PUT_STRIKES = [5.55, 5.50, 5.45, 5.40, 5.35, 5.30, 5.25]
OTM_CALL_STRIKES = [6.60, 6.65, 6.70, 6.75, 6.80, 6.85, 6.90, 6.95]
TARGETS = ([(k, "P") for k in OTM_PUT_STRIKES]
           + [(k, "C") for k in OTM_CALL_STRIKES])
EXPIRY = "20260427"
TICK_SIZE = 0.0005
PRICE_DECIMALS = 4

# ── BUY-side mode ──────────────────────────────────────────────────────
# BUY LimitOrder at 1 tick, cycling between 0.0005 ↔ 0.0010 on each modify.
# Market ask for these deep-OTM options is typically $0.005-$0.05 — our
# bid at 0.0005-0.0010 is 10-100× *below* any realistic ask, guaranteed
# unfillable. BUY-side also uses premium-only margin (max $25/contract at
# 0.0010 × 25000), which fits inside any funded commodities account (vs
# SELL-side which required ~$3k margin per order — rejected on the
# original $14k live test).
ACTION = "BUY"
START_PRICE = TICK_SIZE           # 0.0005 (1 tick)
PRICE_HIGH = TICK_SIZE * 2        # 0.0010 (2 ticks)
# Max premium exposure if impossibly filled: qty × PRICE_HIGH × multiplier
# = 1 × 0.0010 × 25000 = $25/contract × 15 orders = $375 absolute max.


async def lean_connect(ib: IB):
    await ib.client.connectAsync(HOST, PORT, 0, 30)
    ib.reqAutoOpenOrders(True)
    await asyncio.gather(
        asyncio.wait_for(ib.reqPositionsAsync(), 20),
        asyncio.wait_for(ib.reqOpenOrdersAsync(), 20),
        asyncio.wait_for(ib.reqAccountUpdatesAsync(ACCOUNT), 20),
        return_exceptions=True,
    )


async def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--n-orders", type=int, default=15,
                    help=f"concurrent orders (max {len(TARGETS)})")
    ap.add_argument("--rounds", type=int, default=10,
                    help="modify cycles across all N orders")
    ap.add_argument("--round-delay", type=float, default=0.2,
                    help="seconds between rounds")
    args = ap.parse_args()

    n = min(args.n_orders, len(TARGETS))
    targets = TARGETS[:n]
    total_modifies = n * args.rounds

    ib = IB()
    print(f"Connecting {HOST}:{PORT} clientId=0 account={ACCOUNT}")
    await lean_connect(ib)
    print(f"Connected. Server version {ib.client.serverVersion()}")
    print(f"Plan: {n} orders, {args.rounds} rounds = {total_modifies} modifies "
          f"(round-robin, {args.round_delay}s gap between rounds)")

    # Qualify all contracts
    contracts = []
    for strike, right in targets:
        c = FuturesOption(
            symbol=OPT_SYMBOL, lastTradeDateOrContractMonth=EXPIRY,
            strike=strike, right=right, exchange=EXCHANGE,
            multiplier=MULTIPLIER, currency=CURRENCY,
            tradingClass=TRADING_CLASS,
        )
        contracts.append(c)
    await ib.qualifyContractsAsync(*contracts)
    contracts = [c for c in contracts if c.conId > 0]
    if len(contracts) < n:
        print(f"WARN: only {len(contracts)}/{n} contracts qualified — continuing")
        n = len(contracts)

    # Pre-flight ask snapshot: drop any contract whose current ask
    # isn't at least MIN_ASK_MULT × our highest bid ($0.0010). Ensures
    # that if any order somehow fills, we paid ≤ (1/MIN_ASK_MULT) × mid
    # — i.e. a profitable buy.
    MIN_ASK_MULT = 2
    min_ask = PRICE_HIGH * MIN_ASK_MULT  # $0.002
    print(f"\nPre-flight: requiring ask ≥ ${min_ask:.4f} (= {MIN_ASK_MULT}× our max bid)")
    tickers = [ib.reqMktData(c, '', False, False) for c in contracts]
    await asyncio.sleep(3.0)
    survivors = []
    for c, t in zip(contracts, tickers):
        ask = t.ask if (t.ask is not None and t.ask > 0) else None
        ok = ask is not None and ask >= min_ask
        status = "KEEP" if ok else "SKIP"
        print(f"  {status} {c.localSymbol}: ask={ask}")
        ib.cancelMktData(c)
        if ok:
            survivors.append(c)
    contracts = survivors
    n = len(contracts)
    if n == 0:
        print("No contracts passed pre-flight — aborting (live market too tight).")
        ib.disconnect()
        return 1
    total_modifies = n * args.rounds
    print(f"\n{n} contracts passed pre-flight. Proceeding.")

    # ── Event plumbing: match openOrderEvent by (oid, price) ────────────
    pending = {}        # (oid, price) -> sent_ns
    acks = {}           # (oid, price) -> ack_ns
    place_rtts = []
    amend_rtts = []
    ack_event = asyncio.Event()

    def on_open_order(t):
        key = (t.order.orderId, round(t.order.lmtPrice, PRICE_DECIMALS))
        if key in pending:
            acks[key] = time.monotonic_ns()
            ack_event.set()

    ib.openOrderEvent += on_open_order

    # ── Phase 1: place N orders at START_ASK ────────────────────────────
    def canonical(oid):
        latest = None
        for t in ib.openTrades():
            if t.order.orderId == oid:
                latest = t
        return latest

    oid_by_contract = {}
    for c in contracts:
        order = LimitOrder(
            action=ACTION, totalQuantity=1, lmtPrice=START_PRICE, tif="DAY",
            account=ACCOUNT, orderRef=f"probe_{c.localSymbol}",
        )
        sent_ns = time.monotonic_ns()
        trade = ib.placeOrder(c, order)
        oid = trade.order.orderId
        oid_by_contract[c.conId] = (oid, c)
        pending[(oid, round(START_PRICE, PRICE_DECIMALS))] = sent_ns
        print(f"  placed {c.localSymbol} {ACTION} @{START_PRICE} -> orderId={oid}")

    # Wait for all places to ack (up to 20s)
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        done = sum(
            1 for (oid, p), sent in pending.items()
            if (oid, p) in acks
        )
        if done >= n:
            break
        await asyncio.sleep(0.05)
    for (oid, p), sent in list(pending.items()):
        if (oid, p) in acks:
            rtt_us = (acks[(oid, p)] - sent) // 1000
            place_rtts.append(rtt_us)
    pending.clear()
    acks.clear()
    print(f"\nPlace RTTs (n={len(place_rtts)}): "
          f"min={min(place_rtts)/1000:.1f}ms p50={sorted(place_rtts)[len(place_rtts)//2]/1000:.1f}ms "
          f"max={max(place_rtts)/1000:.1f}ms")

    # ── Phase 2: round-robin modifies ───────────────────────────────────
    # Cycle price: START_PRICE → PRICE_HIGH → START_PRICE → ... Each round's
    # price differs from previous, so IBKR sees a real modify every time.
    # Both values stay far below realistic market ask — unfillable.
    print(f"\nRunning {args.rounds} rounds of modifies across {n} orders "
          f"(cycling {START_PRICE}↔{PRICE_HIGH})...")
    for r in range(args.rounds):
        new_price = round(PRICE_HIGH if r % 2 == 0 else START_PRICE,
                          PRICE_DECIMALS)
        round_sent_count = 0
        for oid, c in oid_by_contract.values():
            t = canonical(oid)
            if t is None:
                continue
            t.order.lmtPrice = new_price
            sent_ns = time.monotonic_ns()
            pending[(oid, new_price)] = sent_ns
            try:
                ib.placeOrder(c, t.order)
                round_sent_count += 1
            except Exception as e:
                print(f"  modify error oid={oid}: {e}")
                pending.pop((oid, new_price), None)
        # Wait for this round's acks with deadline
        round_deadline = time.monotonic() + 5
        while time.monotonic() < round_deadline:
            acked = sum(
                1 for (oid, p) in pending
                if (oid, p) in acks
            )
            if acked >= round_sent_count:
                break
            await asyncio.sleep(0.01)
        # Collect samples from this round
        for (oid, p), sent in list(pending.items()):
            if (oid, p) in acks:
                amend_rtts.append((acks[(oid, p)] - sent) // 1000)
                pending.pop((oid, p), None)
                acks.pop((oid, p), None)
        print(f"  round {r+1:2d}: sent={round_sent_count:2d} "
              f"acked={len([k for k in pending if k in acks])}  "
              f"total_samples={len(amend_rtts)}  price={new_price}")
        await asyncio.sleep(args.round_delay)

    # ── Cleanup: cancel all resting orders ──────────────────────────────
    print("\nCancelling all probe orders...")
    for oid, c in oid_by_contract.values():
        t = canonical(oid)
        if t is not None:
            ib.cancelOrder(t.order)
    await asyncio.sleep(2)
    ib.disconnect()

    # ── Report ──────────────────────────────────────────────────────────
    print()
    print("=" * 60)
    if place_rtts:
        s = sorted(place_rtts)
        n_ = len(s)
        print(f"Place RTT (n={n_}):")
        print(f"  mean = {statistics.mean(s)/1000:.1f}ms")
        print(f"  p50  = {s[n_//2]/1000:.1f}ms")
        print(f"  p90  = {s[min(n_-1, n_*9//10)]/1000:.1f}ms")
        print(f"  p99  = {s[min(n_-1, n_*99//100)]/1000:.1f}ms")
        print(f"  min/max = {s[0]/1000:.1f}ms / {s[-1]/1000:.1f}ms")
    if amend_rtts:
        s = sorted(amend_rtts)
        n_ = len(s)
        print(f"\nAmend RTT UNDER LOAD (n orders={n}, n samples={n_}):")
        print(f"  mean = {statistics.mean(s)/1000:.1f}ms")
        print(f"  p10  = {s[n_//10]/1000:.1f}ms" if n_ >= 10 else "")
        print(f"  p50  = {s[n_//2]/1000:.1f}ms")
        print(f"  p90  = {s[min(n_-1, n_*9//10)]/1000:.1f}ms")
        print(f"  p99  = {s[min(n_-1, n_*99//100)]/1000:.1f}ms")
        print(f"  min/max = {s[0]/1000:.1f}ms / {s[-1]/1000:.1f}ms")
    print("=" * 60)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        print("interrupted")
        sys.exit(1)
