"""Position manager for Corsair v2.

Tracks all open positions, computes portfolio Greeks, handles fill recording,
expiry settlement, and daily accounting.
"""

import logging
from dataclasses import dataclass
from datetime import datetime
from typing import List, Tuple

from .utils import time_to_expiry_years

from .greeks import GreeksCalculator, Greeks

logger = logging.getLogger(__name__)


@dataclass
class Position:
    """A single option position.

    Multi-product: ``product`` is the IBKR underlying symbol (e.g.
    "ETHUSDRR" or "HG"). Drives margin/Greek dispatch — each product has
    its own market_data, SABR fit, multiplier, and synthetic SPAN engine,
    and the per-product values must not be mixed at compute time.
    """
    product: str            # IBKR underlying symbol; matches ib.positions().contract.symbol
    strike: float
    expiry: str             # YYYYMMDD
    put_call: str           # "C" or "P"
    quantity: int           # + = long, - = short
    avg_fill_price: float
    fill_time: datetime
    multiplier: float = 100.0  # contract multiplier (ETH=50, HG=25000, …)

    # Computed Greeks (refreshed every 5 min)
    delta: float = 0.0
    gamma: float = 0.0
    theta: float = 0.0     # $/day (already multiplied by multiplier)
    vega: float = 0.0
    current_price: float = 0.0


