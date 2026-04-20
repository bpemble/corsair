"""SABR stochastic volatility model -- Hagan (2002) approximation.

Reused from Corsair v1. Provides:
- sabr_implied_vol: closed-form Black IV for (F, K, T) under SABR dynamics
- calibrate_sabr: fits (alpha, rho, nu) to market IVs with beta fixed
- SABRSurface: higher-level wrapper for Corsair v2's edge filter and stale detection
"""

import logging
import math
import multiprocessing
import threading
import time as _time
from collections import deque
from concurrent.futures import Future, ProcessPoolExecutor
from dataclasses import dataclass
from datetime import datetime
from typing import List, Optional, Tuple

import numpy as np
from scipy.optimize import least_squares

from . import backmonth_surface as _tsi
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

    # Alpha bounds scale with F^(1-beta): for beta=1.0 alpha is ~ATM_IV
    # (~0.6), but for beta=0.5 it's ~ATM_IV * sqrt(F) (~28). The upper
    # bound must accommodate both regimes.
    alpha_ub = max(10.0, alpha_0 * 5.0)

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
                bounds=([0.0001, -0.999, 0.0001], [alpha_ub, 0.999, 5.0]),
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


def implied_forward_from_parity(
    options: dict, ref_forward: float,
    max_spread: float = 10.0, max_deviation: float = 50.0,
) -> Optional[float]:
    """Derive the futures forward implied by put-call parity.

    For each strike where BOTH a call and a put have valid bid/ask with
    spread ≤ max_spread, parity gives F = K + (C_mid - P_mid). The result
    is the median across all such strikes — robust to outliers.

    Why we need this: in fast markets the option BBOs lag the live futures
    mid by a few hundred ms. SABR-fitted with the live mid produces theos
    that are systematically biased relative to the option market. Using the
    parity-implied forward eliminates the lag because we're using the same
    forward the option market is implicitly using.

    ref_forward filters outliers: implied F values that differ from the
    reference by more than max_deviation are dropped (one side of the
    pair is probably stale or has a bad print). If no pairs survive,
    returns None and the caller should fall back to ref_forward.
    """
    pairs: dict = {}  # K -> {"C": mid, "P": mid}
    for opt in options.values():
        if opt.bid <= 0 or opt.ask <= 0:
            continue
        if (opt.ask - opt.bid) > max_spread:
            continue
        mid = (opt.bid + opt.ask) / 2.0
        K = opt.strike
        pairs.setdefault(K, {})[opt.put_call] = mid

    forwards = []
    for K, sides in pairs.items():
        if "C" not in sides or "P" not in sides:
            continue
        F_K = K + (sides["C"] - sides["P"])
        if abs(F_K - ref_forward) <= max_deviation:
            forwards.append(F_K)

    if not forwards:
        return None
    forwards.sort()
    return forwards[len(forwards) // 2]


def _worst_strike_residual(
    result, forward: float, strikes: List[float], market_ivs: List[float], tte: float,
) -> Tuple[Optional[float], float]:
    """Find the strike with the largest model-vs-market IV residual.

    Used in the rejection log path to surface which strike is poisoning
    the fit. A 5-vol residual on a single deep-wing strike is invisible
    from the aggregate RMSE, but it can pull the entire surface — knowing
    the offending strike lets us decide whether the issue is data quality
    (a stale tick) or genuine smile dislocation.
    """
    try:
        worst_strike = None
        worst_resid = 0.0
        for k, mkt_iv in zip(strikes, market_ivs):
            model_iv = sabr_implied_vol(
                forward, k, tte,
                result.alpha, result.beta, result.rho, result.nu,
            )
            resid = abs(model_iv - mkt_iv)
            if resid > worst_resid:
                worst_resid = resid
                worst_strike = k
        return worst_strike, worst_resid
    except Exception:
        return None, 0.0


def _sabr_quality_ok(result, min_strikes: int) -> Tuple[bool, str]:
    """Structural sanity check on a SABR fit result. Returns (ok, reason).

    Catches degenerate fits that the optimizer accepts numerically but
    that produce nonsense theo at the wings:
      - too few input strikes (calibration is overdetermined → unstable)
      - alpha non-positive (vol level must be positive)
      - rho near ±1 (correlation pinned at the boundary → instability)
      - nu non-positive or absurdly large (vol-of-vol degenerate)
    """
    if result.n_points < min_strikes:
        return False, f"only {result.n_points} strikes (need ≥{min_strikes})"
    if result.alpha <= 0:
        return False, f"alpha {result.alpha:.4f} ≤ 0"
    if abs(result.rho) > 0.95:
        return False, f"rho {result.rho:+.3f} pinned to boundary"
    if result.nu <= 0 or result.nu > 10:
        return False, f"nu {result.nu:.3f} out of [0, 10]"
    return True, "ok"


# ── SVI (Stochastic Volatility Inspired) model ─────────────────────────
#
# Gatheral's raw SVI parametrization for total implied variance:
#   w(k) = a + b * (rho * (k - m) + sqrt((k - m)^2 + sigma^2))
# where k = log(K/F) and w = sigma_BS^2 * T (total variance).
#
# 5 free parameters (a, b, rho, m, sigma) give enough flexibility to
# capture the ETH put skew + curvature that SABR's 3 params cannot.


@dataclass
class SVIParams:
    """Fitted SVI parameters for a single expiry/side."""
    a: float
    b: float
    rho: float
    m: float
    sigma: float
    rmse: float
    n_points: int


def svi_total_variance(k: float, a: float, b: float, rho: float,
                       m: float, sigma: float) -> float:
    """Raw SVI total variance w(k) for log-moneyness k."""
    return a + b * (rho * (k - m) + math.sqrt((k - m) ** 2 + sigma ** 2))


def svi_implied_vol(F: float, K: float, T: float,
                    a: float, b: float, rho: float,
                    m: float, sigma: float) -> float:
    """Return Black implied vol from SVI parameters."""
    if T <= 0 or K <= 0 or F <= 0:
        return 0.0
    k = math.log(K / F)
    w = svi_total_variance(k, a, b, rho, m, sigma)
    if w <= 0:
        return 0.001
    return math.sqrt(w / T)


def calibrate_svi(
    F: float, T: float,
    strikes: List[float], market_ivs: List[float],
    max_rmse: float = 0.03,
    weights: Optional[List[float]] = None,
) -> Optional[SVIParams]:
    """Calibrate SVI parameters to market implied vols."""
    n = len(strikes)
    if n < 5 or len(market_ivs) != n:
        return None
    if T <= 0 or F <= 0:
        return None

    K = np.array(strikes, dtype=np.float64)
    mkt = np.array(market_ivs, dtype=np.float64)
    ks = np.log(K / F)  # log-moneyness
    mkt_w = mkt ** 2 * T  # total variance targets

    W = np.ones(n, dtype=np.float64)
    if weights is not None and len(weights) == n:
        W = np.array(weights, dtype=np.float64)
        w_sum = W.sum()
        if w_sum > 0:
            W = W * (n / w_sum)

    atm_var = float(np.interp(0.0, ks, mkt_w))

    def residuals(params):
        a, b, rho, m, sig = params
        model_w = np.array([
            svi_total_variance(k, a, b, rho, m, sig) for k in ks
        ])
        # Fit in IV space for comparability with SABR RMSE
        model_iv = np.sqrt(np.maximum(model_w, 1e-10) / T)
        return (model_iv - mkt) * W

    # Initial guesses spanning mild to steep skew regimes.
    # ETH puts have steep skew that needs larger b, more negative rho,
    # and wider sigma to avoid pinning rho to -1.0.
    initial_guesses = [
        (atm_var, 0.1, -0.3, 0.0, 0.1),
        (atm_var, 0.2, -0.5, 0.0, 0.05),
        (atm_var * 0.8, 0.15, -0.2, -0.05, 0.15),
        (atm_var, 0.05, -0.4, 0.02, 0.08),
        # Steep-skew guesses (puts)
        (atm_var, 0.1, -0.7, -0.1, 0.3),
        (atm_var * 0.5, 0.15, -0.6, -0.05, 0.5),
        (atm_var, 0.08, -0.8, -0.15, 0.4),
        (atm_var * 0.3, 0.2, -0.5, 0.0, 0.6),
    ]

    best_result = None
    best_cost = float("inf")

    for guess in initial_guesses:
        try:
            result = least_squares(
                residuals, x0=guess,
                bounds=(
                    [0.0, 0.0001, -0.999, -1.0, 0.0001],   # lower
                    [5.0, 5.0,     0.999,  1.0, 2.0],       # upper
                ),
                method="trf", max_nfev=500,
            )
            if result.cost < best_cost:
                best_cost = result.cost
                best_result = result
        except Exception:
            continue

    if best_result is None:
        return None

    a, b, rho, m, sig = best_result.x
    rmse = float(np.sqrt(np.mean(best_result.fun ** 2)))

    if rmse > max_rmse:
        return None

    # Butterfly arbitrage check: b * (1 + |rho|) * sigma > 0 is always
    # true given our bounds, but verify total variance is non-negative
    # at the extreme calibration strikes.
    for k in ks:
        if svi_total_variance(k, a, b, rho, m, sig) < 0:
            return None

    return SVIParams(
        a=float(a), b=float(b), rho=float(rho),
        m=float(m), sigma=float(sig),
        rmse=rmse, n_points=n,
    )


def _svi_quality_ok(result: SVIParams, min_strikes: int) -> Tuple[bool, str]:
    """Sanity-check a fitted SVI result."""
    if result.n_points < min_strikes:
        return False, f"only {result.n_points} strikes (need ≥{min_strikes})"
    if result.b <= 0:
        return False, f"b {result.b:.4f} ≤ 0"
    # SVI rho near ±1 is valid for steep skews (unlike SABR) — only
    # reject if it's literally at the optimizer bound AND sigma is tiny,
    # which indicates the fit is degenerate (a kink, not a curve).
    if abs(result.rho) > 0.998 and result.sigma < 0.01:
        return False, f"rho {result.rho:+.3f} at boundary with sigma={result.sigma:.4f} (degenerate)"
    return True, "ok"


class SABRSurface:
    """Vol surface for Corsair v2 — supports SABR or SVI backend.

    Serves two roles:
    1. Edge filter -- reject quotes where penny-jump price < min_edge over theo
    2. Stale quote detection -- flag incumbent quotes implying abnormal IV
    """

    def __init__(self, config, csv_logger=None):
        self.config = config
        self.csv_logger = csv_logger
        self.beta = config.pricing.sabr_beta
        self.forward = 0.0
        # Spot (state.underlying_price) at the time of the fit. Used by
        # delta_adjust_theo to bridge forward by spot-drift rather than
        # spot-vs-forward-gap, which preserves the parity basis between fits.
        self.spot_at_fit = 0.0
        self.last_calibration: Optional[datetime] = None
        # vol_model: "sabr" (default, 3-param per side) or "svi" (5-param per side)
        self._vol_model = str(getattr(config.pricing, "vol_model", "sabr")).lower()
        # Per-side parameters. Separate fits let each side match its own skew.
        self._side_params: dict = {
            "C": {"alpha": 0.6, "rho": -0.2, "nu": 0.4, "params": None,
                   "svi_params": None, "rmse_history": deque(maxlen=20)},
            "P": {"alpha": 0.6, "rho": -0.2, "nu": 0.4, "params": None,
                   "svi_params": None, "rmse_history": deque(maxlen=20)},
        }
        self.params: Optional[SABRParams] = None
        self._front_month_expiry: Optional[str] = None
        self._theo_cache: dict = {}
        # fit() clears this cache on every surface update, so real
        # invalidation is event-driven; the TTL is just a ceiling. 100ms was
        # shorter than the 250ms snapshot interval so every dashboard build
        # missed and recomputed scipy.norm.cdf for ~294 options/cycle.
        self._theo_cache_ttl_sec: float = 5.0
        # is_quote_stale caches: quote-engine hot path was running brentq
        # implied_vol solve (~20-40 scipy.norm.cdf calls) per option per
        # cycle, pegging the event loop and backing up IBKR acks.
        self._iv_cache: dict = {}
        self._iv_cache_ttl_sec: float = 2.0

    def set_expiry(self, expiry: str):
        """Set the front month expiry for TTE calculations."""
        self._front_month_expiry = expiry

    def get_vol(self, strike: float, put_call: str = "C") -> float:
        """Return implied vol for a given strike using the OTM-side's fit.

        Each side's parameters are fitted to that side's OTM wing data
        (C-side against K >= F, P-side against K < F), so we consult the
        fit whose native region actually covers the strike. Routing by
        ``put_call`` instead produced a parity-violating vol gap (back-
        month ATM: ~2 vol points, median ~$408/contract) because a call
        at K=F was priced from the C extrapolation while a put at the
        same K was priced from the P extrapolation — two different IVs
        at one strike. The ``put_call`` argument is kept for call-site
        compatibility but ignored by the routing.
        """
        tte = self._get_tte()
        if tte <= 0:
            return 0.0
        # Before the first calibration establishes forward, fall back to
        # the C-side defaults — matches prior behavior at boot.
        if self.forward > 0:
            side = "C" if strike >= self.forward else "P"
        else:
            side = "C"
        sp = self._side_params[side]
        if self._vol_model == "svi" and sp.get("svi_params") is not None:
            p = sp["svi_params"]
            return svi_implied_vol(self.forward, strike, tte,
                                   p.a, p.b, p.rho, p.m, p.sigma)
        return sabr_implied_vol(
            self.forward, strike, tte,
            sp["alpha"], self.beta, sp["rho"], sp["nu"],
        )

    def get_theo(self, strike: float, put_call: str = "C") -> float:
        """Return theoretical option price from SABR surface (TTL-cached)."""
        key = (strike, put_call)
        now = _time.monotonic()
        cached = self._theo_cache.get(key)
        if cached is not None and (now - cached[1]) < self._theo_cache_ttl_sec:
            return cached[0]
        tte = self._get_tte()
        if tte <= 0:
            return 0.0
        vol = self.get_vol(strike, put_call)
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

        sabr_vol_val = self.get_vol(option.strike, getattr(option, "put_call", "C"))

        bid = getattr(option, "bid", 0) or 0
        ask = getattr(option, "ask", 0) or 0
        if bid <= 0 or ask <= 0:
            return True

        mid_price = (bid + ask) / 2
        put_call = getattr(option, "put_call", "C")
        # Round mid to pricing tick so small float wobbles still cache-hit.
        # HG options are 0.0005 ticks, ETH is 0.5; quantize to 4 decimals
        # which covers both without introducing error.
        iv_key = (option.strike, put_call, round(mid_price, 4))
        now = _time.monotonic()
        cached = self._iv_cache.get(iv_key)
        if cached is not None and (now - cached[1]) < self._iv_cache_ttl_sec:
            implied_vol = cached[0]
        else:
            implied_vol = PricingEngine.implied_vol(
                mid_price, self.forward, option.strike, tte,
                right=put_call,
            )
            self._iv_cache[iv_key] = (implied_vol, now)
            if len(self._iv_cache) > 2000:
                self._iv_cache.clear()
        if implied_vol is None:
            return True

        return abs(implied_vol - sabr_vol_val) > threshold_iv_diff

    def _get_tte(self) -> float:
        """Get time to expiry in years for the front month."""
        if self._front_month_expiry is None:
            return 0.0
        return time_to_expiry_years(self._front_month_expiry)


def delta_adjust_theo(theo: float, delta: float,
                      spot_at_fit: float, current_underlying: float) -> float:
    """First-order Taylor reprice: adjust SABR theo by delta × (S − S_fit).

    Bridges the gap between SABR refit cadence (~seconds) and quoting cadence
    (~ms) so theo tracks spot drift between fits. Uses *spot drift* rather
    than the *spot-vs-forward gap* — the latter mis-adjusted theo by the
    carry basis (forward − spot), pushing calls below mid and puts above
    mid by the full basis amount. Tracking spot drift preserves the parity
    basis locked in at fit time. Floored at 0 — a negative result means
    the extrapolation has overreached and the strike is effectively worthless.
    """
    if delta == 0 or spot_at_fit <= 0 or current_underlying <= 0:
        return theo
    return max(theo + delta * (current_underlying - spot_at_fit), 0.0)


def _parity_implied_forward_dict(opts_list, ref_forward, max_spread, max_dev=50.0):
    """Picklable variant of implied_forward_from_parity that takes plain dicts
    instead of OptionQuote instances. Used by the ProcessPool fit worker."""
    pairs: dict = {}
    for o in opts_list:
        bid, ask = o['bid'], o['ask']
        if bid <= 0 or ask <= 0 or (ask - bid) > max_spread:
            continue
        mid = (bid + ask) / 2.0
        pairs.setdefault(o['strike'], {})[o['put_call']] = mid
    forwards = []
    for K, sides in pairs.items():
        if 'C' not in sides or 'P' not in sides:
            continue
        F_K = K + (sides['C'] - sides['P'])
        if abs(F_K - ref_forward) <= max_dev:
            forwards.append(F_K)
    if not forwards:
        return None
    forwards.sort()
    return forwards[len(forwards) // 2]


def _async_fit_worker(forward_in: float, expiries_data: dict,
                      beta: float, max_rmse: float, use_svi: bool,
                      max_cal_width: float) -> dict:
    """ProcessPool worker — runs SABR/SVI fits off-process to bypass the GIL.

    Quality gates, drift detection, telemetry, and surface mutation all stay
    in the main process; this worker is just the scipy.optimize call (which
    holds the GIL when run in-thread) plus the per-strike brentq IV solves.

    Inputs and outputs are pure-data so they pickle cleanly.
        expiries_data: {expiry: {'tte': float, 'options': [{'strike','put_call','bid','ask'}, ...]}}
    Returns: {expiry: {'forward': float|None, 'parity_delta': float|None,
                       'C': result|None, 'P': result|None,
                       'C_n': int, 'P_n': int}}
    """
    out = {}
    for exp, data in expiries_data.items():
        tte = data['tte']
        opts = data['options']
        if tte <= 0:
            out[exp] = {'forward': forward_in, 'spot_at_fit': forward_in,
                        'parity_delta': None,
                        'C': None, 'P': None, 'C_n': 0, 'P_n': 0}
            continue
        implied = _parity_implied_forward_dict(opts, forward_in, max_cal_width)
        # Trust the parity-implied forward directly rather than blending
        # 50/50 with spot. Prior blend muted the basis (forward − spot) by
        # half, biasing calls below mid and puts above mid (2026-04-20
        # measurement: ~$0.005 systematic bias, 5× our $0.001 edge target).
        forward = implied if implied is not None else forward_in
        parity_delta = (implied - forward_in) if implied is not None else None
        side_results = {}
        side_n = {}
        side_obs = {}
        for side in ('C', 'P'):
            cal_data = []
            for o in opts:
                if o['put_call'] != side:
                    continue
                bid, ask = o['bid'], o['ask']
                if bid > 0 and ask > 0 and (ask - bid) <= max_cal_width:
                    mid = (bid + ask) / 2.0
                    iv = PricingEngine.implied_vol(
                        mid, forward, o['strike'], tte, right=side)
                    if iv is not None and iv > 0:
                        cal_data.append((o['strike'], iv, ask - bid))
            # Carry the cleaned (strike, iv) pairs forward regardless of
            # whether the native fit will accept the set — the TSI fallback
            # needs these as anchors when the min-strike gate fails.
            side_obs[side] = [(s, v) for s, v, _ in cal_data]
            min_pts = 5 if use_svi else 3
            if len(cal_data) < min_pts:
                side_results[side] = None
                side_n[side] = len(cal_data)
                continue
            strikes = [s for s, _, _ in cal_data]
            ivs = [v for _, v, _ in cal_data]
            spreads = [w for _, _, w in cal_data]
            min_spread = min(spreads)
            weights = [min_spread / s for s in spreads]
            if use_svi:
                r = calibrate_svi(F=forward, T=tte, strikes=strikes,
                                  market_ivs=ivs, max_rmse=max_rmse,
                                  weights=weights)
            else:
                r = calibrate_sabr(F=forward, T=tte, strikes=strikes,
                                   market_ivs=ivs, beta=beta,
                                   max_rmse=max_rmse, weights=weights)
            side_results[side] = r
            side_n[side] = len(cal_data)
        out[exp] = {
            'forward': forward, 'spot_at_fit': forward_in,
            'parity_delta': parity_delta,
            'C': side_results.get('C'), 'P': side_results.get('P'),
            'C_n': side_n.get('C', 0), 'P_n': side_n.get('P', 0),
            'C_obs': side_obs.get('C', []),
            'P_obs': side_obs.get('P', []),
        }
    return out


class MultiExpirySABR:
    """Multi-expiry SABR wrapper.

    Holds one ``SABRSurface`` per subscribed expiry and dispatches calls
    to the right surface. Keeps the legacy single-expiry API (``params``,
    ``forward``, ``last_calibration``, ``set_expiry``, ``get_theo``,
    ``is_quote_stale``) working by delegating to the front-month surface
    (first element of the expiry list).
    """

    def __init__(self, config, csv_logger=None):
        self.config = config
        self.csv_logger = csv_logger
        self._surfaces: dict = {}
        self._expiries: List[str] = []
        # Async calibration runs in a subprocess (ProcessPool, not Thread)
        # because scipy.optimize Python orchestration code holds the GIL —
        # measured: a thread-pool fit blocked the main asyncio loop and
        # spiked openOrder ack RTT to ~3.8s p99. Spawn context (slower
        # startup, ~1s) avoids fork() races with asyncio/threads already
        # running in the parent at lazy-init time.
        self._cal_pool = ProcessPoolExecutor(
            max_workers=1, mp_context=multiprocessing.get_context("spawn"),
        )
        self._cal_future: Optional[Future] = None
        # Lock serializes parameter updates so readers of get_theo/get_vol
        # never observe a half-updated surface. Fit computation runs
        # outside the lock; only the final assignment is guarded.
        self._params_lock = threading.Lock()
        # Expiries whose surface just refitted (theo changed). Quote engine
        # consumes this each cycle to force re-eval of active orders for
        # those expiries — eliminates the temporal race where a resting
        # price stays at its pre-refit edge after theo drifts.
        self._refit_pending: set = set()

    # ------------- expiry management -------------

    def set_expiries(self, expiries: List[str]):
        expiries = list(expiries or [])
        self._expiries = expiries
        # Add new
        for exp in expiries:
            if exp not in self._surfaces:
                surf = SABRSurface(self.config, csv_logger=self.csv_logger)
                surf.set_expiry(exp)
                self._surfaces[exp] = surf
            else:
                self._surfaces[exp].set_expiry(exp)
        # Drop retired
        for exp in list(self._surfaces.keys()):
            if exp not in expiries:
                self._surfaces.pop(exp, None)

    def set_expiry(self, expiry: str):
        """Legacy single-expiry shim."""
        self.set_expiries([expiry])

    @property
    def front_month(self) -> Optional[str]:
        return self._expiries[0] if self._expiries else None

    def _front_surface(self) -> Optional[SABRSurface]:
        fm = self.front_month
        if fm is None:
            return None
        return self._surfaces.get(fm)

    # ------------- calibration -------------

    def calibrate(self, forward: float, options: dict, expiry: str = None):
        """Calibrate SABR. If expiry is None, calibrates all subscribed expiries.

        Synchronous — runs on the calling thread. Used at startup and by
        the watchdog on reconnect, where we want to block until fresh
        theos are available. Hot-path callers should use ``calibrate_async``.
        """
        targets = [expiry] if expiry is not None else list(self._expiries)
        params = self._fit_params()
        if params is None:
            return
        expiries_data = self._build_fit_input(targets, options)
        if not expiries_data:
            return
        out = _async_fit_worker(forward, expiries_data, *params)
        with self._params_lock:
            for exp, res in out.items():
                surf = self._surfaces.get(exp)
                if surf is not None:
                    self._apply_fit_to_surface(surf, exp, res)

    def _fit_params(self):
        """Read fit hyper-parameters off the front surface. Returns
        (beta, max_rmse, use_svi, max_cal_width) tuple suitable for splat
        into ``_async_fit_worker``, or None if no surface is set up yet."""
        front_surf = self._front_surface()
        if front_surf is None:
            return None
        return (
            front_surf.beta,
            self.config.pricing.sabr_max_rmse,
            front_surf._vol_model == "svi",
            float(getattr(self.config.pricing, "max_calibration_bbo_width", 10.0)),
        )

    def _build_fit_input(self, expiries, options) -> dict:
        """Flatten the OptionQuote chain into picklable per-expiry input.
        Same shape consumed by ``_async_fit_worker`` whether the call goes
        through a process pool or runs in-process from the sync path."""
        out = {}
        for exp in expiries:
            opts_list = []
            for (_, expiry, _), opt in options.items():
                if expiry != exp:
                    continue
                opts_list.append({
                    'strike': opt.strike,
                    'put_call': opt.put_call,
                    'bid': float(getattr(opt, 'bid', 0) or 0),
                    'ask': float(getattr(opt, 'ask', 0) or 0),
                })
            if opts_list:
                out[exp] = {
                    'tte': time_to_expiry_years(exp),
                    'options': opts_list,
                }
        return out

    def calibrate_async(self, forward: float, options: dict) -> bool:
        """Fire SABR calibration on the background ProcessPool, return
        immediately. Returns True if submitted, False if a prior fit is still
        in flight (we don't queue a backlog — the next tick picks up fresh
        state anyway)."""
        if self._cal_future is not None and not self._cal_future.done():
            return False
        if not self._expiries:
            return False
        params = self._fit_params()
        if params is None:
            return False
        expiries_data = self._build_fit_input(self._expiries, options)
        if not expiries_data:
            return False
        try:
            self._cal_future = self._cal_pool.submit(
                _async_fit_worker, forward, expiries_data, *params,
            )
        except Exception as e:
            logger.warning("ProcessPool submit failed: %s", e)
            return False
        # add_done_callback fires on a pool-internal thread; surface mutation
        # acquires _params_lock so readers never see torn state.
        self._cal_future.add_done_callback(self._apply_fit_results)
        return True

    def _tsi_fallback(self, exp: str, side: str, forward: float,
                       observations) -> Optional["_tsi.SVIParams"]:
        """Build a term-structure-interpolation SVI for ``(exp, side)``.

        Walks the existing surface set to find the earliest expiry (other
        than ``exp``) whose same-side fit is valid — that becomes the donor
        whose skew shape (b, ρ, m, σ) we reuse. Level ``a`` is solved from
        the near-ATM anchors in ``observations``.

        Returns None when no donor is available, no near-ATM anchors exist,
        or the synthesized surface fails a basic no-arb check. Caller
        treats that as "calibration skip" (same as the pre-fallback path).
        """
        donor = None
        donor_exp: Optional[str] = None
        for candidate in self._expiries:
            if candidate == exp:
                continue
            other = self._surfaces.get(candidate)
            if other is None:
                continue
            p = other._side_params.get(side, {}).get("svi_params")
            if p is not None:
                donor = p
                donor_exp = candidate
                break
        if donor is None:
            return None
        tte = time_to_expiry_years(exp)
        if tte <= 0 or forward <= 0:
            return None
        anchors = _tsi.extract_atm_anchors(observations, forward)
        if not anchors:
            return None
        # Adapt the donor to backmonth_surface.SVIParams so the fallback
        # sees the fields it expects (our native SVIParams lacks the
        # provenance fields but duck-types fine for the shape inputs).
        donor_view = _tsi.SVIParams(
            a=donor.a, b=donor.b, rho=donor.rho, m=donor.m, sigma=donor.sigma,
            rmse=getattr(donor, "rmse", 0.0),
            n_points=getattr(donor, "n_points", 0),
        )
        fallback = _tsi.fit_backmonth_from_frontmonth(
            donor_view, anchors, forward, tte, donor_expiry_tag=donor_exp,
        )
        if fallback is None:
            return None
        ok, reason = _tsi.no_arb_check(fallback)
        if not ok:
            logger.warning(
                "TSI fallback [%s %s] rejected by no-arb check: %s",
                exp, side, reason,
            )
            return None
        return fallback

    def _apply_fit_to_surface(self, surf, exp: str, res: dict):
        """Apply a per-expiry fit result to one surface. Caller must hold
        ``self._params_lock``. Shared by the async ProcessPool callback and
        the synchronous startup/watchdog calibrate path."""
        surf.forward = res['forward']
        surf.spot_at_fit = res.get('spot_at_fit', res['forward'])
        pd = res.get('parity_delta')
        if pd is not None and abs(pd) > 1.0:
            logger.info(
                "SABR forward [%s]: blended=%.2f parity_delta=%+.2f",
                exp, res['forward'], pd,
            )
        use_svi = (surf._vol_model == "svi")
        model_name = "SVI" if use_svi else "SABR"
        sabr_min = int(getattr(self.config.pricing, "sabr_min_strikes", 8))
        svi_min = int(getattr(self.config.pricing, "svi_min_strikes", 10))
        any_updated = False
        for side in ("C", "P"):
            result = res.get(side)
            if result is None:
                continue
            quality_ok, quality_reason = (
                _svi_quality_ok(result, svi_min) if use_svi
                else _sabr_quality_ok(result, sabr_min // 2)
            )
            tsi_used = False
            if not quality_ok and use_svi:
                # Native fit failed the gate — attempt the term-structure-
                # interpolation fallback. Borrows skew shape from a donor
                # expiry that DID fit well; anchors level to whatever
                # near-ATM observations the calibrator had at this expiry.
                fallback = self._tsi_fallback(
                    exp, side, res['forward'],
                    res.get(f'{side}_obs') or [],
                )
                if fallback is not None:
                    result = fallback
                    quality_ok = True
                    tsi_used = True
                    logger.info(
                        "SVI %s [%s] TSI fallback: donor=%s anchors=%d "
                        "a=%.4f (borrowed b=%.4f rho=%+.3f m=%.4f sigma=%.4f)",
                        side, exp, result.tsi_donor_expiry,
                        result.tsi_anchor_count, result.a, result.b,
                        result.rho, result.m, result.sigma,
                    )
            if not quality_ok:
                logger.warning(
                    "%s %s [%s] fit rejected (%s): RMSE=%.4f n=%d",
                    model_name, side, exp, quality_reason,
                    result.rmse, result.n_points,
                )
                continue
            sp = surf._side_params[side]
            prior = sp.get("svi_params") if use_svi else sp.get("params")
            # Skip RMSE regression check on TSI fallbacks — their `rmse`
            # field is anchor-dispersion, not a real fit residual, so the
            # 1.5× threshold comparison is meaningless.
            if (not tsi_used and prior is not None
                    and result.rmse > max(prior.rmse * 1.5, 0.01)):
                logger.warning(
                    "%s %s [%s] fit rejected (rmse_regression %.4f > 1.5x prior %.4f)",
                    model_name, side, exp, result.rmse, prior.rmse,
                )
                continue
            if use_svi:
                sp["svi_params"] = result
            else:
                sp["alpha"] = result.alpha
                sp["rho"] = result.rho
                sp["nu"] = result.nu
                sp["params"] = result
            any_updated = True
            if use_svi:
                logger.info(
                    "SVI %s [%s] calibrated: a=%.4f b=%.4f rho=%.3f m=%.4f sig=%.4f RMSE=%.4f (n=%d)",
                    side, exp, result.a, result.b, result.rho,
                    result.m, result.sigma, result.rmse, result.n_points,
                )
            else:
                logger.info(
                    "SABR %s [%s] calibrated: alpha=%.4f rho=%.3f nu=%.3f RMSE=%.4f (n=%d)",
                    side, exp, result.alpha, result.rho, result.nu,
                    result.rmse, result.n_points,
                )
            if surf.csv_logger is not None:
                try:
                    if use_svi:
                        surf.csv_logger.log_calibration(
                            expiry=exp, side=side, model="svi",
                            forward=res['forward'],
                            tte_years=time_to_expiry_years(exp),
                            n_points=result.n_points, rmse=result.rmse,
                            a=result.a, b=result.b, rho_svi=result.rho,
                            m=result.m, sigma=result.sigma,
                        )
                    else:
                        surf.csv_logger.log_calibration(
                            expiry=exp, side=side, model="sabr",
                            forward=res['forward'],
                            tte_years=time_to_expiry_years(exp),
                            n_points=result.n_points, rmse=result.rmse,
                            alpha=result.alpha, beta=surf.beta,
                            rho_sabr=result.rho, nu=result.nu,
                        )
                except Exception as e:
                    logger.debug("calibration telemetry log failed: %s", e)
            sp["rmse_history"].append(result.rmse)
            if len(sp["rmse_history"]) >= 5:
                median_rmse = float(np.median(sp["rmse_history"]))
                if median_rmse > 0 and result.rmse > max(median_rmse * 1.5, 0.005):
                    logger.warning(
                        "%s %s [%s] RMSE drift: current=%.4f vs median=%.4f",
                        model_name, side, exp, result.rmse, median_rmse,
                    )
        if any_updated:
            surf.last_calibration = datetime.now()
            surf._theo_cache.clear()
            surf._iv_cache.clear()
            pp = surf._side_params["P"]
            surf.params = pp.get("svi_params") or pp.get("params")
            self._refit_pending.add(exp)

    def _apply_fit_results(self, future: Future):
        """Async callback: apply ProcessPool fit output to surfaces."""
        try:
            out = future.result()
        except Exception as e:
            logger.warning("Async SABR fit failed: %s", e)
            return
        for exp, res in out.items():
            surf = self._surfaces.get(exp)
            if surf is None:
                continue
            with self._params_lock:
                self._apply_fit_to_surface(surf, exp, res)

    def consume_refit_pending(self) -> set:
        """Return and clear the set of expiries that have refitted since
        the last call. Quote engine uses this to force a full re-eval of
        active orders for those expiries on the next cycle, closing the
        race where a resting price stayed at its pre-refit edge after
        theo drifted."""
        with self._params_lock:
            r = self._refit_pending
            self._refit_pending = set()
        return r

    def shutdown(self, timeout: float = 2.0):
        """Stop the calibration pool. Safe to call multiple times."""
        try:
            self._cal_pool.shutdown(wait=True, cancel_futures=True)
        except Exception:
            pass

    # ------------- theo / vol lookups -------------

    def get_theo(self, strike: float, put_call: str = "C", expiry: str = None) -> float:
        exp = expiry if expiry is not None else self.front_month
        if exp is None:
            return 0.0
        surf = self._surfaces.get(exp)
        if surf is None or surf.last_calibration is None:
            return 0.0
        return surf.get_theo(strike, put_call)

    def get_vol(self, strike: float, expiry: str = None, put_call: str = "C") -> float:
        exp = expiry if expiry is not None else self.front_month
        if exp is None:
            return 0.0
        surf = self._surfaces.get(exp)
        if surf is None:
            return 0.0
        return surf.get_vol(strike, put_call)

    def is_strike_calibrated(self, strike: float, expiry: str) -> Tuple[bool, str]:
        """Check whether SABR/SVI is calibrated well enough to quote this
        strike at this expiry. Per hg_spec_v1.3.md §3.3 (fourth/fifth
        bullets): skip strikes where fit quality (RMSE) exceeds threshold
        or the fit hasn't run.

        Returns ``(ok, reason)``. When ``ok`` is False, ``reason`` is one of
        the spec §17.3 skip_reason codes:
          ``"strike_not_calibrated"`` — no surface or no calibration for
              the expiry, or no params on the fit-domain side
          ``"sabr_rmse_too_high"`` — fit RMSE exceeds config threshold
        """
        surf = self._surfaces.get(expiry)
        if surf is None or surf.last_calibration is None:
            return False, "strike_not_calibrated"
        # Route to the fit-domain side (same logic as get_vol at line 468:
        # C-side fit covers K≥F, P-side covers K<F).
        fit_side = "C" if (surf.forward > 0 and strike >= surf.forward) else "P"
        sp = surf._side_params.get(fit_side, {})
        params = sp.get("svi_params") if surf._vol_model == "svi" else sp.get("params")
        if params is None:
            return False, "strike_not_calibrated"
        max_rmse = float(getattr(self.config.pricing, "sabr_max_rmse", 0.03))
        if getattr(params, "rmse", 0.0) > max_rmse:
            return False, "sabr_rmse_too_high"
        return True, ""

    def is_quote_stale(self, option, threshold_iv_diff: float = None, expiry: str = None) -> bool:
        exp = expiry if expiry is not None else getattr(option, "expiry", None) or self.front_month
        surf = self._surfaces.get(exp) if exp else None
        if surf is None:
            return False
        return surf.is_quote_stale(option, threshold_iv_diff=threshold_iv_diff)

    # ------------- back-compat accessors -------------

    @property
    def last_calibration(self):
        surf = self._front_surface()
        return surf.last_calibration if surf else None

    def get_last_calibration(self, expiry: str):
        surf = self._surfaces.get(expiry)
        return surf.last_calibration if surf else None

    @property
    def params(self):
        surf = self._front_surface()
        return surf.params if surf else None

    @property
    def forward(self) -> float:
        surf = self._front_surface()
        return surf.forward if surf else 0.0

    def get_forward(self, expiry: str = None) -> float:
        """Return the calibration forward for a given expiry."""
        exp = expiry if expiry is not None else self.front_month
        if exp is None:
            return 0.0
        surf = self._surfaces.get(exp)
        return surf.forward if surf else 0.0

    def get_spot_at_fit(self, expiry: str = None) -> float:
        """Return the spot (underlying) price captured at the last fit.
        Used by delta_adjust_theo to bridge theo by spot-drift between fits."""
        exp = expiry if expiry is not None else self.front_month
        if exp is None:
            return 0.0
        surf = self._surfaces.get(exp)
        return surf.spot_at_fit if surf else 0.0
