"""Term-structure-interpolation (TSI) fallback for back-month vol surface.

Motivation
----------
Corsair's per-side SVI calibration fits 5 parameters at each (expiry, side).
At front-month with 20+ strikes per side the fit is honest. At front+1
and front+2 the book has only 5-10 strikes per side, so SVI is
underdetermined — in-sample RMSE looks tight but the surface extrapolates
badly (see ``docs/findings/backmonth_sparsity_2026-04-18.md`` for the
numbers and ``svi_cp_parity_2026-04-17.md`` for the dollar impact).

The fix proposed to corsair (change 3 in ``docs/briefs/corsair_surface_fixes_2026-04-18.md``)
is to raise the SVI min-strike gate to 10. That rejects ~100% of current
back-month fits. This module is the fallback that fills the gap: borrow
front-month SVI **skew shape** (b, ρ, m, σ), anchor **level** (a) to
whatever back-month ATM points are actually observable.

Method
------
SVI total-variance parametrization (Gatheral's raw form):

    w(k) = a + b * (ρ*(k-m) + sqrt((k-m)² + σ²))

where k = log(K / forward). Implied vol is sqrt(w / T).

When corsair's back-month SVI calibration fails the gate (too few strikes
to pin a 5-param fit), we:

1. Keep (b, ρ, m, σ) from the valid front-month SVI fit. Skew structure
   (wing steepness, horizontal center, ATM curvature) is assumed to be
   roughly stable across near-term expiries — what changes with tte is
   primarily the level.
2. Solve for `a` (the level) against the back-month observations the
   calibrator *could* see — typically 1-3 near-ATM strikes. With one
   observation it's a direct solve; with multiple we take the mean
   residual to be robust to single-strike noise.

Sell-side desks with thin back-month data typically do something like
this. It trades skew precision (we assume front-month's skew, not the
back-month's own) for surface consistency (no more 5 params on 5 points).

The returned surface is internally consistent across C and P side
because both sides share the same shape and same level anchor — that
closes the parity gap directly.

Assumes the caller has already resolved which front-month SVI to use
(post-Change-2 corsair code: moneyness-based selection). If corsair
still has per-side front-month fits and the caller picks one, TSI will
inherit that side's shape — so Change 2 needs to land before Change 3
uses this fallback.
"""

from __future__ import annotations

import math
from dataclasses import dataclass, field
from typing import Sequence


@dataclass
class SVIParams:
    """SVI raw parametrization.

    w(k) = a + b * (rho*(k-m) + sqrt((k-m)^2 + sigma^2))
    where k = log(K / F), w = total variance over tte, iv = sqrt(w/tte).

    Mirrors corsair's SVIParams shape (see corsair/src/sabr.py:271) so this
    module's output is drop-in compatible with the corsair fitter's output
    when used as a fallback. Field names chosen to match corsair's.
    """
    a: float
    b: float
    rho: float
    m: float
    sigma: float
    rmse: float = 0.0
    n_points: int = 0
    # TSI-specific provenance — corsair's fitter leaves these at defaults
    fit_method: str = "native"  # "native" | "tsi_fallback"
    tsi_anchor_count: int = 0
    tsi_donor_expiry: str | None = None


def svi_total_variance(k: float, a: float, b: float, rho: float,
                        m: float, sigma: float) -> float:
    """Gatheral raw-SVI total variance at log-moneyness k. Matches corsair's
    ``svi_total_variance`` at corsair/src/sabr.py:282."""
    return a + b * (rho * (k - m) + math.sqrt((k - m) ** 2 + sigma ** 2))


def svi_implied_vol(forward: float, strike: float, tte: float,
                     a: float, b: float, rho: float,
                     m: float, sigma: float) -> float:
    """SVI-implied Black vol at a given strike and tte. Matches corsair's
    ``svi_implied_vol`` at corsair/src/sabr.py:288."""
    if tte <= 0 or forward <= 0 or strike <= 0:
        return 0.0
    k = math.log(strike / forward)
    w = svi_total_variance(k, a, b, rho, m, sigma)
    if w <= 0:
        return 0.0
    return math.sqrt(w / tte)


@dataclass
class AnchorObservation:
    """A single (strike, observed IV) pair used to anchor the back-month level."""
    strike: float
    iv: float
    weight: float = 1.0


def _shape_component(k: float, b: float, rho: float, m: float, sigma: float) -> float:
    """The part of the SVI formula that does NOT depend on `a`."""
    return b * (rho * (k - m) + math.sqrt((k - m) ** 2 + sigma ** 2))


