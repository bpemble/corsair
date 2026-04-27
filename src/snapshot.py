"""Chain snapshot writer for the dashboard.

Serializes the full live state (per-strike call/put blocks, portfolio
greeks, per-side risk buckets, account info, latency, positions) to a
JSON file the Streamlit dashboard reads on its refresh cycle. Atomic
write via temp file + os.replace so the dashboard never sees a partial
snapshot.
"""

import asyncio
import json
import logging
import os
from datetime import datetime, timezone

from .sabr import delta_adjust_theo

logger = logging.getLogger(__name__)

# Default snapshot path — overridable via config.logging.snapshot_path for
# multi-product deployments (each product writes its own file). The docker
# healthcheck and dashboard still look here, so don't change the default
# without also updating docker-compose.yml and scripts/dashboard.py.
DEFAULT_SNAPSHOT_PATH = "data/chain_snapshot.json"


_TAG_MAP = {
    "TotalCashValue": "cash",
    "NetLiquidation": "net_liq",
    "InitMarginReq": "init_margin",
    "MaintMarginReq": "maint_margin",
    "BuyingPower": "buying_power",
    "UnrealizedPnL": "unrealized_pnl",
    "RealizedPnL": "realized_pnl",
}


def _read_account_state(ib, account_id: str) -> dict:
    """Pull cash, margin, and P&L summary from IBKR account values."""
    out = {"account_id": account_id, "cash": 0.0, "net_liq": 0.0,
           "init_margin": 0.0, "maint_margin": 0.0, "buying_power": 0.0,
           "unrealized_pnl": 0.0, "realized_pnl": 0.0}
    try:
        for v in ib.accountValues(account_id):
            if v.currency != "USD" and v.currency != "":
                continue
            key = _TAG_MAP.get(v.tag)
            if key:
                try:
                    out[key] = float(v.value)
                except (ValueError, TypeError):
                    pass
    except Exception:
        pass
    return out


def _build_side(state, market_data, sabr, portfolio, active_quotes,
                strike: float, right: str, expiry: str = None,
                product: str | None = None) -> dict | None:
    """Build the per-right block for one strike, or None if no contract.

    ``product`` (multi-product): when set, only positions tagged with this
    underlying symbol contribute to the per-strike position count. ETH and
    HG can in principle share strike numbers; the filter prevents an ETH
    position from showing up in the HG dashboard's strike row (or vice
    versa).
    """
    if expiry is None:
        expiry = state.front_month_expiry
    opt = state.get_option(strike, expiry=expiry, right=right)
    if opt is None:
        return None
    # Multi-expiry quoting: active_quotes is keyed by (strike, expiry, right, side).
    bid_info = active_quotes.get((strike, expiry, right, "BUY"))
    ask_info = active_quotes.get((strike, expiry, right, "SELL"))
    our_bid = bid_info["price"] if bid_info else None
    our_ask = ask_info["price"] if ask_info else None
    # PreSubmitted and Submitted both mean the order is resting on IBKR's
    # book — paper Gateway frequently leaves orders at PreSubmitted and never
    # advances them, so treating only "Submitted" as live makes the dashboard
    # under-report. Match quote_engine._is_order_live for consistency.
    _LIVE = ("PreSubmitted", "Submitted")
    bid_live = bid_info["status"] in _LIVE if bid_info else False
    ask_live = ask_info["status"] in _LIVE if ask_info else False
    theo = None
    _last_cal = sabr.get_last_calibration(expiry)
    if _last_cal is not None:
        try:
            theo = delta_adjust_theo(
                sabr.get_theo(strike, right, expiry=expiry),
                opt.delta, sabr.get_spot_at_fit(expiry), state.underlying_price,
            )
            theo = round(theo, 4)
        except Exception:
            pass
    pos = sum(
        p.quantity for p in portfolio.positions
        if p.strike == strike and p.expiry == expiry
        and p.put_call == right
        and (product is None or p.product == product)
    )
    if bid_live or ask_live:
        status = "quoting"
    elif bid_info or ask_info:
        status = "pending"
    else:
        status = "idle"
    clean_bid, clean_ask = market_data.get_clean_bbo(strike, right, expiry=expiry)
    return {
        "market_bid": clean_bid,
        "market_ask": clean_ask,
        "raw_bid": opt.bid,
        "raw_ask": opt.ask,
        "our_bid": our_bid,
        "our_ask": our_ask,
        "bid_live": bid_live,
        "ask_live": ask_live,
        "theo": theo,
        "delta": round(opt.delta, 4),
        "iv": round(opt.iv, 4) if opt.iv else 0.0,
        "volume": opt.volume,
        "open_interest": opt.open_interest,
        "position": pos,
        "status": status,
    }


