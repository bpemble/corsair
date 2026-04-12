"""Synthetic SPAN margin calculator for CME futures options.

Approximates CME SPAN margin by re-pricing every position under 16 risk
scenarios (futures price × vol shifts) using Black-76. No CME .pa2 files
required. Stateless: callers pass current positions and the calculator
returns the SPAN scan risk in dollars. Used by ConstraintChecker to gate
quotes.

Calibration (2026-04-06, F=$2,100, IV=66.6%):
  Constants fit against IBKR short-side observations:
    up_scan_pct=0.56, down_scan_pct=0.49, vol_scan_pct=0.25.

  Single-naked-short fit (random ±$3-5K residuals, ~6% of notional):
    SHORT 2100C: model $63,467  ibkr $60,372  (+$3,095)
    SHORT 2200C: model $60,430  ibkr $59,761  (+$669)
    SHORT 2400C: model $55,215  ibkr $58,910  (-$3,695)
    SHORT 2100P: model $51,923  ibkr $47,383  (+$4,540)
    SHORT 1850P: model $40,145  ibkr $45,011  (-$4,866)

  Long-only positions: $0 (matches IBKR exactly via Net Option Value
  credit).

KNOWN ISSUE — verticals are systematically overstated by ~25-30%:
  $100 vertical (-2300C/+2400C): model $4,937  ibkr $4,014  (+23%)
  $200 vertical (-2200C/+2400C): model $9,910  ibkr $7,452  (+33%)

  Real CME caps a defined-risk vertical's margin at structural max loss
  (width * mult - net_credit). Black-76 scenario reval can't replicate
  this because vol shifts amplify the OTM long leg differently than the
  deeper short leg, leaving residual scan risk above the structural cap.

  Impact: bot will hold ~70-75% of the verticals IBKR would actually
  permit at the $50K cap. Conservative (safe direction), not breach
  risk. Capital under-utilization of ~25-30% in the realistic operating
  regime. Expect ~8-9 fills/day instead of the spec's 12.

  Fix (deferred to v2.1): pattern-match defined-risk spreads in the
  position book and use structural max loss as the margin contribution
  for those legs. Requires recognizing verticals/butterflies/condors,
  ~50+ LOC. Not on critical path for v2.0 launch.

The 5-min MARGIN RECON log line in IBKRMarginChecker tracks live
synthetic-vs-IBKR drift; use it to detect if the constants need a
re-fit (e.g. after a large IV regime change).
"""

from typing import Iterable, Tuple

import numpy as np

from .pricing import PricingEngine

# 16-scenario SPAN scan grid: (price_fraction_of_scan, vol_fraction, cover)
# Index 0..13 are the standard scan (price ∈ {0, ±1/3, ±2/3, ±3/3} × vol ±1)
# Index 14..15 are extreme moves (±3 scan range, vol flat, 33% cover)
_PRICE_FRACS = [0, 0, +1/3, +1/3, -1/3, -1/3,
                +2/3, +2/3, -2/3, -2/3,
                +1.0, +1.0, -1.0, -1.0]
_VOL_SIGNS = [+1, -1, +1, -1, +1, -1,
              +1, -1, +1, -1,
              +1, -1, +1, -1]


