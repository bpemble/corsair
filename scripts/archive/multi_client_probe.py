"""Multi-clientId parallelism probe.

Opens N concurrent IB client connections on different clientIds (0, 1, ...,
N-1), each places M deep-OTM BUY orders concurrently. Measures:

1. Whether orders placed on non-master clientIds receive their own
   openOrderEvent acks on FA accounts (CLAUDE.md §1 routing concern).
2. Aggregate wall-clock throughput vs single-client baseline.

All BUY orders use pre-flight filter (ask ≥ 2× our max bid = $0.0020) to
guarantee profitable-if-filled. Orders cancelled at end.

Requires corsair stopped (clientId=0 collision).

Usage:
    docker compose run --rm --no-deps \\
        -v $(pwd)/scripts:/app/scripts \\
        corsair python3 /app/scripts/multi_client_probe.py \\
        --n-clients 3 --orders-per-client 5
"""
import argparse
import asyncio
import os
import statistics
import sys
import time
from collections import defaultdict

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
    print("ERROR: IBKR_ACCOUNT env var required"); sys.exit(1)

OTM_STRIKES = [
    ("P", 5.55), ("P", 5.50), ("P", 5.45), ("P", 5.40), ("P", 5.35),
    ("P", 5.30), ("P", 5.25), ("C", 6.60), ("C", 6.65), ("C", 6.70),
    ("C", 6.75), ("C", 6.80), ("C", 6.85), ("C", 6.90), ("C", 6.95),
]
EXPIRY = "20260427"
TICK = 0.0005
PRICE_DECIMALS = 4
BID_PRICE = TICK  # $0.0005
MIN_ASK_MULT = 2  # require ask >= 2 × BID_PRICE = $0.0010 → profitable vs mid


async def lean_connect(ib: IB, client_id: int):
    """Minimal bootstrap per CLAUDE.md §5."""
    await ib.client.connectAsync(HOST, PORT, client_id, 30)
    if client_id == 0:
        ib.reqAutoOpenOrders(True)
    await asyncio.gather(
        asyncio.wait_for(ib.reqPositionsAsync(), 20),
        asyncio.wait_for(ib.reqOpenOrdersAsync(), 20),
        asyncio.wait_for(ib.reqAccountUpdatesAsync(ACCOUNT), 20),
        return_exceptions=True,
    )


async def preflight_filter(ib: IB, contracts):
    """Drop contracts whose current ask < MIN_ASK_MULT × BID_PRICE."""
    min_ask = BID_PRICE * MIN_ASK_MULT
    tickers = [ib.reqMktData(c, "", False, False) for c in contracts]
    await asyncio.sleep(3.0)
    survivors = []
    for c, t in zip(contracts, tickers):
        ask = t.ask if (t.ask is not None and t.ask > 0) else None
        ok = ask is not None and ask >= min_ask
        ib.cancelMktData(c)
        print(f"  {'KEEP' if ok else 'SKIP'} {c.localSymbol}: ask={ask}")
        if ok:
            survivors.append(c)
    return survivors


async def place_one(ib: IB, client_id: int, contract, pending: dict,
                    oid_sent: dict):
    """Fire one placeOrder. Yields control immediately after so other
    coroutines can queue their own placeOrders onto their own sockets —
    forces true cross-client concurrency."""
    order = LimitOrder(
        action="BUY", totalQuantity=1, lmtPrice=BID_PRICE,
        tif="DAY", account=ACCOUNT,
        orderRef=f"mcp_c{client_id}_{contract.localSymbol}",
    )
    sent_ns = time.monotonic_ns()
    trade = ib.placeOrder(contract, order)
    oid = trade.order.orderId
    key = (oid, round(BID_PRICE, PRICE_DECIMALS))
    pending[key] = sent_ns
    oid_sent[oid] = (sent_ns, contract)
    await asyncio.sleep(0)  # yield — lets other clients' placeOrders run


def install_ack_handler(ib: IB, pending: dict, own_acks: dict):
    def on_open(t):
        key = (t.order.orderId, round(t.order.lmtPrice, PRICE_DECIMALS))
        if key in pending and key not in own_acks:
            own_acks[key] = time.monotonic_ns()
    ib.openOrderEvent += on_open


