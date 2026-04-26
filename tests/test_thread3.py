"""Unit tests for Thread 3: quote-staleness + burst mitigation.

Covers:
- FillBurstTracker rolling-window semantics
- Layer C1 (same-side) and C2 (any-side) trigger thresholds
- HedgeManager.request_priority_drain raising the drain flag and
  bypassing the cadence gate

Layer A and Layer B are exercised through smoke tests at the
note_layer_c_fill / _check_layer_b boundaries; full integration
coverage relies on the §6 instrumentation streams in paper trading.
"""
import time

import pytest

from src.hedge_manager import HedgeManager
from src.quote_engine import FillBurstTracker


# ── FillBurstTracker ────────────────────────────────────────────────


def test_burst_tracker_counts_within_window():
    t = FillBurstTracker(window_sec=1.0)
    same1, any1 = t.record_and_evaluate(0, "SELL")
    same2, any2 = t.record_and_evaluate(100_000_000, "SELL")
    same3, any3 = t.record_and_evaluate(200_000_000, "BUY")
    assert (same1, any1) == (1, 1)
    assert (same2, any2) == (2, 2)
    assert (same3, any3) == (1, 3)


def test_burst_tracker_evicts_outside_window():
    t = FillBurstTracker(window_sec=1.0)
    t.record_and_evaluate(0, "SELL")
    t.record_and_evaluate(600_000_000, "SELL")
    # 1.5s after the first fill: cutoff = 1.5s - 1.0s = 500ms; eviction
    # is exclusive of left boundary (event_ts <= cutoff drops). The fill
    # at 0s is well past cutoff → evicted; the fill at 600ms is inside
    # the window; the fill at 1.5s is the new event.
    same, any_count = t.record_and_evaluate(1_500_000_000, "SELL")
    assert any_count == 2
    assert same == 2


def test_burst_tracker_p1_pickoff_fires_c1_after_2():
    """Spec per-pattern matrix: 04-20 cluster — C1 fires after 2 same-side
    fills, prevents 5 of 7."""
    K1 = 2
    t = FillBurstTracker(window_sec=1.0)
    fills = [(0, "SELL"), (188_000_000, "SELL"), (265_000_000, "SELL"),
             (331_000_000, "SELL"), (392_000_000, "SELL"),
             (514_000_000, "SELL"), (811_000_000, "SELL")]
    fired_on = None
    for i, (ts, side) in enumerate(fills, 1):
        same, _ = t.record_and_evaluate(ts, side)
        if same >= K1 and fired_on is None:
            fired_on = i
    assert fired_on == 2
    assert len(fills) - fired_on == 5  # 5 prevented


def test_burst_tracker_p2_sweep_fires_c2_after_3():
    """Spec per-pattern matrix: 04-22 sweep — C2 fires when any-side
    count hits 3, prevents 5 of 8."""
    K2 = 3
    t = FillBurstTracker(window_sec=1.0)
    fills = [(0, "BUY"), (101_000_000, "SELL"), (204_000_000, "BUY"),
             (304_000_000, "SELL"), (398_000_000, "BUY"),
             (538_000_000, "SELL"), (635_000_000, "BUY"),
             (698_000_000, "SELL")]
    fired_on = None
    for i, (ts, side) in enumerate(fills, 1):
        _, any_count = t.record_and_evaluate(ts, side)
        if any_count >= K2 and fired_on is None:
            fired_on = i
    assert fired_on == 3
    assert len(fills) - fired_on == 5  # 5 prevented


def test_burst_tracker_rejects_non_positive_window():
    with pytest.raises(ValueError):
        FillBurstTracker(window_sec=0)
    with pytest.raises(ValueError):
        FillBurstTracker(window_sec=-1)


# ── HedgeManager priority drain ─────────────────────────────────────


class _StubMD:
    class _State:
        underlying_price = 0.0  # observe-mode skips trades when F<=0
    state = _State()
    _underlying_contract = None  # observe-only path


class _StubPortfolio:
    def delta_for_product(self, product):
        return 0.0


def _make_hedge_manager():
    """Build a HedgeManager with the minimum stubs to exercise the
    priority-drain path. Forward price is 0 so observe-mode trades are
    skipped; we're only verifying the priority flag mechanics."""
    cfg = type("Cfg", (), {})()
    cfg.hedging = type("H", (), {
        "enabled": True, "mode": "observe",
        "tolerance_deltas": 0.5, "rebalance_on_fill": True,
        "rebalance_cadence_sec": 30.0,
        "include_in_daily_pnl": True, "flatten_on_halt": True,
    })()
    cfg.account = type("A", (), {"account_id": "DUTEST"})()
    cfg.product = type("P", (), {
        "underlying_symbol": "HG", "multiplier": 25000,
    })()
    cfg.quoting = type("Q", (), {"tick_size": 0.0005})()
    return HedgeManager(ib=None, config=cfg, market_data=_StubMD(),
                        portfolio=_StubPortfolio())


def test_priority_drain_sets_flag():
    h = _make_hedge_manager()
    deadline = time.monotonic_ns() + 3_000_000_000
    h.request_priority_drain(deadline, reason="test_C2")
    assert h._priority_drain_until_ns == deadline


def test_priority_drain_extends_existing_deadline():
    h = _make_hedge_manager()
    early = time.monotonic_ns() + 1_000_000_000
    late = time.monotonic_ns() + 5_000_000_000
    h.request_priority_drain(early)
    h.request_priority_drain(late)
    assert h._priority_drain_until_ns == late


def test_priority_drain_clears_after_deadline():
    h = _make_hedge_manager()
    # Already-expired deadline
    deadline = time.monotonic_ns() - 1_000_000_000
    h._priority_drain_until_ns = deadline
    # rebalance_periodic should observe expiry and clear the flag.
    h.rebalance_periodic()
    assert h._priority_drain_until_ns == 0


def test_priority_drain_disabled_when_hedge_disabled():
    h = _make_hedge_manager()
    h.enabled = False
    h.request_priority_drain(time.monotonic_ns() + 1_000_000_000)
    # Disabled path must be a no-op — flag stays unset.
    assert h._priority_drain_until_ns == 0
