"""Black-76 pricing engine for futures options.

Reused from Corsair v1. Provides theoretical option pricing and implied
volatility solving via Brent's method.

Black-76 formulas:
    Call = e^(-rT) * [F * N(d1) - K * N(d2)]
    Put  = e^(-rT) * [K * N(-d2) - F * N(-d1)]

    d1 = [ln(F/K) + (sigma^2 / 2) * T] / (sigma * sqrt(T))
    d2 = d1 - sigma * sqrt(T)
"""

import numpy as np
from scipy.optimize import brentq
from scipy.stats import norm


class PricingEngine:
    """Black-76 pricing engine for futures options."""

    @staticmethod
    def black76_price(
        F: float, K: float, T: float, sigma: float,
        r: float = 0.0, right: str = "C",
    ) -> float:
        """Return Black-76 theoretical price for a futures option."""
        right = right.upper()
        if T <= 0 or sigma <= 0 or F <= 0 or K <= 0:
            if right == "C":
                return max(F - K, 0.0)
            return max(K - F, 0.0)

        sqrt_T = T ** 0.5
        d1 = (np.log(F / K) + 0.5 * sigma * sigma * T) / (sigma * sqrt_T)
        d2 = d1 - sigma * sqrt_T
        df = np.exp(-r * T)

        if right == "C":
            return float(df * (F * norm.cdf(d1) - K * norm.cdf(d2)))
        return float(df * (K * norm.cdf(-d2) - F * norm.cdf(-d1)))

    @staticmethod
    def black76_price_vec(
        F, K, T, sigma, r: float = 0.0, right: str = "C",
    ) -> np.ndarray:
        """Vectorized Black-76. F, K, T, sigma may each be scalars or arrays
        broadcastable to a common shape. Returns an ndarray of prices.

        Used by synthetic SPAN to price 16 scenarios in one numpy call.
        """
        right = right.upper()
        F_a = np.asarray(F, dtype=np.float64)
        K_a = np.asarray(K, dtype=np.float64)
        T_a = np.asarray(T, dtype=np.float64)
        s_a = np.asarray(sigma, dtype=np.float64)

        valid = (T_a > 0) & (s_a > 0) & (F_a > 0) & (K_a > 0)
        # Avoid log of non-positive in masked-out lanes
        F_safe = np.where(valid, F_a, 1.0)
        K_safe = np.where(valid, K_a, 1.0)
        T_safe = np.where(valid, T_a, 1.0)
        s_safe = np.where(valid, s_a, 1.0)

        sqrt_T = np.sqrt(T_safe)
        d1 = (np.log(F_safe / K_safe) + 0.5 * s_safe * s_safe * T_safe) / (s_safe * sqrt_T)
        d2 = d1 - s_safe * sqrt_T
        df = np.exp(-r * T_safe)

        if right == "C":
            valid_px = df * (F_safe * norm.cdf(d1) - K_safe * norm.cdf(d2))
            intrinsic = np.maximum(F_a - K_a, 0.0)
        else:
            valid_px = df * (K_safe * norm.cdf(-d2) - F_safe * norm.cdf(-d1))
            intrinsic = np.maximum(K_a - F_a, 0.0)

        return np.where(valid, valid_px, intrinsic)

    @staticmethod
    def implied_vol(
        market_price: float, F: float, K: float, T: float,
        r: float = 0.0, right: str = "C",
    ) -> float | None:
        """Solve for implied volatility via Brent's method."""
        right = right.upper()
        if T <= 0 or market_price <= 0:
            return None

        intrinsic = max(F - K, 0.0) if right == "C" else max(K - F, 0.0)
        df = np.exp(-r * T)
        if market_price < df * intrinsic - 1e-10:
            return None

        def objective(sigma: float) -> float:
            return PricingEngine.black76_price(F, K, T, sigma, r, right) - market_price

        try:
            iv = brentq(objective, 0.01, 5.0, xtol=1e-6)
            return float(iv)
        except (ValueError, RuntimeError):
            return None
