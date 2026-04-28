"""Corsair v2 — Main Event Loop.

Automated options market-making system for CME futures options.
Product selected via config.product.underlying_symbol (ETHUSDRR at the
time of writing). Quotes two-sided markets on OTM options, earns spread
per fill, manages risk through SPAN margin and portfolio theta constraints.
"""

import asyncio
import logging
import os
import signal
import time as _time
from datetime import datetime, date, timezone, timedelta
from zoneinfo import ZoneInfo

from . import ib_insync_patch as _ib_insync_patch
_ib_insync_patch.apply()  # must run before any IB instance is created

from .config import load_config, make_product_config
from .utils import setup_logging
from .connection import IBKRConnection
from .market_data import MarketDataManager
from .position_manager import PortfolioState
from .constraint_checker import IBKRMarginChecker, ConstraintChecker, MarginCoordinator
from .sabr import MultiExpirySABR
from .quote_engine import QuoteManager
from .fill_handler import FillHandler
from .risk_monitor import RiskMonitor, _resolve_daily_halt_threshold
from .hedge_manager import HedgeManager, HedgeFanout
from .operational_kills import OperationalKillMonitor
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

    Walks every entry in ``cfg.products`` so a bad multiplier/quote_size on
    a secondary product is caught the same as on the primary.
    """
    c = cfg.constraints
    k = cfg.kill_switch
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
    assert getattr(cfg, "products", None), "FATAL: config.products is empty"
    for entry in cfg.products:
        p = entry.product
        assert 1 <= int(p.multiplier) <= 100_000, (
            f"FATAL: {entry.name}.product.multiplier out of safe range "
            f"[1,100000], got {p.multiplier}. Wrong multiplier silently "
            f"corrupts margin and P&L proportionally."
        )
        assert 1 <= int(p.quote_size) <= 5, (
            f"FATAL: {entry.name}.product.quote_size out of safe range [1,5], "
            f"got {p.quote_size}"
        )
    products_summary = ", ".join(
        f"{e.name}(mult={e.product.multiplier})" for e in cfg.products)
    logger.info(
        "✓ Safety config validated: products=[%s] cap=$%d ceiling=%.0f%% "
        "kill=%.0f%% delta_ceil=%.1f delta_kill=%.1f",
        products_summary, c.capital, c.margin_ceiling_pct * 100,
        k.margin_kill_pct * 100, c.delta_ceiling, k.delta_kill,
    )


def _reconcile_positions(portfolio, ib, account_id: str, config) -> list:
    """Compare in-memory portfolio against IBKR's authoritative position view.

    Defense vector #9: catches the 2026-04-08 failure mode where order-status
    routing was broken and fills accumulated invisibly. Returns a list of
    (product, strike, expiry, right) keys that disagree; empty list = clean.

    Multi-product: walks ib.positions(account) once and accepts any position
    whose contract.symbol matches a product the portfolio knows about (i.e.
    has been registered via portfolio.register_product). If we used the
    primary product's underlying as the sole filter, HG positions would be
    silently ignored — which is the exact bug that let 145 long HG puts
    accumulate invisibly between 2026-04-13 evening and 2026-04-14 morning.
    """
    known_products = set(portfolio.products())
    ours: dict = {}
    for p in portfolio.positions:
        if p.quantity != 0:
            ours[(p.product, float(p.strike), p.expiry, p.put_call)] = int(p.quantity)
    theirs: dict = {}
    for ib_pos in ib.positions(account_id):
        c = ib_pos.contract
        if c.symbol not in known_products or c.secType != "FOP":
            continue
        if not c.right or c.right not in ("C", "P"):
            continue
        qty = int(ib_pos.position)
        if qty != 0:
            theirs[(c.symbol, float(c.strike), c.lastTradeDateOrContractMonth, c.right)] = qty
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

    # ── 3. Shared infrastructure ──────────────────────────────────────
    csv_logger = CSVLogger(config)
    portfolio = PortfolioState(config)
    # MarginCoordinator owns IBKR account-value comm and cross-product
    # synthetic→IBKR scale calibration. Per-product engines register here
    # so their public margin APIs return COMBINED across-product numbers.
    margin_coord = MarginCoordinator(ib, config.account.account_id,
                                     csv_logger=csv_logger)

    # ── 4. Cancel stale orders from previous runs ─────────────────────
    # On clientId=0 the openTrades cache is populated asynchronously after
    # reqAutoOpenOrders + reqOpenOrders complete during connect. Wait
    # briefly so the cache reflects EVERY orphan still resting on IBKR's
    # books — otherwise we miss most and the new session immediately hits
    # the per-contract working-order limit.
    await asyncio.sleep(3)
    cancelled = 0
    _CANCELLABLE = {"PendingSubmit", "PreSubmitted", "Submitted", "ApiPending"}
    open_snapshot = list(ib.openTrades())
    logger.info("Stale-order sweep: %d trades visible in openTrades cache",
                len(open_snapshot))
    for trade in open_snapshot:
        ref = getattr(trade.order, "orderRef", "") or ""
        if not ref.startswith("corsair"):
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
        await asyncio.sleep(5)

    # ── 5. Per-product engines ────────────────────────────────────────
    # Each entry in config.products gets its own MarketDataManager, SABR,
    # IBKRMarginChecker, ConstraintChecker, QuoteManager, and FillHandler.
    # They share the single PortfolioState and MarginCoordinator (margin/
    # delta/theta budgets are portfolio-level). engines[0] is the
    # "primary" — distinguished only in that the watchdog monitors its
    # tick stream for freshness, its snapshot is the docker healthcheck
    # target, and its discovery has the aggressive retry+escalate path
    # (secondary failures just skip that product without blocking boot).
    engines: list = []
    for i, product_entry in enumerate(config.products):
        is_primary = (i == 0)
        pcfg = make_product_config(config, product_entry)
        logger.info("Setting up product engine: %s (primary=%s)",
                    pcfg.name, is_primary)
        md = MarketDataManager(ib, pcfg, csv_logger=csv_logger)
        sabr = MultiExpirySABR(pcfg, csv_logger=csv_logger)
        portfolio.register_product(
            pcfg.product.underlying_symbol,
            pcfg.product.multiplier,
            md, sabr,
        )

        # Discovery. Primary gets the full retry+escalate ladder — an
        # IB Gateway that's up but wedged inside the discovery path has
        # to be recreated. Secondaries just try once; a failure logs and
        # skips the product without blocking the rest of the boot.
        if is_primary:
            startup_attempt = 0
            discovery_fails = 0
            while not await safe_discover_and_subscribe(md):
                discovery_fails += 1
                if discovery_fails >= RECOVERY_FAILS_BEFORE_GATEWAY_RESTART:
                    logger.critical(
                        "Startup: %d consecutive discovery failures — "
                        "escalating to gateway recreate", discovery_fails,
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
                    "Startup discovery failed (attempt %d) — bouncing "
                    "connection and retrying in %ds", startup_attempt, wait,
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
                logger.info("Startup discovery succeeded after %d retries",
                            startup_attempt)
        else:
            if not await safe_discover_and_subscribe(md):
                logger.warning("Product %s discovery failed, skipping", pcfg.name)
                continue

        # Let initial ticks settle before building SABR / placing orders.
        await asyncio.sleep(5 if is_primary else 3)
        sabr.set_expiries(md.state.expiries)

        margin = IBKRMarginChecker(ib, pcfg, md, portfolio, sabr=sabr,
                                   csv_logger=csv_logger)
        margin_coord.register(margin)
        # v1.4 §5 delta hedge manager. Constructed per-product since
        # each product has its own underlying future. No-op when
        # config.hedging.enabled is false or absent. Constructed BEFORE
        # ConstraintChecker so the effective-delta gating path can take a
        # reference to it (2026-04-27 — see CLAUDE.md §14). The
        # effective-delta toggle (constraints.effective_delta_gating, default
        # true) is read inside ConstraintChecker / RiskMonitor; wiring is
        # unconditional. CLAUDE.md §10 reconciliation required before live.
        hedge = HedgeManager(ib, pcfg, md, portfolio, csv_logger=csv_logger)
        cc = ConstraintChecker(margin, portfolio, pcfg, hedge_manager=hedge)
        quotes = QuoteManager(ib, pcfg, md, sabr, cc, csv_logger=csv_logger)
        # Back-reference so the trade-tape capture in market_data can look
        # up our resting state at print time.
        md.quotes = quotes
        # product_filter must be the IBKR underlying symbol (what
        # fill.contract.symbol returns). Before 2026-04-14 this was set
        # to option_symbol and silently dropped every fill.
        fills_eng = FillHandler(ib, portfolio, margin, quotes, md,
                                csv_logger, pcfg,
                                product_filter=pcfg.product.underlying_symbol,
                                hedge_manager=hedge)

        # Thread 3 Layer C: wire the per-product hedge handle into the
        # quote engine so the burst-pull path can raise the priority-
        # drain signal synchronously (HARD REQUIREMENT, brief §3).
        try:
            quotes.hedge_manager = hedge
        except Exception:
            logger.exception("failed to wire hedge_manager into quotes")

        # Thread 3 deployment runbook Phase 0: resolve a tradeable
        # hedge contract that skips IBKR's near-expiration lockout
        # window. Awaited at startup so the resolved contract is
        # available before any rebalance fires. Failure here logs and
        # falls back to the options engine's underlying — observe-mode
        # behavior is unaffected; execute-mode would re-encounter the
        # lockout if fallback runs and front-month is locked.
        try:
            await hedge.resolve_hedge_contract()
        except Exception:
            logger.exception("hedge.resolve_hedge_contract failed [%s]",
                             pcfg.name)

        # §10 boot reconciliation (added 2026-04-27 after a double-hedge
        # incident: 3 restarts each force_flat'd a fresh -5 SELL because
        # in-memory hedge_qty resets to 0, leaving IBKR at -10 vs local -5
        # and effective_delta gating reading a stale +0.50 instead of the
        # true -4.50). Must run BEFORE the first risk_monitor.check() so
        # hedge_qty reflects ground truth before the delta kill is evaluated.
        try:
            hedge.reconcile_from_ibkr()
        except Exception:
            logger.exception("hedge.reconcile_from_ibkr failed [%s] — "
                             "hedge_qty will start at 0; expect delta_kill "
                             "if options_delta > %g",
                             pcfg.name,
                             getattr(pcfg.kill_switch, "delta_kill", 5.0))

        engines.append({
            "name": pcfg.name,
            "config": pcfg,
            "md": md,
            "sabr": sabr,
            "margin": margin,
            "quotes": quotes,
            "fills": fills_eng,
            "hedge": hedge,
        })
        logger.info(
            "Product %s ready: underlying=%.4f, ATM=%.4f, %d options, expiry=%s",
            pcfg.name, md.state.underlying_price, md.state.atm_strike,
            len(md.state.options), md.state.front_month_expiry,
        )

    if not engines:
        raise RuntimeError("No products successfully initialized — aborting")

    # ── 6. Primary aliases (used by watchdog, disconnect cb, risk) ────
    primary = engines[0]
    market_data = primary["md"]
    sabr = primary["sabr"]
    quotes = primary["quotes"]
    margin = primary["margin"]
    fills = primary["fills"]
    primary_config = primary["config"]

    # ── 7. Risk monitor ───────────────────────────────────────────────
    # v1.4 §6.1/§6.2: flatten_callback iterates every registered engine so
    # a flatten-type kill closes positions across products, not just the
    # primary. hedge_manager is wired after construction if the v1.4
    # hedging block is present in config.
    def _flatten_all_engines(reason: str) -> None:
        for eng in engines:
            try:
                eng["quotes"].flatten_all_positions(portfolio, reason=reason)
            except Exception as e:
                logger.error("flatten_all_positions %s failed: %s",
                             eng["name"], e)

    # Multi-product hedge dispatcher — see HedgeFanout docstring.
    hedge_fanout = HedgeFanout([eng["hedge"] for eng in engines])

    # v1.4 §6.1: flatten_callback must also flatten the futures hedge
    # in addition to options. Compose the options-flatten and
    # hedge-flatten paths.
    #
    # Ordering: options first, then hedge. Both steps submit orders and
    # return immediately (IOC limits); actual fills arrive asynchronously
    # via execDetailsEvent. In observe-mode hedging (Stage 0 default) this
    # is correct because the hedge "flatten" just zeroes local state
    # without real futures orders. In execute mode, there IS a race — if
    # the options flatten fills before the hedge order is sent, the net
    # book may briefly be mis-hedged. That race is tolerated under the
    # "both must flatten per spec §6.1" rule; post-flatten reconciliation
    # against ib.positions() is a v1.5 item that will close this gap.
    def _flatten_all_engines_with_hedge(reason: str) -> None:
        _flatten_all_engines(reason)
        hedge_fanout.flatten_on_halt(reason)

    risk = RiskMonitor(
        portfolio, margin_coord, quotes, csv_logger, primary_config,
        flatten_callback=_flatten_all_engines_with_hedge,
        hedge_manager=hedge_fanout,
    )

    # Inject risk monitor into every engine's fill handler so the per-fill
    # P&L halt check fires on every live execution (v1.4 §6.1).
    for eng in engines:
        try:
            eng["fills"].risk_monitor = risk
        except Exception:
            logger.exception("failed to inject risk_monitor into %s fills",
                             eng["name"])

    # v1.4 §7: operational kill-switch monitor. Sampled from the main
    # loop; fires via risk.kill(source="operational"). Sticky.
    op_kills = OperationalKillMonitor(engines, portfolio, risk, config)

    # ── 8. Kill-switch self-test (v1.4 §9.4 boot gate) ────────────────
    # Exercise every induced sentinel through fresh RiskMonitor
    # instances with mock callbacks. Catches regressions in the
    # sentinel→kill→jsonl plumbing at boot rather than waiting for a
    # real breach to silently not-fire. Refuses to accept live orders
    # on failure. CORSAIR_SKIP_KILL_SELF_TEST=1 bypasses (logs CRITICAL).
    from .startup_checks import run_kill_switch_self_test
    if not run_kill_switch_self_test(
        portfolio, margin_coord, quotes, csv_logger, primary_config,
    ):
        logger.critical(
            "Aborting boot: kill-switch self-test failed. Fix the kill "
            "path and restart. Container will exit with code 1."
        )
        raise SystemExit(1)

    # ── 9. Disconnect callback ────────────────────────────────────────
    def on_disconnect():
        logger.critical("GATEWAY DISCONNECT — panic cancelling all quotes")
        # Emit reconnects.jsonl event (v1.4 §9.5) with pending order
        # count BEFORE cancel_all clears local state.
        try:
            pending_pre = sum(1 for _ in ib.openTrades())
            csv_logger.log_paper_reconnect(
                event="disconnect",
                pending_orders_pre=pending_pre,
                detail="conn.disconnect_callback",
            )
        except Exception:
            pass
        # panic_cancel (reqGlobalCancel) instead of per-order walks —
        # one message is faster and more likely to reach IBKR before the
        # socket fully tears down. Fan out across every engine's
        # QuoteManager because reqGlobalCancel clears ALL clients' orders
        # but we also want each engine to drop its local resting state
        # so reconnect reseeding doesn't see ghosts.
        for eng in engines:
            try:
                eng["quotes"].panic_cancel()
            except Exception as e:
                logger.error("panic_cancel on disconnect failed (%s): %s",
                             eng["name"], e)
        risk.kill("Gateway disconnect", source="disconnect")

    conn.set_disconnect_callback(on_disconnect)

    # ── 10. Seed portfolio + replay + ensure_position_subscribed ─────
    # Reseed now that every product is registered (one walk of ib.positions
    # for the whole account).
    try:
        seeded = portfolio.seed_from_ibkr(ib, config.account.account_id)
        logger.info("Seeded %d position(s) across %d product(s)",
                    seeded, len(engines))
    except Exception as e:
        logger.warning("seed_from_ibkr failed: %s", e)

    reset_hour_ct_early = int(getattr(getattr(config, "operations", object()),
                                      "daily_reset_hour_ct", 17))
    _session_start = _session_start_utc(datetime.now(tz=timezone.utc),
                                        reset_hour_ct_early)

    # Replay and force-subscribe per engine. Each engine's replay sees only
    # its own product's fills (filtered in FillHandler via product_filter);
    # off-window position subscriptions use each engine's own market_data.
    for eng in engines:
        try:
            replayed = await eng["fills"].replay_missed_executions(_session_start)
            if replayed:
                logger.info("Product %s: backfilled %d missed fill(s)",
                            eng["name"], replayed)
        except Exception as e:
            logger.warning("Product %s replay_missed_executions failed: %s",
                           eng["name"], e)
        try:
            own_positions = portfolio.positions_for_product(
                eng["config"].product.underlying_symbol)
            await eng["md"].ensure_position_subscribed(own_positions)
        except Exception as e:
            logger.warning("Product %s ensure_position_subscribed failed: %s",
                           eng["name"], e)


    # ── 11. Handle graceful shutdown ──────────────────────────────────
    shutdown_event = asyncio.Event()

    def handle_signal(sig):
        logger.info("Received signal %s, initiating shutdown...", sig)
        shutdown_event.set()

    loop = asyncio.get_event_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, lambda s=sig: handle_signal(s))

    # ── 12. Main loop ─────────────────────────────────────────────────
    last_greek_refresh = datetime.min
    last_reconcile = datetime.min
    reconcile_interval_sec = float(getattr(getattr(config, "operations", object()),
                                           "reconcile_interval_sec", 60.0))
    # v1.4 §9.5 paper-stream snapshot cadences.
    margin_snap_cadence = float(getattr(config.logging,
                                        "margin_snapshots_cadence_sec", 60.0))
    pnl_snap_cadence = float(getattr(config.logging,
                                     "pnl_snapshots_cadence_sec", 30.0))
    last_margin_snap = _time.monotonic() - margin_snap_cadence  # fire immediately
    last_pnl_snap = _time.monotonic() - pnl_snap_cadence
    last_depth_rotation = datetime.min
    last_recenter = datetime.min
    recenter_interval_sec = float(getattr(getattr(config, "operations", object()),
                                          "strike_recenter_interval_sec", 5.0))
    last_full_refresh = _time.monotonic()
    last_snapshot_write = _time.monotonic()
    depth_rotation_interval = float(getattr(config.quoting, "depth_rotation_interval_sec", 2.0))
    last_session_day: date | None = None
    last_daily_state_save = 0.0
    last_friday_shutdown_date: date | None = None
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
        _persisted_open_nlv = _persisted.get("session_open_nlv")
        if _persisted_open_nlv is not None:
            portfolio.session_open_nlv = float(_persisted_open_nlv)
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

    # Per-fill daily_state save: closes the 1-second window between the
    # main-loop save cadence and the next fill where a hard crash would
    # lose seen_exec_ids and cause the next boot's replay to double-
    # count those fills as new. Reads the current session_day on each
    # call so it remains correct across the 5pm CT rollover.
    def _save_daily_state_now() -> None:
        try:
            cur_day = _session_day(datetime.now(tz=timezone.utc),
                                   reset_hour_ct)
            daily_state.save(
                cur_day,
                fills_today=portfolio.fills_today,
                spread_capture_today=portfolio.spread_capture_today,
                spread_capture_mid_today=portfolio.spread_capture_mid_today,
                daily_pnl=portfolio.daily_pnl,
                realized_pnl=portfolio.realized_pnl_persisted,
                seen_exec_ids=list(fills._seen_exec_ids),
                session_open_nlv=portfolio.session_open_nlv,
            )
        except Exception as e:
            logger.warning("daily_state per-fill save error: %s", e)
    fills._save_state_cb = _save_daily_state_now

    logger.info(
        "Entering event-driven quote loop (batch=%.0fms, fallback=%.1fs, snapshot=%.1fs)",
        batch_window_sec * 1000, fallback_interval, snapshot_interval,
    )

    # Launch the watchdog in the background. Detects connection loss and
    # silent gateway hangs; auto-reconnects with backoff. Survives the
    # lifetime of the main loop and is cancelled on shutdown.
    # Pass the MarginCoordinator (not the per-product margin engine): its
    # invalidate_portfolio() forwards to every registered engine, so a
    # watchdog reseed evicts cross-product caches in one call.
    watchdog_task = asyncio.create_task(
        watchdog_loop(conn, market_data, quotes, portfolio, margin_coord, risk,
                      sabr, config.account.account_id, shutdown_event,
                      fills=fills,
                      session_start_fn=lambda: _session_start_utc(
                          datetime.now(tz=timezone.utc), reset_hour_ct
                      ),
                      csv_logger=csv_logger),
        name="watchdog",
    )

    while not shutdown_event.is_set():
        now = datetime.now()
        now_utc = datetime.now(tz=timezone.utc)
        now_ct = now_utc.astimezone(CT)
        session_day = _session_day(now_utc, reset_hour_ct)

        # Daily reset at 5pm CT (CME daily session boundary)
        if last_session_day != session_day:
            # Paper-trading EOD summary for the closing session, BEFORE the
            # portfolio reset wipes spread_capture_today. One file per
            # product (hg_spec_v1.3.md §17.4). Skip on first-run rollover
            # (last_session_day is None) — no prior session to summarize.
            # Margin extremes/kill-switch counters not yet tracked live —
            # emit as 0 for MVP; live tracking can be added when Stage 1
            # data shows it's needed.
            if last_session_day is not None:
                try:
                    from .daily_summary import write_daily_summary
                    for eng in engines:
                        write_daily_summary(
                            session_date=last_session_day,
                            paper_log_dir=csv_logger._paper_log_dir,
                            portfolio=portfolio,
                            margin_checker=eng["margin"],
                            multiplier=int(eng["config"].product.multiplier),
                        )
                except Exception as e:
                    logger.warning("paper EOD summary emit failed: %s", e)
            portfolio.reset_daily()
            daily_state.clear()
            # v1.4 §6.1: auto-clear daily P&L halt kill at session rollover
            # (gated on config.operations.auto_clear_daily_halt_on_session_rollover,
            # default True). Sticky risk/reconciliation/exception_storm kills
            # remain sticky — only source="daily_halt" is cleared here.
            auto_clear = bool(getattr(
                getattr(config, "operations", object()),
                "auto_clear_daily_halt_on_session_rollover", True))
            if auto_clear and risk.clear_daily_halt():
                logger.info(
                    "Session rollover: daily P&L halt cleared, quoting resumes "
                    "on next quote cycle")
            last_session_day = session_day
            logger.info("New CME session day: %s (5pm CT rollover)", session_day)
            for expiry in _get_todays_expiries(portfolio.positions):
                settlement = market_data.state.underlying_price
                pnl, futures = portfolio.handle_expiry(expiry, settlement)
                logger.info("Expiry settlement: %s, P&L=$%.0f, Futures=%d", expiry, pnl, futures)

        # Weekend protocol
        if weekend_flatten:
            is_friday_close = (now_ct.weekday() == 4 and now_ct.hour >= 16)
            is_sunday_reopen = (now_ct.weekday() == 6 and now_ct.hour >= 17)
            is_weekend = now_ct.weekday() >= 5 and not is_sunday_reopen
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
                mismatches = _reconcile_positions(portfolio, ib, config.account.account_id, config)
                if mismatches:
                    # `ib.positions()` lags `execDetailsEvent` by hundreds of ms,
                    # so a fill that just booked locally can show up here as
                    # drift. Force a fresh position pull and re-check before
                    # killing — persistent silent drift (the 2026-04-08 failure
                    # mode) survives the re-check; a post-fill race does not.
                    logger.warning(
                        "Reconciliation mismatch (%d): %s — re-checking after fresh reqPositions",
                        len(mismatches), mismatches[:5],
                    )
                    try:
                        await ib.reqPositionsAsync()
                    except Exception as ee:
                        logger.warning("reqPositionsAsync during reconcile recheck failed: %s", ee)
                    mismatches = _reconcile_positions(portfolio, ib, config.account.account_id, config)
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

        # Rotate depth subscriptions (all engines)
        if (now - last_depth_rotation).total_seconds() >= depth_rotation_interval:
            for eng in engines:
                try:
                    eng["md"].rotate_depth_subscriptions()
                except Exception as e:
                    logger.warning("depth rotation %s error: %s", eng["name"], e)
            last_depth_rotation = now

        # Re-center the subscribed strike window on live ATM. Cheap no-op
        # when ATM hasn't drifted one strike since the last pass. Held
        # positions are passed as protected_keys so inventory stays
        # subscribed even if it falls outside the fresh window.
        if (now - last_recenter).total_seconds() >= recenter_interval_sec:
            for eng in engines:
                try:
                    own_positions = portfolio.positions_for_product(
                        eng["config"].product.underlying_symbol)
                    protected = {
                        (float(pos.strike), pos.expiry, pos.put_call)
                        for pos in own_positions
                    }
                    await eng["md"].recenter_strike_window(protected)
                except Exception as e:
                    logger.warning("recenter_strike_window %s error: %s",
                                   eng["name"], e)
            last_recenter = now

        # Stage 0 burst-injection sentinel poll (Thread 3 deployment
        # runbook Phase 1). Cheap no-op when no sentinel present. Runs
        # before SABR recal so a burst injection's effects are visible
        # in the upcoming cycle's `update_quotes` cooldown gate.
        for eng in engines:
            try:
                eng["quotes"].check_burst_injection_sentinel()
            except Exception as e:
                logger.warning("burst injection poll %s error: %s",
                               eng["name"], e)

        # SABR recalibration for every engine (hoisted out of update_quotes
        # so it doesn't land between the tick handler and placeOrder, which
        # was inflating TTT by several ms per cycle). The method's own
        # timer + forward-move gates decide whether to actually run.
        for eng in engines:
            try:
                eng["quotes"].maybe_recal_sabr()
            except Exception as e:
                logger.warning("SABR recal %s error: %s", eng["name"], e)

        # v1.4 §5: periodic delta-hedge rebalance (30s default). The
        # HedgeManager's own timer gates the actual trade; this call is
        # cheap when no action is due.
        #
        # Gated on weekend_paused so execute-mode IOCs don't fire into a
        # closed market. Without this, weekend-pause boots issue an IOC
        # to clear residual options delta, the order finds no liquidity,
        # local hedge_qty is updated optimistically (per the v1.5
        # reconciliation gap in CLAUDE.md §10), and IBKR's actual
        # position book diverges from corsair's view. Symptom observed
        # 2026-04-26 (Sunday boot pre-session): oid=141 BUY 3 HGK6 went
        # out, no execDetails arrived, hedge_qty stuck at 3 in memory.
        # Restart resets hedge_qty to 0; this gate prevents recurrence.
        if not weekend_paused:
            for eng in engines:
                try:
                    eng["hedge"].rebalance_periodic()
                except Exception as e:
                    logger.warning("hedge rebalance_periodic %s error: %s",
                                   eng["name"], e)

        # v1.4 §7: operational kill checks. Runs every cycle; the monitor
        # itself gates on sustained-breach windows so spurious single-
        # sample spikes don't halt.
        try:
            op_kills.check()
        except Exception as e:
            logger.warning("operational_kills.check error: %s", e)

        # Full-refresh quote update for secondary engines. The primary's
        # quote update runs below through the event-driven (dirty-set) path
        # so it can ride tick bursts without waiting the full cycle.
        if not (risk.killed or weekend_paused):
            for eng in engines[1:]:
                try:
                    eng["quotes"].update_quotes(portfolio)
                except Exception as e:
                    logger.warning("Quote update %s error: %s", eng["name"], e)

        # ── Snapshot + daily-state persistence ────────────────────────
        # Runs even when killed/paused so Docker's healthcheck (which reads
        # chain_snapshot.json age) doesn't falsely declare the container
        # unhealthy and restart a process that's otherwise alive and
        # recoverable.
        mono = _time.monotonic()
        if (mono - last_snapshot_write) >= snapshot_interval:
            for eng in engines:
                try:
                    write_chain_snapshot(
                        eng["md"], eng["quotes"], portfolio, eng["sabr"],
                        eng["margin"], ib, config.account.account_id,
                        eng["config"], hedge=eng.get("hedge"),
                    )
                except Exception as e:
                    logger.warning("Snapshot %s error: %s", eng["name"], e)
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
                    session_open_nlv=portfolio.session_open_nlv,
                )
            except Exception as e:
                logger.warning("daily_state save error: %s", e)
            last_daily_state_save = mono

        # v1.4 §9.5: margin_snapshots.jsonl (default 60s cadence).
        if (mono - last_margin_snap) >= margin_snap_cadence:
            try:
                cur_margin = margin_coord.get_current_margin()
                capital_v = float(config.constraints.capital)
                pct = cur_margin / capital_v if capital_v > 0 else 0.0
                # Best-effort pull of IBKR-reported margin and scale from
                # the primary engine's checker (MarginCoordinator doesn't
                # expose these centrally yet).
                ibkr_reported = getattr(margin, "cached_ibkr_margin", None)
                ibkr_scale = getattr(margin, "ibkr_scale", None)
                csv_logger.log_paper_margin_snapshot(
                    current_margin_usd=cur_margin,
                    capital_usd=capital_v,
                    margin_pct=pct,
                    ibkr_reported=ibkr_reported,
                    synthetic_raw=None,
                    ibkr_scale=ibkr_scale,
                )
            except Exception as e:
                logger.debug("margin snapshot emit failed: %s", e)
            last_margin_snap = mono

        # v1.4 §6.1 + §9.5: halt check + pnl_snapshots.jsonl emission at
        # 30s cadence. The halt must be sampled "every 30 seconds OR on
        # fill" per spec; the fill path runs check_daily_pnl_only from
        # fill_handler, and this block covers the between-fill 30s leg.
        # compute_mtm_pnl reads live bid/ask so the halt sees fresh MTM
        # without waiting for the 5-min greek refresh.
        if (mono - last_pnl_snap) >= pnl_snap_cadence:
            try:
                # Halt check first so kill+flatten fires before we write
                # the snapshot that would otherwise log "pnl=-4000" with
                # the halt still un-triggered — reconciliation reads
                # kill_switch.jsonl as authoritative on halt timestamps.
                risk.check_daily_pnl_only()
            except Exception as e:
                logger.debug("periodic halt check failed: %s", e)
            try:
                mtm = portfolio.compute_mtm_pnl()
                realized = portfolio.realized_pnl_persisted
                # Sum futures-hedge MTM across every engine (each product
                # hedges independently in its own front-month future).
                hedge_mtm = 0.0
                for eng in engines:
                    try:
                        hedge_mtm += float(eng["hedge"].hedge_mtm_usd())
                    except Exception:
                        pass
                daily = realized + mtm + hedge_mtm
                capital_v = float(config.constraints.capital)
                # Single source of truth with RiskMonitor — prevents the
                # snapshot reporting a threshold that diverges from the
                # one the halt actually uses.
                halt_thr = _resolve_daily_halt_threshold(config) or 0.0
                csv_logger.log_paper_pnl_snapshot(
                    daily_pnl_usd=daily,
                    realized_pnl_usd=realized,
                    mtm_pnl_usd=mtm,
                    hedge_mtm_usd=hedge_mtm,
                    capital_usd=capital_v,
                    halt_threshold_usd=halt_thr,
                    positions_count=len(portfolio.positions),
                    net_delta=portfolio.net_delta,
                    net_theta=portfolio.net_theta,
                    net_vega=portfolio.net_vega,
                    forward=market_data.state.underlying_price,
                )
            except Exception as e:
                logger.debug("pnl snapshot emit failed: %s", e)
            last_pnl_snap = mono

        # ── Blackout recovery check (runs even when killed) ─────────
        # The blackout flag must clear based on fresh option ticks, not
        # on update_quotes running. Otherwise a kill switch + blackout
        # combination deadlocks: kill prevents update_quotes, which
        # prevents the blackout from seeing fresh ticks and clearing.
        if quotes._market_data_blackout and market_data.state.options:
            freshest = min(
                (now - opt.last_update).total_seconds()
                for opt in market_data.state.options.values()
            )
            if freshest < 2.0:
                logger.critical(
                    "MARKET DATA BLACKOUT cleared: fresh option tick "
                    "received (age=%.1fs)", freshest
                )
                quotes._market_data_blackout = False
                quotes._blackout_cancel_sent = False
                from .discord_notify import send_alert
                send_alert(
                    "BLACKOUT CLEARED",
                    "Option data feed restored.",
                    color=0x2ECC71,
                )

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

    # ── Shutdown ──────────────────────────────────────────────────────
    logger.info("Shutting down...")
    watchdog_task.cancel()
    try:
        await watchdog_task
    except (asyncio.CancelledError, Exception):
        pass
    for eng in engines:
        try:
            eng["quotes"].cancel_all_quotes()
            eng["md"].cancel_all_subscriptions()
        except Exception:
            pass
    await conn.disconnect()
    # Flush any queued CSV rows to disk before exiting.
    try:
        csv_logger.shutdown(timeout=5.0)
    except Exception as e:
        logger.warning("csv_logger shutdown error: %s", e)
    # Stop SABR calibration pools (every engine).
    for eng in engines:
        try:
            eng["sabr"].shutdown()
        except Exception:
            pass
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
