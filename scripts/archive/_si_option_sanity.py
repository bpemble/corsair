"""30-second sanity check: subscribe to SI options across a wide strike band
and report final bid/ask state per strike. Distinguishes "dead book" from
"no market data subscription"."""
import asyncio
import os
import sys
from ib_insync import IB, Future, FuturesOption

HOST = os.environ.get("CORSAIR_GATEWAY_HOST", "127.0.0.1")
PORT = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4002"))


async def main():
    ib = IB()
    await ib.client.connectAsync(HOST, PORT, 81, 30)

    # Resolve standard SI (tradingClass=SI) futures
    probe = Future("SI", exchange="COMEX", currency="USD")
    det = await ib.reqContractDetailsAsync(probe)
    futs = [d.contract for d in det
            if d.contract.tradingClass == "SI"
            and d.contract.lastTradeDateOrContractMonth >= "20260422"]
    underlying = min(futs, key=lambda c: c.lastTradeDateOrContractMonth)
    print(f"Underlying: {underlying.localSymbol} conId={underlying.conId}")

    und_ticker = ib.reqMktData(underlying, "", False, False)
    await asyncio.sleep(3)
    print(f"  Underlying bid={und_ticker.bid} ask={und_ticker.ask} last={und_ticker.last}")

    # Subscribe to 21 strikes around $33 (wide band)
    strikes = [round(30.0 + i * 0.25, 2) for i in range(0, 21)]  # 30.00 .. 35.00
    tickers = {}
    for k in strikes:
        for r in ("C", "P"):
            opt = FuturesOption(
                symbol="SI", lastTradeDateOrContractMonth="20260625",
                strike=k, right=r, exchange="COMEX",
                multiplier="5000", currency="USD", tradingClass="SO",
            )
            tickers[(k, r)] = opt

    await ib.qualifyContractsAsync(*tickers.values())
    q = sum(1 for t in tickers.values() if t.conId > 0)
    print(f"Qualified: {q}/{len(tickers)}")

    subs = {}
    for key, c in tickers.items():
        if c.conId > 0:
            subs[key] = ib.reqMktData(c, "", False, False)

    print("Observing 30s...")
    await asyncio.sleep(30)

    print("\n=== Final bid/ask state ===")
    any_live = False
    for k in strikes:
        row = f"  K={k:5.2f} "
        for r in ("C", "P"):
            tkr = subs.get((k, r))
            if not tkr:
                row += f"{r}: N/A  "
                continue
            b = tkr.bid or 0
            a = tkr.ask or 0
            if b > 0 or a > 0:
                any_live = True
                row += f"{r}: bid={b:7.4f} ask={a:7.4f}  "
            else:
                row += f"{r}: bid=---    ask=---      "
        print(row)
    print()
    if any_live:
        print("✓ At least some strikes have live bid/ask — market data subscription works")
    else:
        print("✗ All strikes bid=0 ask=0 — likely NO market data subscription for SI options on paper")

    ib.disconnect()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