def fit_backmonth_from_frontmonth(
    donor: SVIParams,
    anchors: Sequence[AnchorObservation],
    forward: float,
    tte: float,
    *,
    donor_expiry_tag: str | None = None,
    min_anchors: int = 1,
    variance_floor: float = 1e-6,
) -> SVIParams | None:
    """Build a back-month SVI by carrying donor's shape and anchoring level.

    Parameters
    ----------
    donor
        Front-month (or near-expiry) SVI fit whose skew shape we trust.
        ``donor.b / rho / m / sigma`` are reused.
    anchors
        Observed (strike, iv) pairs at the back-month expiry. Must lie near
        ATM for the fallback to be meaningful; the caller's job to filter.
        Each anchor contributes one residual to the level fit.
    forward
        Back-month forward (not the donor's).
    tte
        Back-month time-to-expiry in years (not the donor's).
    donor_expiry_tag
        Optional human-readable tag for the donor expiry — carried into
        the returned SVIParams.tsi_donor_expiry for observability.
    min_anchors
        Minimum number of anchors required. Returns None if fewer.
    variance_floor
        If the solved `a` plus shape component at any anchor strike would
        produce non-positive total variance, the caller should treat that
        as a failure. The function still returns the fit so the caller can
        see it; the no-arb check is a caller responsibility.

    Returns
    -------
    SVIParams | None
        ``None`` if fewer than min_anchors observations or if tte/forward
        are degenerate. Otherwise an SVIParams with shape borrowed from
        donor and `a` solved to match the anchors.

    Notes
    -----
    The level solve is a mean-residual — with one anchor it reduces to a
    direct solve; with multiple it's a weighted average of per-anchor
    ``a`` estimates. This is intentional: no least-squares minimizer is
    needed because `a` enters linearly. Noise on a single anchor is
    averaged out when multiple anchors exist.
    """
    if tte <= 0 or forward <= 0:
        return None
    clean = [x for x in anchors if x.strike > 0 and math.isfinite(x.iv) and x.iv > 0]
    if len(clean) < min_anchors:
        return None

    # For each anchor, compute the implied `a`:
    #   w_target = iv^2 * tte
    #   a_i = w_target - shape(k_i)
    a_estimates: list[float] = []
    weights: list[float] = []
    for obs in clean:
        k = math.log(obs.strike / forward)
        w_target = obs.iv * obs.iv * tte
        shape = _shape_component(k, donor.b, donor.rho, donor.m, donor.sigma)
        a_estimates.append(w_target - shape)
        weights.append(max(obs.weight, 0.0))

    total_w = sum(weights)
    if total_w <= 0:
        return None
    a = sum(a_i * w_i for a_i, w_i in zip(a_estimates, weights)) / total_w

    # Residual from the mean solve — not a true RMSE but a useful signal
    # of anchor disagreement. If all anchors agree this is ~0; if they
    # disagree sharply, caller may want to fall back further (e.g., to a
    # flat-vol surface pegged at the mean IV).
    residual = math.sqrt(
        sum(w_i * (a_i - a) ** 2 for a_i, w_i in zip(a_estimates, weights)) / total_w
    )

    out = SVIParams(
        a=a,
        b=donor.b,
        rho=donor.rho,
        m=donor.m,
        sigma=donor.sigma,
        rmse=residual,
        n_points=len(clean),
        fit_method="tsi_fallback",
        tsi_anchor_count=len(clean),
        tsi_donor_expiry=donor_expiry_tag,
    )
    return out


def extract_atm_anchors(
    strike_iv_pairs: Sequence[tuple[float, float]],
    forward: float,
    moneyness_window: tuple[float, float] = (0.95, 1.05),
    max_anchors: int = 5,
) -> list[AnchorObservation]:
    """Pick near-ATM (K, IV) observations to use as TSI anchors.

    Callers with richer info (quote size, last-update timestamp, bid-ask
    tightness) should build anchors directly and set per-anchor weights.
    This helper is the simplest-useful default for test and backtest code.
    """
    lo, hi = moneyness_window
    candidates = []
    for K, iv in strike_iv_pairs:
        if K <= 0 or forward <= 0 or not math.isfinite(iv) or iv <= 0:
            continue
        moneyness = K / forward
        if not (lo <= moneyness <= hi):
            continue
        # Weight by 1 / |log-moneyness distance from ATM| so strikes right
        # at ATM count more than strikes at the edge of the window.
        dist = abs(math.log(moneyness))
        weight = 1.0 / (dist + 0.01)
        candidates.append((dist, AnchorObservation(K, iv, weight)))
    candidates.sort(key=lambda t: t[0])
    return [anchor for _, anchor in candidates[:max_anchors]]


def no_arb_check(svi: SVIParams, k_grid: Sequence[float] | None = None) -> tuple[bool, str]:
    """Return (ok, reason). Ensures w(k) >= 0 across a plausible moneyness grid.

    A proper no-arb check would also verify butterfly and calendar-spread
    conditions. This one catches only the most obvious failure — negative
    total variance — which is the common way a bad `a` solve breaks the
    surface.
    """
    if k_grid is None:
        # Default: ±30% log-moneyness, 13 points
        k_grid = [x * 0.05 for x in range(-6, 7)]
    for k in k_grid:
        w = svi_total_variance(k, svi.a, svi.b, svi.rho, svi.m, svi.sigma)
        if w < 0:
            return False, f"w(k={k:+.2f})={w:.6f} < 0"
    return True, "ok"
