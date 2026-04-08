"""Synthetic SPAN sanity tests.

These don't try to match IBKR exactly — the documented residuals are ±$3-5K
on single-naked-shorts. They check the structural properties:

  - Long-only books margin to ~$0 (NOV credit absorbs scan)
  - Short-only books margin in the right ballpark for the calibration set
  - Adding offsetting longs to a short reduces margin
  - Long premium does NOT pad the SPAN (bug we just fixed)
"""

import pytest

from src.synthetic_span import SyntheticSpan


def _pos(strike, right, T, iv, qty):
    return (strike, right, T, iv, qty)


def test_long_only_book_floors_at_long_premium(cfg):
    """A pure long book hits the long_premium floor in the conservative
    max-of-three formula. Risk-based margin alone would be 0 (NOV credits
    the prepaid premium against scan), but the floor lifts it to the cash
    outlay so we don't under-budget for capital used by long premium."""
    span = SyntheticSpan(cfg)
    F = 2100.0
    T = 17 / 365.0
    iv = 0.65
    positions = [
        _pos(2100, "C", T, iv, +1),
        _pos(2125, "C", T, iv, +1),
        _pos(2150, "C", T, iv, +1),
    ]
    result = span.portfolio_margin(F, positions)
    # Floor binds — total should equal long_premium for a pure long book.
    assert result["total_margin"] == pytest.approx(result["long_premium"], rel=0.01)
    assert result["long_premium"] > 0


def test_short_naked_call_in_calibrated_range(cfg):
    """A single short ATM call should margin in the calibrated $55-65K range
    per the docstring (model 2100C ≈ $63K, IBKR ≈ $60K)."""
    span = SyntheticSpan(cfg)
    F = 2100.0
    T = 17 / 365.0
    iv = 0.65
    result = span.portfolio_margin(F, [_pos(2100, "C", T, iv, -1)])
    assert 50_000 < result["total_margin"] < 80_000, \
        f"single short naked call should be ~$60K, got ${result['total_margin']:,.0f}"


def test_offsetting_long_reduces_short_margin(cfg):
    """Adding a long call to a short position should reduce SPAN margin —
    the long offsets the short under up scenarios, lowering scan_risk."""
    span = SyntheticSpan(cfg)
    F = 2100.0
    T = 17 / 365.0
    iv = 0.65
    naked = span.portfolio_margin(F, [_pos(2100, "C", T, iv, -1)])
    spread = span.portfolio_margin(F, [
        _pos(2100, "C", T, iv, -1),
        _pos(2200, "C", T, iv, +1),
    ])
    assert spread["total_margin"] < naked["total_margin"], \
        f"call spread ({spread['total_margin']:.0f}) should margin less than naked short ({naked['total_margin']:.0f})"


def test_zero_quantity_skipped(cfg):
    """Position with qty=0 must contribute nothing."""
    span = SyntheticSpan(cfg)
    a = span.portfolio_margin(2100.0, [_pos(2100, "C", 17/365, 0.65, 0)])
    b = span.portfolio_margin(2100.0, [])
    assert a["total_margin"] == b["total_margin"]


def test_short_minimum_floor_applies(cfg):
    """Even when scan_risk is small, the per-short minimum should apply."""
    span = SyntheticSpan(cfg)
    # Deep OTM short with negligible scan loss should still hit short_min.
    result = span.portfolio_margin(2100.0, [_pos(3000, "C", 17/365, 0.65, -1)])
    assert result["short_minimum"] == cfg.synthetic_span.short_option_minimum
    assert result["total_margin"] >= cfg.synthetic_span.short_option_minimum