class PortfolioState:
    """Aggregate portfolio state with position tracking and Greek computation.

    Multi-product: maintains a single position list across ALL trading
    products on the same IBKR sub-account, plus a ``_products`` registry
    that maps product key → (multiplier, market_data, sabr). Each product
    must call ``register_product`` once before ``seed_from_ibkr`` runs;
    seed/refresh/reconcile then dispatch per-position to the correct
    market_data and SABR for IV resolution and Greek computation.
    """

    def __init__(self, config):
        self.config = config
        self.positions: List[Position] = []
        self.fills_today: int = 0
        self.spread_capture_today: float = 0.0       # theo-based, headline
        self.spread_capture_mid_today: float = 0.0   # mid-based, reality check
        self.daily_pnl: float = 0.0
        # Realized P&L for the current CME session, persisted across process
        # restarts via daily_state.json. IBKR's RealizedPnL account tag can
        # read 0 briefly after a reconnect; we hold the last non-zero value
        # so the dashboard shows a stable running total.
        self.realized_pnl_persisted: float = 0.0
        self._greeks_calc = GreeksCalculator()
        # Product registry: underlying_symbol → {multiplier, market_data, sabr}
        # Populated by main.py via register_product() at engine setup time.
        self._products: dict = {}

    def register_product(self, product: str, multiplier: float,
                         market_data, sabr) -> None:
        """Register a product so seed/refresh/reconcile pick up its positions.

        ``product`` is the IBKR underlying symbol (matches
        ``ib.positions().contract.symbol``). Idempotent — re-registering the
        same product just overwrites the entry, which is safe because
        watchdog reseeds re-call this implicitly through main.py wiring.
        """
        self._products[product] = {
            "multiplier": float(multiplier),
            "market_data": market_data,
            "sabr": sabr,
        }
        logger.info("PortfolioState registered product: %s (multiplier=%g)",
                    product, multiplier)

    def products(self) -> List[str]:
        """Return the list of registered product keys."""
        return list(self._products.keys())

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

    # ── Per-product accessors ─────────────────────────────────────
    # Multi-product: the portfolio-wide net_delta / net_theta mix
    # contract-equivalent values across products with different multipliers
    # (ETH 50 vs HG 25000), which is meaningless. Each product's constraint
    # checker should evaluate its own product's risk against its own caps.
    def positions_for_product(self, product: str) -> List[Position]:
        return [p for p in self.positions if p.product == product]

    def delta_for_product(self, product: str) -> float:
        return sum(p.delta * p.quantity for p in self.positions if p.product == product)

    def theta_for_product(self, product: str) -> float:
        return sum(p.theta * p.quantity for p in self.positions if p.product == product)

    def vega_for_product(self, product: str) -> float:
        return sum(p.vega * p.quantity for p in self.positions if p.product == product)

    def seed_from_ibkr(self, ib, account_id: str) -> int:
        """Sync existing option positions from IBKR for ALL registered products.

        Multi-product: walks ``ib.positions(account_id)`` once and accepts
        any position whose contract.symbol matches a registered product key
        (``register_product`` must have been called for each product BEFORE
        this method runs — otherwise that product's positions are silently
        skipped, which is exactly the bug this method was rewritten to
        fix). Sub-account-wide; one IBKR sub-account holds all products.

        Idempotent: clears the local position list first so multiple calls
        (initial startup + each watchdog reseed after a reconnect) don't
        accumulate duplicates. Filters require secType='FOP' and a real
        option right — ignores everything else (futures, equities).
        """
        self.positions.clear()
        if not self._products:
            logger.warning(
                "seed_from_ibkr called with NO registered products — "
                "all positions will be silently dropped")
        seeded = 0
        skipped: dict = {}
        for ib_pos in ib.positions(account_id):
            c = ib_pos.contract
            if c.secType != "FOP":
                continue
            if not c.right or c.right not in ("C", "P"):
                continue
            prod = c.symbol
            if prod not in self._products:
                skipped[prod] = skipped.get(prod, 0) + 1
                continue
            mult = self._products[prod]["multiplier"]
            self.positions.append(Position(
                product=prod,
                strike=float(c.strike),
                expiry=c.lastTradeDateOrContractMonth,
                put_call=c.right,
                quantity=int(ib_pos.position),
                avg_fill_price=float(ib_pos.avgCost) / mult,
                fill_time=datetime.now(),
                multiplier=mult,
            ))
            seeded += 1
        if skipped:
            logger.info(
                "seed_from_ibkr: skipped %d position(s) for unregistered "
                "product(s): %s", sum(skipped.values()), dict(skipped))
        if seeded > 0:
            try:
                self.refresh_greeks()
            except Exception:
                logger.exception("seed_from_ibkr: refresh_greeks failed")
        return seeded

    def add_fill(self, product: str, strike: float, expiry: str, put_call: str,
                 quantity: int, fill_price: float,
                 spread_captured: float = 0.0,
                 spread_captured_mid: float = 0.0):
        """Record a new fill. Merges with existing position at same
        (product, strike, expiry, put_call).

        ``product`` must match the IBKR underlying symbol (and a registered
        product key); the multiplier comes from the product registry so the
        per-position MtM math uses the right contract size.

        spread_captured is the theo-based dollar edge (headline metric).
        spread_captured_mid is the mid-based dollar edge (reality check).
        Both are signed; the caller has already applied the multiplier.
        """
        for pos in self.positions:
            if (pos.product == product and pos.strike == strike
                    and pos.expiry == expiry and pos.put_call == put_call):
                new_qty = pos.quantity + quantity
                if new_qty == 0:
                    self.positions.remove(pos)
                else:
                    pos.quantity = new_qty
                    pos.avg_fill_price = fill_price
                self._record_fill(quantity, spread_captured, spread_captured_mid)
                return

        # New position — pull multiplier from the product registry. Defensive
        # fallback to 100 (with a loud warning) if the product wasn't
        # registered: that means the FillHandler is for a product the
        # portfolio doesn't know about, which is itself a bug.
        prod_entry = self._products.get(product)
        if prod_entry is None:
            logger.warning(
                "add_fill: product %r not registered, defaulting multiplier=100. "
                "This position's MtM and seeding will be wrong.", product)
            mult = 100.0
        else:
            mult = prod_entry["multiplier"]
        self.positions.append(Position(
            product=product,
            strike=strike, expiry=expiry, put_call=put_call,
            quantity=quantity, avg_fill_price=fill_price,
            fill_time=datetime.now(),
            multiplier=mult,
        ))
        self._record_fill(quantity, spread_captured, spread_captured_mid)

    def _record_fill(self, quantity: int, spread_captured: float,
                     spread_captured_mid: float):
        """Track fill for daily accounting."""
        self.fills_today += abs(quantity)
        self.spread_capture_today += spread_captured
        self.spread_capture_mid_today += spread_captured_mid

    def refresh_greeks(self):
        """Recompute all position Greeks using current market data.

        Multi-product: each position dispatches to its product's registered
        market_data and SABR (for IV fallback).
        """
        for pos in self.positions:
            prod_entry = self._products.get(pos.product)
            if prod_entry is None:
                continue
            md = prod_entry["market_data"]
            sabr = prod_entry["sabr"]
            mult = prod_entry["multiplier"]

            underlying = md.state.underlying_price
            if underlying <= 0:
                continue

            option = md.state.get_option(pos.strike, pos.expiry, pos.put_call)

            # Resolve IV: prefer the live brentq IV from market_data; fall
            # back to SABR's fitted vol when the strike hasn't received fresh
            # bid/ask ticks; final fallback is a 0.30 placeholder so Greeks
            # are at least non-zero (they'll firm up on the next tick).
            iv = 0.0
            if option is not None and option.iv and option.iv > 0:
                iv = float(option.iv)
            elif sabr is not None and getattr(sabr, "last_calibration", None) is not None:
                try:
                    v = sabr.get_vol(pos.strike, expiry=pos.expiry)
                    if v and v > 0:
                        iv = float(v)
                except Exception:
                    pass
            if iv <= 0:
                iv = 0.30

            tte = time_to_expiry_years(pos.expiry)
            greeks = self._greeks_calc.calculate(
                F=underlying, K=pos.strike, T=tte, sigma=iv,
                right=pos.put_call, multiplier=mult,
            )

            pos.delta = greeks.delta
            pos.gamma = greeks.gamma
            pos.theta = greeks.theta  # Already in $/day from calculator
            pos.vega = greeks.vega

            if option is not None:
                if option.bid > 0 and option.ask > 0:
                    pos.current_price = (option.bid + option.ask) / 2
                elif option.last > 0:
                    pos.current_price = option.last

    def compute_mtm_pnl(self) -> float:
        """Unrealized P&L across all positions, summed per-position with
        each position's own multiplier (multi-product safe)."""
        total = 0.0
        for pos in self.positions:
            total += (pos.current_price - pos.avg_fill_price) * pos.quantity * pos.multiplier
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
                pnl = (intrinsic - pos.avg_fill_price) * pos.quantity * pos.multiplier
                settlement_pnl += pnl
                futures_assigned += pos.quantity
                logger.info(
                    "EXPIRY ITM: %s%.0f%s qty=%d intrinsic=%.2f pnl=$%.0f",
                    pos.put_call, pos.strike, pos.expiry, pos.quantity, intrinsic, pnl,
                )
            else:
                pnl = -pos.avg_fill_price * pos.quantity * pos.multiplier
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
        self.spread_capture_mid_today = 0.0
        self.daily_pnl = 0.0
        self.realized_pnl_persisted = 0.0
        logger.info("Daily counters reset")


