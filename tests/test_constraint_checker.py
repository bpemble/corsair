"""ConstraintChecker tests — the highest-leverage hot path in the system.

Stage 1+ architecture: combined-budget. Calls and puts share the same
margin/delta/theta caps. Validates:
  - Combined margin ceiling enforcement
  - Combined net_delta ceiling enforcement
  - Combined net_theta floor enforcement
  - Long-premium capital-budget guard
  - Improving-fill exception (a fill that reduces a breach is allowed)
  - Margin check is right-agnostic (calls and puts share one budget)
"""

from types import SimpleNamespace

import pytest

from src.constraint_checker import ConstraintChecker, IBKRMarginChecker
from src.position_manager import PortfolioState, Position
from src.synthetic_span import SyntheticSpan


# ---- Lightweight test doubles -----------------------------------------------

class _StubMargin:
    """Minimal margin checker that returns canned values per call."""
    def __init__(self, current=0.0, post=0.0, cur_long=0.0, post_long=0.0):
        self.current = current
        self.post = post
        self.cur_long = cur_long
        self.post_long = post_long

    def check_fill_margin(self, option_quote, fill_qty):
        return {
            "current_margin": self.current,
            "post_fill_margin": self.post,
            "current_long_premium": self.cur_long,
            "post_long_premium": self.post_long,
        }


@pytest.fixture
def portfolio(cfg):
    return PortfolioState(cfg)


def _opt(strike, put_call, delta, theta):
    """Quick option-quote stub for the checker."""
    return SimpleNamespace(
        strike=strike, put_call=put_call, expiry="20260424",
        delta=delta, gamma=0.0, theta=theta, vega=0.0,
    )


# ---- Combined-budget margin ------------------------------------------------

def test_call_under_combined_ceiling_accepted(cfg, portfolio):
    """A call fill that lands under the $100K combined ceiling is accepted."""
    margin = _StubMargin(current=0, post=50_000)
    chk = ConstraintChecker(margin, portfolio, cfg)
    ok, reason = chk.check_constraints(_opt(2100, "C", 0.5, -1.0), "BUY")
    assert ok, reason


def test_put_under_combined_ceiling_accepted(cfg, portfolio):
    """A put fill uses the SAME ceiling as calls in the combined-budget model."""
    margin = _StubMargin(current=0, post=50_000)
    chk = ConstraintChecker(margin, portfolio, cfg)
    ok, reason = chk.check_constraints(_opt(2050, "P", -0.5, -1.0), "BUY")
    assert ok, reason


def test_margin_breach_rejects(cfg, portfolio):
    """Margin > $100K (combined ceiling) rejects regardless of side."""
    margin = _StubMargin(current=0, post=110_000)  # > $100K
    chk = ConstraintChecker(margin, portfolio, cfg)
    ok, reason = chk.check_constraints(_opt(2100, "C", 0.5, -1.0), "BUY")
    assert not ok
    assert "margin" in reason
    # No "C" or "P" suffix in the combined model
    assert "margin C" not in reason
    assert "margin P" not in reason


def test_improving_margin_accepted(cfg, portfolio):
    """If currently breached, accept fills that reduce the breach."""
    margin = _StubMargin(current=110_000, post=105_000)  # both > $100K, but post < cur
    chk = ConstraintChecker(margin, portfolio, cfg)
    ok, _ = chk.check_constraints(_opt(2100, "C", 0.5, -1.0), "SELL")
    assert ok


# ---- Combined delta enforcement --------------------------------------------

def test_combined_delta_breach_from_calls_rejects(cfg, portfolio):
    """A fill that pushes COMBINED net_delta past +3.0 must be rejected,
    even if the breach comes from the call side alone."""
    portfolio.positions.append(Position(
        product="ETHUSDRR", multiplier=50.0,
        strike=2100, expiry="20260424", put_call="C", quantity=1,
        avg_fill_price=80.0, fill_time=None,
        delta=2.8, gamma=0, theta=0, vega=0,
    ))
    margin = _StubMargin(current=5000, post=6000)
    chk = ConstraintChecker(margin, portfolio, cfg)
    # Adding a +0.5 delta call → 3.3, breaches 3.0
    ok, reason = chk.check_constraints(_opt(2125, "C", 0.5, -1.0), "BUY")
    assert not ok
    assert "delta" in reason


