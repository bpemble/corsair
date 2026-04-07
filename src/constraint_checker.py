"""Constraint checker for Corsair v2.

Pre-trade validation ensuring every potential fill passes:
1. SPAN margin <= margin ceiling   (synthetic SPAN)
2. |Net delta| <= delta ceiling
3. Portfolio theta >= theta floor

Improving-fill exception: if a constraint is currently breached, allow
fills that move the state in the right direction.

Caches portfolio scan-risk and per-strike risk arrays so each
check_fill_margin call only reprices the candidate (not the whole book).
"""

import logging
import time
from typing import Dict, Optional, Tuple

import numpy as np
from ib_insync import IB

from .pricing import PricingEngine
from .synthetic_span import SyntheticSpan
from .utils import days_to_expiry

logger = logging.getLogger(__name__)


# Cache invalidation thresholds
_F_TOLERANCE = 5.0          # invalidate if F moves more than this ($)
_IV_TOLERANCE = 0.01        # invalidate per-strike if IV moves > 1 vol pt
_CACHE_MAX_AGE_SEC = 30.0   # hard time-based invalidation


class IBKRMarginChecker:
    """Synthetic SPAN margin checker with caching.

    Caches:
      - portfolio aggregate (scan-risk array, NOV, long_premium, short_count)
      - per-strike risk arrays (so the candidate's scenario evaluation is
        a numpy add against pre-computed state, not 16 fresh Black-76 calls)

    Invalidation:
      - on fill (call invalidate_portfolio() from FillHandler)
      - when F moves more than $5 from the cached F
      - when 30s have elapsed
      - per-strike: when IV moves > 1 vol pt or F moves > $5
    """

    def __init__(self, ib: IB, config, market_data, portfolio):
        self.ib = ib
        self.config = config
        self.market_data = market_data
        self.portfolio = portfolio
        self.span = SyntheticSpan(config)
        self.cached_ibkr_margin: float = 0.0
        self._account_id = config.account.account_id
        self._mult = float(config.product.multiplier)

        # Portfolio state cache
        self._port_state: Optional[dict] = None
        self._port_F: float = 0.0
        self._port_time: float = 0.0

        # Per-strike (strike, right) → {risk_array, base, F, iv, time}
        self._strike_cache: Dict[Tuple[float, str], dict] = {}

    # ── Cache invalidation ─────────────────────────────────────────
    def invalidate_portfolio(self):
        """Force a fresh portfolio recompute on the next check. Call from
        FillHandler when a fill arrives."""
        self._port_state = None

    def _portfolio_cache_valid(self, F: float) -> bool:
        if self._port_state is None:
            return False
        if abs(F - self._port_F) > _F_TOLERANCE:
            return False
        if (time.monotonic() - self._port_time) > _CACHE_MAX_AGE_SEC:
            return False
        return True

    def _strike_cache_valid(self, entry: dict, F: float, iv: float) -> bool:
        if abs(F - entry["F"]) > _F_TOLERANCE:
            return False
        if abs(iv - entry["iv"]) > _IV_TOLERANCE:
            return False
        if (time.monotonic() - entry["time"]) > _CACHE_MAX_AGE_SEC:
            return False
        return True

    # ── Strike-level cache ─────────────────────────────────────────
    def _get_strike_risk(self, strike: float, right: str, T: float,
                         iv: float, F: float):
        """Return (risk_array_16, base_price) for one long contract at this
        strike, using the per-strike cache when possible."""
        key = (strike, right)
        entry = self._strike_cache.get(key)
        if entry is not None and self._strike_cache_valid(entry, F, iv):
            return entry["risk_array"], entry["base"]
        ra = self.span.position_risk_array(F, strike, T, iv, right)
        base = PricingEngine.black76_price(F, strike, T, iv, right=right)
        self._strike_cache[key] = {
            "risk_array": ra, "base": base,
            "F": F, "iv": iv, "time": time.monotonic(),
        }
        return ra, base

    # ── Portfolio aggregate cache ──────────────────────────────────
    def _get_portfolio_state(self, F: float) -> dict:
        """Aggregate the live position book into (portfolio_ra, nov,
        long_premium, short_count). Cached across cycles."""
        if self._portfolio_cache_valid(F):
            return self._port_state

        portfolio_ra = np.zeros(16)
        nov = 0.0
        long_premium = 0.0
        short_count = 0
        for p in self.portfolio.positions:
            opt = self.market_data.state.get_option(p.strike, p.expiry, p.put_call)
            iv = float(opt.iv) if opt and opt.iv > 0 else 0.0
            T = max(days_to_expiry(p.expiry), 0) / 365.0
            if iv <= 0 or T <= 0 or p.quantity == 0:
                continue
            ra, base = self._get_strike_risk(p.strike, p.put_call, T, iv, F)
            portfolio_ra += ra * p.quantity
            nov += base * p.quantity * self._mult
            if p.quantity > 0:
                long_premium += base * p.quantity * self._mult
            else:
                short_count += abs(p.quantity)

        self._port_state = {
            "portfolio_ra": portfolio_ra,
            "nov": nov,
            "long_premium": long_premium,
            "short_count": short_count,
        }
        self._port_F = F
        self._port_time = time.monotonic()
        return self._port_state

    def _margin_from_state(self, ra: np.ndarray, nov: float,
                           long_premium: float, short_count: int) -> float:
        scan_risk = float(max(ra.max(), 0.0)) if ra.size else 0.0
        risk_margin = max(scan_risk - nov, 0.0)
        short_min = short_count * self.span.short_option_minimum
        return max(risk_margin, short_min, long_premium)

    # ── Public API ─────────────────────────────────────────────────
    def get_current_margin(self) -> float:
        """Synthetic SPAN margin over current positions."""
        F = self.market_data.state.underlying_price
        if F <= 0:
            return 0.0
        st = self._get_portfolio_state(F)
        return self._margin_from_state(
            st["portfolio_ra"], st["nov"],
            st["long_premium"], st["short_count"],
        )

    def update_cached_margin(self):
        """Refresh IBKR's reported maintenance margin (for reconciliation)."""
        try:
            for item in self.ib.accountValues(self._account_id):
                if item.tag == "MaintMarginReq" and item.currency == "USD":
                    self.cached_ibkr_margin = float(item.value)
                    synth = self.get_current_margin()
                    if synth > 0 and self.cached_ibkr_margin > 0:
                        logger.info(
                            "MARGIN RECON: synthetic=$%.0f ibkr=$%.0f ratio=%.2f",
                            synth, self.cached_ibkr_margin, synth / self.cached_ibkr_margin,
                        )
                    return
        except Exception as e:
            logger.warning("Failed to refresh IBKR margin: %s", e)

    def check_fill_margin(self, option_quote, quantity: int) -> Dict:
        """Compute current and post-fill synthetic SPAN margin.

        Hot path: ~0 Black-76 calls when both caches are warm. Just numpy
        adds against the cached portfolio state.
        """
        F = self.market_data.state.underlying_price
        if F <= 0:
            return {"current_margin": 0.0, "post_fill_margin": 0.0}

        st = self._get_portfolio_state(F)
        cur = self._margin_from_state(
            st["portfolio_ra"], st["nov"],
            st["long_premium"], st["short_count"],
        )

        # Candidate contribution
        K = option_quote.strike
        right = option_quote.put_call
        opt = self.market_data.state.get_option(K, option_quote.expiry, right)
        iv = float(opt.iv) if opt and opt.iv > 0 else 0.0
        T = max(days_to_expiry(option_quote.expiry), 0) / 365.0
        if iv <= 0 or T <= 0:
            return {"current_margin": cur, "post_fill_margin": cur}

        cand_ra, cand_base = self._get_strike_risk(K, right, T, iv, F)
        post_ra = st["portfolio_ra"] + cand_ra * quantity
        post_nov = st["nov"] + cand_base * quantity * self._mult
        if quantity > 0:
            post_long = st["long_premium"] + cand_base * quantity * self._mult
            post_short = st["short_count"]
        else:
            post_long = st["long_premium"]
            post_short = st["short_count"] + abs(quantity)

        post = self._margin_from_state(post_ra, post_nov, post_long, post_short)
        return {"current_margin": cur, "post_fill_margin": post}


