"""Connection + data-freshness watchdog with auto-recovery.

Detects three failure modes that the in-process error handlers can't catch:
  1. ib.isConnected() returning False (TCP-level loss)
  2. Underlying tick stale beyond threshold (silent feed death)
  3. All option ticks stale (per-strike subscription hang)

On sustained unhealthy state (2 consecutive failures, ~20s) the watchdog
tears down the IB connection and rebuilds it from scratch: reconnect,
re-discover, re-subscribe, reseed positions, clear the disconnect kill so
quoting resumes. Risk-induced kills (margin/delta/PnL) stay sticky and
require human review — those signal a real problem, not a transient.

Also exposes :func:`safe_discover_and_subscribe`, a hard-timeout wrapper
around the discovery path used by both the watchdog recovery loop and
the startup retry loop in main.py.
"""

import asyncio
import logging
import os
from datetime import datetime

logger = logging.getLogger(__name__)

WATCHDOG_INTERVAL_SEC = 2.0
# Two-tier staleness:
#   FAST_CANCEL: short threshold; on hit we immediately panic_cancel
#     (single reqGlobalCancel + clear local state) WITHOUT tearing down
#     the connection. Goal: minimize bad-fill exposure when the gateway
#     freezes (TWS login race, IBC restart, transient network blip)
#     while quotes are still resting on the book.
#   RECONNECT: longer threshold; on hit we tear down the socket and run
#     the full reconnect+rediscovery cycle.
# 30s GTD on each order is a backstop, but 30s of stale quotes during a
# fast move is a lot of risk — we want detect-to-cancel in single-digit
# seconds.
WATCHDOG_FAST_CANCEL_STALE_SEC = 5.0
# Lowered from 30s to 12s (defense vector D, 2026-04-09). With the new
# Error 1100 handler in connection.py treating soft disconnects immediately,
# the tick-staleness path is now a backstop only — we want it to fire
# faster than the 30s GTD floor so the cancel-on-disconnect timing isn't
# bounded by GTD expiry. 12s gives ~6 watchdog ticks of grace which is
# enough to ride out a quiet ETH options market without false-positive
# cancels (verified against the freshest_signal_age min() semantics).
WATCHDOG_STALE_THRESHOLD_SEC = 12.0
# Post-discovery grace period. After a successful discover_and_subscribe()
# the data feed can take 20-30s to start delivering ticks — especially
# during the ETH options daily close window (CLAUDE.md §4, 16:00-17:00 CT)
# or immediately after a gateway volume wipe. Skipping the stale-tick
# check during the grace window prevents a false-positive reconnect storm
# where each warm-up cycle fires the watchdog before the first tick lands.
# Set generously because the real disconnect paths (connection drop,
# exception storm) have independent detection that does NOT wait for
# this grace window — this only affects the passive freshness check.
WATCHDOG_POST_DISCOVERY_GRACE_SEC = 30.0
WATCHDOG_FAILURES_BEFORE_ACTION = 2
WATCHDOG_BACKOFF_SEC = (5, 10, 30, 60, 60)  # last value repeats
# Quote-loop exception storm: if QuoteManager.consecutive_quote_errors
# exceeds this, the watchdog treats it as an unhealthy state and runs
# the recovery cycle. main.py also independently panic-cancels at its
# own (lower) threshold, so by the time the watchdog sees this the book
# should already be empty — this just kicks the recovery path.
WATCHDOG_QUOTE_ERROR_STORM_THRESHOLD = 10
# Hard timeout on discover_and_subscribe. IB Gateway can complete connect()
# but then hang inside reqSecDefOptParams/reqContractDetails indefinitely.
# Bumped from 30s → 75s on 2026-04-09 to accommodate the multi-expiry
# discovery path: `reqContractDetails(FuturesOption stub)` scans IBKR's
# entire contract database for the symbol and empirically takes ~28-30s
# on the ETH futures option chain — with only 1-2 seconds of margin
# against the old 30s ceiling we were hitting boundary-kill timeouts on
# every cold boot. 75s gives real headroom without masking a genuine
# gateway hang (the gateway-process hang we're guarding against here is
# observationally 60+ seconds deep, so a 75s cap still catches it).
DISCOVERY_TIMEOUT_SEC = 75.0
STARTUP_RETRY_BACKOFF_SEC = (5, 10, 30, 60, 60)
# After this many consecutive failed in-engine recoveries (connect or
# discovery), escalate to a docker-level container restart of the gateway.
# Restarting just the API session can't fix a JVM-level hang inside the
# gateway process; only a fresh container does. ~3 cycles ≈ 50s of trying.
RECOVERY_FAILS_BEFORE_GATEWAY_RESTART = 3
GATEWAY_CONTAINER_NAME = os.environ.get(
    "CORSAIR_GATEWAY_CONTAINER", "corsair-ib-gateway-1"
)
# Seconds to wait after a container restart before resuming watchdog
# health checks (gives the gateway JVM time to come up + IBC to log in).
GATEWAY_RESTART_SETTLE_SEC = 90.0  # 60s wasn't enough — JVM cold-start of fresh container leaves the gateway not-quite-ready and the immediate next reconnect attempt fails


