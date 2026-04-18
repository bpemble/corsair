"""One-shot: cancel working orders, then flatten option positions.

Connects with clientId=0 (FA master), cancels every open FOP order for
the target product on the configured account, then sends closing orders
(MARKET by default; marketable-limit with --order-type limit for thin
products like HG).

    # Flatten primary product (ETH) with market orders:
    docker compose run --rm corsair python3 /app/scripts/flatten.py

    # Flatten HG with marketable-limit orders, keep one residual:
    echo '{"HXEK6 C625": -1}' > /tmp/keep.json
    docker compose run --rm corsair python3 /app/scripts/flatten.py \\
        --product HG --order-type limit --keep-json /tmp/keep.json

DO NOT run while corsair is up — stop the corsair container first.
"""
import argparse
import asyncio
import json
import logging
import os
import sys

import yaml
from ib_insync import IB, LimitOrder, MarketOrder

logging.basicConfig(level=logging.INFO,
                    format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("flatten")

CFG_PATH = os.path.join(os.path.dirname(__file__), "..", "config",
                        "corsair_v2_config.yaml")


def _load_config_products():
    """Return (primary_symbol, {symbol: {tick_size}}) from the config file.

    tick_size lives under each product's ``quoting:`` block — top-level for
    the primary product, nested under each observe-products entry. Used for
    marketable-limit slippage calculation.
    """
    with open(CFG_PATH) as f:
        cfg = yaml.safe_load(f)
    primary = cfg["product"]["underlying_symbol"]
    products: dict = {
        primary: {"tick_size": float(cfg["quoting"].get("tick_size", 0.05))},
    }
    for entry in (cfg.get("observe_products") or []):
        q = entry.get("quoting") or {}
        products[entry["underlying_symbol"]] = {
            "tick_size": float(q.get("tick_size", 0.0005)),
        }
    return primary, products


def parse_args():
    primary, products = _load_config_products()
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--product", default=primary, choices=list(products),
                    help=f"Underlying symbol (default: {primary})")
    ap.add_argument("--order-type", choices=["market", "limit"], default="market",
                    help="MARKET (default) or marketable-LIMIT for thin products")
    ap.add_argument("--limit-slip-ticks", type=int, default=2,
                    help="Ticks past touch for limit orders (default 2)")
    ap.add_argument("--keep-json", default=None,
                    help="JSON {localSymbol: target_qty} of residuals to leave in place")
    ap.add_argument("--per-order-max", type=int, default=100,
                    help="Refuse any single order > this many contracts")
    ap.add_argument("--skip-cancel", action="store_true",
                    help="Skip the cancel-working-orders step")
    args = ap.parse_args()
    args._tick_size = products[args.product]["tick_size"]
    return args


async def _lean_connect(ib: IB, host: str, port: int, account: str):
    """Hand-rolled bootstrap mirroring src/connection.py (CLAUDE.md §5)."""
    await ib.client.connectAsync(host, port, 0, 30)
    ib.reqAutoOpenOrders(True)  # required for clientId=0 (FA master)
    await asyncio.gather(
        asyncio.wait_for(ib.reqPositionsAsync(), 20),
        asyncio.wait_for(ib.reqOpenOrdersAsync(), 20),
        asyncio.wait_for(ib.reqAccountUpdatesAsync(account), 20),
        return_exceptions=True,
    )
    log.info("Connected. Server version %s", ib.client.serverVersion())


async def _cancel_working_orders(ib: IB, symbol: str):
    """Cancel every FOP working order for *symbol* across all clients."""
    ib.reqAllOpenOrders()
    await asyncio.sleep(3)
    pending = [
        t for t in ib.openTrades()
        if t.contract.symbol == symbol
        and t.contract.secType == "FOP"
        and t.orderStatus.status in
        ("PendingSubmit", "PreSubmitted", "Submitted", "ApiPending")
    ]
    log.info("cancelling %d working orders on %s options...", len(pending), symbol)
    for t in pending:
        try:
            ib.cancelOrder(t.order)
        except Exception as e:
            log.warning("  cancel failed: %s", e)
    try:
        ib.reqGlobalCancel()
    except Exception as e:
        log.warning("reqGlobalCancel failed: %s", e)
    log.info("waiting 8s for cancels to settle...")
    await asyncio.sleep(8)


async def _fetch_bbo(ib: IB, contract, timeout: float = 4.0):
    """Subscribe to *contract* market data until we have a BBO, up to *timeout*."""
    await ib.qualifyContractsAsync(contract)
    t = ib.reqMktData(contract, "", False, False)
    deadline = asyncio.get_event_loop().time() + timeout
    while asyncio.get_event_loop().time() < deadline:
        if t.bid and t.ask and t.bid > 0 and t.ask > 0:
            return t.bid, t.ask
        await asyncio.sleep(0.1)
    return 0.0, 0.0


def _round_to_tick(price: float, tick: float) -> float:
    return round(round(price / tick) * tick, 6)


