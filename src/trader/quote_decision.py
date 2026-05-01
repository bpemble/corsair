"""Trader-side quote-decision logic (Option B per mm_service_split).

Independent implementation that *intends* to match the broker's quote
engine. Not a refactor of the broker's path — purposefully separate so
parity drift is the signal we measure.

Phase 6: ``decide`` delegates to the Rust ``corsair_pricing.decide_quote``
when the model is SABR. SVI and any unforeseen model fall back to the
pure-Python path below. The Python fallback also serves as the parity
reference for tests.

Inputs come from forwarded events: option price book, vol surface,
underlying. Outputs a structured decision per (strike, expiry, right,
side):

    {"action": "place" | "skip",
     "side": "BUY" | "SELL",
     "strike": float, "expiry": str, "right": str,
     "price": float | None,
     "theo": float | None,
     "iv": float | None,
     "reason": str}

What the broker does that we *don't* yet model (deliberate v2 simplifications):
    - peer-consensus cap engagement
    - inventory imbalance gates
    - layer-c cooldown
    - modify-storm guard
    - GTD lifetime / minimum-order-lifetime guards
    - active_orders incumbency (we'd amend if already resting)
"""

import os
from typing import Optional

# Rust hot-path pricing (already in production for the broker).
import corsair_pricing as _rs

# SABR implied-vol formula (Hagan 2002). Free function; no broker state.
from ..sabr import sabr_implied_vol, svi_implied_vol, time_to_expiry_years


# Phase 6 toggle. Default ON when the Rust extension is importable;
# operator can force the Python path with CORSAIR_TRADER_BACKEND=python
# for parity debugging or when the Rust version is found wanting.
_USE_RS_DECIDE = (
    hasattr(_rs, "decide_quote")
    and os.environ.get("CORSAIR_TRADER_BACKEND", "").lower() != "python"
)


def compute_theo(
    forward: float,
    strike: float,
    tte: float,
    right: str,
    vol_params: dict,
) -> Optional[tuple[float, float]]:
    """Compute (iv, theo) for one option given fitted vol params.

    ``forward`` MUST be the fit-time forward (the F that the SVI/SABR
    parameterization was calibrated against), NOT the current underlying
    spot. SVI's `m` is in log-moneyness units relative to the fit
    forward; using a different forward at evaluation time silently
    reanchors the wing flex point and produces theos that diverge from
    the broker's (which uses surface.forward in get_theo).

    Live evidence 2026-05-01 ~03:43: trader was passing current spot
    instead of fit forward; theo at K=5.6 came back as 0.0337 vs
    broker's 0.0275 at the same instant — a 23% gap on a deep wing put.
    Caused 28 trader-driven adverse fills with -$3.4K negative edge
    and ultimately tripped the daily P&L halt at -$33K.

    Returns None if inputs are invalid or the surface model is unknown.
    """
    if forward <= 0 or strike <= 0 or tte <= 0:
        return None
    model = vol_params.get("model")
    try:
        if model == "sabr":
            iv = sabr_implied_vol(
                forward, strike, tte,
                float(vol_params["alpha"]),
                float(vol_params["beta"]),
                float(vol_params["rho"]),
                float(vol_params["nu"]),
            )
        elif model == "svi":
            iv = svi_implied_vol(
                forward, strike, tte,
                float(vol_params["a"]),
                float(vol_params["b"]),
                float(vol_params["rho"]),
                float(vol_params["m"]),
                float(vol_params["sigma"]),
            )
        else:
            return None
    except Exception:
        return None
    if iv is None or iv <= 0 or iv != iv:  # NaN
        return None
    theo = _rs.black76_price(forward, strike, tte, iv, 0.0, right)
    if theo is None or theo <= 0:
        return None
    return (float(iv), float(theo))


