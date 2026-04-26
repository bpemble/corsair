"""Unit tests for BurstDetector (spec: hardburst-skip overlay Change 1)."""
import pytest

from src.market_data import BurstDetector


MS = 1_000_000  # ns per ms
TICK = 0.0005


def test_single_5tick_event_fires_then_clears():
    """Spec test: 10 BBO ticks with move_ticks=[0,1,0,2,0,5,0,0,0,0] at 10ms
    apart, window_ms=40, threshold=5. is_hardburst() True right after the
    5-tick event, False 40ms after the event.

    Query clock discipline: on_futs_bbo and is_hardburst both advance
    _evict state, so we check "still in window" mid-sequence before any
    post-event tick has strayed past window_ms."""
    d = BurstDetector(tick_size=TICK, window_ms=40, threshold_ticks=5)
    t0 = 1_000_000_000
    bid, ask = 6.0000, 6.0005
    # Seed
    d.on_futs_bbo(t0 - 10 * MS, bid, ask)
    # Steps 0-4: small moves, no trip
    for i, mv in enumerate([0, 1, 0, 2, 0]):
        bid += mv * TICK
        ask += mv * TICK
        d.on_futs_bbo(t0 + i * 10 * MS, bid, ask)
    assert not d.is_hardburst(t0 + 40 * MS)

    # Step 5: 5-tick move fires the trip
    bid += 5 * TICK
    ask += 5 * TICK
    ts_event = t0 + 5 * 10 * MS  # = t0 + 50ms
    d.on_futs_bbo(ts_event, bid, ask)
    assert d.is_hardburst(ts_event), "expected trip immediately after 5-tick move"

    # Still in window at 39ms
    assert d.is_hardburst(ts_event + 39 * MS), \
        "5-tick event should still be in window at 39ms post-event"

    # Evicted at exactly window_ms (event_ts == cutoff → removed)
    assert not d.is_hardburst(ts_event + 40 * MS), \
        "5-tick event should be evicted at exactly 40ms"


def test_two_3tick_events_below_threshold_never_fire():
    """Per-event max, not summed. Two 3-tick events 30ms apart with
    threshold=5 must keep is_hardburst() False throughout."""
    d = BurstDetector(tick_size=TICK, window_ms=40, threshold_ticks=5)
    t0 = 1_000_000_000
    bid, ask = 6.0000, 6.0005
    d.on_futs_bbo(t0, bid, ask)
    # first 3-tick move at t0+10ms
    bid += 3 * TICK
    ask += 3 * TICK
    d.on_futs_bbo(t0 + 10 * MS, bid, ask)
    assert not d.is_hardburst(t0 + 10 * MS)
    # second 3-tick move at t0+40ms (30ms after the first)
    bid += 3 * TICK
    ask += 3 * TICK
    d.on_futs_bbo(t0 + 40 * MS, bid, ask)
    assert not d.is_hardburst(t0 + 40 * MS), \
        "sum of two 3-tick moves should NOT trip a 5-tick threshold"


def test_window_eviction_over_time():
    """A single 6-tick event should fire, then clear after the window."""
    d = BurstDetector(tick_size=TICK, window_ms=100, threshold_ticks=5)
    t0 = 10_000_000_000
    d.on_futs_bbo(t0, 6.00, 6.0005)
    d.on_futs_bbo(t0 + 1 * MS, 6.003, 6.0035)  # 6-tick move
    assert d.is_hardburst(t0 + 1 * MS)
    assert d.is_hardburst(t0 + 50 * MS)
    assert d.is_hardburst(t0 + 99 * MS)
    assert not d.is_hardburst(t0 + 101 * MS)


def test_snapshot_reports_state():
    d = BurstDetector(tick_size=TICK, window_ms=50, threshold_ticks=3)
    t0 = 5_000_000_000
    snap = d.snapshot(t0)
    assert snap == {"n_events_in_window": 0, "max_ticks_in_window": 0,
                    "is_hardburst": False}
    d.on_futs_bbo(t0, 6.00, 6.0005)
    d.on_futs_bbo(t0 + 1 * MS, 6.001, 6.0015)  # 2-tick
    d.on_futs_bbo(t0 + 2 * MS, 6.003, 6.0035)  # 4-tick
    snap = d.snapshot(t0 + 2 * MS)
    assert snap["n_events_in_window"] == 2
    assert snap["max_ticks_in_window"] == 4
    assert snap["is_hardburst"] is True


def test_ignores_first_tick_no_prior_state():
    """First tick sets reference state without generating an event."""
    d = BurstDetector(tick_size=TICK, window_ms=40, threshold_ticks=5)
    t0 = 1_000_000_000
    d.on_futs_bbo(t0, 6.00, 6.0005)
    assert not d.is_hardburst(t0)
    snap = d.snapshot(t0)
    assert snap["n_events_in_window"] == 0


def test_rejects_bad_config():
    with pytest.raises(ValueError):
        BurstDetector(tick_size=0, window_ms=40, threshold_ticks=5)
    with pytest.raises(ValueError):
        BurstDetector(tick_size=TICK, window_ms=0, threshold_ticks=5)
    with pytest.raises(ValueError):
        BurstDetector(tick_size=TICK, window_ms=40, threshold_ticks=0)


def test_uses_max_of_bid_ask_moves():
    """Detector picks the larger side of bid vs ask move — handles
    one-sided crosses and bid-only/ask-only step moves."""
    d = BurstDetector(tick_size=TICK, window_ms=40, threshold_ticks=3)
    t0 = 1_000_000_000
    d.on_futs_bbo(t0, 6.0000, 6.0005)
    # bid moves 5 ticks, ask doesn't move (hypothetical asymmetric quote)
    d.on_futs_bbo(t0 + 1 * MS, 6.0025, 6.0005)
    assert d.is_hardburst(t0 + 1 * MS), \
        "bid-only 5-tick move should trigger at threshold=3"
