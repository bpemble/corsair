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
from .utils import setup_logging
from .connection import IBKRConnection
from .market_data import MarketDataManager
from .position_manager import PortfolioState
from .constraint_checker import IBKRMarginChecker, ConstraintChecker
from .sabr import MultiExpirySABR
from .quote_engine import QuoteManager
from .fill_handler import FillHandler
from .risk_monitor import RiskMonitor
from .logging_utils import CSVLogger
from .weekend import friday_shutdown, monday_startup
from .snapshot import write_chain_snapshot
from . import daily_state
from .watchdog import (
    safe_discover_and_subscribe,
    watchdog_loop,
    escalate_gateway_recreate,
    STARTUP_RETRY_BACKOFF_SEC,
    GATEWAY_RESTART_SETTLE_SEC,
    RECOVERY_FAILS_BEFORE_GATEWAY_RESTART,
)

CT = ZoneInfo("America/Chicago")

# Number of consecutive update_quotes() exceptions before we declare an
# exception storm and panic-cancel + kill. Each cycle is event-driven so
# 5 typically corresponds to ~a few seconds of zombie quoting.
QUOTE_ERROR_STORM_THRESHOLD = 5

logger = logging.getLogger(__name__)


def _validate_safety_config(cfg) -> None:
    """Range-check safety-critical config params at startup.

    Defense vector #4 / #15: a typo in the YAML (e.g. multiplier=100 instead
    of 50, or margin_kill_pct accidentally below margin_ceiling_pct) silently
    corrupts every margin/PnL calc downstream. Assert at startup so we crash
    loudly instead of trading on bad math.
    """
    p = cfg.product
    c = cfg.constraints
    k = cfg.kill_switch
    assert p.multiplier == 50, (
        f"FATAL: product.multiplier must be 50 (CME ETH FOP spec), got {p.multiplier}. "
        "Wrong multiplier silently corrupts margin and P&L by 2×."
    )
    assert 1 <= int(p.quote_size) <= 5, (
        f"FATAL: product.quote_size out of safe range [1,5], got {p.quote_size}"
    )
    assert 50_000 <= c.capital <= 2_000_000, (
        f"FATAL: constraints.capital out of safe range, got {c.capital}"
    )
    assert 0.20 <= c.margin_ceiling_pct <= 0.80, (
        f"FATAL: margin_ceiling_pct out of safe range, got {c.margin_ceiling_pct}"
    )
    assert 0.40 <= k.margin_kill_pct <= 0.95, (
        f"FATAL: margin_kill_pct out of safe range, got {k.margin_kill_pct}"
    )
    assert k.margin_kill_pct > c.margin_ceiling_pct, (
        f"FATAL: margin_kill_pct ({k.margin_kill_pct}) must be > "
        f"margin_ceiling_pct ({c.margin_ceiling_pct})"
    )
    assert 0.5 <= float(c.delta_ceiling) <= 20.0, (
        f"FATAL: delta_ceiling out of safe range, got {c.delta_ceiling}"
    )
    assert 1.0 <= float(k.delta_kill) <= 50.0, (
        f"FATAL: delta_kill out of safe range, got {k.delta_kill}"
    )
    assert float(k.delta_kill) > float(c.delta_ceiling), (
        f"FATAL: delta_kill ({k.delta_kill}) must be > delta_ceiling ({c.delta_ceiling})"
    )
    logger.info(
        "✓ Safety config validated: mult=%d cap=$%d ceiling=%.0f%% kill=%.0f%% "
        "delta_ceil=%.1f delta_kill=%.1f",
        p.multiplier, c.capital, c.margin_ceiling_pct * 100,
        k.margin_kill_pct * 100, c.delta_ceiling, k.delta_kill,
    )


def _reconcile_positions(portfolio, ib, account_id: str) -> list:
    """Compare in-memory portfolio against IBKR's authoritative position view.

    Defense vector #9: catches the 2026-04-08 failure mode where order-status
    routing was broken and fills accumulated invisibly. Returns a list of
    (strike, expiry, right) keys that disagree; empty list = clean.
    """
    ours: dict = {}
    for p in portfolio.positions:
        if p.quantity != 0:
            ours[(float(p.strike), p.expiry, p.put_call)] = int(p.quantity)
    theirs: dict = {}
    for ib_pos in ib.positions(account_id):
        c = ib_pos.contract
        if c.symbol != "ETHUSDRR" or c.secType != "FOP":
            continue
        if not c.right or c.right not in ("C", "P"):
            continue
        qty = int(ib_pos.position)
        if qty != 0:
            theirs[(float(c.strike), c.lastTradeDateOrContractMonth, c.right)] = qty
    mismatches = []
    for k in set(ours.keys()) | set(theirs.keys()):
        if ours.get(k, 0) != theirs.get(k, 0):
            mismatches.append((k, ours.get(k, 0), theirs.get(k, 0)))
    return mismatches


