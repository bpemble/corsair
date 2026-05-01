"""RiskMonitor kill switch tests.

Validates that the four kill conditions fire correctly and that they're
sticky (can't be cleared by anything but a deliberate disconnect-recovery).
"""

import logging
import os
from datetime import datetime
from types import SimpleNamespace
from unittest.mock import patch

import pytest

from src.position_manager import PortfolioState, Position
from src.risk_monitor import RiskMonitor, INDUCE_SENTINELS


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

    def log_paper_kill_switch(self, **kwargs):
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
    assert "VEGA HALT" in risk.kill_reason
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
    assert "VEGA HALT" in risk.kill_reason


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


# ── Daily halt threshold resolution + silent-disable guard ─────────────

def _cfg_without_halt_fields(cfg):
    """Strip both daily_pnl_halt_pct and max_daily_loss from cfg."""
    ks = cfg.kill_switch
    # Remove if present; leave other kill-switch fields intact.
    for field in ("daily_pnl_halt_pct", "max_daily_loss"):
        if hasattr(ks, field):
            delattr(ks, field)
    return cfg


def test_daily_halt_threshold_cached_at_init(cfg):
    """RiskMonitor resolves and caches the halt threshold at __init__
    so check() and check_daily_pnl_only() share one value."""
    portfolio = PortfolioState(cfg)
    risk = RiskMonitor(portfolio, _StubMargin(current=1_000),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    # cfg.kill_switch.max_daily_loss == -2500 (see conftest.py)
    assert risk.daily_halt_threshold == pytest.approx(-2500.0)


def test_daily_halt_threshold_none_when_unconfigured_logs_critical(cfg, caplog):
    """If neither daily_pnl_halt_pct nor max_daily_loss is set, the
    threshold is None AND a CRITICAL log fires at init. Silent
    disable of the primary v1.4 defense is unacceptable."""
    _cfg_without_halt_fields(cfg)
    portfolio = PortfolioState(cfg)
    with caplog.at_level(logging.CRITICAL, logger="src.risk_monitor"):
        risk = RiskMonitor(portfolio, _StubMargin(current=0),
                           _StubQuoteManager(), _StubCSVLogger(), cfg)
    assert risk.daily_halt_threshold is None
    # Match on a stable substring so the message can evolve without
    # breaking the test.
    assert any("DAILY P&L HALT DISABLED" in rec.message
               for rec in caplog.records)


def test_check_daily_pnl_only_bails_when_threshold_unconfigured(cfg):
    """check_daily_pnl_only must not fire when threshold is None, even
    if daily_pnl looks catastrophic. Short-circuit via the cached
    attribute rather than re-resolving each call."""
    _cfg_without_halt_fields(cfg)
    portfolio = PortfolioState(cfg)
    portfolio.daily_pnl = -999_999
    risk = RiskMonitor(portfolio, _StubMargin(current=0),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    assert risk.check_daily_pnl_only() is False
    assert not risk.killed


# ── Induced-sentinel remove-failure guard ──────────────────────────────

def test_sentinel_remove_failure_aborts_without_firing(cfg, tmp_path, caplog):
    """If os.remove fails after detecting a sentinel, the kill must NOT
    fire — otherwise we get stuck in a loop where the halt re-triggers
    every cycle (worst-case: daily_halt rollover keeps re-arming it).
    Operator needs the WARN signal, not a pinned kill."""
    portfolio = PortfolioState(cfg)
    risk = RiskMonitor(portfolio, _StubMargin(current=0),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    # Create a real sentinel file so the "exists" check passes.
    fname, _, _ = INDUCE_SENTINELS["daily_pnl"]
    sentinel_path = os.path.join("/tmp", fname)
    with open(sentinel_path, "w") as f:
        f.write("test")
    try:
        with patch("src.risk_monitor.os.remove",
                   side_effect=OSError("simulated permission denied")), \
             caplog.at_level(logging.WARNING, logger="src.risk_monitor"):
            fired = risk._check_induced_sentinels()
        assert fired is False
        assert not risk.killed
        assert any("os.remove failed" in rec.message
                   for rec in caplog.records)
    finally:
        # Clean up — the mocked os.remove prevented the internal cleanup
        if os.path.exists(sentinel_path):
            os.remove(sentinel_path)


# ── Margin / theta kills ────────────────────────────────────────────


def test_margin_kill_fires_above_threshold(cfg):
    """SPAN margin > kill_pct × capital → halt (cancel quotes, no flatten)."""
    portfolio = PortfolioState(cfg)
    quotes = _StubQuoteManager()
    # margin_kill_pct = 0.70, capital = 200_000 → 140_000
    risk = RiskMonitor(portfolio, _StubMargin(current=150_000), quotes,
                       _StubCSVLogger(), cfg)
    risk.check(_state())
    assert risk.killed
    assert "MARGIN KILL" in risk.kill_reason
    assert risk.kill_type == "halt"  # halt, not flatten (operator override)
    assert risk.kill_source == "risk"  # sticky
    assert quotes.cancelled >= 1


def test_margin_kill_does_not_fire_below_threshold(cfg):
    """Margin at ceiling but below kill should warn-only (constraint
    checker handles the gate; this just monitors)."""
    portfolio = PortfolioState(cfg)
    risk = RiskMonitor(portfolio, _StubMargin(current=120_000),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    risk.check(_state())
    assert not risk.killed


def test_theta_kill_fires_when_below_floor(cfg):
    """net_theta below theta_kill (most-negative) triggers halt."""
    cfg.kill_switch.theta_kill = -200.0  # halt if theta < -$200
    portfolio = PortfolioState(cfg)
    _add(portfolio, qty=1, theta=-250)  # short premium drift
    risk = RiskMonitor(portfolio, _StubMargin(current=10_000),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    risk.check(_state())
    assert risk.killed
    assert "THETA HALT" in risk.kill_reason


def test_theta_kill_disabled_when_unconfigured(cfg):
    """theta_kill = 0 (the default disable sentinel) means no theta
    kill regardless of how negative net_theta gets."""
    cfg.kill_switch.theta_kill = 0
    portfolio = PortfolioState(cfg)
    _add(portfolio, qty=1, theta=-99999)
    risk = RiskMonitor(portfolio, _StubMargin(current=10_000),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    risk.check(_state())
    assert not risk.killed


# ── Daily P&L halt — fill-path firing + auto-clear ──────────────────


def test_check_daily_pnl_only_fires_on_breach(cfg):
    """check_daily_pnl_only is the per-fill path. It must fire the
    halt when realized + MTM < threshold even though the periodic
    risk.check() hasn't run yet."""
    portfolio = PortfolioState(cfg)
    portfolio.realized_pnl_persisted = -3000.0  # past max_daily_loss=-2500
    quotes = _StubQuoteManager()
    risk = RiskMonitor(portfolio, _StubMargin(current=0), quotes,
                       _StubCSVLogger(), cfg)
    fired = risk.check_daily_pnl_only()
    assert fired is True
    assert risk.killed
    assert risk.kill_source == "daily_halt"
    assert risk.kill_type == "halt"
    assert quotes.cancelled >= 1


def test_clear_daily_halt_clears_daily_halt_kill(cfg):
    """Session rollover (17:00 CT) should auto-clear daily_halt source."""
    portfolio = PortfolioState(cfg)
    portfolio.realized_pnl_persisted = -3000.0
    risk = RiskMonitor(portfolio, _StubMargin(current=0),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    risk.check_daily_pnl_only()
    assert risk.killed and risk.kill_source == "daily_halt"
    cleared = risk.clear_daily_halt()
    assert cleared is True
    assert not risk.killed
    assert risk.kill_source == ""


def test_clear_daily_halt_does_not_clear_risk_kill(cfg):
    """A risk-source kill (margin, delta) must remain sticky across
    rollover. Only daily_halt and induced_daily_halt auto-clear."""
    portfolio = PortfolioState(cfg)
    _add(portfolio, qty=1, vega=2000)
    risk = RiskMonitor(portfolio, _StubMargin(current=10_000),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    risk.check(_state())
    assert risk.killed and risk.kill_source == "risk"
    assert risk.clear_daily_halt() is False
    assert risk.killed  # still sticky


def test_clear_disconnect_kill_clears_only_disconnect(cfg):
    """clear_disconnect_kill is the watchdog's reconnect path. Must not
    clear risk-source kills."""
    portfolio = PortfolioState(cfg)
    risk = RiskMonitor(portfolio, _StubMargin(current=0),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    risk.kill("connection lost", source="disconnect", kill_type="halt")
    assert risk.killed
    assert risk.clear_disconnect_kill() is True
    assert not risk.killed
    # Now fire a risk kill — clear_disconnect should NOT clear it.
    risk.kill("BAD", source="risk", kill_type="halt")
    assert risk.clear_disconnect_kill() is False
    assert risk.killed


# ── kill() dispatch — flatten_callback and hedge_manager wiring ──────


def test_kill_flatten_invokes_callback(cfg):
    """kill_type=flatten must call flatten_callback when wired."""
    portfolio = PortfolioState(cfg)
    flatten_calls = []
    risk = RiskMonitor(
        portfolio, _StubMargin(current=0),
        _StubQuoteManager(), _StubCSVLogger(), cfg,
        flatten_callback=lambda r: flatten_calls.append(r),
    )
    risk.kill("test_flatten", source="risk", kill_type="flatten")
    assert flatten_calls == ["test_flatten"]
    assert risk.killed


def test_kill_flatten_warns_when_callback_unwired(cfg, caplog):
    """kill_type=flatten without a callback should warn (degraded
    behavior — quotes still cancelled, but positions stay)."""
    portfolio = PortfolioState(cfg)
    risk = RiskMonitor(portfolio, _StubMargin(current=0),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    with caplog.at_level(logging.WARNING, logger="src.risk_monitor"):
        risk.kill("test", source="risk", kill_type="flatten")
    assert risk.killed
    assert any("no flatten_callback wired" in rec.message
               for rec in caplog.records)


def test_kill_hedge_flat_calls_force_flat(cfg):
    """kill_type=hedge_flat must invoke hedge_manager.force_flat.
    Used by delta kill (CLAUDE.md §6.2)."""
    from unittest.mock import MagicMock
    portfolio = PortfolioState(cfg)
    hedge = MagicMock()
    risk = RiskMonitor(portfolio, _StubMargin(current=0),
                       _StubQuoteManager(), _StubCSVLogger(), cfg,
                       hedge_manager=hedge)
    risk.kill("test", source="risk", kill_type="hedge_flat")
    hedge.force_flat.assert_called_once()


def test_kill_hedge_flat_warns_when_hedge_unwired(cfg, caplog):
    """kill_type=hedge_flat without hedge_manager — degrade to halt
    with a warning."""
    portfolio = PortfolioState(cfg)
    risk = RiskMonitor(portfolio, _StubMargin(current=0),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    with caplog.at_level(logging.WARNING, logger="src.risk_monitor"):
        risk.kill("test", source="risk", kill_type="hedge_flat")
    assert risk.killed
    assert any("no hedge_manager wired" in rec.message
               for rec in caplog.records)


# ── _hedge_mtm_usd graceful degradation ─────────────────────────────


def test_hedge_mtm_returns_zero_when_hedge_raises(cfg, caplog):
    """If hedge_manager.mtm_usd() raises, _hedge_mtm_usd must return 0
    (not propagate). The halt must keep running with under-reported
    hedge MTM rather than crash."""
    from unittest.mock import MagicMock
    portfolio = PortfolioState(cfg)
    hedge = MagicMock()
    hedge.mtm_usd.side_effect = RuntimeError("synthetic hedge failure")
    risk = RiskMonitor(portfolio, _StubMargin(current=0),
                       _StubQuoteManager(), _StubCSVLogger(), cfg,
                       hedge_manager=hedge)
    with caplog.at_level(logging.WARNING, logger="src.risk_monitor"):
        result = risk._hedge_mtm_usd()
    assert result == 0.0
    # Warning fired once
    assert any("hedge mtm_usd() raised" in rec.message
               for rec in caplog.records)
    # Second call must NOT log the warning again (one-time gate)
    caplog.clear()
    risk._hedge_mtm_usd()
    assert not any("hedge mtm_usd() raised" in rec.message
                   for rec in caplog.records)


def test_hedge_mtm_zero_when_no_hedge_wired(cfg):
    """No hedge_manager wired (single-product path or hedge disabled) —
    return 0 cleanly."""
    portfolio = PortfolioState(cfg)
    risk = RiskMonitor(portfolio, _StubMargin(current=0),
                       _StubQuoteManager(), _StubCSVLogger(), cfg)
    assert risk._hedge_mtm_usd() == 0.0


# ── Effective-delta gating toggle ───────────────────────────────────


def test_effective_delta_gating_off_uses_options_only(cfg):
    """When effective_delta_gating=False, delta kill ignores hedge_qty."""
    from unittest.mock import MagicMock
    cfg.constraints.effective_delta_gating = False
    portfolio = PortfolioState(cfg)
    _add(portfolio, qty=1, delta=5.5)  # options-only delta = 5.5
    hedge = MagicMock()
    hedge.hedge_qty_for_product.return_value = -10  # would offset
    risk = RiskMonitor(portfolio, _StubMargin(current=10_000),
                       _StubQuoteManager(), _StubCSVLogger(), cfg,
                       hedge_manager=hedge)
    risk.check(_state())
    # With gating OFF, the hedge offset is ignored — delta kill fires
    # because options_delta > 5.0
    assert risk.killed
    assert "DELTA KILL" in risk.kill_reason
