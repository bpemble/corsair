#!/usr/bin/env python3
"""Persistent flatten — keeps trying until non-zero positions hit zero.

Stops the broker first (assumed already done), runs market orders, waits up
to 5 minutes per round, retries up to 5 rounds. Each round it queries fresh
position state so it picks up partial fills from prior rounds.

Use for thin-paper-market situations where flatten.py's 60s timeout doesn't
catch fills. Designed for HG specifically.
"""
import asyncio
import sys
from ib_insync import IB, MarketOrder, util

ACCOUNT = "DUP553657"
HOST = "127.0.0.1"
PORT = 4002
SYMBOL = "HG"
ROUND_TIMEOUT_S = 180
MAX_ROUNDS = 5


async def get_positions(ib):
    pos = ib.positions(ACCOUNT)
    return [p for p in pos if p.position != 0 and p.contract.symbol == SYMBOL]


async def submit_flatten(ib, positions):
    trades = []
    for p in positions:
        c = p.contract
        # Need exchange to place orders
        if not c.exchange:
            c.exchange = "COMEX"
        action = "SELL" if p.position > 0 else "BUY"
        qty = abs(int(p.position))
        order = MarketOrder(action, qty, account=ACCOUNT, tif="DAY")
        trade = ib.placeOrder(c, order)
        trades.append(trade)
        print(f"  → {action} {qty} {c.localSymbol} MARKET (orderId={trade.order.orderId})")
    return trades


async def main():
    ib = IB()
    await ib.connectAsync(HOST, PORT, clientId=99, account=ACCOUNT, timeout=15)
    print(f"connected to {HOST}:{PORT}")

    for round_idx in range(1, MAX_ROUNDS + 1):
        positions = await get_positions(ib)
        if not positions:
            print(f"\n✓ ROUND {round_idx}: nothing to flatten — done")
            break
        print(f"\n=== ROUND {round_idx}: {len(positions)} non-zero {SYMBOL} positions ===")
        for p in positions:
            print(f"  {p.contract.localSymbol}: {p.position:+.0f}")

        trades = await submit_flatten(ib, positions)
        deadline = asyncio.get_event_loop().time() + ROUND_TIMEOUT_S
        while asyncio.get_event_loop().time() < deadline:
            await asyncio.sleep(2)
            done = sum(1 for t in trades if t.orderStatus.status in ("Filled", "Cancelled", "Inactive"))
            if done == len(trades):
                break
        # Cancel any unfilled
        for t in trades:
            if t.orderStatus.status not in ("Filled", "Cancelled", "Inactive"):
                ib.cancelOrder(t.order)
        await asyncio.sleep(2)
        for t in trades:
            print(f"  result: {t.contract.localSymbol} status={t.orderStatus.status} filled={t.orderStatus.filled}")
    else:
        print(f"\n⚠ Hit MAX_ROUNDS={MAX_ROUNDS}, may still have residual")

    final = await get_positions(ib)
    print(f"\n=== FINAL STATE ===")
    if not final:
        print("✓ flat")
    else:
        for p in final:
            print(f"  {p.contract.localSymbol}: {p.position:+.0f}")
    ib.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
