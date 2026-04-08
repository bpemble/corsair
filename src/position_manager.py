"""Position manager for Corsair v2.

Tracks all open positions, computes portfolio Greeks, handles fill recording,
expiry settlement, and daily accounting.
"""

import logging
from dataclasses import dataclass, field
from datetime import datetime
from typing import Dict, List, Optional, Tuple

from .greeks import GreeksCalculator, Greeks

logger = logging.getLogger(__name__)


@dataclass
class Position:
    """A single option position."""
    strike: float
    expiry: str             # YYYYMMDD
    put_call: str           # "C" or "P"
    quantity: int           # + = long, - = short
    avg_fill_price: float
    fill_time: datetime

    # Computed Greeks (refreshed every 5 min)
    delta: float = 0.0
    gamma: float = 0.0
    theta: float = 0.0     # $/day (already multiplied by multiplier)
    vega: float = 0.0
    current_price: float = 0.0


class PortfolioState:
    """Aggregate portfolio state with position tracking and Greek computation."""

    def __init__(self, config):
        self.config = config
        self.positions: List[Position] = []
        self.fills_today: int = 0
        self.spread_capture_today: float = 0.0       # theo-based, headline
        self.spread_capture_mid_today: float = 0.0   # mid-based, reality check
        self.daily_pnl: float = 0.0
        self._greeks_calc = GreeksCalculator()
        self._multiplier = config.product.multiplier

    @property
    def net_delta(self) -> float:
        """Portfolio delta in contract-equivalent terms."""
        return sum(p.delta * p.quantity for p in self.positions)

    @property
    def net_theta(self) -> float:
        """Portfolio theta in $/day."""
        return sum(p.theta * p.quantity for p in self.positions)

    @property
    def net_vega(self) -> float:
        """Portfolio vega."""
        return sum(p.vega * p.quantity for p in self.positions)

    @property
    def net_gamma(self) -> float:
        """Portfolio gamma."""
        return sum(p.gamma * p.quantity for p in self.positions)

    @property
    def long_count(self) -> int:
        return sum(p.quantity for p in self.positions if p.quantity > 0)

    @property
    def short_count(self) -> int:
        return sum(abs(p.quantity) for p in self.positions if p.quantity < 0)

    @property
    def gross_positions(self) -> int:
        return sum(abs(p.quantity) for p in self.positions)

    # ── Per-side accessors ─────────────────────────────────────────
    # Calls and puts have independent risk budgets. These return the same
    # quantity-weighted aggregates as the net_* properties above, but
    # filtered to one option type.
    def _aggregate_for(self, right: str, attr: str) -> float:
        return sum(getattr(p, attr) * p.quantity
                   for p in self.positions if p.put_call == right)

    def delta_for(self, right: str) -> float:
        return self._aggregate_for(right, "delta")

    def theta_for(self, right: str) -> float:
        return self._aggregate_for(right, "theta")

    def vega_for(self, right: str) -> float:
        return self._aggregate_for(right, "vega")

    def gross_for(self, right: str) -> int:
        return sum(abs(p.quantity) for p in self.positions if p.put_call == right)

    def seed_from_ibkr(self, ib, account_id: str) -> int:
        """Sync existing ETHUSDRR option positions from IBKR.

        Idempotent: clears the local position list first so multiple calls
        (initial startup + each watchdog reseed after a reconnect) don't
        accumulate duplicates. Filters strictly to symbol='ETHUSDRR' and
        secType='FOP' — ignores everything else (futures, other products,
        equities). Returns the number of positions seeded.
        """
        self.positions.clear()
        seeded = 0
        for ib_pos in ib.positions(account_id):
            c = ib_pos.contract
            if c.symbol != "ETHUSDRR":
                continue
            if c.secType != "FOP":
                continue
            if not c.right or c.right not in ("C", "P"):
                continue

            self.positions.append(Position(
                strike=float(c.strike),
                expiry=c.lastTradeDateOrContractMonth,
                put_call=c.right,
                quantity=int(ib_pos.position),
                avg_fill_price=float(ib_pos.avgCost) / self._multiplier,
                fill_time=datetime.now(),
            ))
            seeded += 1
        return seeded

    def add_fill(self, strike: float, expiry: str, put_call: str,
                 quantity: int, fill_price: float,
                 spread_captured: float = 0.0,
                 spread_captured_mid: float = 0.0):
        """Record a new fill. Merges with existing position at same strike/expiry.

        spread_captured is the theo-based dollar edge (headline metric).
        spread_captured_mid is the mid-based dollar edge (reality check).
        Both are signed; the caller has already applied the multiplier.
        """
        for pos in self.positions:
            if pos.strike == strike and pos.expiry == expiry and pos.put_call == put_call:
                new_qty = pos.quantity + quantity
                if new_qty == 0:
                    self.positions.remove(pos)
                else:
                    pos.quantity = new_qty
                    pos.avg_fill_price = fill_price
                self._record_fill(quantity, spread_captured, spread_captured_mid)
                return

        # New position
        self.positions.append(Position(
            strike=strike, expiry=expiry, put_call=put_call,
            quantity=quantity, avg_fill_price=fill_price,
            fill_time=datetime.now(),
        ))
        self._record_fill(quantity, spread_captured, spread_captured_mid)

    def _record_fill(self, quantity: int, spread_captured: float,
                     spread_captured_mid: float):
        """Track fill for daily accounting."""
        self.fills_today += abs(quantity)
        self.spread_capture_today += spread_captured
        self.spread_capture_mid_today += spread_captured_mid

    def refresh_greeks(self, market_state):
        """Recompute all position Greeks using current market data.

        Args:
            market_state: MarketState with current option quotes and underlying price.
        """
        from .utils import time_to_expiry_years

        underlying = market_state.underlying_price
        if underlying <= 0:
            return

        for pos in self.positions:
            option = market_state.get_option(pos.strike, pos.expiry, pos.put_call)
            if option is None:
                continue

            tte = time_to_expiry_years(pos.expiry)
            iv = option.iv if option.iv > 0 else 0.30  # Fallback IV

            greeks = self._greeks_calc.calculate(
                F=underlying, K=pos.strike, T=tte, sigma=iv,
                right=pos.put_call, multiplier=self._multiplier,
            )

            pos.delta = greeks.delta
            pos.gamma = greeks.gamma
            pos.theta = greeks.theta  # Already in $/day from calculator
            pos.vega = greeks.vega

            if option.bid > 0 and option.ask > 0:
                pos.current_price = (option.bid + option.ask) / 2
            elif option.last > 0:
                pos.current_price = option.last

    def compute_mtm_pnl(self) -> float:
        """Unrealized P&L across all positions."""
        total = 0.0
        for pos in self.positions:
            unrealized = (pos.current_price - pos.avg_fill_price) * pos.quantity * self._multiplier
            total += unrealized
        return total

    def handle_expiry(self, expiry_date: str, settlement_price: float) -> Tuple[float, int]:
        """Process expiring positions.

        Returns (settlement_pnl, futures_assigned).
        """
        expired = [p for p in self.positions if p.expiry == expiry_date]
        settlement_pnl = 0.0
        futures_assigned = 0

        for pos in expired:
            if pos.put_call == "C":
                intrinsic = max(settlement_price - pos.strike, 0)
            else:
                intrinsic = max(pos.strike - settlement_price, 0)

            if intrinsic > 0:
                pnl = (intrinsic - pos.avg_fill_price) * pos.quantity * self._multiplier
                settlement_pnl += pnl
                futures_assigned += pos.quantity
                logger.info(
                    "EXPIRY ITM: %s%.0f%s qty=%d intrinsic=%.2f pnl=$%.0f",
                    pos.put_call, pos.strike, pos.expiry, pos.quantity, intrinsic, pnl,
                )
            else:
                pnl = -pos.avg_fill_price * pos.quantity * self._multiplier
                settlement_pnl += pnl
                logger.info(
                    "EXPIRY OTM: %s%.0f%s qty=%d pnl=$%.0f",
                    pos.put_call, pos.strike, pos.expiry, pos.quantity, pnl,
                )

            self.positions.remove(pos)

        if futures_assigned != 0:
            logger.critical(
                "EXPIRY: %d futures contracts assigned. FLATTEN IMMEDIATELY AT NEXT OPEN.",
                abs(futures_assigned),
            )

        return settlement_pnl, futures_assigned

    def reset_daily(self):
        """Reset daily counters at start of each trading day."""
        self.fills_today = 0
        self.spread_capture_today = 0.0
        self.daily_pnl = 0.0
        logger.info("Daily counters reset")

    def get_summary(self) -> Dict:
        """Return a summary dict of current portfolio state."""
        return {
            "positions": self.gross_positions,
            "longs": self.long_count,
            "shorts": self.short_count,
            "net_delta": self.net_delta,
            "net_theta": self.net_theta,
            "net_vega": self.net_vega,
            "net_gamma": self.net_gamma,
            "fills_today": self.fills_today,
            "spread_capture": self.spread_capture_today,
            "unrealized_pnl": self.compute_mtm_pnl(),
        }