# ── Gateway escalation ────────────────────────────────────────────────
# When in-engine recovery (conn.disconnect/connect + reconnect) fails N
# consecutive times, the watchdog escalates by fully recreating the gateway
# container: stop, remove, wipe its named volume(s), and recreate with the
# same config. Equivalent to ``docker compose down -v ib-gateway && docker
# compose up -d ib-gateway``.
#
# We do NOT do a simple container.restart() first — empirical evidence shows
# the gateway hang is almost always caused by IBC session-state corruption
# in the named volume, which a process restart inside the same container
# can't clear. Skipping straight to the full recreate is faster and more
# reliable than trying the lighter intervention first.


def _gateway_attrs_to_run_kwargs(attrs: dict) -> dict:
    """Convert a container's `inspect` output to kwargs for client.containers.run().
    Used by Tier 2 escalation to recreate a container with identical config."""
    config = attrs.get('Config', {}) or {}
    host_config = attrs.get('HostConfig', {}) or {}

    kwargs = {
        'image': config['Image'],
        'name': attrs['Name'].lstrip('/'),
        'detach': True,
        'network_mode': host_config.get('NetworkMode', 'default'),
        'environment': config.get('Env') or [],
        'labels': config.get('Labels') or {},
    }

    # Restart policy
    rp = host_config.get('RestartPolicy') or {}
    if rp.get('Name') and rp['Name'] != 'no':
        kwargs['restart_policy'] = {
            'Name': rp['Name'],
            'MaximumRetryCount': rp.get('MaximumRetryCount', 0),
        }

    # Healthcheck (the inspect format uses TitleCase keys; run() uses lowercase)
    hc = config.get('Healthcheck') or {}
    if hc.get('Test'):
        kwargs['healthcheck'] = {
            'test': hc['Test'],
            'interval': hc.get('Interval'),
            'timeout': hc.get('Timeout'),
            'retries': hc.get('Retries'),
            'start_period': hc.get('StartPeriod'),
        }

    # Volumes — bind named volumes back at the same mount points
    volume_binds = {}
    for m in attrs.get('Mounts', []) or []:
        if m.get('Type') == 'volume' and m.get('Name'):
            volume_binds[m['Name']] = {
                'bind': m.get('Destination'),
                'mode': 'rw' if m.get('RW', True) else 'ro',
            }
    if volume_binds:
        kwargs['volumes'] = volume_binds

    return kwargs


