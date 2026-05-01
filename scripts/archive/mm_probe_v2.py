"""MM probe v2 — instrumented methodology validation.

Builds on mm_response_probe.py with the following upgrades for
methodology validation (per operator request 2026-04-22):

  - DUAL SUBSCRIPTION: reqMktData (throttled L1) AND
    reqTickByTickData("BidAsk") (unthrottled, if quota allows) on the
    same contracts. Direct comparison isolates the contribution of
    IBKR's 4Hz L1 throttle from true MM response latency.

  - EXCHANGE TIMESTAMP CAPTURE: record t.time (CME-reported timestamp)
    alongside our monotonic receipt time per event. Delta is the
    inbound wire latency from the exchange to us.

  - FULL BBO CHANGE CATEGORIZATION: every update is labeled
    bid_px_change | ask_px_change | bid_size_only | ask_size_only |
    both_px | no_change. Current methodology lumps all as "BBO change"
    which loses information about same-price re-quotes (fast MMs
    holding the best and refreshing size → invisible to v1 probe).

  - UNDERLYING TICK CADENCE BASELINE: log every underlying event
    (size-only included), not only mid-changes. Distribution of
    inter-arrival times tells us IBKR's quantization floor directly.

  - RAW JSONL OUTPUT: every event logged for post-hoc analysis.
    Summary stats printed at end but not canonical; the JSONL is.

  - OPTIONAL L2 DEPTH on one ATM strike per product via reqMktDepth.
    Catches same-price size updates at levels other than BBO, which
    BBO-only measurement cannot see.

  - GRACEFUL QUOTA FALLBACK: TBT and L2 quotas are small on paper.
    If the subscription fails, fall through to L1 only and note it
    in the JSONL so analysis can account for coverage.

Read-only (no orders). Distinct clientId from main corsair.

Usage:
    docker compose run --rm --no-deps -v $(pwd)/scripts:/app/scripts corsair \\
        python3 /app/scripts/mm_probe_v2.py --duration 1800 \\
        --symbol HG --opt-symbol HXE --trading-class HXE \\
        --exchange COMEX --multiplier 25000 \\
        --atm 6.10 --strike-step 0.05 --underlying-tick 0.0005 \\
        --expiry 20260427 --client-id 78

Output:
    logs-paper/mm_probe_v2-<SYMBOL>-<YYYYMMDD-HHMMSS>.jsonl
"""
import argparse
import asyncio
import json
import math
import os
import sys
import time
from collections import defaultdict
from pathlib import Path

import yaml
from ib_insync import IB, Future, FuturesOption


def _safe_int(v):
    """Cast to int, tolerating None / NaN / 0."""
    if v is None:
        return 0
    try:
        fv = float(v)
    except (TypeError, ValueError):
        return 0
    if math.isnan(fv):
        return 0
    return int(fv)


def _safe_float(v):
    """Cast to float, returning None for None/NaN."""
    if v is None:
        return None
    try:
        fv = float(v)
    except (TypeError, ValueError):
        return None
    if math.isnan(fv):
        return None
    return fv

# ── Config loading (optional) ──────────────────────────────────────────
_cfg_name = os.environ.get("CORSAIR_CONFIG", "config/hg_v1_4_paper.yaml")
_cfg_path = os.path.join(os.path.dirname(__file__), "..", _cfg_name)
try:
    with open(_cfg_path) as f:
        _cfg = yaml.safe_load(f)
        _prod = _cfg["products"][0]["product"] if "products" in _cfg else _cfg["product"]
        D_SYMBOL = _prod["underlying_symbol"]
        D_OPT_SYMBOL = _prod.get("option_symbol", D_SYMBOL)
        D_TRADING_CLASS = _prod.get("trading_class", "")
        D_EXCHANGE = _prod.get("exchange", "CME")
        D_CURRENCY = _prod.get("currency", "USD")
        D_MULTIPLIER = str(_prod["multiplier"])
