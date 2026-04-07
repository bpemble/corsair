"""Fill handler for Corsair v2.

Processes IBKR fill events: records fills to portfolio, logs them,
and triggers immediate quote re-evaluation.
"""

import logging
from datetime import datetime

logger = logging.getLogger(__name__)


class FillHandler:
    """Handles fill events from IBKR."""

    def __init__(self, ib, portfolio, margin_checker, quote_manager,
                 market_data, csv_logger, config):
        self.ib = ib
        self.portfolio = portfolio
        self.margin = margin_checker
        self.quotes = quote_manager
        self.market_data = market_data
        self.csv_logger = csv_logger
        self.config = config

        # Register fill callback
        self.ib.execDetailsEvent += self._on_exec_details

        self._seen_exec_ids = set()
        self._MAX_SEEN = 10_000

    def _on_exec_details(self, trade, fill):
        """Called when IBKR reports a fill execution."""
        exec_id = fill.execution.execId
        if exec_id in self._seen_exec_ids:
            return
        self._seen_exec_ids.add(exec_id)

        # Bound the dedup set
        if len(self._seen_exec_ids) > self._MAX_SEEN:
            self._seen_exec_ids = set(list(self._seen_exec_ids)[-5000:])

        # Only process option fills (ignore any stray futures fills)
        contract = fill.contract
        if not hasattr(contract, 'right') or not contract.right:
            return

        strike = float(contract.strike)
        expiry = contract.lastTradeDateOrContractMonth
        put_call = contract.right
        quantity = int(fill.execution.shares)
        if fill.execution.side == "SLD":
            quantity = -quantity
        fill_price = float(fill.execution.price)

        side = "BOUGHT" if quantity > 0 else "SOLD"

        # Record the fill
        self.portfolio.add_fill(
            strike=strike, expiry=expiry, put_call=put_call,
            quantity=quantity, fill_price=fill_price,
        )
        # Invalidate the synthetic SPAN portfolio cache — the position book
        # just changed, so the cached aggregate is stale.
        if hasattr(self.margin, "invalidate_portfolio"):
            self.margin.invalidate_portfolio()

        # Log
        logger.info(
            "FILL: %s %d %s%.0f@%.2f | margin=$%.0f delta=%.2f theta=$%.0f | fills_today=%d",
            side, abs(quantity), put_call, strike, fill_price,
            self.margin.get_current_margin(),
            self.portfolio.net_delta,
            self.portfolio.net_theta,
            self.portfolio.fills_today,
        )

        # CSV log
        self.csv_logger.log_fill(
            strike=strike, expiry=expiry, put_call=put_call,
            side=side, quantity=abs(quantity), fill_price=fill_price,
            margin_after=self.margin.get_current_margin(),
            delta_after=self.portfolio.net_delta,
            theta_after=self.portfolio.net_theta,
            vega_after=self.portfolio.net_vega,
            fills_today=self.portfolio.fills_today,
            cumulative_spread=self.portfolio.spread_capture_today,
        )

        # Immediately re-evaluate all quotes (fill changes portfolio state)
        self.quotes.update_quotes(self.portfolio)
