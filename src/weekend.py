"""Weekend, market-hours, and expiry protocol for Corsair v2.

Friday: Cancel all quotes, log weekend risk summary.
Monday: Check margin, refresh Greeks, validate before resuming.
Market-hours helper: used by the watchdog to skip tick-staleness checks
during the CME daily maintenance window and weekend closure.
"""

import logging
from datetime import datetime, timezone
from zoneinfo import ZoneInfo

from .utils import days_to_expiry

logger = logging.getLogger(__name__)

CT = ZoneInfo("America/Chicago")


def is_market_open(now_utc: datetime = None) -> bool:
    """Return True if CME Globex HG futures / HXE options are currently
    expected to be streaming ticks.

    CME Globex HG/HXE schedule (CT):
      - Sunday 17:00 → Friday 16:00: open
      - Daily maintenance window Mon-Thu 16:00-17:00 CT: closed
      - Friday 16:00 → Sunday 17:00: weekend closure

    Used by the watchdog to suppress tick-staleness reconnect storms
    during known no-tick windows. Connection loss is still treated as
    unhealthy regardless (ib.isConnected() check bypasses this gate).
    """
    if now_utc is None:
        now_utc = datetime.now(tz=timezone.utc)
    elif now_utc.tzinfo is None:
        now_utc = now_utc.replace(tzinfo=timezone.utc)
    now_ct = now_utc.astimezone(CT)
    dow = now_ct.weekday()  # 0=Mon .. 6=Sun
    hour = now_ct.hour

    # Weekend closure: Friday 16:00 CT through Sunday 17:00 CT.
    if dow == 4 and hour >= 16:          # Friday afternoon+
        return False
    if dow == 5:                          # Saturday all day
        return False
    if dow == 6 and hour < 17:            # Sunday before 17:00 CT
        return False

    # Daily maintenance window: Mon-Thu 16:00-17:00 CT. Friday 16:00 is
    # the weekly close (handled above), Sunday 17:00 is the reopen.
    if dow in (0, 1, 2, 3) and hour == 16:
        return False

    return True


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

    # 4. Check for near-expiry positions. monday_startup is called with the
    # global config (has `products` plural); per-product configs (with
    # `product` singular) live on each engine. HG-only deployment today —
    # use products[0]; revisit if multi-product deployments need per-position
    # min_dte lookup.
    min_dte = config.products[0].min_dte
    for pos in portfolio.positions:
        dte = days_to_expiry(pos.expiry)
        if dte <= min_dte:
            logger.info(
                "Position %s%.0f at %d DTE — will not quote this strike",
                pos.put_call, pos.strike, dte,
            )

    logger.info("Monday startup complete: safe to resume quoting")
    return True
