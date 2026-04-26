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


# ── Observational burst_events emission (master flag OFF) ──────────


class _StubLogger:
    """Captures log_paper_burst_event calls for assertion."""
    def __init__(self):
        self.burst_events = []
        self.order_lifecycle = []

    def log_paper_burst_event(self, **kw):
        self.burst_events.append(kw)

    def log_paper_order_lifecycle(self, **kw):
        self.order_lifecycle.append(kw)


def _make_quote_manager_minimal(thread3_overrides=None):
    """Construct just enough of a QuoteManager to call note_layer_c_fill.
    Avoids the full IB/market-data stack — we're testing pure logic here."""
    from src.quote_engine import FillBurstTracker
    qm = type("StubQM", (), {})()
    qm._fill_burst_tracker = FillBurstTracker(window_sec=1.0)
    qm.config = type("C", (), {})()
    qm.config.quoting = type("Q", (), {})()
    qm.config.quoting.thread3 = type("T3", (), {
        "master_enabled": False,
        "lever_c_burst_pull_enabled": False,
        "lever_c1_same_side_enabled": False,
        "lever_c2_any_side_enabled": False,
        "layer_c_window_sec": 1.0,
        "layer_c1_k1_threshold": 2,
        "layer_c2_k2_threshold": 3,
        "layer_c1_cooldown_sec": 2.0,
        "layer_c2_cooldown_sec": 3.0,
    })()
    if thread3_overrides:
        for k, v in thread3_overrides.items():
            setattr(qm.config.quoting.thread3, k, v)
    qm.csv_logger = _StubLogger()
    qm.market_data = type("MD", (), {})()
    qm.market_data.state = type("S", (), {"underlying_price": 6.10})()
    # Bind methods from real QuoteManager so we exercise the actual
    # branching logic, not a stub.
    from src.quote_engine import QuoteManager
    qm.note_layer_c_fill = QuoteManager.note_layer_c_fill.__get__(qm)
    qm._emit_burst_event = QuoteManager._emit_burst_event.__get__(qm)
    qm._emit_burst_event_observation = (
        QuoteManager._emit_burst_event_observation.__get__(qm))
    return qm


def test_burst_events_emit_observational_with_master_off():
    """Phase 3 instrumentation correction: thresholds always log,
    regardless of master flag state. Without this, baseline-window
    detection of P1/P2 patterns is impossible."""
    qm = _make_quote_manager_minimal()  # master_enabled=False (default)
    # 3 same-side fills in 1s — both K1=2 AND K2=3 cross at fill 3.
    qm.note_layer_c_fill(strike=6.10, expiry="20260526", right="C",
                         side="SELL", ts_ns=0)
    qm.note_layer_c_fill(strike=6.10, expiry="20260526", right="C",
                         side="SELL", ts_ns=100_000_000)
    qm.note_layer_c_fill(strike=6.10, expiry="20260526", right="C",
                         side="SELL", ts_ns=200_000_000)
    # Expect: fill 2 emits a C1 row (same=2≥K1); fill 3 emits both
    # C2 (any=3≥K2) and C1 (same=3≥K1).
    events = qm.csv_logger.burst_events
    triggers = [e["trigger"] for e in events]
    assert "C1" in triggers
    assert "C2" in triggers
    # All rows must be observational (action_fired=False) since master OFF
    assert all(not e["action_fired"] for e in events)
    # All rows must have empty pulled_order_ids on observation
    assert all(e["pulled_order_ids"] == [] for e in events)


def test_burst_events_action_fired_when_flags_on():
    """When master + sub-flag are ON, the corresponding action runs and
    its row carries action_fired=True."""
    qm = _make_quote_manager_minimal({
        "master_enabled": True,
        "lever_c_burst_pull_enabled": True,
        "lever_c2_any_side_enabled": True,
        # C1 still off so C2 takes precedence cleanly
    })
    # Stub the action methods so we don't need a full QuoteManager;
    # the row emission is what we're checking.
    qm.active_orders = {}
    qm.hedge_manager = None
    qm._burst_pull_c2_count = 0
    qm._burst_cooldown_global_until_ns = 0
    qm._burst_cooldown_per_side_until_ns = {"BUY": 0, "SELL": 0}
    from src.quote_engine import QuoteManager
    qm._fire_burst_pull_c2 = QuoteManager._fire_burst_pull_c2.__get__(qm)
    qm._pull_resting_orders = QuoteManager._pull_resting_orders.__get__(qm)
    qm._raise_hedge_drain = QuoteManager._raise_hedge_drain.__get__(qm)
    # 3 mixed-side fills to cross K2 only
    qm.note_layer_c_fill(strike=6.10, expiry="20260526", right="C",
                         side="BUY", ts_ns=0)
    qm.note_layer_c_fill(strike=6.10, expiry="20260526", right="C",
                         side="SELL", ts_ns=100_000_000)
    qm.note_layer_c_fill(strike=6.10, expiry="20260526", right="C",
                         side="BUY", ts_ns=200_000_000)
    events = qm.csv_logger.burst_events
    c2_rows = [e for e in events if e["trigger"] == "C2"]
    assert len(c2_rows) >= 1
    assert c2_rows[0]["action_fired"] is True


def test_burst_events_uses_K1_K2_field_names():
    """Brief §6.3 requires field names K1_count and K2_count, not
    same_side_count / any_side_count."""
    qm = _make_quote_manager_minimal()
    qm.note_layer_c_fill(strike=6.10, expiry="20260526", right="C",
                         side="SELL", ts_ns=0)
    qm.note_layer_c_fill(strike=6.10, expiry="20260526", right="C",
                         side="SELL", ts_ns=100_000_000)
    events = qm.csv_logger.burst_events
    assert events  # Should have at least the C1 row
    e = events[0]
    assert "K1_count" in e
    assert "K2_count" in e
    assert "same_side_count" not in e
    assert "any_side_count" not in e