def escalate_gateway_recreate() -> bool:
    """Recreate the gateway container with a fresh volume.

    Stops the container, removes it, wipes its named volume(s), and creates
    a fresh container with the same image/env/network/labels/etc. Equivalent
    to running ``docker compose down -v ib-gateway && docker compose up -d
    ib-gateway`` from the host shell.

    This is the only escalation tier — empirically, the lighter
    container.restart() does not clear the IBC session-state corruption
    that causes the hang we keep hitting, so we skip it entirely and go
    straight to the full recreate.
    """
    try:
        import docker
        from docker.errors import NotFound
    except ImportError as e:
        logger.error("WATCHDOG: docker SDK not available — cannot escalate: %s", e)
        return False
    try:
        client = docker.from_env()
        try:
            container = client.containers.get(GATEWAY_CONTAINER_NAME)
        except NotFound:
            logger.error("WATCHDOG: gateway container '%s' not found",
                         GATEWAY_CONTAINER_NAME)
            return False

        # Snapshot config BEFORE destroying — we need it to recreate.
        attrs = container.attrs
        kwargs = _gateway_attrs_to_run_kwargs(attrs)
        named_volumes = [
            m['Name'] for m in (attrs.get('Mounts') or [])
            if m.get('Type') == 'volume' and m.get('Name')
        ]

        logger.critical(
            "WATCHDOG: TIER 2 ESCALATION — full recreate of %s "
            "(stop + remove + wipe %d volume(s) + recreate)",
            GATEWAY_CONTAINER_NAME, len(named_volumes),
        )

        # Tear down
        try:
            container.stop(timeout=30)
        except Exception as e:
            logger.warning("WATCHDOG: stop failed (continuing): %s", e)
        try:
            container.remove(force=True)
        except Exception as e:
            logger.warning("WATCHDOG: remove failed (continuing): %s", e)

        # Wipe named volumes
        for vol_name in named_volumes:
            try:
                vol = client.volumes.get(vol_name)
                vol.remove(force=True)
                logger.info("WATCHDOG: removed volume %s", vol_name)
            except NotFound:
                pass
            except Exception as e:
                logger.warning("WATCHDOG: volume %s wipe failed: %s", vol_name, e)

        # Recreate
        client.containers.run(**kwargs)
        logger.info("WATCHDOG: gateway recreate complete; settling for %.0fs",
                    GATEWAY_RESTART_SETTLE_SEC)
        return True
    except Exception as e:
        logger.error("WATCHDOG: gateway recreate failed: %s", e, exc_info=True)
        return False


async def safe_discover_and_subscribe(market_data) -> bool:
    """Run discover_and_subscribe with a hard timeout. Returns True on
    success, False on timeout/error. Caller is responsible for retry."""
    try:
        await asyncio.wait_for(
            market_data.discover_and_subscribe(),
            timeout=DISCOVERY_TIMEOUT_SEC,
        )
        return True
    except asyncio.TimeoutError:
        logger.error("Chain discovery timed out after %.0fs — gateway is hung",
                     DISCOVERY_TIMEOUT_SEC)
        return False
    except Exception as e:
        logger.error("Chain discovery failed: %s", e, exc_info=True)
        return False


