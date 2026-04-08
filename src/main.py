"""Corsair v2 — Main Event Loop.

Automated options market-making system for CME ETHUSDRR.
Quotes two-sided markets on OTM calls, earns spread per fill,
manages risk through SPAN margin and portfolio theta constraints.
"""

import asyncio
import logging
import os
import signal
import time as _time
from typing import Optional
from datetime import datetime, date, timezone, timedelta
from zoneinfo import ZoneInfo

from . import ib_insync_patch as _ib_insync_patch
_ib_insync_patch.apply()  # must run before any IB instance is created

from .config import load_config
from .utils import setup_logging, days_to_expiry
from .connection import IBKRConnection
from .market_data import MarketDataManager
from .position_manager import PortfolioState
from .constraint_checker import IBKRMarginChecker, ConstraintChecker
from .sabr import SABRSurface
from .quote_engine import QuoteManager
from .fill_handler import FillHandler
from .risk_monitor import RiskMonitor
from .logging_utils import CSVLogger
from .weekend import friday_shutdown, monday_startup
from .snapshot import write_chain_snapshot
from .watchdog import (
    safe_discover_and_subscribe,
    watchdog_loop,
    escalate_gateway_recreate,
    STARTUP_RETRY_BACKOFF_SEC,
    GATEWAY_RESTART_SETTLE_SEC,
    RECOVERY_FAILS_BEFORE_GATEWAY_RESTART,
)

CT = ZoneInfo("America/Chicago")

logger = logging.getLogger(__name__)


def _session_day(now_utc: datetime, reset_hour_ct: int) -> date:
    """Return the CME session date for `now_utc`. Day rolls at reset_hour_ct CT."""
    now_ct = now_utc.astimezone(CT)
    if now_ct.hour >= reset_hour_ct:
        return (now_ct + timedelta(days=1)).date()
    return now_ct.date()



def _get_todays_expiries(positions):
    """Return set of expiry strings that expire today."""
    today = datetime.now().strftime("%Y%m%d")
    return {p.expiry for p in positions if p.expiry == today}



