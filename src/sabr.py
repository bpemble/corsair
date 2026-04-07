"""SABR stochastic volatility model -- Hagan (2002) approximation.

Reused from Corsair v1. Provides:
- sabr_implied_vol: closed-form Black IV for (F, K, T) under SABR dynamics
- calibrate_sabr: fits (alpha, rho, nu) to market IVs with beta fixed
- SABRSurface: higher-level wrapper for Corsair v2's edge filter and stale detection
"""

import logging
import math
from dataclasses import dataclass
from datetime import datetime
from typing import List, Optional

import numpy as np
from scipy.optimize import least_squares

from .pricing import PricingEngine
from .utils import time_to_expiry_years

logger = logging.getLogger(__name__)


@dataclass
class SABRParams:
    """Fitted SABR parameters for a single expiry."""
    alpha: float
    beta: float
    rho: float
    nu: float
    rmse: float
    n_points: int


def sabr_implied_vol(
    F: float, K: float, T: float,
    alpha: float, beta: float, rho: float, nu: float,
) -> float:
    """Hagan (2002) SABR implied volatility approximation."""
    if T <= 0 or alpha <= 0 or F <= 0 or K <= 0:
        return alpha if alpha > 0 else 0.01

    eps = 1e-7

    # ATM case
    if abs(F - K) < eps * F:
        fmid = F
        fmid_beta = fmid ** (1.0 - beta)
        term1 = alpha / fmid_beta
        p1 = ((1.0 - beta) ** 2 / 24.0) * alpha ** 2 / (fmid ** (2.0 - 2.0 * beta))
        p2 = 0.25 * rho * beta * nu * alpha / fmid_beta
        p3 = (2.0 - 3.0 * rho ** 2) * nu ** 2 / 24.0
        return term1 * (1.0 + (p1 + p2 + p3) * T)

    # General case
    fk = F * K
    fk_beta = fk ** ((1.0 - beta) / 2.0)
    log_fk = math.log(F / K)

    z = (nu / alpha) * fk_beta * log_fk
    if abs(z) < eps:
        xz = 1.0
    else:
        sqrt_term = math.sqrt(1.0 - 2.0 * rho * z + z * z)
        xz = z / math.log((sqrt_term + z - rho) / (1.0 - rho))

    one_minus_beta = 1.0 - beta
    denom1 = fk_beta * (
        1.0
        + one_minus_beta ** 2 / 24.0 * log_fk ** 2
        + one_minus_beta ** 4 / 1920.0 * log_fk ** 4
    )

    p1 = one_minus_beta ** 2 / 24.0 * alpha ** 2 / (fk ** one_minus_beta)
    p2 = 0.25 * rho * beta * nu * alpha / fk_beta
    p3 = (2.0 - 3.0 * rho ** 2) * nu ** 2 / 24.0

    return (alpha / denom1) * xz * (1.0 + (p1 + p2 + p3) * T)


def calibrate_sabr(
    F: float, T: float,
    strikes: List[float], market_ivs: List[float],
    beta: float = 0.5, max_rmse: float = 0.03,
    weights: Optional[List[float]] = None,
) -> Optional[SABRParams]:
    """Calibrate SABR parameters (alpha, rho, nu) to market implied vols."""
    n = len(strikes)
    if n < 3 or len(market_ivs) != n:
        return None
    if T <= 0 or F <= 0:
        return None

    K = np.array(strikes, dtype=np.float64)
    mkt = np.array(market_ivs, dtype=np.float64)
    W = np.ones(n, dtype=np.float64)
    if weights is not None and len(weights) == n:
        W = np.array(weights, dtype=np.float64)
        w_sum = W.sum()
        if w_sum > 0:
            W = W * (n / w_sum)

    atm_idx = int(np.argmin(np.abs(K - F)))
    atm_iv = mkt[atm_idx]
    alpha_0 = atm_iv * F ** (1.0 - beta)
    alpha_0 = max(alpha_0, 0.001)

    def residuals(params):
        a, r, v = params
        model_ivs = np.array([
            sabr_implied_vol(F, k, T, a, beta, r, v) for k in K
        ])
        return (model_ivs - mkt) * W

    best_result = None
    best_cost = float("inf")

    initial_guesses = [
        (alpha_0, -0.3, 0.3),
        (alpha_0, -0.5, 0.5),
        (alpha_0, 0.0, 0.2),
        (alpha_0 * 0.8, -0.2, 0.4),
    ]

    for guess in initial_guesses:
        try:
            result = least_squares(
                residuals, x0=guess,
                bounds=([0.0001, -0.999, 0.0001], [10.0, 0.999, 5.0]),
                method="trf", max_nfev=200,
            )
            if result.cost < best_cost:
                best_cost = result.cost
                best_result = result
        except Exception:
            continue

    if best_result is None:
        logger.debug("SABR calibration failed: all initial guesses diverged")
        return None

    alpha_fit, rho_fit, nu_fit = best_result.x
    rmse = float(np.sqrt(np.mean(best_result.fun ** 2)))

    if rmse > max_rmse:
        logger.debug("SABR fit rejected: RMSE=%.4f > max_rmse=%.4f", rmse, max_rmse)
        return None

    return SABRParams(
        alpha=float(alpha_fit), beta=beta,
        rho=float(rho_fit), nu=float(nu_fit),
        rmse=rmse, n_points=n,
    )