def _check_health(ib, market_data, quotes=None) -> tuple:
    """Return (healthy, fast_cancel_needed, reasons).

    Two tiers:
      - fast_cancel_needed: feed has been stale for FAST_CANCEL_STALE_SEC
        but not yet RECONNECT-stale. Triggers a panic_cancel WITHOUT
        tearing down the connection. Brief blips heal themselves; the
        book stays empty until ticks resume.
      - healthy=False: full reconnect tier (longer staleness, hard
        disconnect, exception storm, etc.).
    """
    now = datetime.now()
    state = market_data.state
    reasons = []
    fast_cancel = False

    if not ib.isConnected():
        reasons.append("disconnected")

    # "Freshest signal" semantics: take the MIN age across underlying and
    # any option. If literally nothing has ticked in WATCHDOG_FAST_CANCEL
    # seconds, the feed is dead. As long as ANY market data is flowing
    # we're fine — ETH futures don't tick every 5s on a quiet market, so
    # using max() (every signal must be fresh) produces a false-positive
    # storm that nukes the book every fast-cancel interval.
    ages = []
    if state.underlying_price > 0:
        ages.append((now - state.underlying_last_update).total_seconds())
    if state.options:
        ages.append(min(
            (now - opt.last_update).total_seconds()
            for opt in state.options.values()
        ))
    freshest_signal_age = min(ages) if ages else 0.0

    # Tick-staleness gate — skip the check if either of these holds:
    #   (a) We've never seen a tick on this session (first_tick_seen False).
    #       The initial last_update timestamps are just construction time,
    #       not real market events — measuring "staleness" against them
    #       produces false positives on cold boots during quiet windows
    #       (especially the ETH options 16:00-17:00 CT close window, when
    #       even the underlying futures can go 30+s between ticks on
    #       paper).
    #   (b) We're still within the post-discovery grace window, as a
    #       belt-and-suspenders against a first tick landing early and
    #       then the feed briefly hiccuping during warm-up.
    # The disconnect + exception-storm branches below run unconditionally
    # so a real gateway hang still triggers recovery.
    in_grace = False
    if not getattr(state, "first_tick_seen", False):
        in_grace = True
    elif state.discovery_completed_at is not None:
        since_discovery = (now - state.discovery_completed_at).total_seconds()
        if since_discovery < WATCHDOG_POST_DISCOVERY_GRACE_SEC:
            in_grace = True

    if (not in_grace) and freshest_signal_age >= WATCHDOG_STALE_THRESHOLD_SEC:
        reasons.append(f"no fresh ticks ({freshest_signal_age:.0f}s)")

    # Fast-cancel tier: feed stale but not yet at reconnect threshold.
    # Only meaningful while still connected — a real disconnect already
    # routes through the reconnect path below. Also skipped during the
    # post-discovery grace window (same rationale as the stale-tick check).
    if (ib.isConnected()
            and not in_grace
            and freshest_signal_age >= WATCHDOG_FAST_CANCEL_STALE_SEC
            and not reasons):
        fast_cancel = True

    # Quote-loop exception storm — see WATCHDOG_QUOTE_ERROR_STORM_THRESHOLD.
    if quotes is not None:
        n_errs = getattr(quotes, "consecutive_quote_errors", 0)
        if n_errs >= WATCHDOG_QUOTE_ERROR_STORM_THRESHOLD:
            reasons.append(f"quote loop exception storm ({n_errs})")

    return (len(reasons) == 0, fast_cancel, reasons)


