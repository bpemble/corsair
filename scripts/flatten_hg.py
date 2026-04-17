"""One-shot HG-position flatten utility.

Connects to the IB Gateway on a fresh clientId, walks DUP553657's HG
positions, and submits marketable-limit closes for everything except a
small monitoring residual:

    KEEP = {
        ("HG", "20260424"...,  5.80, "P", +1),  # 1× HXEK6 P5.80 long
        ("HG", "20260424"..., 6.15, "C", -1),   # 1× HXEK6 C6.15 short
    }

Run with `corsair` stopped so the quote engine doesn't race or re-grow
inventory as IBKR margin frees up. Sized for paper deployment; not for
production trading without a second pair of eyes.
"""

import asyncio
import logging
import sys
from typing import Dict, Tuple

from ib_insync import IB, LimitOrder, Ticker

logging.basicConfig(level=logging.INFO,
                    format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("flatten_hg")

GATEWAY_HOST = "127.0.0.1"
GATEWAY_PORT = 4002
CLIENT_ID = 87  # fresh id; IBKR holds stale sessions briefly on disconnect
ACCOUNT = "DUP553657"
PRODUCT = "HG"  # IBKR contract.symbol for HG/HXE futures options

# (localSymbol, target_residual_qty). Anything not listed gets flattened to 0.
# Negative residual means a short, positive a long. The script computes the
# delta vs current and orders that many contracts in the closing direction.
KEEP_TARGETS: Dict[str, int] = {
    # Leave any active live-quoting positions in place; close everything
    # else (including the previous monitoring residual P580 / C615 that the
    # operator asked to flatten on 2026-04-14 13:30).
    "HXEK6 C625": -1,
}

# Hard sanity cap: refuse to send any single order > this many contracts.
# Defends against a math error sending a 1000-lot.
PER_ORDER_MAX = 100

# Marketable-limit slippage in TICKS past the touch (sells go below bid by
# this many ticks, buys go above ask by this many ticks). HG tick = $0.0005.
# 2 ticks = $0.001 = $25/contract. Cheap insurance the order crosses.
SLIP_TICKS = 2
TICK = 0.0005


async def get_ticker(ib: IB, contract, timeout: float = 4.0) -> Ticker:
    """Subscribe to a contract's market data and wait until we have a BBO.

    Uses qualifyContractsAsync (NOT the sync qualifyContracts) because we're
    inside an asyncio.run loop; the sync wrapper tries to spin up its own
    loop and crashes with "This event loop is already running".
    """
    await ib.qualifyContractsAsync(contract)
    t = ib.reqMktData(contract, "", False, False)
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if t.bid and t.ask and t.bid > 0 and t.ask > 0:
            return t
        await asyncio.sleep(0.1)
    return t  # may be incomplete; caller decides


def _round_to_tick(price: float) -> float:
    """Round to nearest HG option tick ($0.0005)."""
    return round(round(price / TICK) * TICK, 4)


async def lean_connect(ib: IB):
    """Hand-rolled bootstrap (mirrors src/connection.py CLAUDE.md §5).

    ib_insync's stock connectAsync issues completed-orders + per-sub-account
    multi-account requests that consistently time out on FA paper accounts.
    We only need: TCP+API handshake → reqPositionsAsync → reqAccountUpdates.
    """
    await ib.client.connectAsync(GATEWAY_HOST, GATEWAY_PORT, CLIENT_ID, 15)
    await asyncio.gather(
        asyncio.wait_for(ib.reqPositionsAsync(), 15),
        asyncio.wait_for(ib.reqAccountUpdatesAsync(ACCOUNT), 15),
    )
    if not ib.client.isReady():
        raise RuntimeError("API socket not ready after lean bootstrap")
    log.info("Connected (lean bootstrap). Server version: %s", ib.client.serverVersion())


async def main():
    ib = IB()
    log.info("Connecting to %s:%d clientId=%d", GATEWAY_HOST, GATEWAY_PORT, CLIENT_ID)
    await lean_connect(ib)

    # Pull positions for the target sub-account and filter to HG options only.
    ib_positions = ib.positions(ACCOUNT)
    hg_pos = []
    for p in ib_positions:
        c = p.contract
        if c.symbol != PRODUCT or c.secType != "FOP":
            continue
        if c.right not in ("C", "P"):
            continue
        if int(p.position) == 0:
            continue
        hg_pos.append(p)

    log.info("Found %d HG positions on %s", len(hg_pos), ACCOUNT)
    for p in hg_pos:
        log.info("  %s qty=%+d avgCost=%.4f",
                 p.contract.localSymbol, int(p.position), p.avgCost)

    # Compute close orders.
    plan = []  # (contract, side BUY/SELL, qty, current_qty, target_qty)
    for p in hg_pos:
        cur = int(p.position)
        target = KEEP_TARGETS.get(p.contract.localSymbol, 0)
        delta = target - cur  # +N → buy N, -N → sell N
        if delta == 0:
            continue
        side = "BUY" if delta > 0 else "SELL"
        qty = abs(delta)
        if qty > PER_ORDER_MAX:
            log.error("REFUSING to send %s qty=%d on %s — exceeds PER_ORDER_MAX=%d",
                      side, qty, p.contract.localSymbol, PER_ORDER_MAX)
            continue
        plan.append((p.contract, side, qty, cur, target))

    if not plan:
        log.info("Nothing to do — already at target residual.")
        ib.disconnect()
        return

    log.info("=" * 60)
    log.info("PLAN (%d orders):", len(plan))
    for c, side, qty, cur, tgt in plan:
        log.info("  %s %d %s  (cur=%+d → target=%+d)",
                 side, qty, c.localSymbol, cur, tgt)
    log.info("=" * 60)

    # Place orders one at a time so each fill releases margin before the next
    # order's pre-trade margin check runs at IBKR.
    submitted = []
    for c, side, qty, cur, tgt in plan:
        try:
            t = await get_ticker(ib, c)
            bid, ask = t.bid or 0.0, t.ask or 0.0
            if bid <= 0 or ask <= 0:
                log.warning("  SKIP %s — no BBO (bid=%s ask=%s)", c.localSymbol, bid, ask)
                continue
            # Marketable limit: sells go below bid, buys go above ask.
            if side == "SELL":
                limit = max(_round_to_tick(bid - SLIP_TICKS * TICK), TICK)
            else:
                limit = _round_to_tick(ask + SLIP_TICKS * TICK)
            order = LimitOrder(side, qty, limit, account=ACCOUNT, tif="DAY")
            log.info("  → %s %d %s @ %.4f (bid=%.4f ask=%.4f)",
                     side, qty, c.localSymbol, limit, bid, ask)
            trade = ib.placeOrder(c, order)
            submitted.append(trade)
            # Wait briefly for fill / status update before next order so each
            # fill propagates to IBKR's account-margin view.
            for _ in range(80):  # up to ~8s
                await asyncio.sleep(0.1)
                if trade.orderStatus.status in ("Filled", "Cancelled", "Inactive"):
                    break
            log.info("    status=%s filled=%d remaining=%d avg=%.4f",
                     trade.orderStatus.status,
                     int(trade.orderStatus.filled),
                     int(trade.orderStatus.remaining),
                     trade.orderStatus.avgFillPrice or 0.0)
        except Exception as e:
            log.exception("  FAILED on %s: %s", c.localSymbol, e)

    # Final summary.
    log.info("=" * 60)
    filled = sum(1 for t in submitted if t.orderStatus.status == "Filled")
    log.info("Submitted: %d  Filled: %d  Other: %d",
             len(submitted), filled, len(submitted) - filled)

    # Refresh and report residual positions.
    await asyncio.sleep(2)
    final = ib.positions(ACCOUNT)
    log.info("Residual HG positions:")
    for p in final:
        c = p.contract
        if c.symbol == PRODUCT and c.secType == "FOP" and int(p.position) != 0:
            log.info("  %s qty=%+d", c.localSymbol, int(p.position))

    ib.disconnect()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        log.info("interrupted")
        sys.exit(1)
