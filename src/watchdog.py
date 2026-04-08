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

WATCHDOG_INTERVAL_SEC = 10.0
WATCHDOG_STALE_THRESHOLD_SEC = 30.0
WATCHDOG_FAILURES_BEFORE_ACTION = 2
WATCHDOG_BACKOFF_SEC = (5, 10, 30, 60, 60)  # last value repeats
# Hard timeout on discover_and_subscribe. IB Gateway can complete connect()
# but then hang inside reqSecDefOptParams/reqContractDetails indefinitely.
DISCOVERY_TIMEOUT_SEC = 30.0
STARTUP_RETRY_BACKOFF_SEC = (5, 10, 30, 60, 60)
# After this many consecutive failed in-engine recoveries (connect or
# discovery), escalate to a docker-level container restart of the gateway.
# Restarting just the API session can't fix a JVM-level hang inside the
# gateway process; only a fresh container does. ~3 cycles ≈ 50s of trying.
RECOVERY_FAILS_BEFORE_GATEWAY_RESTART = 3
GATEWAY_CONTAINER_NAME = os.environ.get(
    "CORSAIR_GATEWAY_CONTAINER", "corsair2-ib-gateway-1"
)
# Seconds to wait after a container restart before resuming watchdog
# health checks (gives the gateway JVM time to come up + IBC to log in).
GATEWAY_RESTART_SETTLE_SEC = 60.0


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


def _check_health(ib, market_data) -> tuple:
    """Return (healthy: bool, reasons: list[str])."""
    now = datetime.now()
    state = market_data.state
    reasons = []

    if not ib.isConnected():
        reasons.append("disconnected")

    if state.underlying_price > 0:
        underlying_age = (now - state.underlying_last_update).total_seconds()
        if underlying_age >= WATCHDOG_STALE_THRESHOLD_SEC:
            reasons.append(f"underlying stale {underlying_age:.0f}s")

    if state.options:
        any_fresh = any(
            (now - opt.last_update).total_seconds() < WATCHDOG_STALE_THRESHOLD_SEC
            for opt in state.options.values()
        )
        if not any_fresh:
            reasons.append("no fresh option ticks")

    return (len(reasons) == 0, reasons)


async def watchdog_loop(conn, market_data, quotes, portfolio, margin, risk,
                        sabr, account_id: str, shutdown_event: asyncio.Event):
    """Periodic health check + auto-recovery. See module docstring."""
    consecutive_failures = 0
    consecutive_recovery_failures = 0
    backoff_idx = 0
    ib = conn.ib

    while not shutdown_event.is_set():
        try:
            await asyncio.sleep(WATCHDOG_INTERVAL_SEC)

            healthy, reasons = _check_health(ib, market_data)
            if healthy:
                if consecutive_failures > 0:
                    logger.info("WATCHDOG: health restored after %d failed cycles",
                                consecutive_failures)
                consecutive_failures = 0
                consecutive_recovery_failures = 0
                backoff_idx = 0
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
                quotes.cancel_all_quotes()
            except Exception as e:
                logger.warning("WATCHDOG: cancel_all_quotes failed: %s", e)
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
                    sabr.set_expiry(market_data.state.front_month_expiry)

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

        except asyncio.CancelledError:
            logger.info("WATCHDOG: cancelled, exiting")
            raise
        except Exception as e:
            logger.error("WATCHDOG: loop exception: %s", e, exc_info=True)
            await asyncio.sleep(WATCHDOG_INTERVAL_SEC)