class ConstraintChecker:
    """Pre-trade constraint validation: margin + delta + theta."""

    def __init__(self, margin_checker, portfolio, config):
        self.margin = margin_checker
        self.portfolio = portfolio
        self.config = config

    def check_constraints(self, option_quote, side: str,
                          quantity: int = 1) -> Tuple[bool, str]:
        """Check if a hypothetical fill passes all constraints.

        Improving-fill exception: if currently breached, allow fills that
        move state in the right direction.
        """
        fill_qty = quantity if side == "BUY" else -quantity
        multiplier = self.config.product.multiplier

        # ── Constraint 1: SPAN margin ───────────────────────────────
        margin_ceiling = (self.config.constraints.capital
                          * self.config.constraints.margin_ceiling_pct)
        margin_result = self.margin.check_fill_margin(option_quote, fill_qty)
        cur_margin = margin_result["current_margin"]
        post_margin = margin_result["post_fill_margin"]
        if post_margin > margin_ceiling:
            if not (cur_margin > margin_ceiling and post_margin <= cur_margin):
                return False, f"margin (${post_margin:,.0f} > ${margin_ceiling:,.0f})"

        # ── Constraint 2: Net delta ─────────────────────────────────
        delta_ceiling = self.config.constraints.delta_ceiling
        cur_delta = self.portfolio.net_delta
        post_delta = cur_delta + (option_quote.delta * fill_qty)
        if abs(post_delta) > delta_ceiling:
            if not (abs(cur_delta) > delta_ceiling and abs(post_delta) < abs(cur_delta)):
                return False, f"delta ({post_delta:+.2f} > ±{delta_ceiling})"

        # ── Constraint 3: Portfolio theta ───────────────────────────
        option_theta = option_quote.theta * multiplier
        cur_theta = self.portfolio.net_theta
        post_theta = cur_theta + (option_theta * fill_qty)
        theta_floor = self.config.constraints.theta_floor
        if post_theta < theta_floor:
            if not (cur_theta < theta_floor and post_theta >= cur_theta):
                return False, f"theta (${post_theta:,.0f} < ${theta_floor:,.0f})"

        return True, "ok"