def _session_day(now_utc: datetime, reset_hour_ct: int) -> date:
    """Return the CME session date for `now_utc`. Day rolls at reset_hour_ct CT."""
    now_ct = now_utc.astimezone(CT)
    if now_ct.hour >= reset_hour_ct:
        return (now_ct + timedelta(days=1)).date()
    return now_ct.date()


def _session_start_utc(now_utc: datetime, reset_hour_ct: int) -> datetime:
    """Return the wall-clock start of the current CME session as a UTC datetime.

    The CME session that contains `now_utc` runs from reset_hour_ct CT on
    one day to reset_hour_ct CT on the next. This is the lower bound for
    the replay-missed-executions filter.
    """
    now_ct = now_utc.astimezone(CT)
    if now_ct.hour >= reset_hour_ct:
        start_ct = now_ct.replace(hour=reset_hour_ct, minute=0, second=0, microsecond=0)
    else:
        start_ct = (now_ct - timedelta(days=1)).replace(
            hour=reset_hour_ct, minute=0, second=0, microsecond=0,
        )
    return start_ct.astimezone(timezone.utc)



def _get_todays_expiries(positions):
    """Return set of expiry strings that expire today."""
    today = datetime.now().strftime("%Y%m%d")
    return {p.expiry for p in positions if p.expiry == today}



