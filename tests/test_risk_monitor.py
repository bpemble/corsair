"""RiskMonitor kill switch tests.

Validates that the four kill conditions fire correctly and that they're
sticky (can't be cleared by anything but a deliberate disconnect-recovery).
"""

from datetime import datetime
from types import SimpleNamespace

import pytest

from src.position_manager import PortfolioState, Position
from src.risk_monitor import RiskMonitor


class _StubMargin:
    def __init__(self, current=0.0):
        self.current = current

    def get_current_margin(self, right=None):
        return self.current

    def update_cached_margin(self):
        pass


class _StubQuoteManager:
    def __init__(self):
        self.cancelled = 0

    def cancel_all_quotes(self):
        self.cancelled += 1


class _StubCSVLogger:
    def log_risk_snapshot(self, **kwargs):
        pass


def _state(F=2100.0):
    """Minimal market state — refresh_greeks tolerates a missing option lookup."""
    return SimpleNamespace(
        underlying_price=F,
        get_option=lambda strike, expiry, right: None,
    )


def _add(p, qty, delta=0, theta=0, vega=0):
    p.positions.append(Position(
        product="ETHUSDRR", multiplier=50.0,
        strike=2100, expiry="20260424", put_call="C", quantity=qty,
        avg_fill_price=0.0, fill_time=datetime.now(),
        delta=delta, gamma=0, theta=theta, vega=vega,
    ))


def test_vega_kill_fires_when_above_threshold(cfg):
    portfolio = PortfolioState(cfg)
    # vega 1500 per long contract * 1 contract = 1500 > 1000 threshold
    _add(portfolio, qty=1, vega=1500)
    quotes = _StubQuoteManager()
    risk = RiskMonitor(portfolio, _StubMargin(current=10_000), quotes,
                       _StubCSVLogger(), cfg)
    risk.check(_state())
    assert risk.killed
    assert "VEGA KILL" in risk.kill_reason
    assert quotes.cancelled >= 1


def test_vega_kill_does_not_fire_when_below(cfg):
    portfolio = PortfolioState(cfg)
    _add(portfolio, qty=1, vega=500)  # 500 < 1000 threshold
    risk = RiskMonitor(portfolio, _StubMargin(current=10_000),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    risk.check(_state())
    assert not risk.killed


def test_vega_kill_uses_combined_net_vega(cfg):
    """Vega kill must check the combined net_vega across calls + puts."""
    portfolio = PortfolioState(cfg)
    _add(portfolio, qty=1, vega=600)  # +600
    portfolio.positions.append(Position(
        product="ETHUSDRR", multiplier=50.0,
        strike=2050, expiry="20260424", put_call="P", quantity=1,
        avg_fill_price=5.0, fill_time=datetime.now(),
        delta=-0.1, gamma=0, theta=0, vega=600,
    ))  # +600 long put vega
    # net_vega = +1200 > 1000 threshold
    quotes = _StubQuoteManager()
    risk = RiskMonitor(portfolio, _StubMargin(current=10_000), quotes,
                       _StubCSVLogger(), cfg)
    risk.check(_state())
    assert risk.killed
    assert "VEGA KILL" in risk.kill_reason


def test_delta_kill_fires_at_threshold(cfg):
    portfolio = PortfolioState(cfg)
    _add(portfolio, qty=1, delta=5.5)  # > 5.0 threshold
    quotes = _StubQuoteManager()
    risk = RiskMonitor(portfolio, _StubMargin(current=10_000), quotes,
                       _StubCSVLogger(), cfg)
    risk.check(_state())
    assert risk.killed
    assert "DELTA KILL" in risk.kill_reason


def test_kill_is_sticky(cfg):
    """Once killed by risk, subsequent check() calls should not unkill."""
    portfolio = PortfolioState(cfg)
    _add(portfolio, qty=1, vega=2000)
    risk = RiskMonitor(portfolio, _StubMargin(current=10_000),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    risk.check(_state())
    assert risk.killed
    # Even if we now look healthy, the kill stays
    portfolio.positions.clear()
    risk.check(_state())
    assert risk.killed
