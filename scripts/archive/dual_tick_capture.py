"""Dual tick capture: BTC + ETH CME futures BBO updates to JSONL.

Phase 0 of the BTC-leads-ETH lead-lag investigation. Read-only probe;
runs alongside corsair via a separate clientId so it can't interfere
with quoting.

Symbols:
  - ETH futures: IBKR symbol "ETHUSDRR" (= CME CF Ether-Dollar Reference
    Rate-settled CME ether futures). Front month, multiplier=50.
  - BTC futures: IBKR symbol "BRR" (= CME CF Bitcoin Reference Rate-
    settled CME bitcoin futures). Front month, multiplier=5.

Output: one JSONL row per unique (bid, ask, bid_size, ask_size) change
per symbol.

Usage (from host):
  docker compose exec -T corsair python3 /app/scripts/dual_tick_capture.py \\
      --duration 3600

Defaults: 30-minute capture, output path under logs-paper/.
"""
import argparse
import asyncio
import json
import math
import os
import time
from datetime import datetime, timezone

from ib_insync import IB, Future, Ticker


def _ok(x) -> bool:
    return x is not None and not (isinstance(x, float) and math.isnan(x)) and x > 0


def _intify(x) -> int | None:
    if x is None or (isinstance(x, float) and math.isnan(x)):
        return None
    return int(x)


def _exch_ts(t: Ticker) -> str | None:
    ts = getattr(t, "time", None)
    if ts is None:
        return None
    if isinstance(ts, datetime):
        return ts.astimezone(timezone.utc).isoformat()
    return str(ts)


async def _resolve_front(ib: IB, symbol: str, exchange: str) -> Future:
    """Return the qualified front-month Future for symbol/exchange."""
    stub = Future(symbol=symbol, exchange=exchange, currency="USD")
    details = await ib.reqContractDetailsAsync(stub)
    if not details:
        raise RuntimeError(f"no contract details for {symbol}/{exchange}")
    today = datetime.now(timezone.utc).strftime("%Y%m%d")
    valid = [d.contract for d in details
             if (d.contract.lastTradeDateOrContractMonth or "") >= today]
    if not valid:
        raise RuntimeError(f"no non-expired {symbol} contracts")
    valid.sort(key=lambda c: c.lastTradeDateOrContractMonth)
    front = valid[0]
    qualified = await ib.qualifyContractsAsync(front)
    if not qualified or qualified[0].conId == 0:
        raise RuntimeError(f"qualify failed for {front.localSymbol}")
    return qualified[0]


async def main(duration_s: int, out_path: str, client_id: int):
    ib = IB()
    await ib.connectAsync("127.0.0.1", 4002, clientId=client_id, timeout=15)

    eth = await _resolve_front(ib, "ETHUSDRR", "CME")
    btc = await _resolve_front(ib, "BRR", "CME")
    print(f"ETH: {eth.localSymbol} expiry={eth.lastTradeDateOrContractMonth} "
          f"mult={eth.multiplier} conId={eth.conId}")
    print(f"BTC: {btc.localSymbol} expiry={btc.lastTradeDateOrContractMonth} "
          f"mult={btc.multiplier} conId={btc.conId}")

    out_dir = os.path.dirname(out_path)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    f = open(out_path, "a", buffering=1)

    last: dict[str, tuple] = {}
    counters = {"ETH": 0, "BTC": 0}

    def emit(sym: str, contract_local: str, t: Ticker):
        b, a = t.bid, t.ask
        if not (_ok(b) and _ok(a)):
            return
        bs, as_ = _intify(t.bidSize), _intify(t.askSize)
        key = (b, a, bs, as_)
        if last.get(sym) == key:
            return
        last[sym] = key
        counters[sym] += 1
        row = {
            "t_local_ns": time.time_ns(),
            "t_exch":     _exch_ts(t),
            "sym":        sym,
            "bid":        b,
            "ask":        a,
            "bid_size":   bs,
            "ask_size":   as_,
            "contract":   contract_local,
        }
        f.write(json.dumps(row, separators=(",", ":")) + "\n")

    eth_t = ib.reqMktData(eth, "", False, False)
    btc_t = ib.reqMktData(btc, "", False, False)
    eth_t.updateEvent += lambda t: emit("ETH", eth.localSymbol, t)
    btc_t.updateEvent += lambda t: emit("BTC", btc.localSymbol, t)

    print(f"capturing for {duration_s}s → {out_path}")
    start = time.time()
    deadline = start + duration_s
    next_status = start + 30
    while time.time() < deadline:
        await asyncio.sleep(1)
        if time.time() >= next_status:
            elapsed = int(time.time() - start)
            print(f"  +{elapsed}s  ETH={counters['ETH']}  BTC={counters['BTC']}")
            next_status = time.time() + 30

    print(f"done  ETH={counters['ETH']}  BTC={counters['BTC']}  → {out_path}")
    f.close()
    ib.disconnect()


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", type=int, default=1800,
                    help="capture duration in seconds (default 1800 = 30min)")
    ap.add_argument("--out", type=str, default="",
                    help="output path; default logs-paper/dual_ticks-YYYY-MM-DD.jsonl")
    ap.add_argument("--client-id", type=int, default=45,
                    help="IBKR client id (default 45; avoid 0 + active probes)")
    args = ap.parse_args()
    if not args.out:
        d = datetime.now(timezone.utc).strftime("%Y-%m-%d")
        args.out = f"/app/logs-paper/dual_ticks-{d}.jsonl"
    asyncio.run(main(args.duration, args.out, args.client_id))