async def watchdog_loop(conn, market_data, quotes, portfolio, margin, risk,
                        sabr, account_id: str, shutdown_event: asyncio.Event,
                        fills=None, session_start_fn=None):
    """Periodic health check + auto-recovery. See module docstring."""
    consecutive_failures = 0
    consecutive_recovery_failures = 0
    consecutive_fast_cancel = 0
    backoff_idx = 0
    ib = conn.ib

    while not shutdown_event.is_set():
        try:
            await asyncio.sleep(WATCHDOG_INTERVAL_SEC)

            healthy, fast_cancel, reasons = _check_health(ib, market_data, quotes)
            if healthy:
                if consecutive_failures > 0:
                    logger.info("WATCHDOG: health restored after %d failed cycles",
                                consecutive_failures)
                # Auto-clear disconnect kill when connectivity restores via
                # Error 1102 (soft-restore). Without this, the kill persists
                # indefinitely because clear_disconnect_kill() only lived in
                # the watchdog's own reconnect path — which is never entered
                # when the health check passes.
                if risk.killed and risk.kill_source == "disconnect":
                    if risk.clear_disconnect_kill():
                        logger.info(
                            "WATCHDOG: auto-cleared disconnect kill "
                            "(connectivity restored externally)")
                consecutive_failures = 0
                consecutive_recovery_failures = 0
                backoff_idx = 0
                # Fast-cancel tier: feed has been stale long enough to be
                # risky (>5s) but not long enough to warrant a reconnect.
                # Require 2 consecutive fast-cancel observations so a
                # single quiet tick gap doesn't nuke the book — and only
                # actually fire once per stale period (not every cycle
                # while still stale), so we don't churn place→cancel.
                if fast_cancel:
                    consecutive_fast_cancel += 1
                    if (consecutive_fast_cancel == 2
                            and quotes.active_orders):
                        logger.warning(
                            "WATCHDOG: feed stale ≥%.0fs (2 cycles) — fast panic_cancel (no reconnect)",
                            WATCHDOG_FAST_CANCEL_STALE_SEC,
                        )
                        try:
                            quotes.panic_cancel()
                        except Exception as e:
                            logger.error("WATCHDOG: fast panic_cancel failed: %s", e)
                else:
                    consecutive_fast_cancel = 0
                continue

            consecutive_failures += 1
            logger.warning("WATCHDOG: unhealthy (#%d): %s",
                           consecutive_failures, "; ".join(reasons))
            if consecutive_failures < WATCHDOG_FAILURES_BEFORE_ACTION:
                continue

            # ── Recovery: tear down + reconnect ──────────────────────
            logger.critical("WATCHDOG: triggering reconnect after %d unhealthy cycles",
                            consecutive_failures)
            try:
                quotes.panic_cancel()
            except Exception as e:
                logger.warning("WATCHDOG: panic_cancel failed: %s", e)
            try:
                await conn.disconnect()
            except Exception as e:
                logger.warning("WATCHDOG: disconnect failed: %s", e)

            wait = WATCHDOG_BACKOFF_SEC[min(backoff_idx, len(WATCHDOG_BACKOFF_SEC) - 1)]
            logger.info("WATCHDOG: backoff %ds before reconnect attempt", wait)
            await asyncio.sleep(wait)
            backoff_idx += 1

            recovery_succeeded = False
            try:
                if not await conn.connect():
                    logger.error("WATCHDOG: reconnect failed; will retry next cycle")
                elif not await safe_discover_and_subscribe(market_data):
                    logger.error("WATCHDOG: discovery failed/hung post-reconnect; "
                                 "tearing down for next cycle")
                    try:
                        await conn.disconnect()
                    except Exception:
                        pass
                else:
                    await asyncio.sleep(3)  # let initial ticks settle
                    seeded = portfolio.seed_from_ibkr(ib, account_id)
                    logger.info("WATCHDOG: reseeded %d position(s) after reconnect", seeded)
                    margin.invalidate_portfolio()
                    sabr.set_expiries(market_data.state.expiries)
                    try:
                        await market_data.ensure_position_subscribed(portfolio.positions)
                    except Exception as e:
                        logger.warning("WATCHDOG: ensure_position_subscribed failed: %s", e)
                    # Replay any executions that landed during the gap. The
                    # disconnect → reconnect window is exactly when fills go
                    # silently missing if not backfilled.
                    if fills is not None and session_start_fn is not None:
                        try:
                            await fills.replay_missed_executions(session_start_fn())
                        except Exception as e:
                            logger.warning("WATCHDOG: replay_missed_executions failed: %s", e)

                    cancelled = 0
                    for trade in ib.openTrades():
                        ref = getattr(trade.order, "orderRef", "") or ""
                        if ref.startswith("corsair"):
                            try:
                                ib.cancelOrder(trade.order)
                                cancelled += 1
                            except Exception:
                                pass
                    if cancelled:
                        logger.info("WATCHDOG: cancelled %d orphaned orders post-reconnect",
                                    cancelled)

                    if risk.clear_disconnect_kill():
                        logger.info("WATCHDOG: cleared disconnect kill, quoting resumes")
                    elif risk.killed:
                        logger.warning(
                            "WATCHDOG: reconnected but risk kill is sticky (%s) — "
                            "quoting will NOT resume without human intervention",
                            risk.kill_reason,
                        )

                    # Reset the quote-loop exception counter so a stale
                    # storm flag doesn't immediately re-trip us next cycle.
                    quotes.consecutive_quote_errors = 0
                    logger.info("WATCHDOG: recovery complete")
                    recovery_succeeded = True
            except Exception as e:
                logger.error("WATCHDOG: reconnect attempt failed: %s", e, exc_info=True)

            if recovery_succeeded:
                consecutive_failures = 0
                consecutive_recovery_failures = 0
                backoff_idx = 0
                continue

            # Recovery failed. After N consecutive failures, escalate to a
            # full gateway recreate (volume wipe + fresh container). The
            # lighter container.restart() doesn't reliably clear the IBC
            # session-state hangs we keep hitting in practice.
            consecutive_recovery_failures += 1
            if consecutive_recovery_failures >= RECOVERY_FAILS_BEFORE_GATEWAY_RESTART:
                logger.critical(
                    "WATCHDOG: %d consecutive recovery failures — escalating "
                    "to gateway recreate", consecutive_recovery_failures,
                )
                if escalate_gateway_recreate():
                    await asyncio.sleep(GATEWAY_RESTART_SETTLE_SEC)
                    consecutive_recovery_failures = 0
                    backoff_idx = 0
                    # Defense vector E (2026-04-09): immediately force a
                    # clean reconnect against the fresh gateway. Without
                    # this, we'd fall through to the next loop iteration
                    # which still holds stale ib_insync state and may take
                    # several more health-check cycles + backoff to start
                    # connecting — empirically this caused corsair to sit
                    # idle even after a successful gateway escalation,
                    # requiring a manual `docker compose restart corsair`.
                    logger.info(
                        "WATCHDOG: post-escalation, forcing clean reconnect"
                    )
                    try:
                        await conn.disconnect()
                    except Exception as e:
                        logger.warning("WATCHDOG: post-escalation disconnect: %s", e)
                    try:
                        if await conn.connect() and await safe_discover_and_subscribe(market_data):
                            await asyncio.sleep(3)
                            seeded = portfolio.seed_from_ibkr(ib, account_id)
                            logger.info(
                                "WATCHDOG: post-escalation reseed: %d position(s)",
                                seeded,
                            )
                            margin.invalidate_portfolio()
                            sabr.set_expiries(market_data.state.expiries)
                            try:
                                await market_data.ensure_position_subscribed(portfolio.positions)
                            except Exception as e:
                                logger.warning("WATCHDOG: post-escalation ensure_position_subscribed failed: %s", e)
                            if fills is not None and session_start_fn is not None:
                                try:
                                    await fills.replay_missed_executions(session_start_fn())
                                except Exception as e:
                                    logger.warning("WATCHDOG: post-escalation replay_missed_executions failed: %s", e)
                            quotes.consecutive_quote_errors = 0
                            if risk.clear_disconnect_kill():
                                logger.info(
                                    "WATCHDOG: post-escalation cleared disconnect kill, "
                                    "quoting resumes"
                                )
                            consecutive_failures = 0
                            logger.info("WATCHDOG: post-escalation recovery complete")
                        else:
                            logger.error(
                                "WATCHDOG: post-escalation reconnect failed; "
                                "next normal cycle will retry"
                            )
                    except Exception as e:
                        logger.error(
                            "WATCHDOG: post-escalation reconnect exception: %s",
                            e, exc_info=True,
                        )

        except asyncio.CancelledError:
            logger.info("WATCHDOG: cancelled, exiting")
            raise
        except Exception as e:
            logger.error("WATCHDOG: loop exception: %s", e, exc_info=True)
            await asyncio.sleep(WATCHDOG_INTERVAL_SEC)
