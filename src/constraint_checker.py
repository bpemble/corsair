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

    def __init__(self, ib: IB, config, market_data, portfolio, sabr=None):
        self.ib = ib
        self.config = config
        self.market_data = market_data
        self.portfolio = portfolio
        # SABR is optional but strongly recommended: it's the fallback IV
        # source for positions whose strike hasn't received a fresh bid/ask
        # tick (otherwise those positions are silently dropped from the
        # margin calc, which silently bypasses risk constraints).
        self.sabr = sabr
        self.span = SyntheticSpan(config)
        self.cached_ibkr_margin: float = 0.0
        # Calibration ratio synthetic ÷ ibkr_actual, refreshed at every recon.
        # Used to scale synthetic numbers down to IBKR-equivalent so the
        # constraint checker isn't gating on the structural overstatement
        # documented in synthetic_span.py (verticals/strangles run ~25-30%
        # high vs IBKR's actual margin). 1.0 means "no scaling, use synthetic
        # as-is" — applies before we have a recon datapoint.
        self.ibkr_scale: float = 1.0
        self._account_id = config.account.account_id
        self._mult = float(config.product.multiplier)

        # Combined portfolio state cache (Stage 1+: shared budget across
        # calls and puts). One entry covering ALL positions.
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

    def _get_forward(self) -> float:
        """Return the forward to price scenarios against. Prefer SABR's
        parity-implied forward (matches the option market's effective
        forward), fall back to the raw futures mid if SABR isn't ready."""
        if self.sabr is not None and self.sabr.forward and self.sabr.forward > 0:
            return float(self.sabr.forward)
        return float(self.market_data.state.underlying_price)

    def _resolve_iv(self, opt, strike: float, right: str) -> float:
        """Get a usable IV for SPAN computation. Prefer the live brentq IV
        from market_data; fall back to SABR's fitted vol when the strike
        hasn't received fresh bid/ask ticks. Returns 0 only if both fail."""
        if opt is not None and opt.iv and opt.iv > 0:
            return float(opt.iv)
        if self.sabr is not None and self.sabr.last_calibration is not None:
            try:
                v = self.sabr.get_vol(strike)
                if v and v > 0:
                    return float(v)
            except Exception:
                pass
        return 0.0

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
    def _accumulate(self, positions, F: float) -> dict:
        """Run a position iterable through the SPAN risk pipeline and
        return the aggregate state dict. Shared by the cached portfolio
        path and the uncached per-side display path."""
        portfolio_ra = np.zeros(16)
        nov = 0.0
        long_premium = 0.0
        short_count = 0
        for p in positions:
            opt = self.market_data.state.get_option(p.strike, p.expiry, p.put_call)
            iv = self._resolve_iv(opt, p.strike, p.put_call)
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
        return {
            "portfolio_ra": portfolio_ra,
            "nov": nov,
            "long_premium": long_premium,
            "short_count": short_count,
        }

    def _get_portfolio_state(self, F: float) -> dict:
        """Cached aggregate of the entire position book (calls + puts)."""
        if self._portfolio_cache_valid(F):
            return self._port_state
        state = self._accumulate(self.portfolio.positions, F)
        self._port_state = state
        self._port_F = F
        self._port_time = time.monotonic()
        return state

    def _margin_from_state(self, ra: np.ndarray, nov: float,
                           long_premium: float, short_count: int) -> float:
        scan_risk = float(max(ra.max(), 0.0)) if ra.size else 0.0
        risk_margin = max(scan_risk - nov, 0.0)
        short_min = short_count * self.span.short_option_minimum
        # Mirror SyntheticSpan.portfolio_margin: max-of-three. Conservative
        # +50% bias vs IBKR; documented in synthetic_span.py.
        return max(risk_margin, short_min, long_premium)

    # ── Public API ─────────────────────────────────────────────────
    def get_current_margin(self, right: Optional[str] = None) -> float:
        """SPAN margin over current positions, scaled to IBKR-equivalent.

        Returns raw synthetic × ibkr_scale, where ibkr_scale is the live
        calibration factor refreshed in update_cached_margin. This grounds
        the gating decision (and dashboard display) in real IBKR margin
        rather than synthetic's structural overstatement.

        Combined-budget mode: there is no per-side margin. The `right`
        argument is accepted for backward compatibility with callers that
        still pass it (snapshot writer's per-side display) — when passed,
        we return the contribution of just that side's positions to the
        combined SPAN. With no argument or right=None, returns the total."""
        F = self._get_forward()
        if F <= 0:
            return 0.0
        if right is None:
            st = self._get_portfolio_state(F)
            raw = self._margin_from_state(
                st["portfolio_ra"], st["nov"],
                st["long_premium"], st["short_count"],
            )
            return raw * self.ibkr_scale
        # Per-side display only — recompute over the filtered subset.
        # Not used by constraint check; just for the dashboard breakdown.
        return self._margin_for_side(F, right) * self.ibkr_scale

    def _margin_for_side(self, F: float, right: str) -> float:
        """Display-only per-side SPAN. Uncached; called by the snapshot
        writer at ~1 Hz for the per-side dashboard breakdown."""
        side_positions = (p for p in self.portfolio.positions if p.put_call == right)
        st = self._accumulate(side_positions, F)
        return self._margin_from_state(
            st["portfolio_ra"], st["nov"], st["long_premium"], st["short_count"],
        )

    def update_cached_margin(self):
        """Refresh IBKR's reported maintenance margin (for reconciliation)
        and recompute the synthetic-to-IBKR scaling ratio.

        We need to read RAW synthetic here (not the scaled get_current_margin
        output, which would be 1.0× tautologically) — so we call the span
        engine directly with the same portfolio state get_current_margin uses.
        """
        try:
            for item in self.ib.accountValues(self._account_id):
                if item.tag == "MaintMarginReq" and item.currency == "USD":
                    self.cached_ibkr_margin = float(item.value)
                    raw_synth = self._raw_current_margin()
                    if raw_synth > 0 and self.cached_ibkr_margin > 0:
                        ratio = raw_synth / self.cached_ibkr_margin
                        # Bound the scale to a sensible range.
                        # ratio > 1.0 = IBKR margin is LOWER than synthetic
                        #   (expected: SPAN offsets, or unmodeled hedges like
                        #   manual futures positions). Trust the IBKR number.
                        # ratio < 0.8 = IBKR margin is HIGHER than synthetic
                        #   (unexpected: synthetic is underestimating risk).
                        #   Fall back to raw synthetic (conservative).
                        if ratio >= 0.8:
                            self.ibkr_scale = 1.0 / ratio
                        else:
                            self.ibkr_scale = 1.0
                        logger.info(
                            "MARGIN RECON: synthetic=$%.0f ibkr=$%.0f ratio=%.2f scale=%.2f",
                            raw_synth, self.cached_ibkr_margin, ratio, self.ibkr_scale,
                        )
                    return
        except Exception as e:
            logger.warning("Failed to refresh IBKR margin: %s", e)

    def _raw_current_margin(self) -> float:
        """Internal: synthetic SPAN over current positions, NO scaling.
        Used by update_cached_margin to compute the calibration ratio."""
        F = self._get_forward()
        if F <= 0:
            return 0.0
        st = self._get_portfolio_state(F)
        return self._margin_from_state(
            st["portfolio_ra"], st["nov"],
            st["long_premium"], st["short_count"],
        )

    def check_fill_margin(self, option_quote, quantity: int) -> Dict:
        """Compute current and post-fill synthetic SPAN margin against the
        combined portfolio (calls + puts share one budget).

        Hot path: ~0 Black-76 calls when caches are warm. Just numpy adds
        against the cached combined portfolio state.
        """
        F = self._get_forward()
        if F <= 0:
            return {"current_margin": 0.0, "post_fill_margin": 0.0,
                    "current_long_premium": 0.0, "post_long_premium": 0.0}

        K = option_quote.strike
        right = option_quote.put_call
        st = self._get_portfolio_state(F)
        cur = self._margin_from_state(
            st["portfolio_ra"], st["nov"],
            st["long_premium"], st["short_count"],
        )

        # Candidate contribution
        opt = self.market_data.state.get_option(K, option_quote.expiry, right)
        iv = self._resolve_iv(opt, K, right)
        T = max(days_to_expiry(option_quote.expiry), 0) / 365.0
        if iv <= 0 or T <= 0:
            return {"current_margin": cur, "post_fill_margin": cur,
                    "current_long_premium": st["long_premium"],
                    "post_long_premium": st["long_premium"]}

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
        # Scale both current and post into IBKR-equivalent units so the
        # gating decision compares against the real margin we have to spend,
        # not synthetic's structural overstatement. ibkr_scale is bounded
        # in update_cached_margin to a sensible range with a 1.0 fallback.
        return {
            "current_margin": cur * self.ibkr_scale,
            "post_fill_margin": post * self.ibkr_scale,
            "current_long_premium": st["long_premium"],
            "post_long_premium": post_long,
        }


