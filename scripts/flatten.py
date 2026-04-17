"""One-shot: cancel all working orders, then flatten option positions.

Connects with clientId=0 (FA-required), cancels every open FOP order for the
configured product on the configured account (clears the per-contract
working-order limit), then sends MARKET orders sized to take each position
to zero.

    docker compose run --rm -v ~/corsair/scripts:/app/scripts corsair python3 /app/scripts/flatten.py

DO NOT run while corsair is up — stop the corsair container first.
"""
import asyncio
import os
import sys
import time

import yaml
from ib_insync import IB, MarketOrder

HOST = os.environ.get("CORSAIR_GATEWAY_HOST", "127.0.0.1")
PORT = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4002"))
ACCOUNT = os.environ.get("CORSAIR_ACCOUNT_ID") or os.environ.get("IBKR_ACCOUNT")
if not ACCOUNT:
    print("ERROR: CORSAIR_ACCOUNT_ID/IBKR_ACCOUNT not set")
    sys.exit(1)

# Read product symbol from config so scripts stay in sync with the engine.
_cfg_path = os.path.join(os.path.dirname(__file__), "..", "config", "corsair_v2_config.yaml")
with open(_cfg_path) as _f:
    SYMBOL = yaml.safe_load(_f)["product"]["underlying_symbol"]

print(f"Connecting to {HOST}:{PORT} as clientId=0, account={ACCOUNT}")
ib = IB()


async def _lean_connect():
    await ib.client.connectAsync(HOST, PORT, 0, 30)
    ib.reqAutoOpenOrders(True)  # required for clientId=0
    reqs = [
        ib.reqPositionsAsync(),
        ib.reqOpenOrdersAsync(),
        ib.reqAccountUpdatesAsync(ACCOUNT),
    ]
    await asyncio.gather(
        *(asyncio.wait_for(c, 20) for c in reqs),
        return_exceptions=True,
    )

ib.run(_lean_connect())
print(f"Connected. Server version {ib.client.serverVersion()}")

# Step 1: cancel every open order on ETHUSDRR FOP contracts across ALL
# clients. reqAllOpenOrders() pulls orders from every client on the account,
# which is what we need to clear orphans from prior corsair sessions that
# are clogging the per-contract working-order limit (max 15 per side).
ib.reqAllOpenOrders()
ib.sleep(3)  # let the cache populate from the response stream

open_trades = [
    t for t in ib.openTrades()
    if t.contract.symbol == SYMBOL
    and t.contract.secType == "FOP"
    and t.orderStatus.status in ("PendingSubmit", "PreSubmitted", "Submitted", "ApiPending")
]
print(f"Step 1: cancelling {len(open_trades)} working orders on {SYMBOL} options (all clients)...")
for t in open_trades:
    try:
        ib.cancelOrder(t.order)
    except Exception as e:
        print(f"  cancel failed: {e}")

# Some orders may need a global cancel via reqGlobalCancel. Try it as a
# belt-and-suspenders to nuke anything reqAllOpenOrders missed.
try:
    ib.reqGlobalCancel()
    print("  reqGlobalCancel sent")
except Exception as e:
    print(f"  reqGlobalCancel failed: {e}")

print("  waiting 8s for cancels to settle...")
ib.sleep(8)

# Step 2: pull current positions and flatten with MKT orders.
positions = [
    p for p in ib.positions(ACCOUNT)
    if p.contract.symbol == SYMBOL
    and p.contract.secType == "FOP"
    and int(p.position) != 0
]
print(f"\nStep 2: flattening {len(positions)} non-zero positions:")
for p in positions:
    c = p.contract
    print(f"  {c.right}{c.strike} {c.lastTradeDateOrContractMonth} qty={int(p.position)} avgCost={p.avgCost}")

if not positions:
    print("Nothing to flatten. Done.")
    ib.disconnect()
    sys.exit(0)

placed = []
for p in positions:
    c = p.contract
    qty = int(p.position)
    action = "SELL" if qty > 0 else "BUY"
    abs_qty = abs(qty)
    ib.qualifyContracts(c)
    order = MarketOrder(
        action=action,
        totalQuantity=abs_qty,
        account=ACCOUNT,
        orderRef=f"flatten_{c.right}{int(c.strike)}",
    )
    order.tif = "DAY"
    trade = ib.placeOrder(c, order)
    placed.append(trade)
    print(f"  -> {action} {abs_qty} {c.right}{c.strike} (orderId={trade.order.orderId})")

print(f"\nPlaced {len(placed)} flatten orders. Waiting up to 60s for fills...")
deadline = time.time() + 60
while time.time() < deadline:
    ib.sleep(1)
    pending = [t for t in placed if t.orderStatus.status not in ("Filled", "Cancelled", "Inactive")]
    if not pending:
        break

print("\nFinal status:")
for t in placed:
    s = t.orderStatus
    print(f"  oid={t.order.orderId} {t.order.action} {int(t.order.totalQuantity)} "
          f"{t.contract.right}{t.contract.strike} -> status={s.status} filled={s.filled} avgFill={s.avgFillPrice}")

ib.sleep(2)
remaining = [
    p for p in ib.positions(ACCOUNT)
    if p.contract.symbol == SYMBOL
    and p.contract.secType == "FOP"
    and int(p.position) != 0
]
print(f"\nRemaining non-zero {SYMBOL} option positions: {len(remaining)}")
for p in remaining:
    c = p.contract
    print(f"  {c.right}{c.strike} qty={int(p.position)}")

ib.disconnect()
print("Done.")