class SyntheticSpan:
    """Stateless SPAN-equivalent margin calculator."""

    def __init__(self, config):
        sp = getattr(config, "synthetic_span", None)
        self.up_scan_pct = float(getattr(sp, "up_scan_pct", 0.599))
        self.down_scan_pct = float(getattr(sp, "down_scan_pct", 0.558))
        self.vol_scan_pct = float(getattr(sp, "vol_scan_pct", 0.25))
        self.extreme_mult = float(getattr(sp, "extreme_mult", 3.0))
        self.extreme_cover = float(getattr(sp, "extreme_cover", 0.33))
        self.short_option_minimum = float(getattr(sp, "short_option_minimum", 500.0))
        self._multiplier = float(config.product.multiplier)

    def position_risk_array(self, F: float, K: float, T: float,
                            iv: float, right: str) -> np.ndarray:
        """Return 16-element risk array (loss $ per LONG contract per scenario).

        Positive = loss for a long position. For a short position, negate.
        Uses vectorized Black-76 to price all 16 scenarios in one call.
        """
        if F <= 0 or T <= 0 or iv <= 0:
            return np.zeros(16)

        base = PricingEngine.black76_price(F, K, T, iv, right=right)
        up_scan = F * self.up_scan_pct
        down_scan = F * self.down_scan_pct
        vol_scan = iv * self.vol_scan_pct
        mult = self._multiplier

        # Build 16 (F, iv) scenario arrays
        F_scen = np.empty(16)
        iv_scen = np.empty(16)
        cover = np.ones(16)
        for i in range(14):
            frac = _PRICE_FRACS[i]
            shift = (frac * up_scan) if frac >= 0 else (frac * down_scan)
            F_scen[i] = max(F + shift, 1.0)
            iv_scen[i] = max(iv + _VOL_SIGNS[i] * vol_scan, 0.01)
        # Extreme up
        F_scen[14] = max(F + self.extreme_mult * up_scan, 1.0)
        iv_scen[14] = iv
        cover[14] = self.extreme_cover
        # Extreme down
        F_scen[15] = max(F - self.extreme_mult * down_scan, 1.0)
        iv_scen[15] = iv
        cover[15] = self.extreme_cover

        # One vectorized Black-76 call for all 16 scenarios
        prices = PricingEngine.black76_price_vec(
            F_scen, K, T, iv_scen, right=right,
        )
        return -(prices - base) * mult * cover

    def portfolio_margin(self, F: float, positions: Iterable[Tuple]) -> dict:
        """Compute portfolio SPAN margin.

        positions: iterable of (strike, right, T_years, iv, quantity).
                   Quantity is signed (+long / -short).

        Uses CME risk-based methodology:
            margin = max(scan_risk - net_option_value, short_minimum)
        Net option value = sum(base_price * qty) * multiplier (positive for
        net long), which credits the prepaid premium of long inventory.

        Returns {scan_risk, net_option_value, short_minimum, total_margin,
                 worst_scenario}.
        """
        portfolio = np.zeros(16)
        nov = 0.0
        long_premium = 0.0
        short_count = 0
        mult = self._multiplier
        for (K, right, T, iv, qty) in positions:
            if qty == 0 or T <= 0 or iv <= 0:
                continue
            ra = self.position_risk_array(F, K, T, iv, right)
            portfolio += ra * qty
            base = PricingEngine.black76_price(F, K, T, iv, right=right)
            nov += base * qty * mult
            if qty > 0:
                long_premium += base * qty * mult
            else:
                short_count += abs(qty)

        scan_risk = float(max(portfolio.max(), 0.0)) if portfolio.size else 0.0
        worst = int(portfolio.argmax()) + 1 if scan_risk > 0 else 0
        short_min = short_count * self.short_option_minimum
        risk_margin = max(scan_risk - nov, 0.0)
        # Conservative max-of-three. Empirically the floor matches IBKR with
        # a +50% systematic over-estimate (ratio ~1.5). This is acceptable as
        # a launch posture: we err in the safe direction (refuse fills IBKR
        # would allow). Compensate by setting margin_ceiling at 2/3 of the
        # nominal target. Two alternatives that I tested today and which
        # perform worse:
        #   - Removing long_premium entirely (ratio 0.05, dangerous)
        #   - Additive (risk + long_premium): ratio 1.58, worse than max
        # The right long-term fix is to model SPAN inter-month / inter-strike
        # offsets properly, but that's a v2.1 effort.
        total = max(risk_margin, short_min, long_premium)
        return {
            "scan_risk": scan_risk,
            "net_option_value": nov,
            "long_premium": long_premium,
            "short_minimum": short_min,
            "total_margin": total,
            "worst_scenario": worst,
        }