async def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--n-clients", type=int, default=3)
    ap.add_argument("--orders-per-client", type=int, default=5)
    args = ap.parse_args()

    n_clients = args.n_clients
    per = args.orders_per_client
    total = n_clients * per
    if total > len(OTM_STRIKES):
        print(f"ERROR: requested {total} orders but only {len(OTM_STRIKES)} strikes available")
        return 1

    print(f"Connecting {n_clients} clients to {HOST}:{PORT}, account={ACCOUNT}")
    clients = []
    for i in range(n_clients):
        ib = IB()
        await lean_connect(ib, i)
        print(f"  clientId={i} connected, server version {ib.client.serverVersion()}")
        clients.append(ib)

    # Qualify all contracts via the master client
    master = clients[0]
    all_contracts = []
    for right, strike in OTM_STRIKES[:total]:
        c = FuturesOption(
            symbol=OPT_SYMBOL, lastTradeDateOrContractMonth=EXPIRY,
            strike=strike, right=right, exchange=EXCHANGE,
            multiplier=MULTIPLIER, currency=CURRENCY,
            tradingClass=TRADING_CLASS,
        )
        all_contracts.append(c)
    await master.qualifyContractsAsync(*all_contracts)
    all_contracts = [c for c in all_contracts if c.conId > 0]

    # Pre-flight: master snapshot, only survivors are traded
    print(f"\nPre-flight filter (ask >= ${BID_PRICE*MIN_ASK_MULT:.4f}):")
    survivors = await preflight_filter(master, all_contracts)
    if len(survivors) < total:
        print(f"Only {len(survivors)}/{total} passed pre-flight — trimming")
        total = len(survivors) - (len(survivors) % n_clients)
        per = total // n_clients
        survivors = survivors[:total]
    print(f"\n{total} contracts -> {n_clients} clients × {per} orders each")

    # Assign contracts to clients: round-robin
    assignments = defaultdict(list)
    for i, c in enumerate(survivors):
        assignments[i % n_clients].append(c)

    # Install per-client ack handlers BEFORE placing any orders
    own_acks_per_client = {i: {} for i in range(n_clients)}
    pending_per_client = [{} for _ in range(n_clients)]
    oid_sent_per_client = [{} for _ in range(n_clients)]
    for i in range(n_clients):
        install_ack_handler(clients[i], pending_per_client[i],
                             own_acks_per_client[i])

    # Fire all orders concurrently — one coroutine per (client, order),
    # each yields after its placeOrder so the N asyncio Tasks interleave
    # across client sockets (true concurrency, not serial within one client).
    coros = []
    for client_idx, contracts_for_client in assignments.items():
        for c in contracts_for_client:
            coros.append(place_one(
                clients[client_idx], client_idx, c,
                pending_per_client[client_idx],
                oid_sent_per_client[client_idx],
            ))

    t_start = time.monotonic_ns()
    await asyncio.gather(*coros)
    t_all_sent = time.monotonic_ns()

    # Wait up to 10s for all clients to have received their own acks
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        total_own = sum(len(d) for d in own_acks_per_client.values())
        if total_own >= total:
            break
        await asyncio.sleep(0.05)
    t_all_acked = time.monotonic_ns()

    # Cleanup — cancel all orders via master
    print("\nCancelling all orders...")
    for oid_sent in oid_sent_per_client:
        for oid, (_, c) in oid_sent.items():
            # find matching trade in master's openTrades
            for t in master.openTrades():
                if t.order.orderId == oid:
                    master.cancelOrder(t.order)
                    break
    await asyncio.sleep(1.5)
    for ib in clients:
        ib.disconnect()

    # ═══ REPORT ═══
    print("\n" + "="*64)
    print(f"n_clients={n_clients} orders_per_client={per} total={total}")
    sent_to_all_acked_ms = (t_all_acked - t_start) / 1e6
    print(f"Wall-clock: place→all_acked = {sent_to_all_acked_ms:.0f}ms")
    print(f"Expected single-client serialization: {total * 75:.0f}ms "
          f"(speedup target: {n_clients}x → ~{total * 75 / n_clients:.0f}ms)")

    # Per-client RTT (from own openOrderEvent)
    print("\nFA routing check — does each clientId see its own order acks?")
    for i in range(n_clients):
        own = own_acks_per_client[i]
        pending = pending_per_client[i]
        hits = sum(1 for k in pending if k in own)
        total_c = len(pending)
        print(f"  clientId={i}: {hits}/{total_c} own acks received "
              f"({'✓' if hits == total_c else '✗ FA routing confirmed — events go to master only'})")

    # RTT via each client's own openOrderEvent
    print("\nPer-order RTT via each client's own openOrderEvent:")
    for i in range(n_clients):
        own = own_acks_per_client[i]
        rtts = []
        for key, sent_ns in pending_per_client[i].items():
            if key in own:
                rtts.append((own[key] - sent_ns) / 1e6)
        if rtts:
            s = sorted(rtts)
            print(f"  clientId={i}  n={len(rtts)}  "
                  f"min={s[0]:.0f} p50={s[len(s)//2]:.0f} "
                  f"p90={s[min(len(s)-1, int(len(s)*0.9))]:.0f} "
                  f"max={s[-1]:.0f} ms")

    # Aggregate across all clients
    all_rtts = []
    for i in range(n_clients):
        own = own_acks_per_client[i]
        for key, sent_ns in pending_per_client[i].items():
            if key in own:
                all_rtts.append((own[key] - sent_ns) / 1e6)
    if all_rtts:
        s = sorted(all_rtts)
        print(f"\nAggregate (n={len(s)}):  min={s[0]:.0f} p50={s[len(s)//2]:.0f} "
              f"p90={s[min(len(s)-1, int(len(s)*0.9))]:.0f} "
              f"p99={s[min(len(s)-1, int(len(s)*0.99))]:.0f} "
              f"max={s[-1]:.0f} ms")
    print("="*64)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        print("interrupted")
        sys.exit(1)
