"""Risk monitor for Corsair v2.

Continuous monitoring (every 5 minutes or more frequently):
- SPAN margin vs kill threshold
- Portfolio delta vs kill threshold
- Daily P&L vs kill threshold
- Margin warning (above ceiling but below kill)

Kill switch cancels all quotes immediately on breach.
"""

import logging
from datetime import datetime

logger = logging.getLogger(__name__)


class RiskMonitor:
    """Monitors portfolio risk and triggers kill switch on breaches."""

    def __init__(self, portfolio, margin_checker, quote_manager, csv_logger, config):
        self.portfolio = portfolio
        self.margin = margin_checker
        self.quotes = quote_manager
        self.csv_logger = csv_logger
        self.config = config
        self.killed = False
        self._kill_reason: str = ""
        # Tracks who triggered the kill so the watchdog knows whether it can
        # auto-clear it. "risk" kills are sticky (margin/delta/pnl breach
        # demands human review). "disconnect" kills clear automatically when
        # the watchdog successfully reconnects.
        self._kill_source: str = ""

    def check(self, market_state):
        """Run all risk checks. Called every greek_refresh_seconds."""
        if self.killed:
            return

        # Refresh Greeks
        self.portfolio.refresh_greeks(market_state)

        # Update cached margin
        if hasattr(self.margin, 'update_cached_margin'):
            self.margin.update_cached_margin()

        current_margin = self.margin.get_current_margin()
        capital = self.config.constraints.capital

        # Log risk snapshot
        self.csv_logger.log_risk_snapshot(
            underlying_price=market_state.underlying_price,
            margin_used=current_margin,
            margin_pct=current_margin / capital if capital > 0 else 0,
            net_delta=self.portfolio.net_delta,
            net_theta=self.portfolio.net_theta,
            net_vega=self.portfolio.net_vega,
            long_count=self.portfolio.long_count,
            short_count=self.portfolio.short_count,
            gross_positions=self.portfolio.gross_positions,
            unrealized_pnl=self.portfolio.compute_mtm_pnl(),
            daily_spread_capture=self.portfolio.spread_capture_today,
        )

        logger.info(
            "RISK: margin=$%.0f (%.0f%%) delta=%.2f theta=$%.0f vega=$%.0f "
            "positions=%d (L%d/S%d) pnl=$%.0f",
            current_margin, (current_margin / capital * 100) if capital > 0 else 0,
            self.portfolio.net_delta, self.portfolio.net_theta,
            self.portfolio.net_vega, self.portfolio.gross_positions,
            self.portfolio.long_count, self.portfolio.short_count,
            self.portfolio.compute_mtm_pnl(),
        )

        # Kill switch checks
        margin_kill = capital * self.config.kill_switch.margin_kill_pct
        if current_margin > margin_kill:
            self.kill(f"MARGIN KILL: ${current_margin:,.0f} > ${margin_kill:,.0f}")
            return

        if abs(self.portfolio.net_delta) > self.config.kill_switch.delta_kill:
            self.kill(
                f"DELTA KILL: {self.portfolio.net_delta:.2f} > "
                f"±{self.config.kill_switch.delta_kill}"
            )
            return

        # Vega kill switch (Stage 1+). Vega is the largest unmodeled risk
        # at production scale; bound it explicitly.
        vega_kill = float(getattr(self.config.kill_switch, "vega_kill", 0) or 0)
        if vega_kill > 0 and abs(self.portfolio.net_vega) > vega_kill:
            self.kill(
                f"VEGA KILL: ${self.portfolio.net_vega:+,.0f} > ±${vega_kill:,.0f}"
            )
            return

        if self.portfolio.daily_pnl < self.config.kill_switch.max_daily_loss:
            self.kill(
                f"P&L KILL: ${self.portfolio.daily_pnl:,.0f} < "
                f"${self.config.kill_switch.max_daily_loss:,.0f}"
            )
            return

        # Margin warning (above ceiling but below kill)
        margin_ceiling = capital * self.config.constraints.margin_ceiling_pct
        if current_margin > margin_ceiling:
            logger.warning(
                "MARGIN WARNING: $%.0f above ceiling $%.0f. Pulling all quotes.",
                current_margin, margin_ceiling,
            )
            self.quotes.cancel_all_quotes()

    def kill(self, reason: str, source: str = "risk"):
        """Emergency shutdown: cancel all quotes.

        source="risk" (default) for margin/delta/pnl breaches — sticky.
        source="disconnect" for gateway-loss kills — clearable by the
        watchdog after a successful reconnect.
        """
        logger.critical("KILL SWITCH ACTIVATED [%s]: %s", source, reason)
        self.quotes.cancel_all_quotes()
        self.killed = True
        self._kill_reason = reason
        self._kill_source = source

    def clear_disconnect_kill(self) -> bool:
        """Clear a kill IFF it was caused by a disconnect. Returns True if
        cleared. Risk-induced kills (margin/delta/pnl) remain sticky and
        will return False — those need human review."""
        if self.killed and self._kill_source == "disconnect":
            logger.info("Clearing disconnect-induced kill: %s", self._kill_reason)
            self.killed = False
            self._kill_reason = ""
            self._kill_source = ""
            return True
        return False

    @property
    def kill_reason(self) -> str:
        return self._kill_reason

    @property
    def kill_source(self) -> str:
        return self._kill_source