def decide(
    *,
    forward: float,
    strike: float,
    expiry: str,
    right: str,
    side: str,
    vol_params: dict,
    market_bid: Optional[float],
    market_ask: Optional[float],
    min_edge_ticks: int,
    tick_size: float,
    tte: Optional[float] = None,
    # New defensive params (cleanup pass 3, 2026-05-01). All optional so
    # parity tests still work; trader sets them via state.
    market_bid_size: Optional[int] = None,
    market_ask_size: Optional[int] = None,
    min_bbo_size: int = 1,
    fit_forward: Optional[float] = None,
    current_forward: Optional[float] = None,
    # Tuned 2026-05-01: futures forward moves continuously vs SABR
    # refit cadence. 200 ticks = $0.10 of underlying drift before we
    # bail. Below that, broker's surface refit (every ~60s) is the
    # right cure. Above that, surface extrapolation is too suspect.
    max_forward_drift_ticks: int = 200,
) -> dict:
    """Make a single (strike, expiry, right, side) quote decision.

    Returns a dict with keys: action, side, strike, expiry, right,
    price, theo, iv, reason. Always returns a dict — never raises.

    Delegates to the Rust ``corsair_pricing.decide_quote`` for the SABR
    fast path; SVI and edge cases use the pure-Python implementation
    below (still the reference for parity tests).
    """
    if tte is None:
        try:
            tte = time_to_expiry_years(expiry)
        except Exception:
            return {
                "side": side, "strike": strike, "expiry": expiry,
                "right": right, "price": None, "theo": None, "iv": None,
                "action": "skip", "reason": "tte_calc_failed",
            }

    if _USE_RS_DECIDE and (vol_params or {}).get("model") == "sabr":
        # Rust hot path. SVI / unknown models fall through to Python
        # (the Rust impl returns "model_not_in_rust" → we honor that
        # and rerun in Python).
        try:
            d = _rs.decide_quote(
                forward, strike, expiry, right, side,
                vol_params, market_bid, market_ask,
                int(min_edge_ticks), float(tick_size), float(tte),
            )
            if d.get("reason") != "model_not_in_rust":
                return d
        except Exception:
            # Rust raised — log once per process and fall through.
            import logging
            logging.getLogger(__name__).debug(
                "rs.decide_quote failed; using Python", exc_info=True,
            )

    # Pure-Python path (also the parity reference).
    base = {
        "side": side, "strike": strike, "expiry": expiry, "right": right,
        "price": None, "theo": None, "iv": None,
    }

    if not vol_params:
        return {**base, "action": "skip", "reason": "no_vol_surface"}

    res = compute_theo(forward, strike, tte, right, vol_params)
    if res is None:
        return {**base, "action": "skip", "reason": "theo_unavailable"}
    iv, theo = res
    base["theo"] = theo
    base["iv"] = iv

    # Require a two-sided market. Same reasoning as the Rust path in
    # lib.rs:decide_quote — see that comment for the 2026-05-01
    # incidents (~21 adverse fills) that motivated this guard.
    bid_live = market_bid is not None and market_bid > 0
    ask_live = market_ask is not None and market_ask > 0
    if not bid_live or not ask_live:
        return {**base, "action": "skip", "reason": "one_sided_or_dark"}

    # NEW: require minimum displayed size on both sides. The 2026-05-01
    # daytime incident showed that paper IBKR matches resting orders
    # against ghost flow when displayed size is tiny (1@bid 1@ask), even
    # if the BBO LOOKED alive at decision time. Refusing to quote when
    # either side has < min_bbo_size eliminates that vector.
    if market_bid_size is not None and market_ask_size is not None:
        if market_bid_size < min_bbo_size or market_ask_size < min_bbo_size:
            return {**base, "action": "skip", "reason": "thin_book"}

    # NEW: forward-drift guard. SVI/SABR fits are anchored on the fit-
    # time forward; if the underlying has drifted far from that anchor,
    # the surface extrapolation is unreliable and our wing-strike theos
    # become meaningless. Refuse to quote when |spot - fit_F| exceeds
    # max_forward_drift_ticks. This is a safety net BELOW the fit-cadence
    # — broker should refit before this triggers, but if it doesn't we
    # don't want to keep quoting on stale params.
    if fit_forward is not None and current_forward is not None:
        drift = abs(current_forward - fit_forward)
        max_drift = max_forward_drift_ticks * tick_size
        # Underlying tick is much bigger than option tick (HG: $0.0005
        # option vs $0.0005 future, but futures move in dollars — so
        # use 20 ticks ~ $0.01 default). Override at call site if needed.
        # Actually for HG futures the tick is $0.0005, same as options.
        # 20 ticks = $0.01 drift on underlying — generous.
        if drift > max_drift:
            return {**base, "action": "skip", "reason": "forward_drift",
                    "drift": drift, "fit_forward": fit_forward}

    edge = min_edge_ticks * tick_size

    if side == "BUY":
        target = theo - edge
        if target <= 0:
            return {**base, "action": "skip", "reason": "target_nonpositive"}
        if market_ask is not None and market_ask > 0 and target >= market_ask:
            # Crossing the existing ask — broker's logic would also skip.
            return {**base, "price": target, "action": "skip",
                    "reason": "would_cross_ask"}
        # Round to tick — broker's quote engine quantizes, we should too.
        target = round(target / tick_size) * tick_size
        return {**base, "price": target, "action": "place",
                "reason": "edge_below_theo"}
    elif side == "SELL":
        target = theo + edge
        if market_bid is not None and market_bid > 0 and target <= market_bid:
            return {**base, "price": target, "action": "skip",
                    "reason": "would_cross_bid"}
        target = round(target / tick_size) * tick_size
        return {**base, "price": target, "action": "place",
                "reason": "edge_above_theo"}
    else:
        return {**base, "action": "skip", "reason": "unknown_side"}