def test_combined_delta_offsetting_puts_allow_call(cfg, portfolio):
    """Negative delta puts can offset positive delta calls in the combined
    bucket, allowing additional call fills the per-side model would reject."""
    # +2.5 calls + (-1.0) puts = +1.5 net, well within ±3.0
    portfolio.positions.append(Position(
        product="ETHUSDRR", multiplier=50.0,
        strike=2100, expiry="20260424", put_call="C", quantity=1,
        avg_fill_price=80.0, fill_time=None,
        delta=2.5, gamma=0, theta=0, vega=0,
    ))
    portfolio.positions.append(Position(
        product="ETHUSDRR", multiplier=50.0,
        strike=2050, expiry="20260424", put_call="P", quantity=1,
        avg_fill_price=80.0, fill_time=None,
        delta=-1.0, gamma=0, theta=0, vega=0,
    ))
    margin = _StubMargin(current=5000, post=6000)
    chk = ConstraintChecker(margin, portfolio, cfg)
    # Adding a +0.5 delta call → net 2.0, within ±3.0
    ok, _ = chk.check_constraints(_opt(2125, "C", 0.5, -1.0), "BUY")
    assert ok


# ---- Combined theta enforcement --------------------------------------------

def test_combined_theta_breach_rejects(cfg, portfolio):
    """Combined theta floor at -200 should reject fills that deepen below it."""
    portfolio.positions.append(Position(
        product="ETHUSDRR", multiplier=50.0,
        strike=2100, expiry="20260424", put_call="C", quantity=1,
        avg_fill_price=80.0, fill_time=None,
        delta=0.5, gamma=0, theta=-180, vega=0,
    ))
    margin = _StubMargin(current=5000, post=6000)
    chk = ConstraintChecker(margin, portfolio, cfg)
    # Buying a long call with -2 theta * 50 mult = -100 → -280, breaches -200
    ok, reason = chk.check_constraints(_opt(2125, "C", 0.4, -2.0), "BUY")
    assert not ok
    assert "theta" in reason


def test_combined_theta_offsetting_short_puts_allow_long_call(cfg, portfolio):
    """Short puts contribute positive theta that can offset long-call theta
    in the combined bucket."""
    # Long call: theta -180 → contribution -180
    # Short put: theta -1.5 * qty -1 = +1.5 per contract, but theta is per-day
    #   already, so adding a short with theta=-3 → +3 contribution.
    # Net: -180 + 30 = -150 (using 10 short puts at theta=-3 each)
    portfolio.positions.append(Position(
        product="ETHUSDRR", multiplier=50.0,
        strike=2100, expiry="20260424", put_call="C", quantity=1,
        avg_fill_price=80.0, fill_time=None,
        delta=0.5, gamma=0, theta=-180, vega=0,
    ))
    portfolio.positions.append(Position(
        product="ETHUSDRR", multiplier=50.0,
        strike=2050, expiry="20260424", put_call="P", quantity=-10,
        avg_fill_price=5.0, fill_time=None,
        delta=-0.1, gamma=0, theta=-3, vega=0,
    ))
    margin = _StubMargin(current=5000, post=6000)
    chk = ConstraintChecker(margin, portfolio, cfg)
    # net_theta = -180 + (-3 * -10) = -180 + 30 = -150 (within -200 floor)
    # Adding a small additional theta cost
    ok, _ = chk.check_constraints(_opt(2125, "C", 0.4, -0.5), "BUY")
    assert ok


# ---- Long-premium capital budget -------------------------------------------

def test_long_premium_breach_rejects(cfg, portfolio):
    """Post-fill long premium > capital ($200K Stage 1) should reject."""
    margin = _StubMargin(current=0, post=0,
                         cur_long=199_000, post_long=201_000)  # > $200K capital
    chk = ConstraintChecker(margin, portfolio, cfg)
    ok, reason = chk.check_constraints(_opt(2100, "C", 0.5, -1.0), "BUY")
    assert not ok
    assert "long_premium" in reason


def test_long_premium_under_capital_accepted(cfg, portfolio):
    margin = _StubMargin(current=0, post=0,
                         cur_long=100_000, post_long=110_000)
    chk = ConstraintChecker(margin, portfolio, cfg)
    ok, _ = chk.check_constraints(_opt(2100, "C", 0.5, -1.0), "BUY")
    assert ok
