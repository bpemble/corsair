"""Concurrency sweep probe — amend/place RTT vs N-in-flight.

Generalizes multi_amend_probe.py to sweep N ∈ {1, 2, 4, 8, 15} so we can
measure the *slope* of per-order queue cost as concurrency increases.
The slope is the diagnostic for whether FIX will flatten our loaded-p50
latency:

  - Linear (slope ≥30ms/N): IBKR backend serializes — FIX won't help much.
  - Sub-linear or plateau: Gateway JVM serializes — FIX should flatten it.

Each level: place N orders, run K amend rounds (default 20) alternating
$0.0005 ↔ $0.0010, tear down, verify positions=0, 3s gap, next N.

Uses clientId=0 (FA master). DO NOT run while corsair is up:
    docker compose stop corsair

Usage (from host):
    docker compose run --rm --no-deps \\
        -v $(pwd)/scripts:/app/scripts \\
        -v $(pwd)/logs-paper:/app/logs-paper \\
        corsair python3 /app/scripts/concurrency_sweep_probe.py
"""
import argparse
import asyncio
import json
import os
import statistics
import sys
import time
from datetime import datetime

import yaml
from ib_insync import IB, FuturesOption, LimitOrder
from ib_insync.util import UNSET_DOUBLE, UNSET_INTEGER

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
    print("ERROR: CORSAIR_ACCOUNT_ID/IBKR_ACCOUNT env var must be set")
    sys.exit(1)

OTM_PUT_STRIKES = [5.55, 5.50, 5.45, 5.40, 5.35, 5.30, 5.25]
OTM_CALL_STRIKES = [6.60, 6.65, 6.70, 6.75, 6.80, 6.85, 6.90, 6.95]
TARGETS = ([(k, "P") for k in OTM_PUT_STRIKES]
           + [(k, "C") for k in OTM_CALL_STRIKES])
EXPIRY = "20260427"
TICK_SIZE = 0.0005
PRICE_DECIMALS = 4

ACTION = "BUY"
START_PRICE = TICK_SIZE           # 0.0005
PRICE_HIGH = TICK_SIZE * 2        # 0.0010
MIN_ASK_MULT = 2                  # drop contracts whose ask < 2× our max bid


def strip_contamination_fields(order) -> None:
    """Inline copy of QuoteManager._strip_contamination_fields (engine.py:973).

    Keeps this probe self-contained — avoids importing the engine, which
    would pull the whole corsair module graph into a measurement script.
    Body tracks engine copy; update both if one changes.
    """
    order.volatility = UNSET_DOUBLE
    order.volatilityType = UNSET_INTEGER
    order.continuousUpdate = False
    order.referencePriceType = UNSET_INTEGER
    order.deltaNeutralOrderType = ""
    order.deltaNeutralAuxPrice = UNSET_DOUBLE
    order.deltaNeutralConId = 0
    order.deltaNeutralOpenClose = ""
    order.deltaNeutralShortSale = False
    order.deltaNeutralShortSaleSlot = 0
    order.deltaNeutralDesignatedLocation = ""
    order.deltaNeutralSettlingFirm = ""
    order.deltaNeutralClearingAccount = ""
    order.deltaNeutralClearingIntent = ""
    order.algoStrategy = ""
    order.algoParams = []
    order.algoId = ""
    order.referenceContractId = 0
    order.referenceExchangeId = ""
    order.referenceChangeAmount = 0.0


async def lean_connect(ib: IB):
    await ib.client.connectAsync(HOST, PORT, 0, 30)
    ib.reqAutoOpenOrders(True)
    await asyncio.gather(
        asyncio.wait_for(ib.reqPositionsAsync(), 20),
        asyncio.wait_for(ib.reqOpenOrdersAsync(), 20),
        asyncio.wait_for(ib.reqAccountUpdatesAsync(ACCOUNT), 20),
        return_exceptions=True,
    )


def canonical(ib: IB, oid: int):
    """Walk openTrades() last-write-wins (CLAUDE.md §2)."""
    latest = None
    for t in ib.openTrades():
        if t.order.orderId == oid:
            latest = t
    return latest


