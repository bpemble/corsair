"""One-off: read FRMM stock bid/ask/last from paper Gateway."""
import asyncio
import os
from ib_insync import IB, Stock

async def main():
    ib = IB()
    port = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4002"))
    await ib.connectAsync("127.0.0.1", port, clientId=33, timeout=15)
    print(f"Connected on port {port}")

    stk = Stock("FRMM", "SMART", "USD")
    q = await ib.qualifyContractsAsync(stk)
    if not q or q[0].conId == 0:
        print("Could not qualify FRMM")
        ib.disconnect()
        return
    c = q[0]
    print(f"FRMM conId={c.conId} primaryExchange={c.primaryExchange}")

    t = ib.reqMktData(c, "", False, False)
    # Wait up to 5s for data to arrive
    for _ in range(50):
        await asyncio.sleep(0.1)
        if (t.bid is not None or t.ask is not None or t.last is not None
                or t.close is not None):
            # Check we have meaningful data
            if ((t.bid and t.bid > 0) or (t.ask and t.ask > 0)
                    or (t.last and t.last > 0) or (t.close and t.close > 0)):
                break
    print(f"FRMM:  bid={t.bid}  ask={t.ask}  last={t.last}  close={t.close}  "
          f"volume={t.volume}  bidSize={t.bidSize}  askSize={t.askSize}")
    ib.disconnect()

asyncio.run(main())
