"""PortfolioState tests — per-side accessors and the IBKR seed filter.

Validates:
  - delta_for / theta_for / vega_for / gross_for filter by put_call
  - net_* properties sum across both sides
  - seed_from_ibkr filters strictly to ETHUSDRR FOPs (no futures, no other
    symbols, no equities)
"""

from datetime import datetime
from types import SimpleNamespace

import pytest

from src.position_manager import PortfolioState, Position


def _add(portfolio, strike, put_call, qty, delta=0.0, theta=0.0, vega=0.0,
         product="ETHUSDRR", multiplier=50.0):
    portfolio.positions.append(Position(
        product=product,
        strike=strike, expiry="20260424", put_call=put_call, quantity=qty,
        avg_fill_price=80.0, fill_time=datetime.now(),
        multiplier=multiplier,
        delta=delta, gamma=0.0, theta=theta, vega=vega,
    ))


def test_delta_for_filters_by_right(cfg):
    p = PortfolioState(cfg)
    _add(p, 2100, "C", +1, delta=0.5)
    _add(p, 2125, "C", -1, delta=0.4)
    _add(p, 2050, "P", +2, delta=-0.4)
    # calls: +0.5 - 0.4 = 0.1
    assert p.delta_for("C") == pytest.approx(0.1)
    # puts:  -0.4 * 2 = -0.8
    assert p.delta_for("P") == pytest.approx(-0.8)


def test_theta_for_filters_by_right(cfg):
    p = PortfolioState(cfg)
    _add(p, 2100, "C", +1, theta=-2.0)
    _add(p, 2050, "P", -1, theta=-1.5)
    assert p.theta_for("C") == pytest.approx(-2.0)
    # short put: theta * qty = -1.5 * -1 = +1.5
    assert p.theta_for("P") == pytest.approx(1.5)


def test_gross_for_filters_by_right(cfg):
    p = PortfolioState(cfg)
    _add(p, 2100, "C", +3)
    _add(p, 2125, "C", -2)
    _add(p, 2050, "P", +1)
    assert p.gross_for("C") == 5
    assert p.gross_for("P") == 1


def test_net_delta_sums_both_sides(cfg):
    p = PortfolioState(cfg)
    _add(p, 2100, "C", +1, delta=0.5)
    _add(p, 2050, "P", +1, delta=-0.4)
    assert p.net_delta == pytest.approx(0.5 - 0.4)


# ---- seed_from_ibkr filter --------------------------------------------------

class _FakeIB:
    def __init__(self, positions):
        self._positions = positions

    def positions(self, account_id):
        return self._positions


def _ib_pos(symbol, secType, right, strike, qty, expiry="20260424"):
    contract = SimpleNamespace(
        symbol=symbol, secType=secType, right=right, strike=strike,
        lastTradeDateOrContractMonth=expiry,
    )
    return SimpleNamespace(contract=contract, position=qty, avgCost=80.0)


def test_seed_from_ibkr_keeps_eth_options_only(cfg):
    p = PortfolioState(cfg)
    p.register_product("ETHUSDRR", 50.0, market_data=None, sabr=None)
    ib = _FakeIB([
        _ib_pos("ETHUSDRR", "FOP", "C", 2100, 1),
        _ib_pos("ETHUSDRR", "FOP", "P", 2050, 2),
        _ib_pos("ETHUSDRR", "FUT", "", 0, 5),       # underlying futures — drop
        _ib_pos("MCL", "FOP", "C", 95, 3),           # other product — drop
        _ib_pos("AAPL", "STK", "", 0, 100),          # equity — drop
        _ib_pos("ETHUSDRR", "FOP", "X", 2100, 1),    # bad right — drop
    ])
    seeded = p.seed_from_ibkr(ib, "U1234567")
    assert seeded == 2
    assert all(pos.put_call in ("C", "P") for pos in p.positions)
    assert all(pos.strike in (2100, 2050) for pos in p.positions)


def test_seed_from_ibkr_multi_product(cfg):
    """When multiple products are registered, seed must pick up all of them."""
    p = PortfolioState(cfg)
    p.register_product("ETHUSDRR", 50.0, market_data=None, sabr=None)
    p.register_product("HG", 25000.0, market_data=None, sabr=None)
    ib = _FakeIB([
        _ib_pos("ETHUSDRR", "FOP", "C", 2100, 1),
        _ib_pos("HG", "FOP", "P", 5.80, 5),
        _ib_pos("HG", "FOP", "C", 6.20, -2),
        _ib_pos("MCL", "FOP", "C", 95, 3),  # unregistered — drop
    ])
    seeded = p.seed_from_ibkr(ib, "U1234567")
    assert seeded == 3
    assert sum(1 for x in p.positions if x.product == "HG") == 2
    assert sum(1 for x in p.positions if x.product == "ETHUSDRR") == 1
    # Multipliers must propagate per-product
    hg = [x for x in p.positions if x.product == "HG"][0]
    assert hg.multiplier == 25000.0


def test_seed_from_ibkr_is_idempotent(cfg):
    """Calling seed_from_ibkr twice (e.g., initial startup + watchdog
    reseed after a reconnect) must NOT double the position list."""
    p = PortfolioState(cfg)
    p.register_product("ETHUSDRR", 50.0, market_data=None, sabr=None)
    ib = _FakeIB([
        _ib_pos("ETHUSDRR", "FOP", "C", 2100, 1),
        _ib_pos("ETHUSDRR", "FOP", "P", 2050, 2),
    ])
    p.seed_from_ibkr(ib, "U1234567")
    p.seed_from_ibkr(ib, "U1234567")  # second call should NOT duplicate
    assert len(p.positions) == 2