def _build_plan(positions, keep_targets, per_order_max):
    """Given IBKR positions and optional residual targets, return a list of
    (contract, side, qty, cur_qty, target_qty) tuples. Orders sized to take
    current → target (usually target=0 = full flatten)."""
    plan = []
    for p in positions:
        c = p.contract
        cur = int(p.position)
        target = int(keep_targets.get(c.localSymbol, 0))
        delta = target - cur  # +N → BUY, -N → SELL
        if delta == 0:
            continue
        side = "BUY" if delta > 0 else "SELL"
        qty = abs(delta)
        if qty > per_order_max:
            log.error("REFUSING %s qty=%d on %s — exceeds --per-order-max=%d",
                      side, qty, c.localSymbol, per_order_max)
            continue
        plan.append((c, side, qty, cur, target))
    return plan


async def main():
    args = parse_args()
    host = os.environ.get("CORSAIR_GATEWAY_HOST", "127.0.0.1")
    port = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4002"))
    account = os.environ.get("CORSAIR_ACCOUNT_ID") or os.environ.get("IBKR_ACCOUNT")
    if not account:
        log.error("CORSAIR_ACCOUNT_ID/IBKR_ACCOUNT env var must be set")
        return 1

    keep_targets: dict = {}
    if args.keep_json:
        with open(args.keep_json) as f:
            keep_targets = json.load(f)
        log.info("keep_targets loaded: %s", keep_targets)

    log.info("Connecting to %s:%d as clientId=0, account=%s, product=%s",
             host, port, account, args.product)
    ib = IB()
    await _lean_connect(ib, host, port, account)

    if not args.skip_cancel:
        await _cancel_working_orders(ib, args.product)

    positions = [
        p for p in ib.positions(account)
        if p.contract.symbol == args.product
        and p.contract.secType == "FOP"
        and int(p.position) != 0
    ]
    log.info("%d non-zero %s option position(s) before flatten",
             len(positions), args.product)
    for p in positions:
        c = p.contract
        log.info("  %s qty=%+d avgCost=%.4f",
                 c.localSymbol, int(p.position), p.avgCost)

    plan = _build_plan(positions, keep_targets, args.per_order_max)
    if not plan:
        log.info("Nothing to flatten — already at target residual.")
        ib.disconnect()
        return 0

    log.info("=" * 60)
    log.info("PLAN (%d orders, %s):", len(plan), args.order_type.upper())
    for c, side, qty, cur, tgt in plan:
        log.info("  %s %d %s  (cur=%+d → target=%+d)",
                 side, qty, c.localSymbol, cur, tgt)
    log.info("=" * 60)

    submitted = []
    for c, side, qty, cur, tgt in plan:
        try:
            await ib.qualifyContractsAsync(c)
            if args.order_type == "limit":
                bid, ask = await _fetch_bbo(ib, c)
                if bid <= 0 or ask <= 0:
                    log.warning("  SKIP %s — no BBO", c.localSymbol)
                    continue
                slip = args.limit_slip_ticks * args._tick_size
                if side == "SELL":
                    limit = max(_round_to_tick(bid - slip, args._tick_size),
                                args._tick_size)
                else:
                    limit = _round_to_tick(ask + slip, args._tick_size)
                order = LimitOrder(side, qty, limit, account=account, tif="DAY")
                log.info("  → %s %d %s @ %.4f (bid=%.4f ask=%.4f)",
                         side, qty, c.localSymbol, limit, bid, ask)
            else:
                order = MarketOrder(side, qty, account=account,
                                    orderRef=f"flatten_{c.right}{int(c.strike)}")
                order.tif = "DAY"
                log.info("  → %s %d %s  MARKET", side, qty, c.localSymbol)
            trade = ib.placeOrder(c, order)
            submitted.append(trade)
            # Wait briefly for each order to fill so margin frees up before
            # the next order's pre-trade check runs at IBKR.
            for _ in range(80):  # up to ~8s
                await asyncio.sleep(0.1)
                if trade.orderStatus.status in ("Filled", "Cancelled", "Inactive"):
                    break
            log.info("    status=%s filled=%d avg=%.4f",
                     trade.orderStatus.status,
                     int(trade.orderStatus.filled or 0),
                     trade.orderStatus.avgFillPrice or 0.0)
        except Exception as e:
            log.exception("  FAILED on %s: %s", c.localSymbol, e)

    log.info("=" * 60)
    filled = sum(1 for t in submitted if t.orderStatus.status == "Filled")
    log.info("Submitted: %d  Filled: %d  Other: %d",
             len(submitted), filled, len(submitted) - filled)

    await asyncio.sleep(2)
    remaining = [
        p for p in ib.positions(account)
        if p.contract.symbol == args.product
        and p.contract.secType == "FOP"
        and int(p.position) != 0
    ]
    log.info("Residual %s positions: %d", args.product, len(remaining))
    for p in remaining:
        c = p.contract
        log.info("  %s qty=%+d", c.localSymbol, int(p.position))

    ib.disconnect()
    return 0 if not remaining else 1


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        log.info("interrupted")
        sys.exit(1)
