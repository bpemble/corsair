"""Weekend and expiry protocol for Corsair v2.

Friday: Cancel all quotes, log weekend risk summary.
Monday: Check margin, refresh Greeks, validate before resuming.
"""

import logging

from .utils import days_to_expiry

logger = logging.getLogger(__name__)


def friday_shutdown(quotes, portfolio, config):
    """Run Friday afternoon before CME close."""
    # 1. Cancel all quotes
    quotes.cancel_all_quotes()
    logger.info("Friday shutdown: all quotes cancelled")

    # 2. Review positions for weekend risk
    for pos in portfolio.positions:
        dte = days_to_expiry(pos.expiry)
        if dte <= 3:
            logger.warning(
                "Near-expiry position over weekend: %s%.0f DTE=%d qty=%d",
                pos.put_call, pos.strike, dte, pos.quantity,
            )

    # 3. Log weekend risk summary
    logger.info(
        "Weekend summary: %d positions, delta=%.2f, theta=$%.0f/day",
        portfolio.gross_positions, portfolio.net_delta, portfolio.net_theta,
    )


def monday_startup(market_state, portfolio, margin_checker, config) -> bool:
    """Run Monday before resuming quotes.

    Returns True if safe to resume quoting.
    """
    # 1. Check if market data is flowing
    if market_state.underlying_price <= 0:
        logger.warning("Monday startup: no underlying price yet, waiting...")
        return False

    # 2. Refresh Greeks with Monday's prices
    portfolio.refresh_greeks()

    # 3. Check margin
    current_margin = margin_checker.get_current_margin()
    margin_ceiling = config.constraints.capital * config.constraints.margin_ceiling_pct

    if current_margin > margin_ceiling:
        logger.warning(
            "Monday margin spike: $%.0f > $%.0f. Quoting suspended.",
            current_margin, margin_ceiling,
        )
        return False

    # 4. Check for near-expiry positions
    for pos in portfolio.positions:
        dte = days_to_expiry(pos.expiry)
        if dte <= config.product.min_dte:
            logger.info(
                "Position %s%.0f at %d DTE — will not quote this strike",
                pos.put_call, pos.strike, dte,
            )

    logger.info("Monday startup complete: safe to resume quoting")
    return True