def summarize(samples_us):
    """Return dict of mean/p50/p90/p99/min/max in ms, or None if empty."""
    if not samples_us:
        return None
    s = sorted(samples_us)
    n = len(s)
    return {
        "n": n,
        "mean": statistics.mean(s) / 1000,
        "p50": s[n // 2] / 1000,
        "p90": s[min(n - 1, n * 9 // 10)] / 1000,
        "p99": s[min(n - 1, n * 99 // 100)] / 1000,
        "min": s[0] / 1000,
        "max": s[-1] / 1000,
    }


async def qualify_and_preflight(ib: IB, n_max: int):
    """Qualify TARGETS[:n_max] and filter to contracts with ask ≥ 2× max bid."""
    contracts = []
    for strike, right in TARGETS[:n_max]:
        c = FuturesOption(
            symbol=OPT_SYMBOL, lastTradeDateOrContractMonth=EXPIRY,
            strike=strike, right=right, exchange=EXCHANGE,
            multiplier=MULTIPLIER, currency=CURRENCY,
            tradingClass=TRADING_CLASS,
        )
        contracts.append(c)
    await ib.qualifyContractsAsync(*contracts)
    contracts = [c for c in contracts if c.conId > 0]
    if not contracts:
        return []

    min_ask = PRICE_HIGH * MIN_ASK_MULT
    print(f"Pre-flight: requiring ask ≥ ${min_ask:.4f} (= {MIN_ASK_MULT}× max bid)")
    tickers = [ib.reqMktData(c, '', False, False) for c in contracts]
    await asyncio.sleep(3.0)
    survivors = []
    for c, t in zip(contracts, tickers):
        ask = t.ask if (t.ask is not None and t.ask > 0) else None
        ok = ask is not None and ask >= min_ask
        status = "KEEP" if ok else "SKIP"
        print(f"  {status} {c.localSymbol}: ask={ask}")
        ib.cancelMktData(c)
        if ok:
            survivors.append(c)
    return survivors


async def run_level(ib: IB, contracts, N: int, rounds: int,
                    round_gap_ms: int, out_fp, iteration_tag: int):
    """Run place phase + K amend rounds at concurrency N.

    Returns (place_rtts_us, amend_rtts_us) for this N level.
    """
    contracts = contracts[:N]
    pending = {}      # (oid, price) -> sent_ns
    acks = {}         # (oid, price) -> ack_ns
    place_rtts = []
    amend_rtts = []

    def on_open_order(t):
        key = (t.order.orderId, round(t.order.lmtPrice, PRICE_DECIMALS))
        if key in pending:
            acks[key] = time.monotonic_ns()

    ib.openOrderEvent += on_open_order
    try:
        # ── Place phase ──────────────────────────────────────────────
        oid_by_contract = {}
        for c in contracts:
            order = LimitOrder(
                action=ACTION, totalQuantity=1, lmtPrice=START_PRICE,
                tif="DAY", account=ACCOUNT,
                orderRef=f"sweep_N{N}_{c.localSymbol}",
            )
            sent_ns = time.monotonic_ns()
            trade = ib.placeOrder(c, order)
            oid = trade.order.orderId
            oid_by_contract[c.conId] = (oid, c)
            pending[(oid, round(START_PRICE, PRICE_DECIMALS))] = sent_ns
            await asyncio.sleep(0)

        # Wait for all N place acks (up to 20s)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if all((oid, p) in acks for (oid, p) in pending):
                break
            await asyncio.sleep(0.01)

        for (oid, p), sent in list(pending.items()):
            if (oid, p) in acks:
                rtt_us = (acks[(oid, p)] - sent) // 1000
                place_rtts.append(rtt_us)
                out_fp.write(json.dumps({
                    "ts": datetime.utcnow().isoformat() + "Z",
                    "N": N, "phase": "place", "order_id": oid,
                    "round": 0, "rtt_ms": rtt_us / 1000,
                    "lmt_price": p, "iteration": iteration_tag,
                }) + "\n")
        out_fp.flush()
        pending.clear()
        acks.clear()

        if len(place_rtts) < N:
            print(f"  N={N}: only {len(place_rtts)}/{N} place acks arrived — "
                  f"continuing with what we have")

        # ── Amend phase ──────────────────────────────────────────────
        for r in range(rounds):
            new_price = round(PRICE_HIGH if r % 2 == 0 else START_PRICE,
                              PRICE_DECIMALS)
            round_sent = 0
            for oid, c in oid_by_contract.values():
                t = canonical(ib, oid)
                if t is None:
                    continue
                strip_contamination_fields(t.order)
                t.order.lmtPrice = new_price
                sent_ns = time.monotonic_ns()
                pending[(oid, new_price)] = sent_ns
                try:
                    ib.placeOrder(c, t.order)
                    round_sent += 1
                except Exception as e:
                    print(f"  N={N} r={r+1} modify err oid={oid}: {e}")
                    pending.pop((oid, new_price), None)

            # Wait for this round's acks (5s budget)
            round_deadline = time.monotonic() + 5
            while time.monotonic() < round_deadline:
                acked_this_round = sum(
                    1 for (oid, p) in pending if (oid, p) in acks
                )
                if acked_this_round >= round_sent:
                    break
                await asyncio.sleep(0.01)

            for (oid, p), sent in list(pending.items()):
                if (oid, p) in acks:
                    rtt_us = (acks[(oid, p)] - sent) // 1000
                    amend_rtts.append(rtt_us)
                    out_fp.write(json.dumps({
                        "ts": datetime.utcnow().isoformat() + "Z",
                        "N": N, "phase": "amend", "order_id": oid,
                        "round": r + 1, "rtt_ms": rtt_us / 1000,
                        "lmt_price": p, "iteration": iteration_tag,
                    }) + "\n")
                    pending.pop((oid, p), None)
                    acks.pop((oid, p), None)
            out_fp.flush()

            await asyncio.sleep(round_gap_ms / 1000)

        # ── Teardown ─────────────────────────────────────────────────
        for oid, c in oid_by_contract.values():
            t = canonical(ib, oid)
            if t is not None:
                try:
                    ib.cancelOrder(t.order)
                except Exception as e:
                    print(f"  cancel err oid={oid}: {e}")
        await asyncio.sleep(2)
    finally:
        ib.openOrderEvent -= on_open_order

    return place_rtts, amend_rtts


def print_report(level_results):
    """level_results: list of (N, place_rtts_us, amend_rtts_us)."""
    print()
    print("=" * 72)
    print(f"{'N':>3}  {'place p50':>10}  {'place p99':>10}  "
          f"{'amend p50':>10}  {'amend p99':>10}  "
          f"{'n_place':>8}  {'n_amend':>8}")
    print("-" * 72)
    for N, places, amends in level_results:
        ps = summarize(places)
        as_ = summarize(amends)
        print(f"{N:>3}  "
              f"{(ps['p50'] if ps else 0):>8.1f}ms  "
              f"{(ps['p99'] if ps else 0):>8.1f}ms  "
              f"{(as_['p50'] if as_ else 0):>8.1f}ms  "
              f"{(as_['p99'] if as_ else 0):>8.1f}ms  "
              f"{(ps['n'] if ps else 0):>8}  "
              f"{(as_['n'] if as_ else 0):>8}")
    print("=" * 72)
    if len(level_results) >= 2:
        amend_p50s = [(N, summarize(a)['p50']) for N, _, a in level_results
                      if summarize(a) is not None]
        if len(amend_p50s) >= 2:
            (n1, t1), (n2, t2) = amend_p50s[0], amend_p50s[-1]
            if n2 > n1:
                slope = (t2 - t1) / (n2 - n1)
                print(f"Amend p50 slope {n1}→{n2}: {slope:.1f}ms/N  "
                      f"(linear-in-N ⇒ backend serialized; "
                      f"sub-linear ⇒ gateway-JVM)")
    print("=" * 72)


async def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--n-levels", type=str, default="1,2,4,8,15",
                    help="comma-separated N values")
    ap.add_argument("--rounds-per-n", type=int, default=20)
    ap.add_argument("--round-gap-ms", type=int, default=300)
    ap.add_argument("--level-gap-s", type=float, default=3.0)
    ap.add_argument("--out", type=str, default=None,
                    help="JSONL output path (default logs-paper/concurrency_sweep-<ts>.jsonl)")
    args = ap.parse_args()

    n_levels = [int(x) for x in args.n_levels.split(",") if x.strip()]
    n_max = max(n_levels)
    if n_max > len(TARGETS):
        print(f"ERROR: n_max={n_max} > {len(TARGETS)} available contracts")
        return 1

    out_path = args.out or (
        f"logs-paper/concurrency_sweep-"
        f"{datetime.utcnow().strftime('%Y%m%d-%H%M%S')}.jsonl"
    )
    # Resolve relative to repo root so both in-container and host invocations work
    if not os.path.isabs(out_path):
        out_path = os.path.join(os.path.dirname(__file__), "..", out_path)
    os.makedirs(os.path.dirname(out_path), exist_ok=True)

    ib = IB()
    print(f"Connecting {HOST}:{PORT} clientId=0 account={ACCOUNT}")
    await lean_connect(ib)
    print(f"Connected. Server version {ib.client.serverVersion()}")
    print(f"Plan: N levels {n_levels}, {args.rounds_per_n} rounds each, "
          f"{args.round_gap_ms}ms round gap, {args.level_gap_s}s level gap")
    print(f"Output: {out_path}")

    contracts = await qualify_and_preflight(ib, n_max)
    if len(contracts) < n_max:
        print(f"ERROR: only {len(contracts)}/{n_max} contracts passed pre-flight")
        ib.disconnect()
        return 1
    print(f"{len(contracts)} contracts qualified. Starting sweep.\n")

    level_results = []
    with open(out_path, "w") as out_fp:
        for N in n_levels:
            print(f"── N={N} ──")
            t_start = time.monotonic()
            places, amends = await run_level(
                ib, contracts, N, args.rounds_per_n,
                args.round_gap_ms, out_fp, iteration_tag=0,
            )
            dur = time.monotonic() - t_start
            ps = summarize(places)
            as_ = summarize(amends)
            print(f"  N={N} done in {dur:.1f}s: "
                  f"place p50={ps['p50']:.1f}ms n={ps['n']}, "
                  f"amend p50={as_['p50']:.1f}ms p99={as_['p99']:.1f}ms n={as_['n']}")
            level_results.append((N, places, amends))

            # Position sanity check
            await asyncio.sleep(1)
            positions = ib.positions(ACCOUNT)
            probe_positions = [
                p for p in positions
                if p.contract.conId in {c.conId for c in contracts}
            ]
            nonzero = [p for p in probe_positions if p.position != 0]
            if nonzero:
                print(f"  WARN: non-zero probe positions after N={N}: {nonzero}")

            if N != n_levels[-1]:
                await asyncio.sleep(args.level_gap_s)

    ib.disconnect()
    print_report(level_results)
    print(f"\nJSONL saved to: {out_path}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main()))
    except KeyboardInterrupt:
        print("interrupted")
        sys.exit(1)
