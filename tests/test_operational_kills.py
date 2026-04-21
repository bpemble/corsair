"""OperationalKillMonitor tests.

Covers the sustained-breach pattern (``first_breach_ts``) which replaced
an earlier sliding-window check whose "oldest sample within 1s of
cutoff" gate silently failed at moderately-slow check cadences.
"""

from types import SimpleNamespace

import pytest

from src.operational_kills import OperationalKillMonitor


class _StubRisk:
    def __init__(self):
        self.killed = False
        self.kill_calls: list = []

    def kill(self, reason, source="risk", kill_type="halt"):
        self.killed = True
        self.kill_calls.append((reason, source, kill_type))


class _StubSabr:
    def __init__(self, rmse: float = None):
        self._rmse = rmse

    def latest_rmse(self, expiry: str):
        return self._rmse


class _StubQuotes:
    def __init__(self, p50_us: float = None):
        self._p50_us = p50_us

    def get_latency_snapshot(self):
        return {"place_rtt_us": {"p50": self._p50_us}}


def _make_monitor(rmse=None, p50_us=None, enabled=True,
                  rmse_threshold=0.05, rmse_window_sec=300.0,
                  latency_ms_max=2000.0, latency_window_sec=60.0,
                  fill_rate_mul=10.0,
                  fill_rate_baseline_window_sec=3600.0,
                  fill_rate_min_baseline_sec=1800.0):
    """Build an OperationalKillMonitor with a single synthetic engine."""
    config = SimpleNamespace(
        operational_kills=SimpleNamespace(
            enabled=enabled,
            sabr_rmse_threshold=rmse_threshold,
            sabr_rmse_window_sec=rmse_window_sec,
            quote_latency_max_ms=latency_ms_max,
            quote_latency_window_sec=latency_window_sec,
            abnormal_fill_rate_mul=fill_rate_mul,
            abnormal_fill_baseline_window_sec=fill_rate_baseline_window_sec,
            abnormal_fill_baseline_min_coverage_sec=fill_rate_min_baseline_sec,
        )
    )
    engines = [{
        "name": "HG",
        "sabr": _StubSabr(rmse=rmse),
        "md": SimpleNamespace(state=SimpleNamespace(expiries=["20260427"])),
        "quotes": _StubQuotes(p50_us=p50_us),
    }]
    portfolio = SimpleNamespace(fills_today=0)
    risk = _StubRisk()
    return OperationalKillMonitor(engines, portfolio, risk, config), risk


# ── RMSE sustained-breach pattern ─────────────────────────────────────

def test_rmse_single_sample_does_not_fire():
    """One breach sample starts a streak but doesn't fire — window_sec
    hasn't elapsed yet."""
    mon, risk = _make_monitor(rmse=0.1, rmse_window_sec=10.0)
    mon._check_sabr_rmse(now=1000.0)
    assert not risk.killed
    # Streak started
    assert mon._rmse_first_breach_ts[("HG", "20260427")] == 1000.0


def test_rmse_sustained_breach_fires_after_window():
    """Consecutive breaches spanning more than window_sec fire the kill."""
    mon, risk = _make_monitor(rmse=0.1, rmse_window_sec=10.0)
    mon._check_sabr_rmse(now=1000.0)
    assert not risk.killed
    mon._check_sabr_rmse(now=1015.0)  # 15s later, still breaching
    assert risk.killed
    assert risk.kill_calls[0][1] == "operational"


def test_rmse_good_sample_resets_streak():
    """A good sample (rmse ≤ threshold) resets the streak — next breach
    must re-accumulate from scratch before firing."""
    mon, _ = _make_monitor(rmse=0.1, rmse_window_sec=10.0)
    mon._check_sabr_rmse(now=1000.0)
    # Flip to a good sample
    mon.engines[0]["sabr"]._rmse = 0.01
    mon._check_sabr_rmse(now=1005.0)
    assert mon._rmse_first_breach_ts[("HG", "20260427")] is None

    # Breach resumes — fresh streak
    mon.engines[0]["sabr"]._rmse = 0.1
    mon._check_sabr_rmse(now=1010.0)
    assert mon._rmse_first_breach_ts[("HG", "20260427")] == 1010.0