except Exception:
    D_SYMBOL, D_OPT_SYMBOL, D_TRADING_CLASS = "HG", "HXE", "HXE"
    D_EXCHANGE, D_CURRENCY, D_MULTIPLIER = "COMEX", "USD", "25000"

HOST = os.environ.get("CORSAIR_GATEWAY_HOST", "127.0.0.1")
PORT = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4002"))


def epoch_ns_from_ib_time(t):
    """IB's ticker.time is a datetime; convert to ns since UNIX epoch.
    Returns None if unavailable (pre-first-tick or subscription error)."""
    if t is None:
        return None
    try:
        return int(t.timestamp() * 1_000_000_000)
    except Exception:
        return None


async def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--duration", type=int, default=1800, help="observation seconds")
    ap.add_argument("--expiry", default="20260427", help="option expiry YYYYMMDD")
    ap.add_argument("--atm", type=float, required=True, help="ATM strike")
    ap.add_argument("--n-strikes", type=int, default=3,
                    help="strikes on each side of ATM")
    # Product overrides (defaults from CORSAIR_CONFIG)
    ap.add_argument("--symbol", default=D_SYMBOL)
    ap.add_argument("--opt-symbol", default=D_OPT_SYMBOL)
    ap.add_argument("--trading-class", default=D_TRADING_CLASS)
    ap.add_argument("--exchange", default=D_EXCHANGE)
    ap.add_argument("--currency", default=D_CURRENCY)
    ap.add_argument("--multiplier", default=D_MULTIPLIER)
    ap.add_argument("--strike-step", type=float, default=0.05)
    ap.add_argument("--underlying-tick", type=float, default=0.0005)
    ap.add_argument("--client-id", type=int, default=77)
    ap.add_argument("--underlying-trading-class", default="",
                    help="filter futures by tradingClass (e.g. SI for std silver)")
    # v2-specific
    ap.add_argument("--enable-tbt", action="store_true", default=True,
                    help="subscribe reqTickByTickData on underlying")
    ap.add_argument("--no-tbt", dest="enable_tbt", action="store_false")
    ap.add_argument("--tbt-options", action="store_true", default=False,
                    help="also subscribe TBT on ATM call+put (burns 2 more TBT lines; "
                         "account-wide quota is ~5, so be careful when running parallel probes)")
    ap.add_argument("--enable-depth", action="store_true", default=True,
                    help="subscribe L2 depth on ATM strike")
    ap.add_argument("--no-depth", dest="enable_depth", action="store_false")
    ap.add_argument("--depth-rows", type=int, default=5)
    ap.add_argument("--out-dir", default="logs-paper")
    args = ap.parse_args()

    SYMBOL = args.symbol
    OPT_SYMBOL = args.opt_symbol
    TRADING_CLASS = args.trading_class
    EXCHANGE = args.exchange
    CURRENCY = args.currency
    MULTIPLIER = args.multiplier
    UNDERLYING_TICK = args.underlying_tick
    CLIENT_ID = args.client_id

    # ── Output file ─────────────────────────────────────────────────────
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    tag = time.strftime("%Y%m%d-%H%M%S")
    out_path = out_dir / f"mm_probe_v2-{SYMBOL}-{tag}.jsonl"
    jsonl = open(out_path, "w", buffering=1)  # line-buffered for safety

    def emit(event):
        """Write one JSONL row with our monotonic-ns receipt timestamp."""
        event["ts_mono_ns"] = time.monotonic_ns()
        event["ts_wall_ns"] = time.time_ns()
        jsonl.write(json.dumps(event, default=str) + "\n")

    # ── Connect ─────────────────────────────────────────────────────────
    ib = IB()
    await ib.client.connectAsync(HOST, PORT, CLIENT_ID, 30)
    print(f"Connected. Server version {ib.client.serverVersion()}  clientId={CLIENT_ID}  symbol={SYMBOL}")
    emit({
        "event": "connected",
        "symbol": SYMBOL, "client_id": CLIENT_ID,
        "server_version": ib.client.serverVersion(),
        "duration_sec": args.duration,
        "enable_tbt": args.enable_tbt, "enable_depth": args.enable_depth,
    })

    # ── Resolve underlying ──────────────────────────────────────────────
    probe = Future(SYMBOL, exchange=EXCHANGE, currency=CURRENCY)
    det = await ib.reqContractDetailsAsync(probe)
    today = time.strftime("%Y%m%d")
    future_expiries = [
        d.contract for d in det
        if d.contract.lastTradeDateOrContractMonth >= today
        and (not args.underlying_trading_class
             or d.contract.tradingClass == args.underlying_trading_class)
    ]
    if not future_expiries:
        print(f"Could not resolve {SYMBOL} futures with trading-class filter", file=sys.stderr)
        ib.disconnect()
        return 1
    underlying = min(future_expiries, key=lambda c: c.lastTradeDateOrContractMonth)
    print(f"Underlying: {underlying.localSymbol} conId={underlying.conId} "
          f"tradingClass={underlying.tradingClass} expiry={underlying.lastTradeDateOrContractMonth}")
    emit({
        "event": "underlying_resolved",
        "symbol": underlying.localSymbol, "con_id": underlying.conId,
        "trading_class": underlying.tradingClass,
        "expiry": underlying.lastTradeDateOrContractMonth,
    })

    # ── Resolve option contracts ────────────────────────────────────────
    contracts = {}
    for k_off in range(-args.n_strikes, args.n_strikes + 1):
        strike = round(args.atm + k_off * args.strike_step, 4)
        for right in ("C", "P"):
            opt = FuturesOption(
                symbol=OPT_SYMBOL, lastTradeDateOrContractMonth=args.expiry,
                strike=strike, right=right, exchange=EXCHANGE,
                multiplier=MULTIPLIER, currency=CURRENCY,
                tradingClass=TRADING_CLASS,
            )
            contracts[(strike, right)] = opt
    await ib.qualifyContractsAsync(*contracts.values())
    qualified = {k: c for k, c in contracts.items() if c.conId > 0}
    print(f"Options qualified: {len(qualified)}/{len(contracts)}")
    emit({
        "event": "options_qualified",
        "qualified": len(qualified), "requested": len(contracts),
        "keys": [list(k) for k in qualified.keys()],
    })

    if not qualified:
        print("No options qualified — aborting", file=sys.stderr)
        ib.disconnect()
        return 1

    # ── Prev-state caches for change categorization ─────────────────────
    prev_und = {"bid": None, "ask": None, "bid_size": None, "ask_size": None}
    prev_opt = defaultdict(lambda: {"bid": None, "ask": None, "bid_size": None, "ask_size": None})
    tbt_seq = defaultdict(int)

    def categorize(prev, bid, ask, bid_size, ask_size):
        """Return one of:
            'first' | 'bid_px' | 'ask_px' | 'both_px'
          | 'bid_size_only' | 'ask_size_only' | 'both_size_only'
          | 'no_change' | 'mixed'
        'first' = first observation, prev was None.
        'mixed' = simultaneous price + size change on at least one side.
        """
        if prev["bid"] is None and prev["ask"] is None:
            return "first"
        d_bid_px = bid != prev["bid"]
        d_ask_px = ask != prev["ask"]
        d_bid_sz = bid_size != prev["bid_size"]
        d_ask_sz = ask_size != prev["ask_size"]
        if not any([d_bid_px, d_ask_px, d_bid_sz, d_ask_sz]):
            return "no_change"
        if d_bid_px and d_ask_px:
            return "both_px"
        if d_bid_px:
            return "bid_px"
        if d_ask_px:
            return "ask_px"
        # Pure size
        if d_bid_sz and d_ask_sz:
            return "both_size_only"
        if d_bid_sz:
            return "bid_size_only"
        if d_ask_sz:
            return "ask_size_only"
        return "mixed"

    # ── Underlying handlers (mktData + optional TBT) ────────────────────
    und_ticker = ib.reqMktData(underlying, "", False, False)

    def on_underlying_mkt(t):
        b = _safe_float(t.bid)
        a = _safe_float(t.ask)
        if b is None or a is None or b <= 0 or a <= 0:
            return
        bs = _safe_int(t.bidSize)
        as_ = _safe_int(t.askSize)
        cat = categorize(prev_und, b, a, bs, as_)
        emit({
            "event": "und_mkt",
            "source": "reqMktData",
            "bid": b, "ask": a, "bid_size": bs, "ask_size": as_,
            "last": _safe_float(t.last),
            "last_size": _safe_int(t.lastSize),
            "ts_exchange_ns": epoch_ns_from_ib_time(t.time),
            "prev_bid": prev_und["bid"], "prev_ask": prev_und["ask"],
            "prev_bid_size": prev_und["bid_size"], "prev_ask_size": prev_und["ask_size"],
            "change_type": cat,
        })
        prev_und.update({"bid": b, "ask": a, "bid_size": bs, "ask_size": as_})

    und_ticker.updateEvent += on_underlying_mkt

    if args.enable_tbt:
        try:
            tbt_und = ib.reqTickByTickData(underlying, "BidAsk", 0, False)
            und_tbt_seen = [0]  # mutable closure — log all ticks since last call

            def on_underlying_tbt(t):
                # updateEvent may fire with batched ticks; drain all new
                # tickByTicks entries since last invocation to avoid
                # undercounting (smoke test showed this bias).
                n = len(t.tickByTicks)
                for i in range(und_tbt_seen[0], n):
                    tb = t.tickByTicks[i]
                    b = _safe_float(getattr(tb, "bidPrice", 0)) or 0.0
                    a = _safe_float(getattr(tb, "askPrice", 0)) or 0.0
                    bs = _safe_int(getattr(tb, "bidSize", 0))
                    as_ = _safe_int(getattr(tb, "askSize", 0))
                    if b <= 0 and a <= 0:
                        continue
                    emit({
                        "event": "und_tbt",
                        "source": "reqTickByTickData",
                        "bid": b, "ask": a, "bid_size": bs, "ask_size": as_,
                        "ts_exchange_ns": int(tb.time.timestamp() * 1_000_000_000)
                            if getattr(tb, "time", None) else None,
                        "tbt_idx": i,
                    })
                und_tbt_seen[0] = n

            tbt_und.updateEvent += on_underlying_tbt
            emit({"event": "tbt_subscribed", "target": "underlying",
                  "con_id": underlying.conId})
        except Exception as e:
            emit({"event": "tbt_error", "target": "underlying", "error": str(e)})

    # ── Option handlers ─────────────────────────────────────────────────
    atm_key = min(qualified.keys(), key=lambda k: abs(k[0] - args.atm))

    for key, opt in qualified.items():
        tkr = ib.reqMktData(opt, "", False, False)

        def make_on_opt_mkt(k):
            def fn(t):
                b = _safe_float(t.bid)
                a = _safe_float(t.ask)
                bs = _safe_int(t.bidSize)
                as_ = _safe_int(t.askSize)
                p = prev_opt[k]
                cat = categorize(p, b, a, bs, as_)
                emit({
                    "event": "opt_mkt",
                    "source": "reqMktData",
                    "strike": k[0], "right": k[1],
                    "bid": b, "ask": a, "bid_size": bs, "ask_size": as_,
                    "last": _safe_float(t.last),
                    "last_size": _safe_int(t.lastSize),
                    "ts_exchange_ns": epoch_ns_from_ib_time(t.time),
                    "prev_bid": p["bid"], "prev_ask": p["ask"],
                    "prev_bid_size": p["bid_size"], "prev_ask_size": p["ask_size"],
                    "change_type": cat,
                })
                p.update({"bid": b, "ask": a, "bid_size": bs, "ask_size": as_})
            return fn

        tkr.updateEvent += make_on_opt_mkt(key)

    # TBT on 2 contracts only (ATM C and P) to stay under quota
    if args.enable_tbt and args.tbt_options:
        tbt_targets = []
        for right in ("C", "P"):
            key = (atm_key[0], right)
            if key in qualified:
                tbt_targets.append(key)
        for k in tbt_targets:
            opt = qualified[k]
            try:
                tbt_tkr = ib.reqTickByTickData(opt, "BidAsk", 0, False)

                def make_on_opt_tbt(kk):
                    seen = [0]
                    def fn(t):
                        n = len(t.tickByTicks)
                        for i in range(seen[0], n):
                            tb = t.tickByTicks[i]
                            b = _safe_float(getattr(tb, "bidPrice", 0)) or 0.0
                            a = _safe_float(getattr(tb, "askPrice", 0)) or 0.0
                            bs = _safe_int(getattr(tb, "bidSize", 0))
                            as_ = _safe_int(getattr(tb, "askSize", 0))
                            if b <= 0 and a <= 0:
                                continue
                            tbt_seq[kk] += 1
                            emit({
                                "event": "opt_tbt",
                                "source": "reqTickByTickData",
                                "strike": kk[0], "right": kk[1],
                                "bid": b, "ask": a, "bid_size": bs, "ask_size": as_,
                                "ts_exchange_ns": int(tb.time.timestamp() * 1_000_000_000)
                                    if getattr(tb, "time", None) else None,
                                "tbt_seq": tbt_seq[kk],
                                "tbt_idx": i,
                            })
                        seen[0] = n
                    return fn

                tbt_tkr.updateEvent += make_on_opt_tbt(k)
                emit({"event": "tbt_subscribed", "target": "option",
                      "strike": k[0], "right": k[1], "con_id": opt.conId})
            except Exception as e:
                emit({"event": "tbt_error", "target": "option",
                      "strike": k[0], "right": k[1], "error": str(e)})

    # L2 depth on ATM (just the call — depth quota is small)
    if args.enable_depth:
        atm_c = (atm_key[0], "C")
        if atm_c in qualified:
            try:
                depth_tkr = ib.reqMktDepth(
                    qualified[atm_c], numRows=args.depth_rows,
                    isSmartDepth=False)

                def on_depth(t):
                    rows = []
                    for lvl in (t.domBids or [])[:args.depth_rows]:
                        rows.append({"side": "B", "position": lvl.position,
                                     "price": float(lvl.price), "size": int(lvl.size),
                                     "mm": lvl.marketMaker or ""})
                    for lvl in (t.domAsks or [])[:args.depth_rows]:
                        rows.append({"side": "A", "position": lvl.position,
                                     "price": float(lvl.price), "size": int(lvl.size),
                                     "mm": lvl.marketMaker or ""})
                    if rows:
                        emit({
                            "event": "opt_depth",
                            "source": "reqMktDepth",
                            "strike": atm_c[0], "right": atm_c[1],
                            "levels": rows,
                        })

                depth_tkr.updateEvent += on_depth
                emit({"event": "depth_subscribed", "strike": atm_c[0], "right": atm_c[1]})
            except Exception as e:
                emit({"event": "depth_error", "strike": atm_c[0], "right": atm_c[1],
                      "error": str(e)})

    # ── Observe ─────────────────────────────────────────────────────────
    print(f"Observing for {args.duration}s... output → {out_path}")
    emit({"event": "observation_started"})
    await asyncio.sleep(args.duration)
    emit({"event": "observation_complete"})

    ib.disconnect()
    jsonl.close()
    print(f"\nWrote {out_path}")
    # Minimal stdout summary; real analysis is post-hoc on JSONL
    size = out_path.stat().st_size
    with open(out_path) as f:
        n_events = sum(1 for _ in f)
    print(f"  {n_events:,} events, {size / 1024:,.1f} KB")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
