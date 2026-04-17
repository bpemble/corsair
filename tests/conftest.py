"""Shared pytest fixtures for corsair tests.

Provides:
  - cfg: a SimpleNamespace config matching the production YAML schema, with
    realistic numbers from the $25K margin variant spec.
  - fake_market_data: a stub MarketDataManager exposing just the surface
    constraint_checker / synthetic_span need (state.options, get_option,
    underlying_price).
  - fake_option / make_option: helpers to build OptionQuote-like objects.
"""

import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

# Make `src` importable when pytest is run from repo root
ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))


def _ns(**kwargs):
    return SimpleNamespace(**kwargs)


@pytest.fixture
def cfg():
    """Production-shaped config matching Stage 1 of the deployment ramp."""
    return _ns(
        product=_ns(
            symbol="ETH",
            underlying_symbol="ETHUSDRR",
            multiplier=50,
            option_type="both",
            strike_increment=25,
            quote_size=1,
            min_dte=3,
            quote_range_low=0,
            quote_range_high=20,
        ),
        constraints=_ns(
            capital=200000,
            margin_ceiling_pct=0.50,    # → $100K
            delta_ceiling=3.0,
            theta_floor=-200,
        ),
        puts=_ns(
            enabled=True,
            quote_range_low=-20,
            quote_range_high=0,
        ),
        kill_switch=_ns(
            margin_kill_pct=0.70,        # → $140K
            max_daily_loss=-2500,
            delta_kill=5.0,
            vega_kill=1000,
        ),
        synthetic_span=_ns(
            up_scan_pct=0.56,
            down_scan_pct=0.49,
            vol_scan_pct=0.25,
            extreme_mult=3.0,
            extreme_cover=0.33,
            short_option_minimum=500.0,
        ),
        pricing=_ns(
            sabr_enabled=True,
            sabr_beta=1.0,
            sabr_max_rmse=0.03,
            sabr_min_strikes=3,         # tests use small synthetic chains
            sabr_recalibrate_seconds=60,
            sabr_fast_recal_dollars=10.0,
            min_edge_points=1.0,
            max_calibration_bbo_width=10.0,
            stale_iv_threshold=0.10,
        ),
    )


class _FakeOption:
    """Subset of OptionQuote/Position needed by checker tests."""
    def __init__(self, strike, put_call, expiry, iv, delta, theta, vega,
                 bid=0.0, ask=0.0):
        self.strike = strike
        self.put_call = put_call
        self.expiry = expiry
        self.iv = iv
        self.delta = delta
        self.gamma = 0.0
        self.theta = theta
        self.vega = vega
        self.bid = bid
        self.ask = ask


@pytest.fixture
def make_option():
    """Factory for building option-quote stubs."""
    def _make(strike=2100, put_call="C", expiry="20260424", iv=0.65,
              delta=0.45, theta=-1.5, vega=8.0, bid=80.0, ask=82.0):
        return _FakeOption(strike, put_call, expiry, iv, delta, theta, vega, bid, ask)
    return _make


class _FakeState:
    def __init__(self):
        self.underlying_price = 2100.0
        self.options = {}
        self.front_month_expiry = "20260424"

    def get_option(self, strike, expiry=None, right="C"):
        return self.options.get((strike, expiry or self.front_month_expiry, right))


class _FakeMarketData:
    def __init__(self):
        self.state = _FakeState()

    def add(self, opt):
        self.state.options[(opt.strike, opt.expiry, opt.put_call)] = opt


@pytest.fixture
def fake_market_data(make_option):
    md = _FakeMarketData()
    # Pre-populate a couple of liquid call strikes
    for strike, delta in ((2100, 0.55), (2125, 0.45), (2150, 0.36)):
        md.add(make_option(strike=strike, delta=delta))
    return md
