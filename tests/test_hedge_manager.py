"""HedgeManager tests — local fill accounting, MTM, halt paths, fanout.

Fills the coverage gap for `_apply_local_fill` (the realized-P&L state
machine), `hedge_mtm_usd`, `flatten_on_halt`, `force_flat`, and the
HedgeFanout multi-product dispatcher. test_thread3.py already covers
contract resolution + burst tracker + priority drain.

Not covered here (require ib_insync mocking):
- IOC placement in `_place_or_log` execute branch
- `_on_exec_details` callback
- `reconcile_from_ibkr` against ib.positions()
- `resolve_hedge_contract` against reqContractDetailsAsync

Those need real IBKR plumbing or extensive mocks; deferred to
integration tests under live paper.
"""

from types import SimpleNamespace
from unittest.mock import MagicMock

import pytest

from src.hedge_manager import HedgeFanout, HedgeManager


# ── Fixtures ─────────────────────────────────────────────────────────


def _hedge_cfg(mode: str = "observe", multiplier: int = 25000,
               flatten_enabled: bool = True) -> SimpleNamespace:
    """Minimal config that HedgeManager accepts. Real config has more
    fields but HedgeManager only reads what's listed in __init__."""
    return SimpleNamespace(
        product=SimpleNamespace(
            symbol="HG", underlying_symbol="HG", multiplier=multiplier),
        hedging=SimpleNamespace(
            enabled=True, mode=mode,
            tolerance_deltas=0.5,
            rebalance_on_fill=True,
            rebalance_cadence_sec=30.0,
            include_in_daily_pnl=True,
            flatten_on_halt=flatten_enabled,
            ioc_tick_offset=2,
            hedge_lockout_days=0,  # disable lockout for tests
        ),
        quoting=SimpleNamespace(tick_size=0.0005),
        account=SimpleNamespace(account_id="DUP000000"),
    )


def _market_data(F: float = 6.0):
    md = SimpleNamespace()
    md.state = SimpleNamespace(underlying_price=F)
    md._underlying_contract = SimpleNamespace(
        symbol="HG", localSymbol="HGK6",
        lastTradeDateOrContractMonth="20260528",
        conId=1234567, multiplier=25000,
    )
    return md


def _make_hedge_manager(mode: str = "observe", F: float = 6.0,
                        multiplier: int = 25000,
                        flatten_enabled: bool = True) -> HedgeManager:
    """Build a hedge manager wired to a stub IB. Stub IB silences
    execDetailsEvent subscription; tests that exercise the on-fill
    handler can attach manually."""
    ib = MagicMock()
    ib.execDetailsEvent = MagicMock()
    cfg = _hedge_cfg(mode=mode, multiplier=multiplier,
                     flatten_enabled=flatten_enabled)
    md = _market_data(F=F)
    portfolio = SimpleNamespace(
        delta_for_product=lambda p: 0.0,
    )
    h = HedgeManager(ib, cfg, md, portfolio, csv_logger=None)
    return h


# ── _apply_local_fill: the realized-P&L state machine ───────────────


def test_apply_fill_open_from_flat_long():
    """BUY from hedge_qty=0 sets long; no realization."""
    h = _make_hedge_manager()
    h._apply_local_fill("BUY", qty=3, price=6.05)
    assert h.hedge_qty == 3
    assert h.avg_entry_F == pytest.approx(6.05)
    assert h.realized_pnl_usd == 0.0


def test_apply_fill_open_from_flat_short():
    """SELL from hedge_qty=0 sets short (negative); no realization."""
    h = _make_hedge_manager()
    h._apply_local_fill("SELL", qty=2, price=6.05)
    assert h.hedge_qty == -2
    assert h.avg_entry_F == pytest.approx(6.05)
    assert h.realized_pnl_usd == 0.0


def test_apply_fill_same_direction_add_averages():
    """BUY when already long should size-weight average the entry."""
    h = _make_hedge_manager()
    h._apply_local_fill("BUY", qty=2, price=6.00)  # long 2 @ 6.00
    h._apply_local_fill("BUY", qty=2, price=6.10)  # add 2 @ 6.10
    assert h.hedge_qty == 4
    # weighted avg: (6.00*2 + 6.10*2) / 4 = 6.05
    assert h.avg_entry_F == pytest.approx(6.05)
    assert h.realized_pnl_usd == 0.0


