"""SABR stochastic volatility model -- Hagan (2002) approximation.

Reused from Corsair v1. Provides:
- sabr_implied_vol: closed-form Black IV for (F, K, T) under SABR dynamics
- calibrate_sabr: fits (alpha, rho, nu) to market IVs with beta fixed
- SABRSurface: higher-level wrapper for Corsair v2's edge filter and stale detection
"""

import logging
import math
import threading
import time as _time
from collections import deque
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass
from datetime import datetime
from typing import List, Optional, Tuple

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
        self._theo_cache_ttl_sec: float = 0.1

    def set_expiry(self, expiry: str):
        """Set the front month expiry for TTE calculations."""
        self._front_month_expiry = expiry

    def calibrate(self, forward: float, options: dict):
        """Calibrate SABR parameters to current option chain.

        Args:
            forward: Current underlying futures price (used as the fallback
                forward and as the outlier reference for parity derivation).
            options: Dict of {(strike, expiry, right): OptionQuote} with
                .iv, .bid, .ask, .strike, .put_call attributes.
        """
        # Drop strikes with wide BBOs from calibration — synthetic mids on
        # illiquid (typically deep ITM/OTM) strikes inject garbage IVs that
        # bias the entire fit. ETH options at this point have widths $5-8 on
        # liquid strikes vs $30+ on illiquid ones, so a $10 cutoff cleanly
        # separates them.
        max_cal_width = float(getattr(self.config.pricing, "max_calibration_bbo_width", 10.0))

        # Blend futures mid with parity-implied forward. Pure futures mid
        # underprices near-ATM puts (~-$2.50); pure parity overprices OTM
        # puts (~+$3). The 50/50 blend centers the residuals across both
        # zones. The parity premium in crypto reflects call demand skew,
        # not a real forward offset — splitting the difference acknowledges
        # both signals without committing fully to either.
        implied = implied_forward_from_parity(
            options, ref_forward=forward, max_spread=max_cal_width,
        )
        if implied is not None:
            if abs(implied - forward) > 1.0:
                logger.info(
                    "SABR forward: futures-mid=%.2f, parity-implied=%.2f (Δ=%+.2f)",
                    forward, implied, implied - forward,
                )
            forward = (forward + implied) / 2.0
        self.forward = forward

        tte = self._get_tte()
        if tte <= 0:
            return

        # Per-side calibration: fit calls and puts independently so each
        # side gets its own alpha/rho/nu. A single 3-parameter fit across
        # both sides forces SABR to compromise between two different skew
        # shapes, producing systematic bias on OTM puts.
        min_strikes = int(getattr(self.config.pricing, "sabr_min_strikes", 8))
        max_rmse = self.config.pricing.sabr_max_rmse
        use_svi = self._vol_model == "svi"
        any_updated = False

        for side in ("C", "P"):
            cal_data = []  # (strike, iv, spread)
            for key, option in options.items():
                if option.put_call != side:
                    continue
                bid = getattr(option, "bid", 0) or 0
                ask = getattr(option, "ask", 0) or 0
                if bid > 0 and ask > 0 and (ask - bid) <= max_cal_width:
                    mid = (bid + ask) / 2.0
                    iv = PricingEngine.implied_vol(
                        mid, forward, option.strike, tte, right=side,
                    )
                    if iv is not None and iv > 0:
                        cal_data.append((option.strike, iv, ask - bid))

            min_pts = 5 if use_svi else 3
            if len(cal_data) < min_pts:
                continue

            strikes = [s for s, _, _ in cal_data]
            ivs = [v for _, v, _ in cal_data]
            spreads = [w for _, _, w in cal_data]

            # Spread-inverse weighting
            min_spread = min(spreads)
            weights = [min_spread / s for s in spreads]

            sp = self._side_params[side]

            if use_svi:
                result = calibrate_svi(
                    F=forward, T=tte, strikes=strikes, market_ivs=ivs,
                    max_rmse=max_rmse, weights=weights,
                )
                if result is None:
                    continue
                quality_ok, quality_reason = _svi_quality_ok(result, min_strikes // 2)
                if not quality_ok:
                    logger.warning(
                        "SVI %s fit rejected (%s): a=%.4f b=%.3f rho=%.3f "
                        "m=%.3f sig=%.3f RMSE=%.4f n=%d",
                        side, quality_reason, result.a, result.b, result.rho,
                        result.m, result.sigma, result.rmse, result.n_points,
                    )
                    continue
                prior = sp["svi_params"]
                if (prior is not None
                        and result.rmse > max(prior.rmse * 1.5, 0.01)):
                    logger.warning(
                        "SVI %s fit rejected (rmse_regression %.4f > 1.5x prior %.4f)",
                        side, result.rmse, prior.rmse,
                    )
                    continue
                sp["svi_params"] = result
                any_updated = True
                logger.info(
                    "SVI %s calibrated: a=%.4f b=%.4f rho=%.3f m=%.4f sig=%.4f RMSE=%.4f (n=%d)",
                    side, result.a, result.b, result.rho, result.m, result.sigma,
                    result.rmse, result.n_points,
                )
                if self.csv_logger is not None:
                    try:
                        self.csv_logger.log_calibration(
                            expiry=self._front_month_expiry or "",
                            side=side, model="svi",
                            forward=forward, tte_years=tte,
                            n_points=result.n_points, rmse=result.rmse,
                            a=result.a, b=result.b, rho_svi=result.rho,
                            m=result.m, sigma=result.sigma,
                        )
                    except Exception as e:
                        logger.debug("calibration telemetry log failed: %s", e)
            else:
                result = calibrate_sabr(
                    F=forward, T=tte, strikes=strikes, market_ivs=ivs,
                    beta=self.beta, max_rmse=max_rmse, weights=weights,
                )
                if result is None:
                    continue
                quality_ok, quality_reason = _sabr_quality_ok(result, min_strikes // 2)
                if not quality_ok:
                    worst_strike, worst_resid = _worst_strike_residual(
                        result, forward, strikes, ivs, tte,
                    )
                    worst_str = (
                        f" worst_strike={worst_strike:.0f} resid={worst_resid:.4f}"
                        if worst_strike is not None else ""
                    )
                    logger.warning(
                        "SABR %s fit rejected (%s): alpha=%.4f rho=%.3f nu=%.3f "
                        "RMSE=%.4f n=%d%s",
                        side, quality_reason, result.alpha, result.rho, result.nu,
                        result.rmse, result.n_points, worst_str,
                    )
                    continue
                prior = sp["params"]
                if (prior is not None
                        and result.rmse > max(prior.rmse * 1.5, 0.01)):
                    logger.warning(
                        "SABR %s fit rejected (rmse_regression %.4f > 1.5x prior %.4f)",
                        side, result.rmse, prior.rmse,
                    )
                    continue
                sp["alpha"] = result.alpha
                sp["rho"] = result.rho
                sp["nu"] = result.nu
                sp["params"] = result
                any_updated = True
                logger.info(
                    "SABR %s calibrated: alpha=%.4f rho=%.3f nu=%.3f RMSE=%.4f (n=%d)",
                    side, result.alpha, result.rho, result.nu, result.rmse, result.n_points,
                )
                if self.csv_logger is not None:
                    try:
                        self.csv_logger.log_calibration(
                            expiry=self._front_month_expiry or "",
                            side=side, model="sabr",
                            forward=forward, tte_years=tte,
                            n_points=result.n_points, rmse=result.rmse,
                            alpha=result.alpha, beta=self.beta,
                            rho_sabr=result.rho, nu=result.nu,
                        )
                    except Exception as e:
                        logger.debug("calibration telemetry log failed: %s", e)

            # Drift detection (shared across both models)
            rmse_val = result.rmse
            sp["rmse_history"].append(rmse_val)
            if len(sp["rmse_history"]) >= 5:
                median_rmse = float(np.median(sp["rmse_history"]))
                if (median_rmse > 0
                        and rmse_val > max(median_rmse * 1.5, 0.005)):
                    logger.warning(
                        "%s %s RMSE drift: current=%.4f vs median=%.4f",
                        "SVI" if use_svi else "SABR",
                        side, rmse_val, median_rmse,
                    )

        if any_updated:
            self.last_calibration = datetime.now()
            self._theo_cache.clear()
            # Back-compat: .params exposes the put-side result (primary quoting side)
            pp = self._side_params["P"]
            self.params = pp.get("svi_params") or pp.get("params")

    def get_vol(self, strike: float, put_call: str = "C") -> float:
        """Return implied vol for a given strike using per-side params."""
        tte = self._get_tte()
        if tte <= 0:
            return 0.0
        side = put_call.upper()
        sp = self._side_params.get(side, self._side_params["C"])
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
        # Async calibration: a single-worker pool fits SABR off the event
        # loop. scipy.optimize.least_squares releases the GIL during its
        # numerical inner loop, so this genuinely parallelizes against the
        # main Python thread. We keep the pool at 1 worker: calibration
        # is already fast enough (~100ms for 3 expiries); adding more
        # workers doesn't help and just complicates concurrency.
        self._cal_pool = ThreadPoolExecutor(
            max_workers=1, thread_name_prefix="sabr-cal",
        )
        self._cal_future: Optional[Future] = None
        # Lock serializes parameter updates so readers of get_theo/get_vol
        # never observe a half-updated surface. Fit computation runs
        # outside the lock; only the final assignment is guarded.
        self._params_lock = threading.Lock()

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
        for exp in targets:
            surf = self._surfaces.get(exp)
            if surf is None:
                continue
            # Filter options dict by this expiry
            filtered = {k: v for k, v in options.items() if k[1] == exp}
            if not filtered:
                continue
            with self._params_lock:
                surf.calibrate(forward, filtered)

    def calibrate_async(self, forward: float, options: dict) -> bool:
        """Fire SABR calibration on the background pool and return immediately.

        Returns True if a new calibration was submitted, False if one is
        already in flight (we don't queue up a backlog — the next timer
        tick will pick up fresh state anyway).

        Note: ``options`` is snapshotted by reference. Callers should not
        mutate the dict while the calibration is running — but in practice
        the market_data state.options dict is only updated in place (new
        bid/ask fields on existing OptionQuote objects), not
        rebuilt, so concurrent reads are safe even if slightly stale.
        """
        # Skip if previous calibration is still running. Using done() is
        # cheap and avoids the cost of cancelling queued work.
        if self._cal_future is not None and not self._cal_future.done():
            return False

        # Snapshot the expiry list at submission time. If set_expiries is
        # called while the worker runs, the worker still operates on what
        # was valid at submission — no mid-flight pivots.
        expiries_snapshot = list(self._expiries)

        def _run():
            for exp in expiries_snapshot:
                surf = self._surfaces.get(exp)
                if surf is None:
                    continue
                filtered = {k: v for k, v in options.items() if k[1] == exp}
                if not filtered:
                    continue
                try:
                    # The fit itself (Black-76 scenarios + least_squares)
                    # is the expensive part — runs outside the lock.
                    # SABRSurface.calibrate internally mutates params at
                    # the end; we serialize that block with the lock so
                    # readers don't observe a half-updated surface.
                    with self._params_lock:
                        surf.calibrate(forward, filtered)
                except Exception as e:
                    logger.warning(
                        "Async SABR calibrate failed for %s: %s", exp, e,
                    )

        self._cal_future = self._cal_pool.submit(_run)
        return True

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