async def main():
    # ── 1. Load config ────────────────────────────────────────────────
    config_path = os.environ.get("CORSAIR_CONFIG", "config/corsair_v2_config.yaml")
    config = load_config(config_path)
    setup_logging(config.logging)
    _validate_safety_config(config)

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

    sabr = MultiExpirySABR(config)
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
        logger.critical("GATEWAY DISCONNECT — panic cancelling all quotes")
        # Use panic_cancel (reqGlobalCancel) instead of the per-order
        # walk: a single message is faster and more likely to reach
        # IBKR before the socket fully tears down. Worst-case bad-fill
        # exposure on disconnect is determined by how fast we can get
        # the book empty here.
        try:
            quotes.panic_cancel()
        except Exception as e:
            logger.error("panic_cancel on disconnect failed: %s", e)
        # Tag the kill as disconnect-induced so the watchdog can clear it
        # after a successful reconnect.
        risk.kill("Gateway disconnect", source="disconnect")

    conn.set_disconnect_callback(on_disconnect)

    # ── 5. Cancel any stale orders from previous runs ───────────────
    # On clientId=0 the openTrades cache is populated asynchronously after
    # reqAutoOpenOrders + reqOpenOrders complete during connect. Wait briefly
    # so the cache reflects EVERY orphan that's still resting on IBKR's books
    # — otherwise we miss most of them and the new session immediately hits
    # IBKR's per-contract working-order limit (Error 201, "minimum of 15
    # orders working on either the buy or sell side for this particular
    # contract").
    await asyncio.sleep(3)

    cancelled = 0
    # Only cancel orders that are actually in a cancellable state. ib_insync's
    # openTrades() can return trades whose terminal status (Filled, Cancelled,
    # Inactive) hasn't been pruned yet from the local cache, and blindly
    # cancelling those produces "Error 161: Cancel attempted when order is not
    # in a cancellable state" noise in the logs.
    _CANCELLABLE = {"PendingSubmit", "PreSubmitted", "Submitted", "ApiPending"}
    snapshot = list(ib.openTrades())
    logger.info("Stale-order sweep: %d trades visible in openTrades cache", len(snapshot))
    for trade in snapshot:
        ref = getattr(trade.order, "orderRef", "") or ""
        if not ref.startswith("corsair2"):
            continue
        status = getattr(getattr(trade, "orderStatus", None), "status", "") or ""
        if status not in _CANCELLABLE:
            continue
        try:
            ib.cancelOrder(trade.order)
            cancelled += 1
        except Exception:
            pass
    if cancelled:
        logger.info("Cancelled %d stale orders from previous run", cancelled)
        # Give IBKR time to actually process the cancels and free up the
        # per-contract working-order slots before we start placing new ones.
        await asyncio.sleep(5)

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
    sabr.set_expiries(market_data.state.expiries)

    # Force-subscribe market data for any seeded position whose strike sits
    # outside the initial ATM±N discovery window. Without this, off-window
    # positions get delta=theta=0 in refresh_greeks() forever and silently
    # understate header risk.
    try:
        await market_data.ensure_position_subscribed(portfolio.positions)
    except Exception as e:
        logger.warning("ensure_position_subscribed at startup failed: %s", e)


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
    last_reconcile = datetime.min
    reconcile_interval_sec = float(getattr(getattr(config, "operations", object()),
                                           "reconcile_interval_sec", 60.0))
    last_depth_rotation = datetime.min
    last_full_refresh = _time.monotonic()
    last_snapshot_write = _time.monotonic()
    depth_rotation_interval = float(getattr(config.quoting, "depth_rotation_interval_sec", 2.0))
    last_session_day: Optional[date] = None
    last_daily_state_save = 0.0
    last_friday_shutdown_date: Optional[date] = None
    weekend_paused = False
    reset_hour_ct = int(getattr(getattr(config, "operations", object()),
                                "daily_reset_hour_ct", 17))
    weekend_flatten = bool(getattr(getattr(config, "operations", object()),
                                   "weekend_flatten", True))
    fallback_interval = config.quoting.refresh_interval_ms / 1000.0
    batch_window_sec = float(getattr(config.quoting, "tick_batch_window_sec", 0.005))
    snapshot_interval = float(getattr(config.quoting, "snapshot_write_interval_sec", 1.0))

    # Restore daily counters from disk if the persisted session day still
    # matches the current CME session. This is what makes Spread Capture and
    # Realized P&L survive a `docker compose up -d --build corsair`.
    _startup_session_day = _session_day(datetime.now(tz=timezone.utc), reset_hour_ct)
    _persisted = daily_state.load(_startup_session_day)
    if _persisted is not None:
        portfolio.fills_today = int(_persisted.get("fills_today", 0))
        portfolio.spread_capture_today = float(_persisted.get("spread_capture_today", 0.0))
        portfolio.spread_capture_mid_today = float(_persisted.get("spread_capture_mid_today", 0.0))
        portfolio.daily_pnl = float(_persisted.get("daily_pnl", 0.0))
        portfolio.realized_pnl_persisted = float(_persisted.get("realized_pnl", 0.0))
        # Restore the dedup set so the replay path doesn't double-count
        # fills already attributed in a prior process within the same session.
        for eid in _persisted.get("seen_exec_ids", []):
            fills._seen_exec_ids.add(eid)
        # Suppress the first-iteration reset_daily() that would otherwise
        # wipe what we just restored.
        last_session_day = _startup_session_day
        logger.info(
            "daily_state restored: fills=%d spread=$%.2f spread_mid=$%.2f "
            "realized=$%.2f seen_execs=%d (session %s)",
            portfolio.fills_today, portfolio.spread_capture_today,
            portfolio.spread_capture_mid_today, portfolio.realized_pnl_persisted,
            len(fills._seen_exec_ids), _startup_session_day,
        )

    # Replay any executions that happened during a downtime window in the
    # current CME session. Without this, every fill that lands while
    # corsair is restarting / reconnecting is silently invisible to
    # fill_handler — the position appears (via seed_from_ibkr) but
    # fills_today / spread_capture / daily_pnl never reflect it. This is
    # what was happening before 2026-04-09 ~17:00: ~95k Error 104s and a
    # session of fills with fills_today=0.
    _session_start = _session_start_utc(datetime.now(tz=timezone.utc), reset_hour_ct)
    try:
        await fills.replay_missed_executions(_session_start)
    except Exception as e:
        logger.warning("replay_missed_executions at startup failed: %s", e)

    logger.info(
        "Entering event-driven quote loop (batch=%.0fms, fallback=%.1fs, snapshot=%.1fs)",
        batch_window_sec * 1000, fallback_interval, snapshot_interval,
    )

    # Launch the watchdog in the background. Detects connection loss and
    # silent gateway hangs; auto-reconnects with backoff. Survives the
    # lifetime of the main loop and is cancelled on shutdown.
    watchdog_task = asyncio.create_task(
        watchdog_loop(conn, market_data, quotes, portfolio, margin, risk,
                      sabr, config.account.account_id, shutdown_event,
                      fills=fills,
                      session_start_fn=lambda: _session_start_utc(
                          datetime.now(tz=timezone.utc), reset_hour_ct
                      )),
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
            daily_state.clear()
            last_session_day = session_day
            logger.info("New CME session day: %s (5pm CT rollover)", session_day)
            for expiry in _get_todays_expiries(portfolio.positions):
                settlement = market_data.state.underlying_price
                pnl, futures = portfolio.handle_expiry(expiry, settlement)
                logger.info("Expiry settlement: %s, P&L=$%.0f, Futures=%d", expiry, pnl, futures)

        # Weekend protocol
        if weekend_flatten:
            is_friday_close = (now_ct.weekday() == 4
                               and (now_ct.hour > 15
                                    or (now_ct.hour == 15 and now_ct.minute >= 55)))
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

        # Periodic position reconciliation (defense vector #9). Catches the
        # 2026-04-08 silent-fill failure mode where order-status routing
        # broke and our portfolio diverged from IBKR's authoritative view.
        # On any disagreement, kill — we cannot trust risk numbers computed
        # against a stale position book.
        if (now - last_reconcile).total_seconds() >= reconcile_interval_sec:
            try:
                mismatches = _reconcile_positions(portfolio, ib, config.account.account_id)
                if mismatches:
                    logger.critical(
                        "POSITION RECONCILIATION MISMATCH (%d): %s — killing",
                        len(mismatches), mismatches[:5],
                    )
                    try:
                        quotes.panic_cancel()
                    except Exception as ee:
                        logger.error("panic_cancel after reconcile mismatch failed: %s", ee)
                    risk.kill("Position reconciliation mismatch", source="reconciliation")
            except Exception as e:
                logger.warning("Position reconciliation check failed: %s", e)
            last_reconcile = now

        # Rotate depth subscriptions
        if (now - last_depth_rotation).total_seconds() >= depth_rotation_interval:
            try:
                market_data.rotate_depth_subscriptions()
            except Exception as e:
                logger.warning("depth rotation error: %s", e)
            last_depth_rotation = now

        # SABR recalibration (hoisted out of update_quotes so it doesn't
        # land between the tick handler and placeOrder, which was inflating
        # TTT by several ms per cycle). The method's own timer + forward-move
        # gates decide whether to actually run.
        try:
            quotes.maybe_recal_sabr()
        except Exception as e:
            logger.warning("SABR recal error: %s", e)

        # ── Snapshot + daily-state persistence ────────────────────────
        # Runs even when killed/paused so Docker's healthcheck (which reads
        # chain_snapshot.json age) doesn't falsely declare the container
        # unhealthy and restart a process that's otherwise alive and
        # recoverable. Previously these lived after the quote phase, so
        # the `continue` below starved the snapshot writer for hours.
        mono = _time.monotonic()
        if (mono - last_snapshot_write) >= snapshot_interval:
            try:
                write_chain_snapshot(market_data, quotes, portfolio, sabr,
                                     margin, ib, config.account.account_id, config)
            except Exception as e:
                logger.warning("Snapshot write error: %s", e)
            last_snapshot_write = mono

        if (mono - last_daily_state_save) >= 1.0:
            try:
                daily_state.save(
                    session_day,
                    fills_today=portfolio.fills_today,
                    spread_capture_today=portfolio.spread_capture_today,
                    spread_capture_mid_today=portfolio.spread_capture_mid_today,
                    daily_pnl=portfolio.daily_pnl,
                    realized_pnl=portfolio.realized_pnl_persisted,
                    seen_exec_ids=list(fills._seen_exec_ids),
                )
            except Exception as e:
                logger.warning("daily_state save error: %s", e)
            last_daily_state_save = mono

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
                quotes.consecutive_quote_errors = 0
            except Exception as e:
                quotes.consecutive_quote_errors += 1
                logger.error("Quote update error (#%d): %s",
                             quotes.consecutive_quote_errors, e, exc_info=True)
                # Storm guard: if update_quotes is throwing every cycle
                # we are zombie-quoting (the AssertionError / Error 103
                # mode hit on 2026-04-09 morning). Cancel everything via
                # reqGlobalCancel and kill the engine — the watchdog will
                # pick up the kill and try a recovery cycle. Threshold is
                # deliberately low (5 cycles ~ a few seconds) so we don't
                # bleed bad fills while erroring on every modify.
                if quotes.consecutive_quote_errors >= QUOTE_ERROR_STORM_THRESHOLD:
                    logger.critical(
                        "Quote loop exception storm (#%d) — panic cancelling and killing",
                        quotes.consecutive_quote_errors,
                    )
                    try:
                        quotes.panic_cancel()
                    except Exception as ee:
                        logger.error("panic_cancel after storm failed: %s", ee)
                    risk.kill("Quote loop exception storm", source="exception_storm")
                    quotes.consecutive_quote_errors = 0

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
