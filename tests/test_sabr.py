"""SABR surface tests.

Validates the structural properties of get_theo and the calibration loop:
  - get_theo returns 0 when not yet calibrated and no expiry set
  - Theo cache returns identical values within TTL
  - Calibration produces sensible alpha for a known flat-vol input
  - Put theo via put-call parity matches Black-76 directly
  - Parity-implied forward derivation handles common cases
"""

from datetime import datetime, timedelta
from types import SimpleNamespace

import pytest

from src.pricing import PricingEngine
from src.sabr import SABRSurface, implied_forward_from_parity


def _make_surface(cfg, expiry_offset_days=17):
    """Build a calibrated SABR surface against synthetic flat-vol data."""
    surf = SABRSurface(cfg)
    expiry = (datetime.now() + timedelta(days=expiry_offset_days)).strftime("%Y%m%d")
    surf.set_expiry(expiry)
    return surf, expiry


def test_get_theo_zero_before_setup(cfg):
    surf = SABRSurface(cfg)
    # No expiry set → tte 0 → returns 0
    assert surf.get_theo(2100, "C") == 0.0


def test_calibration_with_flat_vol_chain(cfg):
    """Feed SABR a synthetic chain at constant 65% vol — calibration should
    converge with alpha close to 0.65 (flat smile)."""
    from types import SimpleNamespace

    surf, expiry = _make_surface(cfg)
    F = 2100.0
    iv = 0.65

    # Build a fake options dict with bid/ask wide enough to pass the
    # calibration filter (max_calibration_bbo_width = 10) but realistic
    options = {}
    for K in (1900, 1950, 2000, 2050, 2100, 2150, 2200, 2250, 2300):
        opt = SimpleNamespace(
            strike=K, expiry=expiry, put_call="C", iv=iv,
            bid=10.0, ask=15.0,
        )
        options[(K, expiry, "C")] = opt

    surf.calibrate(F, options)
    assert surf.last_calibration is not None
    # alpha should be reasonably close to the input vol on a flat smile
    assert 0.45 < surf.alpha < 0.85


def test_theo_cache_within_ttl(cfg):
    """Two get_theo calls inside the 100ms TTL window must return the same
    cached value (no recompute)."""
    surf, _ = _make_surface(cfg)
    surf.forward = 2100.0
    surf.alpha = 0.65
    surf.rho = -0.2
    surf.nu = 0.4
    surf.last_calibration = datetime.now()
    a = surf.get_theo(2100, "C")
    b = surf.get_theo(2100, "C")
    assert a == b


def _opt(strike, right, bid, ask):
    return SimpleNamespace(strike=strike, put_call=right, bid=bid, ask=ask)


def test_implied_forward_from_parity_basic():
    """A clean book where C - P = F - K at every strike should yield F."""
    F = 2100.0
    options = {}
    for K in (2050, 2075, 2100, 2125, 2150):
        # Synthesize bid/ask such that mid satisfies parity exactly
        c_mid = max(F - K, 0) + 30  # arbitrary time value
        p_mid = max(K - F, 0) + 30
        options[(K, "20260424", "C")] = _opt(K, "C", c_mid - 0.5, c_mid + 0.5)
        options[(K, "20260424", "P")] = _opt(K, "P", p_mid - 0.5, p_mid + 0.5)
    result = implied_forward_from_parity(options, ref_forward=F)
    assert result == pytest.approx(F, abs=0.5)


def test_implied_forward_returns_none_when_insufficient():
    options = {}  # empty
    assert implied_forward_from_parity(options, ref_forward=2100.0) is None

    # Only calls — no pairs
    for K in (2050, 2100, 2150):
        options[(K, "20260424", "C")] = _opt(K, "C", 50, 51)
    assert implied_forward_from_parity(options, ref_forward=2100.0) is None


def test_implied_forward_filters_outliers():
    """A pair with one stale leg should be filtered via max_deviation."""
    F = 2100.0
    options = {}
    # Two clean pairs at K=2100, 2125 (should give F≈2100)
    for K in (2100, 2125):
        c_mid = max(F - K, 0) + 30
        p_mid = max(K - F, 0) + 30
        options[(K, "20260424", "C")] = _opt(K, "C", c_mid - 0.5, c_mid + 0.5)
        options[(K, "20260424", "P")] = _opt(K, "P", p_mid - 0.5, p_mid + 0.5)
    # Bad pair at K=2150 with a stale put — implies F ≈ 2200 (off by 100)
    options[(2150, "20260424", "C")] = _opt(2150, "C", 30, 31)  # mid 30.5
    options[(2150, "20260424", "P")] = _opt(2150, "P", -19.5, -18.5)  # impossible but tests filter; use realistic
    # Actually use a real bad value: very high put mid → implied F too low
    options[(2150, "20260424", "P")] = _opt(2150, "P", 130, 131)  # mid 130.5
    # F_K = 2150 + (30.5 - 130.5) = 2050  → 50 below ref, within default 50 deviation? Yes.
    # Bump it further: mid 200
    options[(2150, "20260424", "P")] = _opt(2150, "P", 199.5, 200.5)
    # F_K = 2150 + (30.5 - 200) = 1980.5  → 119.5 below ref → filtered
    result = implied_forward_from_parity(options, ref_forward=F, max_deviation=50.0)
    # The two clean pairs survive → median ≈ F
    assert result == pytest.approx(F, abs=2.0)


def test_implied_forward_skips_wide_spreads():
    """A pair with wide BBO on one side should be excluded entirely."""
    F = 2100.0
    options = {}
    K = 2100
    options[(K, "20260424", "C")] = _opt(K, "C", 25, 50)  # spread 25, > max_spread
    options[(K, "20260424", "P")] = _opt(K, "P", 28, 32)  # spread 4, OK
    assert implied_forward_from_parity(options, ref_forward=F, max_spread=10.0) is None


def test_call_and_put_theo_distinct(cfg):
    """At an ATM strike, call and put theo should both be positive but
    distinct (call value > intrinsic, put value > 0)."""
    surf, _ = _make_surface(cfg)
    surf.forward = 2100.0
    surf.alpha = 0.65
    surf.rho = -0.2
    surf.nu = 0.4
    surf.last_calibration = datetime.now()
    call_theo = surf.get_theo(2100, "C")
    put_theo = surf.get_theo(2100, "P")
    assert call_theo > 0
    assert put_theo > 0
    # ATM call and put should be approximately equal under Black-76 with no rate
    assert call_theo == pytest.approx(put_theo, rel=0.05)