def test_apply_fill_full_reverse_flat_realizes_full_pnl():
    """SELL closes a long position back to 0 — full realization, avg
    resets, hedge_qty == 0."""
    h = _make_hedge_manager()
    h._apply_local_fill("BUY", qty=3, price=6.00)  # long 3 @ 6.00
    h._apply_local_fill("SELL", qty=3, price=6.10)  # close 3 @ 6.10
    assert h.hedge_qty == 0
    assert h.avg_entry_F == 0.0
    # Profit = (6.10 - 6.00) * 3 * 25000 = $7,500
    assert h.realized_pnl_usd == pytest.approx(7500.0)


def test_apply_fill_partial_reverse_realizes_proportional():
    """SELL of less than current long size — realizes only the closed
    portion, avg_entry_F unchanged on the residual."""
    h = _make_hedge_manager()
    h._apply_local_fill("BUY", qty=4, price=6.00)
    h._apply_local_fill("SELL", qty=1, price=6.05)
    assert h.hedge_qty == 3
    assert h.avg_entry_F == pytest.approx(6.00)  # residual unchanged
    # Profit = (6.05 - 6.00) * 1 * 25000 = $1,250
    assert h.realized_pnl_usd == pytest.approx(1250.0)


def test_apply_fill_over_reverse_flips_position():
    """SELL larger than current long flattens then re-opens short at the
    same price. Realizes only the closing portion."""
    h = _make_hedge_manager()
    h._apply_local_fill("BUY", qty=2, price=6.00)
    h._apply_local_fill("SELL", qty=5, price=6.10)
    assert h.hedge_qty == -3  # flipped to short 3
    assert h.avg_entry_F == pytest.approx(6.10)
    # Profit on close = (6.10 - 6.00) * 2 * 25000 = $5,000
    assert h.realized_pnl_usd == pytest.approx(5000.0)


def test_apply_fill_short_close_realizes_correctly():
    """BUY closing a short — profit when buy_price < avg short entry."""
    h = _make_hedge_manager()
    h._apply_local_fill("SELL", qty=4, price=6.10)  # short 4 @ 6.10
    h._apply_local_fill("BUY", qty=4, price=6.00)   # close 4 @ 6.00
    assert h.hedge_qty == 0
    # Short profit = (6.00 - 6.10) * -4 * 25000 = $10,000
    assert h.realized_pnl_usd == pytest.approx(10000.0)


# ── hedge_mtm_usd ───────────────────────────────────────────────────


def test_hedge_mtm_zero_when_flat():
    h = _make_hedge_manager()
    assert h.hedge_mtm_usd() == 0.0


def test_hedge_mtm_long_position_profits_on_F_rise():
    h = _make_hedge_manager(F=6.05)  # current F
    h._apply_local_fill("BUY", qty=3, price=6.00)  # entered at 6.00
    # MTM = (6.05 - 6.00) * 3 * 25000 = $3,750
    assert h.hedge_mtm_usd() == pytest.approx(3750.0)


def test_hedge_mtm_short_position_profits_on_F_drop():
    h = _make_hedge_manager(F=5.95)
    h._apply_local_fill("SELL", qty=2, price=6.00)
    # MTM = (5.95 - 6.00) * -2 * 25000 = $2,500
    assert h.hedge_mtm_usd() == pytest.approx(2500.0)


def test_hedge_mtm_zero_when_F_unavailable():
    """If underlying price hasn't populated (or went to 0/-1),
    hedge_mtm_usd returns 0 — must not propagate NaN to daily P&L."""
    h = _make_hedge_manager(F=6.05)
    h._apply_local_fill("BUY", qty=3, price=6.00)
    h.market_data.state.underlying_price = 0.0
    assert h.hedge_mtm_usd() == 0.0


# ── flatten_on_halt ─────────────────────────────────────────────────


def test_flatten_on_halt_noop_when_flat():
    """flatten_on_halt with hedge_qty=0 must not call _place_or_log
    — there's nothing to flatten."""
    h = _make_hedge_manager()
    # _place_or_log writes log lines; we mock it to count calls.
    h._place_or_log = MagicMock()
    h.flatten_on_halt(reason="halt_test")
    h._place_or_log.assert_not_called()


