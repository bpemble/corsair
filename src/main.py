"""Corsair v2 — Main Event Loop.

Automated options market-making system for CME ETHUSDRR.
Quotes two-sided markets on OTM calls, earns spread per fill,
manages risk through SPAN margin and portfolio theta constraints.
"""

import asyncio
import json
import logging
import os
import signal
import time as _time
from typing import Optional
from datetime import datetime, date, timezone, timedelta
from zoneinfo import ZoneInfo

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

CT = ZoneInfo("America/Chicago")


def _session_day(now_utc: datetime, reset_hour_ct: int) -> date:
    """Return the CME session date for `now_utc`. Day rolls at reset_hour_ct CT."""
    now_ct = now_utc.astimezone(CT)
    if now_ct.hour >= reset_hour_ct:
        return (now_ct + timedelta(days=1)).date()
    return now_ct.date()

logger = logging.getLogger(__name__)

SNAPSHOT_PATH = "data/chain_snapshot.json"
SNAPSHOT_TMP = "data/chain_snapshot.json.tmp"


def _read_account_state(ib, account_id: str) -> dict:
    """Pull cash, margin, and P&L summary from IBKR account values."""
    out = {
        "account_id": account_id,
        "cash": 0.0,
        "net_liq": 0.0,
        "init_margin": 0.0,
        "maint_margin": 0.0,
        "buying_power": 0.0,
        "unrealized_pnl": 0.0,
        "realized_pnl": 0.0,
    }
    tag_map = {
        "TotalCashValue": "cash",
        "NetLiquidation": "net_liq",
        "InitMarginReq": "init_margin",
        "MaintMarginReq": "maint_margin",
        "BuyingPower": "buying_power",
        "UnrealizedPnL": "unrealized_pnl",
        "RealizedPnL": "realized_pnl",
    }
    try:
        for v in ib.accountValues(account_id):
            if v.currency != "USD" and v.currency != "":
                continue
            key = tag_map.get(v.tag)
            if key:
                try:
                    out[key] = float(v.value)
                except (ValueError, TypeError):
                    pass
    except Exception:
        pass
    return out