class SABRSurface:
    """SABR vol surface for Corsair v2.

    Serves two roles:
    1. Edge filter -- reject quotes where penny-jump price < min_edge over theo
    2. Stale quote detection -- flag incumbent quotes implying abnormal IV
    """

    def __init__(self, config):
        self.config = config
        self.alpha = config.pricing.sabr_beta  # Will be overwritten by calibration
        self.beta = config.pricing.sabr_beta
        self.rho = -0.2
        self.nu = 0.4
        self.forward = 0.0
        self.last_calibration: Optional[datetime] = None
        self.params: Optional[SABRParams] = None
        self._front_month_expiry: Optional[str] = None
        # Theo cache: {(strike, put_call): (theo, monotonic_time)}
        # 100ms TTL — quote loop ticks faster than this so we hit warm cache often
        self._theo_cache: dict = {}
        self._theo_cache_ttl_sec: float = 0.1

    def set_expiry(self, expiry: str):
        """Set the front month expiry for TTE calculations."""
        self._front_month_expiry = expiry

    def calibrate(self, forward: float, options: dict):
        """Calibrate SABR parameters to current option chain.

        Args:
            forward: Current underlying futures price.
            options: Dict of {(strike, expiry, right): OptionQuote} with .iv attribute.
        """
        self.forward = forward

        # Drop strikes with wide BBOs from calibration — synthetic mids on
        # illiquid (typically deep ITM/OTM) strikes inject garbage IVs that
        # bias the entire fit. ETH options at this point have widths $5-8 on
        # liquid strikes vs $30+ on illiquid ones, so a $10 cutoff cleanly
        # separates them.
        max_cal_width = float(getattr(self.config.pricing, "max_calibration_bbo_width", 10.0))
        market_ivs = []
        for key, option in options.items():
            bid = getattr(option, "bid", 0) or 0
            ask = getattr(option, "ask", 0) or 0
            iv = getattr(option, "iv", 0) or 0
            if bid > 0 and ask > 0 and iv > 0 and (ask - bid) <= max_cal_width:
                market_ivs.append((option.strike, iv))

        if len(market_ivs) < 3:
            logger.warning("SABR calibration skipped: only %d valid quotes", len(market_ivs))
            return

        strikes = [s for s, _ in market_ivs]
        ivs = [v for _, v in market_ivs]

        tte = self._get_tte()
        if tte <= 0:
            return

        result = calibrate_sabr(
            F=forward, T=tte, strikes=strikes, market_ivs=ivs,
            beta=self.beta, max_rmse=self.config.pricing.sabr_max_rmse,
        )

        if result is not None:
            self.alpha = result.alpha
            self.rho = result.rho
            self.nu = result.nu
            self.params = result
            self.last_calibration = datetime.now()
            self._theo_cache.clear()  # parameters changed; flush stale theo
            logger.info(
                "SABR calibrated: alpha=%.4f rho=%.3f nu=%.3f RMSE=%.4f (n=%d)",
                result.alpha, result.rho, result.nu, result.rmse, result.n_points,
            )
        else:
            logger.warning("SABR calibration failed, keeping previous parameters")

    def get_vol(self, strike: float) -> float:
        """Return SABR-implied vol for a given strike."""
        tte = self._get_tte()
        if tte <= 0:
            return 0.0
        return sabr_implied_vol(
            self.forward, strike, tte,
            self.alpha, self.beta, self.rho, self.nu,
        )

    def get_theo(self, strike: float, put_call: str = "C") -> float:
        """Return theoretical option price from SABR surface (TTL-cached)."""
        import time as _time
        key = (strike, put_call)
        now = _time.monotonic()
        cached = self._theo_cache.get(key)
        if cached is not None and (now - cached[1]) < self._theo_cache_ttl_sec:
            return cached[0]
        tte = self._get_tte()
        if tte <= 0:
            return 0.0
        vol = self.get_vol(strike)
        theo = PricingEngine.black76_price(self.forward, strike, tte, vol, right=put_call)
        self._theo_cache[key] = (theo, now)
        return theo

    def is_quote_stale(self, option, threshold_iv_diff: float = None) -> bool:
        """Check if incumbent quote is stale by comparing IV to SABR surface."""
        if threshold_iv_diff is None:
            threshold_iv_diff = self.config.pricing.stale_iv_threshold

        tte = self._get_tte()
        if tte <= 0:
            return False

        sabr_vol_val = self.get_vol(option.strike)

        bid = getattr(option, "bid", 0) or 0
        ask = getattr(option, "ask", 0) or 0
        if bid <= 0 or ask <= 0:
            return True

        mid_price = (bid + ask) / 2
        implied_vol = PricingEngine.implied_vol(
            mid_price, self.forward, option.strike, tte,
            right=getattr(option, "put_call", "C"),
        )
        if implied_vol is None:
            return True

        return abs(implied_vol - sabr_vol_val) > threshold_iv_diff

    def _get_tte(self) -> float:
        """Get time to expiry in years for the front month."""
        if self._front_month_expiry is None:
            return 0.0
        return time_to_expiry_years(self._front_month_expiry)
