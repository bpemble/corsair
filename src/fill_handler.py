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

        # Capture place→fill latency BEFORE recording the fill (the order id
        # mapping in QuoteManager survives until we explicitly pop it).
        fill_latency_ms = None
        try:
            order_id = trade.order.orderId
            if hasattr(self.quotes, "fill_latency_ms"):
                fill_latency_ms = self.quotes.fill_latency_ms(order_id)
        except Exception:
            pass

        # Compute realized edge two ways:
        #   theo-based: signed distance from our SABR theo (model PnL view)
        #   mid-based:  signed distance from clean-BBO mid (microstructure view)
        # Theo is the headline metric; mid is logged as a reality check.
        # Both are in dollars (multiplier applied) and signed — negatives mean
        # we paid above theo / above mid (bought) or sold below.
        mult = self.portfolio._multiplier
        sign = 1 if quantity > 0 else -1  # buy: want fill < ref; sell: want fill > ref

        spread_captured_theo = 0.0
        try:
            theo = self.quotes.sabr.get_theo(strike, put_call)
            if theo and theo > 0:
                edge_theo = (theo - fill_price) * sign
                spread_captured_theo = edge_theo * mult * abs(quantity)
        except Exception:
            pass

        spread_captured_mid = 0.0
        try:
            bid, ask = self.market_data.get_clean_bbo(strike, put_call)
            if bid > 0 and ask > 0 and ask > bid:
                mid = (bid + ask) / 2.0
                edge_mid = (mid - fill_price) * sign
                spread_captured_mid = edge_mid * mult * abs(quantity)
        except Exception:
            pass

        # Record the fill
        self.portfolio.add_fill(
            strike=strike, expiry=expiry, put_call=put_call,
            quantity=quantity, fill_price=fill_price,
            spread_captured=spread_captured_theo,
            spread_captured_mid=spread_captured_mid,
        )
        # Invalidate the synthetic SPAN portfolio cache — the position book
        # just changed, so the cached aggregate is stale.
        if hasattr(self.margin, "invalidate_portfolio"):
            self.margin.invalidate_portfolio()

        # Refresh Greeks immediately so the just-added position contributes
        # real delta/theta/vega to the fill log line and CSV row. Without this
        # the new Position carries zeros until the next 5-minute refresh tick,
        # which makes per-fill decomposition (spread vs theta vs MtM) useless.
        try:
            self.portfolio.refresh_greeks(self.market_data.state)
        except Exception:
            logger.exception("refresh_greeks after fill failed")

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
            spread_captured_theo=spread_captured_theo,
            spread_captured_mid=spread_captured_mid,
            margin_after=self.margin.get_current_margin(),
            delta_after=self.portfolio.net_delta,
            theta_after=self.portfolio.net_theta,
            vega_after=self.portfolio.net_vega,
            fills_today=self.portfolio.fills_today,
            cumulative_spread_theo=self.portfolio.spread_capture_today,
            cumulative_spread_mid=self.portfolio.spread_capture_mid_today,
            fill_latency_ms=fill_latency_ms,
        )

        # Immediately re-evaluate all quotes (fill changes portfolio state)
        self.quotes.update_quotes(self.portfolio)
