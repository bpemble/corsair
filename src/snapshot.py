"""Chain snapshot writer for the dashboard.

Serializes the full live state (per-strike call/put blocks, portfolio
greeks, per-side risk buckets, account info, latency, positions) to a
JSON file the Streamlit dashboard reads on its refresh cycle. Atomic
write via temp file + os.replace so the dashboard never sees a partial
snapshot.
"""

import json
import logging
import os
from datetime import datetime, timezone
from typing import Optional

logger = logging.getLogger(__name__)

SNAPSHOT_PATH = "data/chain_snapshot.json"
SNAPSHOT_TMP = "data/chain_snapshot.json.tmp"


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


def _build_side(state, market_data, quotes, sabr, portfolio, active_quotes,
                strike: float, right: str) -> Optional[dict]:
    """Build the per-right block for one strike, or None if no contract."""
    opt = state.get_option(strike, right=right)
    if opt is None:
        return None
    bid_info = active_quotes.get((strike, right, "BUY"))
    ask_info = active_quotes.get((strike, right, "SELL"))
    our_bid = bid_info["price"] if bid_info else None
    our_ask = ask_info["price"] if ask_info else None
    bid_live = bid_info["status"] == "Submitted" if bid_info else False
    ask_live = ask_info["status"] == "Submitted" if ask_info else False
    theo = None
    if sabr.last_calibration is not None:
        try:
            theo = round(sabr.get_theo(strike, right), 2)
        except Exception:
            pass
    pos = sum(
        p.quantity for p in portfolio.positions
        if p.strike == strike and p.expiry == state.front_month_expiry
        and p.put_call == right
    )
    if bid_live or ask_live:
        status = "quoting"
    elif bid_info or ask_info:
        status = "pending"
    else:
        status = "idle"
    clean_bid, clean_ask = market_data.get_clean_bbo(strike, right)
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


def write_chain_snapshot(market_data, quotes, portfolio, sabr, margin,
                         ib, account_id, config):
    """Write a JSON snapshot of the full option chain for the dashboard."""
    state = market_data.state
    if state.underlying_price <= 0:
        return

    active_quotes = quotes.get_active_quotes()
    strikes_data = {}
    for strike in state.get_all_strikes():
        block = {
            "call": _build_side(state, market_data, quotes, sabr, portfolio, active_quotes, strike, "C"),
            "put": _build_side(state, market_data, quotes, sabr, portfolio, active_quotes, strike, "P"),
        }
        if block["call"] is None and block["put"] is None:
            continue
        strikes_data[str(int(strike))] = block

    # Per-position detail with MtM P&L. If we don't yet have a fresh
    # bid/ask for this strike (just-subscribed, mid-rotation, etc.), fall
    # back to the avg fill price so unrealized_pnl reads as 0 instead of a
    # nonsensical "compared to zero mark" number.
    mult = config.product.multiplier
    positions_detail = []
    for p in portfolio.positions:
        opt = state.get_option(p.strike, p.expiry, p.put_call)
        if opt and opt.bid > 0 and opt.ask > 0:
            mark = (opt.bid + opt.ask) / 2
        elif p.current_price > 0:
            mark = p.current_price
        else:
            mark = p.avg_fill_price  # no live mark yet; show zero P&L
        unrealized = (mark - p.avg_fill_price) * p.quantity * mult
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

    snapshot = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "underlying_price": state.underlying_price,
        "atm_strike": state.atm_strike,
        "front_month_expiry": state.front_month_expiry,
        "account": _read_account_state(ib, account_id),
        "latency": quotes.get_latency_snapshot(),
        "limits": limits,
        "portfolio": {
            "net_delta": round(portfolio.net_delta, 4),
            "net_theta": round(portfolio.net_theta, 2),
            "net_vega": round(portfolio.net_vega, 2),
            "net_gamma": round(portfolio.net_gamma, 6),
            "total_contracts": portfolio.gross_positions,
            "margin": margin.get_current_margin(),
            "fills_today": portfolio.fills_today,
            "spread_capture": round(portfolio.spread_capture_today, 2),
            "positions": positions_detail,
            "calls": {
                "delta": round(portfolio.delta_for("C"), 4),
                "theta": round(portfolio.theta_for("C"), 2),
                "vega": round(portfolio.vega_for("C"), 2),
                "margin": round(margin.get_current_margin("C"), 0),
                "gross": portfolio.gross_for("C"),
            },
            "puts": {
                "delta": round(portfolio.delta_for("P"), 4),
                "theta": round(portfolio.theta_for("P"), 2),
                "vega": round(portfolio.vega_for("P"), 2),
                "margin": round(margin.get_current_margin("P"), 0),
                "gross": portfolio.gross_for("P"),
            },
        },
        "strikes": strikes_data,
    }

    try:
        os.makedirs(os.path.dirname(SNAPSHOT_TMP), exist_ok=True)
        with open(SNAPSHOT_TMP, "w") as f:
            json.dump(snapshot, f, indent=2)
        os.replace(SNAPSHOT_TMP, SNAPSHOT_PATH)
    except Exception as e:
        logger.warning("Failed to write chain snapshot: %s", e)