class ConstraintChecker:
    """Pre-trade constraint validation: margin + delta + theta.

    Combined-portfolio architecture: calls and puts share one budget for
    each constraint. A call fill and a put fill both consume from the same
    margin/delta/theta caps. This is the Stage 1+ architecture per the
    deployment ramp spec.
    """

    def __init__(self, margin_checker, portfolio, config):
        self.margin = margin_checker
        self.portfolio = portfolio
        self.config = config

    def _ceilings(self) -> Tuple[float, float, float]:
        """Return (margin_ceiling, delta_ceiling, theta_floor) — the single
        combined budget."""
        c = self.config.constraints
        return (float(c.capital) * float(c.margin_ceiling_pct),
                float(c.delta_ceiling), float(c.theta_floor))

    def check_constraints(self, option_quote, side: str,
                          quantity: int = 1) -> Tuple[bool, str]:
        """Check if a hypothetical fill passes all constraints.

        Two operating modes:

        1. Strict (default): each constraint (margin, delta, theta) is
           checked independently. An improving-fill exception exists
           per-constraint: if currently breached, allow fills that move
           that specific constraint in the right direction.

        2. Margin-escape (constraints.margin_escape_enabled + margin
           currently breached): tier-1 margin takes priority over
           tier-2 delta/theta. Any fill that strictly reduces post-fill
           margin is allowed, even if it drags tier-2 constraints into
           soft-breach territory, as long as no HARD kill limit is
           crossed (delta_kill, theta_kill, margin_kill_pct,
           long_premium). This exists to unwedge states where the
           strict per-constraint rule locks us past the margin ceiling
           with no permitted unwind path (observed 2026-04-09 after
           an adverse-fill burst — closing a short reduced margin but
           simultaneously tripped the theta floor, and the strict
           check rejected the recovery trade).
        """
        fill_qty = quantity if side == "BUY" else -quantity
        multiplier = self.config.product.multiplier
        margin_ceiling, delta_ceiling, theta_floor = self._ceilings()

        # Compute post-fill state once up front (needed by both paths).
        margin_result = self.margin.check_fill_margin(option_quote, fill_qty)
        cur_margin = margin_result["current_margin"]
        post_margin = margin_result["post_fill_margin"]

        cur_delta = self.portfolio.net_delta
        post_delta = cur_delta + (option_quote.delta * fill_qty)

        option_theta = option_quote.theta * multiplier
        cur_theta = self.portfolio.net_theta
        post_theta = cur_theta + (option_theta * fill_qty)

        cur_long_premium = margin_result["current_long_premium"]
        post_long_premium = margin_result["post_long_premium"]
        capital = float(self.config.constraints.capital)

        # ── Tier-1 margin priority escape ──────────────────────────
        escape_enabled = bool(getattr(
            self.config.constraints, "margin_escape_enabled", False))
        if (escape_enabled
                and cur_margin > margin_ceiling
                and post_margin < cur_margin):
            ks = getattr(self.config, "kill_switch", None)
            delta_kill = float(getattr(ks, "delta_kill", 5.0) or 5.0) if ks else 5.0
            theta_kill = float(getattr(ks, "theta_kill", -500) or -500) if ks else -500.0
            margin_kill_pct = float(
                getattr(ks, "margin_kill_pct", 0.70) or 0.70) if ks else 0.70
            margin_kill = capital * margin_kill_pct

            # Hard kills remain binding even in escape mode.
            if post_margin > margin_kill:
                return False, (
                    f"margin_kill (${post_margin:,.0f} > ${margin_kill:,.0f}) "
                    f"[escape]"
                )
            if abs(post_delta) > delta_kill:
                return False, (
                    f"delta_kill ({post_delta:+.2f} > ±{delta_kill}) [escape]"
                )
            if post_theta < theta_kill:
                return False, (
                    f"theta_kill (${post_theta:,.0f} < ${theta_kill:,.0f}) "
                    f"[escape]"
                )
            # Long-premium capital check is a solvency constraint
            # (cash outlay), not a risk metric — enforce it even in
            # escape mode, with the same improving-fill exception.
            if post_long_premium > capital:
                if not (cur_long_premium > capital
                        and post_long_premium <= cur_long_premium):
                    return False, (
                        f"long_premium (${post_long_premium:,.0f} > "
                        f"${capital:,.0f}) [escape]"
                    )
            return True, "margin_escape"

        # ── Constraint 1: SPAN margin (combined) ────────────────────
        if post_margin > margin_ceiling:
            if not (cur_margin > margin_ceiling and post_margin <= cur_margin):
                return False, f"margin (${post_margin:,.0f} > ${margin_ceiling:,.0f})"

        # ── Constraint 1b: Capital budget for long premium ──────────
        # SPAN risk margin doesn't fully capture the cash outlay for long
        # premium. Cap total long-premium outlay at the configured capital
        # so a runaway long path can't silently bleed cash.
        if post_long_premium > capital:
            if not (cur_long_premium > capital
                    and post_long_premium <= cur_long_premium):
                return False, (
                    f"long_premium (${post_long_premium:,.0f} > ${capital:,.0f})"
                )

        # ── Constraint 2: Combined net delta ────────────────────────
        if abs(post_delta) > delta_ceiling:
            if not (abs(cur_delta) > delta_ceiling and abs(post_delta) < abs(cur_delta)):
                return False, f"delta ({post_delta:+.2f} > ±{delta_ceiling})"

        # ── Constraint 3: Combined net theta ────────────────────────
        if post_theta < theta_floor:
            if not (cur_theta < theta_floor and post_theta >= cur_theta):
                return False, f"theta (${post_theta:,.0f} < ${theta_floor:,.0f})"

        return True, "ok"