def write_chain_snapshot(market_data, quotes, portfolio, sabr, margin, ib, account_id, config):
    """Write a JSON snapshot of the full option chain for the dashboard."""
    state = market_data.state
    if state.underlying_price <= 0:
        return

    strikes_data = {}
    active_quotes = quotes.get_active_quotes()

    for strike in state.get_all_strikes():
        opt = state.get_option(strike)
        if opt is None:
            continue

        # Look up our bid/ask from active orders
        bid_info = active_quotes.get((strike, "BUY"))
        ask_info = active_quotes.get((strike, "SELL"))

        our_bid = bid_info["price"] if bid_info else None
        our_ask = ask_info["price"] if ask_info else None
        bid_live = bid_info["status"] == "Submitted" if bid_info else False
        ask_live = ask_info["status"] == "Submitted" if ask_info else False

        # Theo from SABR if calibrated
        theo = None
        if sabr.last_calibration is not None:
            try:
                theo = round(sabr.get_theo(strike), 2)
            except Exception:
                pass

        # Position at this strike
        pos = 0
        for p in portfolio.positions:
            if p.strike == strike and p.expiry == state.front_month_expiry:
                pos += p.quantity

        # Status
        if bid_live or ask_live:
            status = "quoting"
        elif bid_info or ask_info:
            status = "pending"
        else:
            status = "idle"

        # Get the real incumbent bid/ask from the quote engine's tracking
        inc_bid, inc_ask = quotes.get_incumbent(strike)
        if inc_bid <= 0:
            inc_bid = opt.bid
        if inc_ask <= 0:
            inc_ask = opt.ask

        strikes_data[str(int(strike))] = {
            "market_bid": inc_bid,
            "market_ask": inc_ask,
            "raw_bid": opt.bid,
            "raw_ask": opt.ask,
            "our_bid": our_bid,
            "our_ask": our_ask,
            "bid_live": bid_live,
            "ask_live": ask_live,
            "theo": theo,
            "delta": round(opt.delta, 4),
            "iv": round(opt.iv, 4) if opt.iv else 0.0,
            "volume": opt.volume,
            "open_interest": opt.open_interest,
            "position": pos,
            "status": status,
        }

    # Per-position detail with MtM P&L
    positions_detail = []
    for p in portfolio.positions:
        opt = state.get_option(p.strike, p.expiry, p.put_call)
        mark = p.current_price
        if opt and opt.bid > 0 and opt.ask > 0:
            mark = (opt.bid + opt.ask) / 2
        mult = config.product.multiplier
        unrealized = (mark - p.avg_fill_price) * p.quantity * mult
        positions_detail.append({
            "strike": p.strike,
            "expiry": p.expiry,
            "right": p.put_call,
            "qty": p.quantity,
            "avg_price": round(p.avg_fill_price, 2),
            "mark": round(mark, 2),
            "unrealized_pnl": round(unrealized, 2),
            "delta": round(p.delta, 4),
            "theta": round(p.theta, 2),
        })

    snapshot = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "underlying_price": state.underlying_price,
        "atm_strike": state.atm_strike,
        "front_month_expiry": state.front_month_expiry,
        "account": _read_account_state(ib, account_id),
        "portfolio": {
            "net_delta": round(portfolio.net_delta, 4),
            "net_theta": round(portfolio.net_theta, 2),
            "net_vega": round(portfolio.net_vega, 2),
            "net_gamma": round(portfolio.net_gamma, 6),
            "total_contracts": portfolio.gross_positions,
            "margin": margin.get_current_margin(),
            "fills_today": portfolio.fills_today,
            "spread_capture": round(portfolio.spread_capture_today, 2),
            "positions": positions_detail,
        },
        "strikes": strikes_data,
    }

    try:
        with open(SNAPSHOT_TMP, "w") as f:
            json.dump(snapshot, f, indent=2)
        os.replace(SNAPSHOT_TMP, SNAPSHOT_PATH)
    except Exception as e:
        logger.warning("Failed to write chain snapshot: %s", e)


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

    # ── 2. Connect to IBKR ───────────────────────────────────────────
    conn = IBKRConnection(config)
    if not await conn.connect():
        logger.critical("Failed to connect to IBKR Gateway. Exiting.")
        return

    ib = conn.ib

    # ── 3. Initialize components ──────────────────────────────────────
    csv_logger = CSVLogger(config)
    market_data = MarketDataManager(ib, config)
    portfolio = PortfolioState(config)

    # Reconcile any existing ETHUSDRR option positions from IBKR
    seeded = portfolio.seed_from_ibkr(ib, config.account.account_id)
    if seeded:
        logger.info("Reconciled %d existing ETHUSDRR option position(s) from IBKR", seeded)

    margin = IBKRMarginChecker(ib, config, market_data, portfolio)
    sabr = SABRSurface(config)
    constraint_checker = ConstraintChecker(margin, portfolio, config)
    quotes = QuoteManager(ib, config, market_data, sabr, constraint_checker,
                          csv_logger=csv_logger)
    risk = RiskMonitor(portfolio, margin, quotes, csv_logger, config)
    fills = FillHandler(ib, portfolio, margin, quotes, market_data, csv_logger, config)

    # ── 4. Register disconnect callback ───────────────────────────────
    def on_disconnect():
        logger.critical("GATEWAY DISCONNECT — cancelling all quotes")
        quotes.cancel_all_quotes()
        risk.kill("Gateway disconnect")

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

    # ── 6. Subscribe to market data ───────────────────────────────────
    logger.info("Discovering option chain and subscribing to market data...")
    await market_data.discover_and_subscribe()

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
        opt = market_data.state.get_option(strike)
        if opt and opt.bid > 0:
            quotes._incumbent_bid[strike] = opt.bid
        if opt and opt.ask > 0:
            quotes._incumbent_ask[strike] = opt.ask
    logger.info("Incumbent tracker seeded with %d strikes", len(quotes._incumbent_bid))

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
            dirty_strikes = None  # signal full refresh
            last_full_refresh = mono
        else:
            # Convert OptionKeys to strike floats; expand on underlying tick
            if underlying_dirty:
                dirty_strikes = None
                last_full_refresh = mono
            elif dirty_keys:
                dirty_strikes = {k[0] for k in dirty_keys}
            else:
                dirty_strikes = set()  # nothing to do this cycle

        if dirty_strikes is None or dirty_strikes:
            try:
                quotes.update_quotes(portfolio, dirty_strikes=dirty_strikes)
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
