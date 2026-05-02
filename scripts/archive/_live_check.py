"""Read-only snapshot: positions, open orders. With step-by-step logging
so we can see exactly where things hang if they do."""
import asyncio
import os
import sys
from ib_insync import IB

ACCOUNT = os.environ.get("IBKR_ACCOUNT") or os.environ.get("CORSAIR_ACCOUNT_ID")
PORT = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4001"))
CLIENT_ID = 55


async def main():
    print(f"[1] Starting. account={ACCOUNT} port={PORT} clientId={CLIENT_ID}", flush=True)
    ib = IB()
    try:
        await asyncio.wait_for(
            ib.connectAsync("127.0.0.1", PORT, clientId=CLIENT_ID, timeout=15),
            timeout=20,
        )
    except asyncio.TimeoutError:
        print("[!] connectAsync timed out", flush=True)
        return 1
    print(f"[2] Connected", flush=True)

    try:
        pos = await asyncio.wait_for(ib.reqPositionsAsync(), timeout=15)
    except asyncio.TimeoutError:
        print("[!] reqPositionsAsync timed out", flush=True)
        ib.disconnect()
        return 1
    print(f"[3] Got {len(pos)} positions total across all accounts", flush=True)

    mine = [p for p in pos if p.account == ACCOUNT and p.position != 0]
    print(f"[4] Non-zero positions on {ACCOUNT}: {len(mine)}", flush=True)
    for p in mine:
        c = p.contract
        print(f"    {c.secType} {c.symbol} K={c.strike} {c.right} "
              f"{c.lastTradeDateOrContractMonth} qty={p.position:+g} avg={p.avgCost:.4f}",
              flush=True)

    try:
        await asyncio.wait_for(ib.reqAllOpenOrdersAsync(), timeout=10)
    except asyncio.TimeoutError:
        print("[!] reqAllOpenOrdersAsync timed out", flush=True)
    await asyncio.sleep(2)
    print(f"[5] Checking open orders...", flush=True)
    opens = [t for t in ib.openTrades()
             if getattr(t.order, "account", None) == ACCOUNT
             and t.orderStatus.status in
             ("PendingSubmit", "PreSubmitted", "Submitted", "ApiPending")]
    print(f"[6] Live open orders on {ACCOUNT}: {len(opens)}", flush=True)
    for t in opens:
        c = t.contract
        print(f"    oid={t.order.orderId} {t.order.action} {t.order.totalQuantity} "
              f"{c.localSymbol} @{t.order.lmtPrice} status={t.orderStatus.status}",
              flush=True)

    print(f"[7] Done. Disconnecting.", flush=True)
    ib.disconnect()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