async def main():
    # ── 1. Load config ────────────────────────────────────────────────
    config_path = os.environ.get("CORSAIR_CONFIG", "config/corsair_v2_config.yaml")
    config = load_config(config_path)
    setup_logging(config.logging)

    os.makedirs("data", exist_ok=True)

    logger.info("=" * 60)
    logger.info("Corsair v2 starting up")
    logger.info("=" * 60)
    loop_name = type(asyncio.get_running_loop()).__module__
    logger.info("Asyncio loop: %s", loop_name)

    # ── 2. Connect to IBKR with retry ────────────────────────────────
    # Initial connect can fail when the gateway is in a bad post-restart
    # state (accepts TCP but rejects API handshake). Retry with backoff
    # so the engine self-heals instead of exiting and relying on docker
    # to crash-loop us.
    conn = IBKRConnection(config)
    connect_attempt = 0
    while not await conn.connect():
        wait = STARTUP_RETRY_BACKOFF_SEC[
            min(connect_attempt, len(STARTUP_RETRY_BACKOFF_SEC) - 1)
        ]
        connect_attempt += 1
        logger.warning(
            "Initial connect failed (attempt %d) — retrying in %ds",
            connect_attempt, wait,
        )
        await asyncio.sleep(wait)
    if connect_attempt > 0:
        logger.info("Initial connect succeeded after %d retries", connect_attempt)

    ib = conn.ib

    # ── 3. Initialize components ──────────────────────────────────────
    csv_logger = CSVLogger(config)
    market_data = MarketDataManager(ib, config, csv_logger=csv_logger)
    portfolio = PortfolioState(config)

    # Reconcile any existing ETHUSDRR option positions from IBKR
    seeded = portfolio.seed_from_ibkr(ib, config.account.account_id)
    if seeded:
        logger.info("Reconciled %d existing ETHUSDRR option position(s) from IBKR", seeded)

    sabr = SABRSurface(config)
    # SABR is passed to the margin checker so it can fall back to fitted vol
    # when a position's strike has no fresh bid/ask tick (otherwise positions
    # at quiet wing strikes get silently dropped from the SPAN calc).
    margin = IBKRMarginChecker(ib, config, market_data, portfolio, sabr=sabr)
    constraint_checker = ConstraintChecker(margin, portfolio, config)
    quotes = QuoteManager(ib, config, market_data, sabr, constraint_checker,
                          csv_logger=csv_logger)
    # Back-reference so the trade-tape capture in market_data can look up
    # our resting state at print time.
    market_data.quotes = quotes
    risk = RiskMonitor(portfolio, margin, quotes, csv_logger, config)
    fills = FillHandler(ib, portfolio, margin, quotes, market_data, csv_logger, config)

    # ── 4. Register disconnect callback ───────────────────────────────
    def on_disconnect():
        logger.critical("GATEWAY DISCONNECT — cancelling all quotes")
        quotes.cancel_all_quotes()
        # Tag the kill as disconnect-induced so the watchdog can clear it
        # after a successful reconnect.
        risk.kill("Gateway disconnect", source="disconnect")

    conn.set_disconnect_callback(on_disconnect)

    # ── 5. Cancel any stale orders from previous runs ───────────────
    cancelled = 0
    for trade in ib.openTrades():
        ref = getattr(trade.order, "orderRef", "") or ""
        if ref.startswith("corsair2"):
            try:
                ib.cancelOrder(trade.order)
                cancelled += 1
            except Exception:
                pass
    if cancelled:
        logger.info("Cancelled %d stale orders from previous run", cancelled)
        await asyncio.sleep(2)  # Let cancels settle

    # ── 6. Subscribe to market data with hard timeout + retry ─────────
    # IB Gateway can complete connect() but then hang inside the discovery
    # path. Wrap in a timeout and retry with backoff. After N consecutive
    # discovery failures, escalate to a gateway container recreate (volume
    # wipe + fresh container) — the lighter container.restart() doesn't
    # reliably clear the IBC session-state corruption we keep hitting.
    logger.info("Discovering option chain and subscribing to market data...")
    startup_attempt = 0
    discovery_fails = 0
    while not await safe_discover_and_subscribe(market_data):
        discovery_fails += 1
        if discovery_fails >= RECOVERY_FAILS_BEFORE_GATEWAY_RESTART:
            logger.critical(
                "Startup: %d consecutive discovery failures — escalating "
                "to gateway recreate", discovery_fails,
            )
            try:
                await conn.disconnect()
            except Exception:
                pass
            if escalate_gateway_recreate():
                await asyncio.sleep(GATEWAY_RESTART_SETTLE_SEC)
                discovery_fails = 0
                startup_attempt = 0
            if not await conn.connect():
                logger.error("Post-escalation reconnect failed; will retry")
            continue

        wait = STARTUP_RETRY_BACKOFF_SEC[
            min(startup_attempt, len(STARTUP_RETRY_BACKOFF_SEC) - 1)
        ]
        startup_attempt += 1
        logger.warning(
            "Startup discovery failed (attempt %d) — bouncing connection and "
            "retrying in %ds", startup_attempt, wait,
        )
        try:
            await conn.disconnect()
        except Exception:
            pass
        await asyncio.sleep(wait)
        if not await conn.connect():
            logger.error("Reconnect attempt failed; will retry next cycle")
            continue
    if startup_attempt > 0:
        logger.info("Startup discovery succeeded after %d retries", startup_attempt)

    # Wait for initial data to flow in (clean BBO without our orders)
    logger.info("Waiting for initial market data (5s)...")
    await asyncio.sleep(5)

    logger.info(
        "Market data ready: underlying=%.2f, ATM=%.0f, %d options, expiry=%s",
        market_data.state.underlying_price,
        market_data.state.atm_strike,
        len(market_data.state.options),
        market_data.state.front_month_expiry,
    )

    # Set SABR expiry
    sabr.set_expiry(market_data.state.front_month_expiry)

    # Seed incumbent tracker with clean market data (before we place any orders)
    for strike in market_data.state.get_all_strikes():
        for right in ("C", "P"):
            opt = market_data.state.get_option(strike, right=right)
            if opt is None:
                continue
            if opt.bid > 0:
                quotes._incumbent_bid[(strike, right)] = opt.bid
            if opt.ask > 0:
                quotes._incumbent_ask[(strike, right)] = opt.ask
    logger.info("Incumbent tracker seeded with %d (strike,right) pairs", len(quotes._incumbent_bid))

    # ── 6. Handle graceful shutdown ───────────────────────────────────
    shutdown_event = asyncio.Event()

    def handle_signal(sig):
        logger.info("Received signal %s, initiating shutdown...", sig)
        shutdown_event.set()

    loop = asyncio.get_event_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, lambda s=sig: handle_signal(s))

    # ── 7. Main loop ─────────────────────────────────────────────────
    last_greek_refresh = datetime.min
    last_depth_rotation = datetime.min
    last_full_refresh = _time.monotonic()
    last_snapshot_write = _time.monotonic()
    depth_rotation_interval = float(getattr(config.quoting, "depth_rotation_interval_sec", 2.0))
    last_session_day: Optional[date] = None
    last_friday_shutdown_date: Optional[date] = None
    weekend_paused = False
    reset_hour_ct = int(getattr(getattr(config, "operations", object()),
                                "daily_reset_hour_ct", 17))
    weekend_flatten = bool(getattr(getattr(config, "operations", object()),
                                   "weekend_flatten", True))
    fallback_interval = config.quoting.refresh_interval_ms / 1000.0
    batch_window_sec = float(getattr(config.quoting, "tick_batch_window_sec", 0.005))
    snapshot_interval = float(getattr(config.quoting, "snapshot_write_interval_sec", 1.0))

    logger.info(
        "Entering event-driven quote loop (batch=%.0fms, fallback=%.1fs, snapshot=%.1fs)",
        batch_window_sec * 1000, fallback_interval, snapshot_interval,
    )

    # Launch the watchdog in the background. Detects connection loss and
    # silent gateway hangs; auto-reconnects with backoff. Survives the
    # lifetime of the main loop and is cancelled on shutdown.
    watchdog_task = asyncio.create_task(
        watchdog_loop(conn, market_data, quotes, portfolio, margin, risk,
                      sabr, config.account.account_id, shutdown_event),
        name="watchdog",
    )

    while not shutdown_event.is_set():
        now = datetime.now()
        now_utc = datetime.now(tz=timezone.utc)
        now_ct = now_utc.astimezone(CT)
        session_day = _session_day(now_utc, reset_hour_ct)

        # Daily reset at 5pm CT (CME daily session boundary)
        if last_session_day != session_day:
            portfolio.reset_daily()
            last_session_day = session_day
            logger.info("New CME session day: %s (5pm CT rollover)", session_day)
            for expiry in _get_todays_expiries(portfolio.positions):
                settlement = market_data.state.underlying_price
                pnl, futures = portfolio.handle_expiry(expiry, settlement)
                logger.info("Expiry settlement: %s, P&L=$%.0f, Futures=%d", expiry, pnl, futures)

        # Weekend protocol
        if weekend_flatten:
            is_friday_close = (now_ct.weekday() == 4 and now_ct.hour >= 15)
            is_weekend = now_ct.weekday() >= 5
            if (is_friday_close or is_weekend) and not weekend_paused:
                if last_friday_shutdown_date != now_ct.date():
                    friday_shutdown(quotes, portfolio, config)
                    last_friday_shutdown_date = now_ct.date()
                weekend_paused = True
            elif weekend_paused and not (is_friday_close or is_weekend):
                if monday_startup(market_data.state, portfolio, margin, config):
                    weekend_paused = False
                    logger.info("Weekend protocol cleared; resuming quotes")

        # Greek refresh (every 5 minutes)
        if (now - last_greek_refresh).total_seconds() >= config.quoting.greek_refresh_seconds:
            risk.check(market_data.state)
            last_greek_refresh = now

        # Rotate depth subscriptions
        if (now - last_depth_rotation).total_seconds() >= depth_rotation_interval:
            try:
                market_data.rotate_depth_subscriptions()
            except Exception as e:
                logger.warning("depth rotation error: %s", e)
            last_depth_rotation = now

        # ── Event-driven quote phase ─────────────────────────────────
        if risk.killed or weekend_paused:
            await asyncio.sleep(fallback_interval)
            continue

        dirty_keys: set = set()
        underlying_dirty = False
        try:
            tick_type, key = await asyncio.wait_for(
                market_data.tick_queue.get(), timeout=fallback_interval,
            )
            if tick_type == "underlying":
                underlying_dirty = True
            elif key is not None:
                dirty_keys.add(key)

            # Drain whatever is already queued, instantly. Most of the time
            # this empties immediately and we proceed without any sleep.
            while True:
                try:
                    tick_type, key = market_data.tick_queue.get_nowait()
                    if tick_type == "underlying":
                        underlying_dirty = True
                    elif key is not None:
                        dirty_keys.add(key)
                except asyncio.QueueEmpty:
                    break

            # Only on an underlying tick (which fans out to ~28 options) is
            # there a real benefit to waiting briefly for the burst — those
            # 28 ticks may not have arrived yet. For pure option ticks, we
            # already have everything we need; skip the wait.
            if underlying_dirty:
                await asyncio.sleep(batch_window_sec)
                while True:
                    try:
                        tick_type, key = market_data.tick_queue.get_nowait()
                        if tick_type == "underlying":
                            pass
                        elif key is not None:
                            dirty_keys.add(key)
                    except asyncio.QueueEmpty:
                        break
        except asyncio.TimeoutError:
            pass  # No ticks — fall through to periodic refresh check

        # Decide what to reprice
        mono = _time.monotonic()
        do_full_refresh = (mono - last_full_refresh) >= fallback_interval

        if do_full_refresh:
            dirty = None  # signal full refresh
            last_full_refresh = mono
        else:
            # Convert OptionKeys (strike, expiry, right) to (strike, right)
            # pairs that the quoter iterates. Underlying ticks fan out to all
            # strikes so we promote to a full refresh.
            if underlying_dirty:
                dirty = None
                last_full_refresh = mono
            elif dirty_keys:
                dirty = {(k[0], k[2]) for k in dirty_keys}
            else:
                dirty = set()  # nothing to do this cycle

        if dirty is None or dirty:
            try:
                quotes.update_quotes(portfolio, dirty=dirty)
            except Exception as e:
                logger.error("Quote update error: %s", e, exc_info=True)

        # Throttled snapshot write for dashboard
        if (mono - last_snapshot_write) >= snapshot_interval:
            try:
                write_chain_snapshot(market_data, quotes, portfolio, sabr,
                                     margin, ib, config.account.account_id, config)
            except Exception as e:
                logger.warning("Snapshot write error: %s", e)
            last_snapshot_write = mono

    # ── 8. Shutdown ──────────────────────────────────────────────────
    logger.info("Shutting down...")
    watchdog_task.cancel()
    try:
        await watchdog_task
    except (asyncio.CancelledError, Exception):
        pass
    quotes.cancel_all_quotes()
    market_data.cancel_all_subscriptions()
    await conn.disconnect()
    logger.info("Corsair v2 shutdown complete.")


def run():
    """Entry point for running Corsair v2."""
    # uvloop must be installed BEFORE the asyncio loop is created.
    # Logging isn't set up yet, so report status via print().
    try:
        import uvloop
        uvloop.install()
        print(f"[corsair] uvloop {uvloop.__version__} installed as asyncio policy")
    except ImportError:
        print("[corsair] uvloop not available, using default asyncio loop")
    asyncio.run(main())


if __name__ == "__main__":
    run()