def test_flatten_on_halt_disabled_when_flatten_off():
    """If config.hedging.flatten_on_halt is False, the hedge stays even
    on halt — operator opt-out path."""
    h = _make_hedge_manager(flatten_enabled=False)
    h._apply_local_fill("BUY", qty=3, price=6.00)
    h._place_or_log = MagicMock()
    h.flatten_on_halt(reason="halt_test")
    h._place_or_log.assert_not_called()
    assert h.hedge_qty == 3  # untouched


def test_flatten_on_halt_long_position_calls_sell():
    """flatten_on_halt with hedge_qty=+3 must place a SELL of 3."""
    h = _make_hedge_manager()
    h._apply_local_fill("BUY", qty=3, price=6.00)
    h._place_or_log = MagicMock()
    h.flatten_on_halt(reason="halt_test")
    h._place_or_log.assert_called_once()
    args, kwargs = h._place_or_log.call_args
    assert args[0] == "SELL"
    assert args[1] == 3
    assert kwargs.get("target_qty") == 0


def test_flatten_on_halt_short_position_calls_buy():
    h = _make_hedge_manager()
    h._apply_local_fill("SELL", qty=2, price=6.00)
    h._place_or_log = MagicMock()
    h.flatten_on_halt(reason="halt_test")
    args, _kwargs = h._place_or_log.call_args
    assert args[0] == "BUY"
    assert args[1] == 2


# ── force_flat ──────────────────────────────────────────────────────


def test_force_flat_calls_rebalance_with_zero_tolerance():
    """force_flat bypasses the tolerance band by passing
    tolerance_override=0.0 to _rebalance."""
    h = _make_hedge_manager()
    h._rebalance = MagicMock()
    h.force_flat(reason="delta_kill")
    h._rebalance.assert_called_once()
    _args, kwargs = h._rebalance.call_args
    assert kwargs.get("tolerance_override") == 0.0


def test_force_flat_noop_when_disabled():
    h = _make_hedge_manager()
    h.enabled = False
    h._rebalance = MagicMock()
    h.force_flat(reason="delta_kill")
    h._rebalance.assert_not_called()


# ── HedgeFanout: multi-product dispatcher ───────────────────────────


def test_fanout_force_flat_calls_all_managers():
    m1, m2 = MagicMock(), MagicMock()
    m1._product = "HG"
    m2._product = "ETHUSDRR"
    fanout = HedgeFanout([m1, m2])
    fanout.force_flat(reason="delta_kill")
    m1.force_flat.assert_called_once_with(reason="delta_kill")
    m2.force_flat.assert_called_once_with(reason="delta_kill")


def test_fanout_isolates_exceptions():
    """A failing manager must not block subsequent managers — the kill
    must reach every product."""
    m1, m2 = MagicMock(), MagicMock()
    m1._product = "HG"
    m2._product = "ETHUSDRR"
    m1.force_flat.side_effect = RuntimeError("simulated hedge failure")
    fanout = HedgeFanout([m1, m2])
    fanout.force_flat(reason="delta_kill")  # must not raise
    # m2 must still have been called despite m1 raising
    m2.force_flat.assert_called_once()


def test_fanout_mtm_sums_across_managers():
    m1, m2 = MagicMock(), MagicMock()
    m1.hedge_mtm_usd.return_value = 1500.0
    m2.hedge_mtm_usd.return_value = -500.0
    fanout = HedgeFanout([m1, m2])
    assert fanout.mtm_usd() == pytest.approx(1000.0)


def test_fanout_mtm_zero_when_one_raises():
    """One manager raising returns 0 for that one but does not abort
    the sum (other managers' MTM still counts)."""
    m1, m2 = MagicMock(), MagicMock()
    m1.hedge_mtm_usd.side_effect = RuntimeError("mtm error")
    m2.hedge_mtm_usd.return_value = 750.0
    fanout = HedgeFanout([m1, m2])
    assert fanout.mtm_usd() == pytest.approx(750.0)


def test_fanout_hedge_qty_for_product_matches():
    m1, m2 = MagicMock(), MagicMock()
    m1._product = "HG"
    m1.hedge_qty = 4
    m2._product = "ETHUSDRR"
    m2.hedge_qty = -1
    fanout = HedgeFanout([m1, m2])
    assert fanout.hedge_qty_for_product("HG") == 4
    assert fanout.hedge_qty_for_product("ETHUSDRR") == -1
    assert fanout.hedge_qty_for_product("UNKNOWN") == 0