def _hedge_block(hedge, state):
    """Snapshot view of HedgeManager state. None-safe at call site —
    this is only invoked when ``hedge`` is not None.
    """
    contract = hedge._resolved_hedge_contract
    qty = int(hedge.hedge_qty)
    return {
        "enabled": bool(hedge.enabled),
        "mode": str(hedge.mode),
        "qty": qty,
        "avg_entry": round(float(hedge.avg_entry_F), 4) if qty != 0 else 0.0,
        "forward": round(float(state.underlying_price), 4),
        "mtm_usd": round(float(hedge.hedge_mtm_usd()), 2),
        "realized_pnl_usd": round(float(hedge.realized_pnl_usd), 2),
        "contract": (getattr(contract, "localSymbol", None)
                     or getattr(contract, "symbol", None) or "—"),
        "expiry": getattr(contract, "lastTradeDateOrContractMonth", "") or "",
        "tolerance": float(hedge.tolerance),
    }


def write_chain_snapshot(market_data, quotes, portfolio, sabr, margin,
                         ib, account_id, config, hedge=None):
    """Write a JSON snapshot of the full option chain for the dashboard.

    Multi-product: every per-product invocation (primary + each observer)
    filters portfolio.positions to its own product so the dashboard
    table doesn't mix HG and ETH rows. Uses each position's own
    ``multiplier`` for MtM math so a 25000-multiplier HG row doesn't get
    rendered with ETH's 50, or vice versa.
    """
    state = market_data.state
    if state.underlying_price <= 0:
        return

    product = config.product.underlying_symbol

    active_quotes = quotes.get_active_quotes()
    chains_data: dict = {}
    expiries_list = list(state.expiries) if state.expiries else (
        [state.front_month_expiry] if state.front_month_expiry else []
    )
    for exp in expiries_list:
        exp_strikes: dict = {}
        for strike in state.get_all_strikes(expiry=exp):
            block = {
                "call": _build_side(state, market_data, sabr, portfolio,
                                    active_quotes, strike, "C",
                                    expiry=exp, product=product),
                "put": _build_side(state, market_data, sabr, portfolio,
                                   active_quotes, strike, "P",
                                   expiry=exp, product=product),
            }
            if block["call"] is None and block["put"] is None:
                continue
            strike_key = f"{strike:.2f}" if strike != int(strike) else str(int(strike))
            exp_strikes[strike_key] = block
        chains_data[exp] = {"strikes": exp_strikes}
    # Legacy top-level strikes = front-month chain (for older dashboard).
    strikes_data = (
        chains_data.get(state.front_month_expiry, {}).get("strikes", {})
        if state.front_month_expiry else {}
    )

    # Per-position detail with MtM P&L. Mark priority:
    #   1. live BBO mid (authoritative during market hours)
    #   2. IBKR's portfolio() marketPrice (authoritative when markets are
    #      closed — weekends, daily close window; our BBO is stale then and
    #      fell through to avg_fill_price, rendering every unrealized_pnl
    #      as 0 and masking real exposure)
    #   3. our cached current_price (from prior live update)
    #   4. avg_fill_price (gives unrealized=0; last resort)
    #
    # Multi-product: use pos.multiplier (per-position) — using
    # config.product.multiplier here would render HG positions with ETH's
    # 50× when this function is called for the ETH dashboard, or render
    # ETH positions with HG's 25000× when called for the HG dashboard.
    ibkr_marks: dict = {}
    try:
        for it in ib.portfolio(account_id):
            c = it.contract
            if c.secType != "FOP" or c.right not in ("C", "P"):
                continue
            px = float(it.marketPrice) if it.marketPrice else 0.0
            if px > 0:
                ibkr_marks[(c.symbol, float(c.strike),
                            c.lastTradeDateOrContractMonth, c.right)] = px
    except Exception as e:
        logger.debug("snapshot: ib.portfolio() read failed: %s", e)

    positions_detail = []
    options_unrealized_total = 0.0
    own_positions = [p for p in portfolio.positions if p.product == product]
    for p in own_positions:
        opt = state.get_option(p.strike, p.expiry, p.put_call)
        if opt and opt.bid > 0 and opt.ask > 0:
            mark = (opt.bid + opt.ask) / 2
        elif (ibkr_px := ibkr_marks.get(
                (p.product, float(p.strike), p.expiry, p.put_call))):
            mark = ibkr_px
        elif p.current_price > 0:
            mark = p.current_price
        else:
            mark = p.avg_fill_price
        unrealized = (mark - p.avg_fill_price) * p.quantity * p.multiplier
        options_unrealized_total += unrealized
        positions_detail.append({
            "strike": p.strike,
            "expiry": p.expiry,
            "right": p.put_call,
            "qty": p.quantity,
            "avg_price": round(p.avg_fill_price, 2),
            "mark": round(mark, 2),
            "unrealized_pnl": round(unrealized, 2),
            "delta": round(p.delta * p.quantity, 4),
            "theta": round(p.theta * p.quantity, 2),
        })

    # Pull constraint + kill thresholds for the dashboard so its color
    # bands auto-adapt to the active stage's limits without hardcoding.
    constraints = config.constraints
    ks = getattr(config, "kill_switch", None)
    limits = {
        "delta_ceiling": float(constraints.delta_ceiling),
        "theta_floor": float(constraints.theta_floor),
        "margin_ceiling": float(constraints.capital) * float(constraints.margin_ceiling_pct),
        "delta_kill": float(getattr(ks, "delta_kill", 0) or 0) if ks else 0,
        "vega_kill": float(getattr(ks, "vega_kill", 0) or 0) if ks else 0,
        "margin_kill": float(constraints.capital) * float(getattr(ks, "margin_kill_pct", 0) or 0) if ks else 0,
    }

    # Compute margin once for total + per-side. Each get_current_margin call
    # walks the (filtered) position list and runs the synthetic SPAN scenario
    # array, so doing this three times per snapshot at 4Hz adds up fast.
    _margin_total = margin.get_current_margin()
    _margin_calls = margin.get_current_margin("C")
    _margin_puts = margin.get_current_margin("P")

    # Account block: header Unrealized P&L is rewritten to be the sum of
    # the options-only positions table (so the header and the table agree —
    # IBKR's account-level UnrealizedPnL also folds in any non-options
    # holdings, which don't appear in the table). Realized P&L is sourced
    # from portfolio.realized_pnl_persisted so it survives a process restart;
    # we update that field here from IBKR whenever IBKR returns a non-zero
    # value (zero usually means the account-values stream hasn't repopulated
    # yet after a reconnect).
    account = _read_account_state(ib, account_id)
    # Sum the SAME whole-dollar-rounded values the per-row table displays,
    # not the unrounded mid-precision sum, so the header and the visual sum
    # of the rows agree to the dollar. (Sum-of-rounds ≠ round-of-sum; the
    # difference is sub-dollar but visible in the `:,.0f` formatting.)
    account["unrealized_pnl"] = float(
        sum(round(d["unrealized_pnl"]) for d in positions_detail)
    )
    _ibkr_realized = account.get("realized_pnl", 0.0)
    if _ibkr_realized != 0.0:
        portfolio.realized_pnl_persisted = _ibkr_realized
    account["realized_pnl"] = round(portfolio.realized_pnl_persisted, 2)
    # Fold hedge into account totals: IBKR's RealizedPnL stream doesn't
    # pick up futures fills on this paper account (verified 2026-04-26),
    # and account.unrealized_pnl above is options-only. Without folding,
    # the account row silently understates exposure by hedge MTM +
    # cumulative hedge realized. Note: hedge.realized_pnl_usd is local
    # in-memory and resets on restart — partial coverage until v1.5
    # reconciliation lands.
    if hedge is not None:
        account["unrealized_pnl"] += round(float(hedge.hedge_mtm_usd()), 2)
        account["realized_pnl"] += round(float(hedge.realized_pnl_usd), 2)

    # "Today's P&L" = current Net Liq − Net Liq at session open. Anchor
    # is captured opportunistically here on the first non-zero NLV reading
    # after reset_daily() / startup, then persisted via daily_state so a
    # mid-session container rebuild doesn't re-anchor against intraday NLV.
    _net_liq_raw = float(account.get("net_liq", 0) or 0)
    if portfolio.session_open_nlv is None and _net_liq_raw > 0:
        portfolio.session_open_nlv = _net_liq_raw
    if portfolio.session_open_nlv is not None and _net_liq_raw > 0:
        account["session_open_nlv"] = round(portfolio.session_open_nlv, 2)
        account["daily_pnl_nlv"] = round(_net_liq_raw - portfolio.session_open_nlv, 2)
    else:
        account["session_open_nlv"] = None
        account["daily_pnl_nlv"] = None

    # Per-product portfolio aggregates: each dashboard shows only its own
    # product's risk numbers (mixing ETH and HG contract-equivalent delta
    # in a "net_delta" field is meaningless given different multipliers).
    # ``margin`` stays cross-product (it's the combined number gating the
    # constraint check, and that's what the operator wants to see).
    own_calls = [p for p in own_positions if p.put_call == "C"]
    own_puts = [p for p in own_positions if p.put_call == "P"]

    def _agg(group, attr):
        return sum(getattr(p, attr) * p.quantity for p in group)

    # Forward staleness: weekend / maintenance windows freeze the
    # underlying tick feed, leaving daily P&L marked against last-known
    # forward (e.g. 2026-04-26 showed +$31K MtM frozen for 8h until
    # Sunday session reopen, then rebased to +$5K). Surface the age so
    # the dashboard can suppress / badge stale-mark P&L.
    _now = datetime.now(timezone.utc)
    _last = state.underlying_last_update
    if _last.tzinfo is None:
        _last = _last.replace(tzinfo=timezone.utc)
    forward_age_s = max(0.0, (_now - _last).total_seconds())

    snapshot = {
        "timestamp": _now.isoformat(),
        "underlying_price": state.underlying_price,
        "underlying_age_s": round(forward_age_s, 1),
        "atm_strike": state.atm_strike,
        "front_month_expiry": state.front_month_expiry,
        "expiries": expiries_list,
        "chains": chains_data,
        "account": account,
        "latency": quotes.get_latency_snapshot(),
        "limits": limits,
        "portfolio": {
            # Delta breakdown: options_delta + hedge_delta = effective_delta.
            # As of 2026-04-27 (CLAUDE.md §14) effective_delta is the metric
            # the constraint checker and risk monitor gate against. Surfacing
            # the triple side-by-side lets the operator see when the hedge
            # is masking option-side drift (which would matter immediately
            # if local hedge_qty diverges from IBKR's actual position —
            # see CLAUDE.md §10 reconciliation gap).
            "options_delta": round(portfolio.delta_for_product(product), 4),
            "hedge_delta": (int(hedge.hedge_qty) if hedge is not None else 0),
            "effective_delta": round(
                portfolio.delta_for_product(product)
                + (int(hedge.hedge_qty) if hedge is not None else 0),
                4,
            ),
            # net_delta retained for dashboard backward-compat — alias of
            # effective_delta now that gates use effective. Will be removed
            # once dashboard reads options_delta / hedge_delta explicitly.
            "net_delta": round(
                portfolio.delta_for_product(product)
                + (int(hedge.hedge_qty) if hedge is not None else 0),
                4,
            ),
            "net_theta": round(portfolio.theta_for_product(product), 2),
            "net_vega": round(portfolio.vega_for_product(product), 2),
            "net_gamma": round(_agg(own_positions, "gamma"), 6),
            "total_contracts": sum(abs(p.quantity) for p in own_positions),
            "margin": _margin_total,
            "fills_today": portfolio.fills_today,
            "spread_capture": round(portfolio.spread_capture_today, 2),
            "spread_capture_mid": round(portfolio.spread_capture_mid_today, 2),
            "positions": positions_detail,
            "calls": {
                "delta": round(_agg(own_calls, "delta"), 4),
                "theta": round(_agg(own_calls, "theta"), 2),
                "vega": round(_agg(own_calls, "vega"), 2),
                "margin": round(_margin_calls, 0),
                "gross": sum(abs(p.quantity) for p in own_calls),
            },
            "puts": {
                "delta": round(_agg(own_puts, "delta"), 4),
                "theta": round(_agg(own_puts, "theta"), 2),
                "vega": round(_agg(own_puts, "vega"), 2),
                "margin": round(_margin_puts, 0),
                "gross": sum(abs(p.quantity) for p in own_puts),
            },
        },
        "hedge": _hedge_block(hedge, state) if hedge is not None else None,
        "strikes": strikes_data,
    }

    # Per-product snapshot path is attached to the product-view config by
    # make_product_config(). Fall back to the legacy logging.snapshot_path
    # or the project default for config paths where it's not set.
    path = getattr(config, "snapshot_path",
                   getattr(config.logging, "snapshot_path", DEFAULT_SNAPSHOT_PATH))
    _schedule_snapshot_write(snapshot, path)


def _write_snapshot_to_disk(snapshot: dict, path: str) -> None:
    """Atomic-write one snapshot dict to *path*. Always runs in a worker
    thread via the asyncio default executor (fall back to inline on paths
    that have no running loop, e.g. unit tests)."""
    tmp = path + ".tmp"
    try:
        os.makedirs(os.path.dirname(tmp), exist_ok=True)
        with open(tmp, "w") as f:
            json.dump(snapshot, f, indent=2)
        os.replace(tmp, path)
    except Exception as e:
        logger.warning("Failed to write chain snapshot: %s", e)


def _schedule_snapshot_write(snapshot: dict, path: str) -> None:
    """Fire-and-forget the JSON encode + disk write. `json.dump(indent=2)`
    on our ~10KB payload plus the atomic rename add up to several ms per
    snapshot at 4Hz — doing that on the event loop blocks tick handling."""
    try:
        loop = asyncio.get_running_loop()
    except RuntimeError:
        _write_snapshot_to_disk(snapshot, path)
        return
    loop.run_in_executor(None, _write_snapshot_to_disk, snapshot, path)


