"""Discover silver options contract spec on IBKR paper."""
import asyncio
import os
import sys
from ib_insync import IB, Future, FuturesOption

HOST = os.environ.get("CORSAIR_GATEWAY_HOST", "127.0.0.1")
PORT = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4002"))


async def main():
    ib = IB()
    await ib.client.connectAsync(HOST, PORT, 80, 30)
    print(f"Connected clientId=80")

    # Resolve the underlying SI futures
    for sym in ("SI",):
        print(f"\n--- Futures for symbol='{sym}' exchange=COMEX ---")
        probe = Future(sym, exchange="COMEX", currency="USD")
        det = await ib.reqContractDetailsAsync(probe)
        for d in det[:5]:
            c = d.contract
            print(f"  localSymbol={c.localSymbol}  expiry={c.lastTradeDateOrContractMonth}  "
                  f"tradingClass={c.tradingClass}  mult={c.multiplier}  conId={c.conId}")

    # Now try to find options — use loose FuturesOption and let IB return all
    for sym in ("SI",):
        print(f"\n--- Options for symbol='{sym}' (any expiry, any strike, any class) ---")
        probe = FuturesOption(symbol=sym, exchange="COMEX", currency="USD")
        try:
            det = await ib.reqContractDetailsAsync(probe)
        except Exception as e:
            print(f"  reqContractDetails error: {e}")
            det = []
        if not det:
            print("  NO results")
        seen_classes = {}
        for d in det[:200]:
            c = d.contract
            key = (c.tradingClass, c.multiplier, c.lastTradeDateOrContractMonth)
            if key not in seen_classes:
                seen_classes[key] = c
        print(f"  Total results: {len(det)}  Unique (tradingClass, multiplier, expiry): {len(seen_classes)}")
        # Print a handful of distinct classes and their next-3-expiries
        by_class = {}
        for (cls, mult, exp), c in seen_classes.items():
            by_class.setdefault((cls, mult), []).append(exp)
        for (cls, mult), exps in sorted(by_class.items()):
            exps_sorted = sorted(exps)[:5]
            print(f"    tradingClass={cls}  mult={mult}  next expiries: {exps_sorted}")

    ib.disconnect()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
