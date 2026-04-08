"""One-shot script to flatten all ETHUSDRR option positions in the configured
account using market orders. Use when corsair is stopped — it grabs its own
client_id so it won't conflict with a running engine.

Usage (from inside the corsair container with ib_insync available):
    python3 /app/scripts/flatten_positions.py

Or via docker:
    docker run --rm --network host -v $PWD:/app corsair2-corsair \
        python3 /app/scripts/flatten_positions.py
"""

import asyncio
import os
import sys

from ib_insync import IB, FuturesOption, MarketOrder

GATEWAY_HOST = os.environ.get("CORSAIR_GATEWAY_HOST", "127.0.0.1")
GATEWAY_PORT = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4002"))
ACCOUNT_ID = os.environ.get("CORSAIR_ACCOUNT_ID", "")
CLIENT_ID = 99   # distinct from corsair's client_id=1


async def main():
    if not ACCOUNT_ID:
        print("ERROR: CORSAIR_ACCOUNT_ID env var must be set", file=sys.stderr)
        return 1

    ib = IB()
    print(f"Connecting to {GATEWAY_HOST}:{GATEWAY_PORT} as client {CLIENT_ID}...")
    await ib.connectAsync(GATEWAY_HOST, GATEWAY_PORT, clientId=CLIENT_ID, timeout=15)
    print(f"Connected. Server version: {ib.client.serverVersion()}")
    print(f"Filtering to account: {ACCOUNT_ID}")

    # Pull positions for ONLY the configured account
    raw_positions = ib.positions(ACCOUNT_ID)
    eth_options = []
    for p in raw_positions:
        c = p.contract
        if p.account != ACCOUNT_ID:
            continue
        if c.symbol != "ETHUSDRR" or c.secType != "FOP":
            continue
        if c.right not in ("C", "P"):
            continue
        if int(p.position) == 0:
            continue
        eth_options.append((c, int(p.position), p.account))

    if not eth_options:
        print("No ETHUSDRR option positions to flatten.")
        ib.disconnect()
        return 0

    print(f"\n{len(eth_options)} position(s) to flatten:")
    for c, qty, acct in eth_options:
        print(f"  {acct} {int(c.strike)}{c.right} qty={qty:+d}")

    # Re-qualify contracts so they have exchange/tradingClass populated
    # (Position contracts come back without these and IB rejects orders).
    print("\nQualifying contracts...")
    contracts = [c for c, _, _ in eth_options]
    await ib.qualifyContractsAsync(*contracts)

    # Submit market orders to flatten each
    print("\nSubmitting market orders...")
    trades = []
    for c, qty, acct in eth_options:
        action = "SELL" if qty > 0 else "BUY"
        order = MarketOrder(
            action=action,
            totalQuantity=abs(qty),
            account=acct,
            orderRef="corsair2_flatten",
        )
        trade = ib.placeOrder(c, order)
        trades.append(trade)
        print(f"  {action} {abs(qty)} {int(c.strike)}{c.right} -> orderId {trade.order.orderId}")

    # Wait for fills (or 30s timeout)
    print("\nWaiting up to 30s for fills...")
    deadline = asyncio.get_event_loop().time() + 30.0
    while asyncio.get_event_loop().time() < deadline:
        await asyncio.sleep(0.5)
        all_done = all(t.orderStatus.status in ("Filled", "Cancelled", "ApiCancelled")
                       for t in trades)
        if all_done:
            break

    print("\nResults:")
    any_unfilled = False
    for t in trades:
        c = t.contract
        s = t.orderStatus
        line = f"  {int(c.strike)}{c.right} {t.order.action} {int(t.order.totalQuantity)} -> status={s.status}"
        if s.status == "Filled":
            line += f" filled@{s.avgFillPrice:.2f}"
        else:
            any_unfilled = True
        print(line)

    # Re-check positions
    print("\nFinal positions check:")
    raw_positions = ib.positions(ACCOUNT_ID) if ACCOUNT_ID else ib.positions()
    remaining = [p for p in raw_positions
                 if p.contract.symbol == "ETHUSDRR"
                 and p.contract.secType == "FOP"
                 and int(p.position) != 0]
    if not remaining:
        print("  ALL FLAT ✓")
    else:
        print(f"  {len(remaining)} position(s) still open:")
        for p in remaining:
            print(f"    {int(p.contract.strike)}{p.contract.right} qty={int(p.position):+d}")

    ib.disconnect()
    return 0 if not any_unfilled and not remaining else 1


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