def test_rmse_none_does_not_start_streak():
    """Pre-fit state (rmse is None) is NOT a breach — streak stays
    unarmed. A fresh SABR surface must not tag the kill."""
    mon, risk = _make_monitor(rmse=None, rmse_window_sec=10.0)
    mon._check_sabr_rmse(now=1000.0)
    mon._check_sabr_rmse(now=1015.0)
    assert not risk.killed
    assert ("HG", "20260427") not in mon._rmse_first_breach_ts


# ── Latency sustained-breach pattern ──────────────────────────────────

def test_latency_sustained_breach_fires_after_window():
    """Median place-RTT above threshold for window_sec triggers kill."""
    mon, risk = _make_monitor(p50_us=3_000_000,  # 3000ms
                              latency_ms_max=2000.0, latency_window_sec=30.0)
    mon._check_quote_latency(now=500.0)
    assert not risk.killed
    mon._check_quote_latency(now=535.0)
    assert risk.killed


def test_latency_good_sample_resets_streak():
    """p50 drops below threshold → streak resets → next breach
    starts fresh."""
    mon, risk = _make_monitor(p50_us=3_000_000,
                              latency_ms_max=2000.0, latency_window_sec=30.0)
    mon._check_quote_latency(now=500.0)
    assert mon._latency_first_breach_ts == 500.0
    mon.engines[0]["quotes"]._p50_us = 800_000  # 800ms
    mon._check_quote_latency(now=510.0)
    assert mon._latency_first_breach_ts is None
    assert not risk.killed


def test_latency_skips_when_no_samples_yet():
    """get_latency_snapshot returns p50=None pre-samples; the check
    must skip without starting a streak."""
    mon, _ = _make_monitor(p50_us=None)
    mon._check_quote_latency(now=500.0)
    assert mon._latency_first_breach_ts is None


# ── Abnormal fill rate — baseline-coverage gate ────────────────────────

def test_fill_rate_skips_when_baseline_span_too_short():
    """Observed 2026-04-21 08:37 CT: 4 fills in 110ms at market open
    tripped the kill against a 49-min-old baseline. The
    min-coverage gate must suppress the ratio check until enough time
    has elapsed."""
    mon, risk = _make_monitor(
        fill_rate_min_baseline_sec=1800.0,  # 30 min
    )
    # Simulate fresh boot: 5 quiet fills over ~15 min, then a burst.
    mon.portfolio.fills_today = 0
    mon._fill_hist = []
    # Populate baseline samples covering only 900s (15 min).
    for ts, fc in [(0, 0), (300, 1), (600, 2), (900, 3)]:
        mon._fill_hist.append((ts, fc))
    # Burst at t=960s: 4 fills in ~60s
    mon.portfolio.fills_today = 7
    mon._check_fill_rate(now=960.0)
    assert not risk.killed, (
        "fill_rate kill must not fire when baseline span < min_coverage"
    )


def test_fill_rate_fires_once_baseline_span_sufficient():
    """After the baseline window is populated, a genuine burst still
    fires the kill. Needs ≥2 samples in the last 60s so the short-
    window rate has a denominator."""
    mon, risk = _make_monitor(
        fill_rate_min_baseline_sec=1800.0,
        fill_rate_mul=10.0,
    )
    # ~35 min of slow-fill baseline + a pre-burst anchor at t=2040.
    mon._fill_hist = [
        (0, 0), (400, 1), (800, 2), (1200, 3), (1600, 4),
        (2000, 5), (2040, 5),
    ]
    # Burst: 4 fills between t=2040 and t=2100
    mon.portfolio.fills_today = 9
    mon._check_fill_rate(now=2100.0)
    assert risk.killed
    assert "ABNORMAL FILL RATE" in risk.kill_calls[0][0]


# ── Disabled / safety ─────────────────────────────────────────────────

def test_disabled_monitor_is_no_op():
    """config.operational_kills.enabled=False → check() returns without
    touching streak state."""
    mon, risk = _make_monitor(rmse=0.1, enabled=False)
    mon.check()
    assert not risk.killed
    assert mon._rmse_first_breach_ts == {}


def test_already_killed_monitor_is_no_op():
    """If risk is already killed, check() bails before any detection —
    operational kill layers on top, doesn't re-fire."""
    mon, risk = _make_monitor(rmse=0.1)
    risk.killed = True
    mon.check()
    # No additional kill call — it was already killed before entry.
    assert len(risk.kill_calls) == 0
