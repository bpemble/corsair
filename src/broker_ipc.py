"""Broker-side IPC forwarder.

Wraps an ``IPCServer`` and subscribes to ib_insync events. Each callback
serializes the event into the IPC wire format and publishes to the
trader. Opt-in via ``CORSAIR_BROKER_MODE=1``; default off → zero
behavior change.

Phase 2 scope: forwards ticker updates, fills, order acks, kills, vol
surface updates. Trader-side commands (``place_order`` etc.) are
received via the IPC server's command handler but only logged in this
phase — actual order placement stays on the broker until Phase 3.

Phase 1 (just plumbing) doesn't need any of this — the trader connects
and prints "hello" frames. Phase 2 fills in the forwarders below.
"""

import logging
import os
import time
from typing import Callable, Optional

from .ipc import make_server

logger = logging.getLogger(__name__)


def _ts_ns() -> int:
    """Wall-clock ns. Used for IPC latency measurement across processes —
    monotonic_ns isn't comparable across pid boundaries."""
    return time.time_ns()


class BrokerIPC:
    """Owns the IPCServer + the subscriptions that publish to it."""

    def __init__(self) -> None:
        self.server = make_server()
        self._unsubscribers: list[Callable[[], None]] = []
        self._command_log: list[dict] = []  # phase 2: just log, don't act
        # Phase 3 cut-over: when the trader takes over order placement,
        # the broker dispatches its place_order/cancel_order commands
        # through these handlers. Wired via set_order_dispatchers().
        self._place_order_dispatcher: Optional[Callable[[dict], None]] = None
        self._cancel_order_dispatcher: Optional[Callable[[dict], None]] = None

    def set_order_dispatchers(
        self,
        place_handler: Callable[[dict], None],
        cancel_handler: Callable[[dict], None],
    ) -> None:
        """Wire the broker-side handlers that turn trader commands into
        ib.placeOrder / ib.cancelOrder calls. Called from main.py only
        when CORSAIR_TRADER_PLACES_ORDERS=1.
        """
        self._place_order_dispatcher = place_handler
        self._cancel_order_dispatcher = cancel_handler

    async def start(self) -> None:
        await self.server.start()
        self.server.set_command_handler(self._on_command)

    async def stop(self) -> None:
        for unsub in self._unsubscribers:
            try:
                unsub()
            except Exception:
                pass
        self._unsubscribers.clear()
        await self.server.stop()

    # ── Subscriptions ──────────────────────────────────────────────────

    def attach_to_ib(self, ib) -> None:
        """Subscribe to ib_insync events that the trader needs.

        Called once at boot, after the IB connection is up. We hold the
        bound methods so they can be unsubscribed cleanly on shutdown.
        """
        # orderStatusEvent fires per status change. We forward every one
        # the trader might care about; trader filters internally.
        order_status_cb = self._on_order_status
        ib.orderStatusEvent += order_status_cb
        self._unsubscribers.append(lambda: ib.orderStatusEvent.__isub__(order_status_cb))

        # execDetailsEvent fires per execution. Trader uses these for
        # position updates (Phase 3) and parity comparisons (Phase 2).
        exec_cb = self._on_exec_details
        ib.execDetailsEvent += exec_cb
        self._unsubscribers.append(lambda: ib.execDetailsEvent.__isub__(exec_cb))

    def attach_ticker(self, ticker, strike: float, expiry: str, right: str) -> None:
        """Subscribe to one option ticker's updateEvent.

        Called from market_data.py as each option subscription is added,
        only when broker mode is active.
        """
        cb = self._make_tick_callback(strike, expiry, right)
        ticker.updateEvent += cb
        # No unsub bookkeeping for tickers — the existing market_data
        # cleanup path (cancel_option_subscription) already drops the
        # ticker; the cb goes with it.

    def attach_underlying(self, ticker) -> None:
        cb = self._on_underlying_tick
        ticker.updateEvent += cb

    # ── Forwarders (publish API for non-event sources like SABR fits) ──

    def publish_vol_surface(
        self, expiry: str, side: str, params: dict,
        forward: float, tte: float, rmse: float,
    ) -> None:
        """Forward SABR/SVI fit params after a successful calibration.

        Signature matches MultiExpirySABR's vol_surface_publisher
        callback so it can be wired directly via
        ``sabr.set_vol_surface_publisher(broker_ipc.publish_vol_surface)``.
        """
        self.server.publish({
            "type": "vol_surface",
            "ts_ns": _ts_ns(),
            "expiry": expiry,
            "side": side,
            "params": params,
            "forward": forward,
            "tte": tte,
            "rmse": rmse,
        })

    def publish_kill(self, source: str, reason: str, kill_type: str) -> None:
        self.server.publish({
            "type": "kill",
            "ts_ns": _ts_ns(),
            "source": source,
            "reason": reason,
            "kill_type": kill_type,
        })

    def publish_resume(self, source: str) -> None:
        self.server.publish({
            "type": "resume",
            "ts_ns": _ts_ns(),
            "source": source,
        })

    def publish_weekend_pause(self, paused: bool) -> None:
        self.server.publish({
            "type": "weekend_pause",
            "ts_ns": _ts_ns(),
            "paused": paused,
        })

    def publish_risk_state(self, *, margin_usd: float, margin_pct: float,
                           options_delta: float, hedge_delta: int,
                           effective_delta: float,
                           theta: float, vega: float, gamma: float,
                           total_contracts: int, n_positions: int) -> None:
        """Cleanup #8: forward broker's risk aggregates to the trader.

        Called periodically from the main loop (~1s cadence) so the
        trader can gate new-order placement on margin / delta /
        position-count limits without needing to compute them
        independently. Without this, trader-driven cut-over has no
        risk feedback and rebuilds adverse positions whenever broker
        is silent (the 2026-05-01 -14.48 delta_kill incident).
        """
        self.server.publish({
            "type": "risk_state",
            "ts_ns": _ts_ns(),
            "margin_usd": margin_usd,
            "margin_pct": margin_pct,
            "options_delta": options_delta,
            "hedge_delta": hedge_delta,
            "effective_delta": effective_delta,
            "theta": theta,
            "vega": vega,
            "gamma": gamma,
            "total_contracts": total_contracts,
            "n_positions": n_positions,
        })

    # ── Internal callbacks ─────────────────────────────────────────────

    def _make_tick_callback(self, strike: float, expiry: str, right: str):
        srv = self.server
        def _cb(ticker):
            try:
                srv.publish({
                    "type": "tick",
                    "ts_ns": _ts_ns(),
                    "strike": strike,
                    "expiry": expiry,
                    "right": right,
                    "bid": _safe_float(ticker.bid),
                    "ask": _safe_float(ticker.ask),
                    "bid_size": _safe_int(ticker.bidSize),
                    "ask_size": _safe_int(ticker.askSize),
                    "last": _safe_float(ticker.last),
                })
            except Exception:
                logger.debug("tick forward failed", exc_info=True)
        return _cb

    def _on_underlying_tick(self, ticker) -> None:
        try:
            self.server.publish({
                "type": "underlying_tick",
                "ts_ns": _ts_ns(),
                "price": _safe_float(getattr(ticker, "marketPrice", lambda: None)()),
                "bid": _safe_float(ticker.bid),
                "ask": _safe_float(ticker.ask),
                "last": _safe_float(ticker.last),
            })
        except Exception:
            logger.debug("underlying tick forward failed", exc_info=True)

    def _on_order_status(self, trade) -> None:
        try:
            self.server.publish({
                "type": "order_ack",
                "ts_ns": _ts_ns(),
                "orderId": trade.order.orderId,
                "status": trade.orderStatus.status,
                "filled": float(trade.orderStatus.filled or 0),
                "remaining": float(trade.orderStatus.remaining or 0),
                "lmtPrice": float(getattr(trade.order, "lmtPrice", 0) or 0),
                "side": trade.order.action,
                "qty": float(trade.order.totalQuantity or 0),
                "orderRef": getattr(trade.order, "orderRef", "") or "",
            })
        except Exception:
            logger.debug("order_status forward failed", exc_info=True)

    def _on_exec_details(self, trade, fill) -> None:
        try:
            ex = fill.execution
            self.server.publish({
                "type": "fill",
                "ts_ns": _ts_ns(),
                "orderId": trade.order.orderId,
                "exec_id": ex.execId,
                "side": ex.side,
                "qty": float(ex.shares or 0),
                "price": float(ex.price or 0),
                "symbol": getattr(fill.contract, "localSymbol", "") or "",
                "secType": getattr(fill.contract, "secType", "") or "",
            })
        except Exception:
            logger.debug("fill forward failed", exc_info=True)

    def _on_command(self, msg: dict) -> None:
        """Trader → broker commands. Phase 2: log only.
        Phase 3: dispatch to placeOrder / cancelOrder."""
        t = msg.get("type")
        if t == "telemetry":
            logger.info(
                "trader telemetry: events=%s ipc_p50=%sus ipc_p99=%sus "
                "ttt_p50=%sus ttt_p99=%sus opts=%s orders=%s",
                sum((msg.get("events") or {}).values()),
                msg.get("ipc_p50_us"), msg.get("ipc_p99_us"),
                msg.get("ttt_p50_us"), msg.get("ttt_p99_us"),
                msg.get("n_options"), msg.get("n_active_orders"),
            )
        elif t == "welcome":
            logger.warning("trader welcome: version=%s", msg.get("trader_version"))
            # Send a hello back with broker state seed: version + config
            # snapshot the trader needs for its decision logic. Cleanup
            # #11 — replaces the hard-coded constants in trader/main.py
            # with values from the broker's authoritative config.
            cfg = self._build_config_snapshot()
            self.server.publish({
                "type": "hello",
                "ts_ns": _ts_ns(),
                "broker_version": "v1",
                "config": cfg,
            })
        elif t == "place_order":
            # Phase 3: dispatch to ib.placeOrder when wired. If no
            # dispatcher is set (broker pre-cutover), fall back to
            # logging — same as Phase 2.
            if self._place_order_dispatcher is not None:
                try:
                    self._place_order_dispatcher(msg)
                except Exception:
                    logger.exception("place_order dispatch failed")
            else:
                self._command_log.append(msg)
        elif t == "cancel_order":
            if self._cancel_order_dispatcher is not None:
                try:
                    self._cancel_order_dispatcher(msg)
                except Exception:
                    logger.exception("cancel_order dispatch failed")
            else:
                self._command_log.append(msg)
        elif t == "ping":
            self._command_log.append(msg)

    # ── Config snapshot (sent in hello to trader) ─────────────────────
    def _build_config_snapshot(self) -> dict:
        """Pack broker config values that the trader needs into a dict
        for the welcome→hello handshake. Subset only — fields trader's
        decision logic actually reads."""
        cfg = getattr(self, "_broker_config", None)
        if cfg is None:
            return {}
        try:
            quoting = getattr(cfg, "quoting", object())
            constraints = getattr(cfg, "constraints", object())
            kill_switch = getattr(cfg, "kill_switch", object())
            return {
                "min_edge_ticks": int(getattr(quoting, "min_edge_ticks", 2)),
                "tick_size": float(getattr(quoting, "tick_size", 0.0005)),
                "underlying_symbol": str(getattr(
                    cfg.product, "underlying_symbol", "HG")),
                "multiplier": int(getattr(cfg.product, "multiplier", 25000)),
                # Risk limits — trader uses these to self-gate (cleanup #8).
                "delta_ceiling": float(getattr(constraints, "delta_ceiling", 3.0)),
                "delta_kill": float(getattr(kill_switch, "delta_kill", 5.0)),
                "margin_ceiling_pct": float(getattr(
                    constraints, "margin_ceiling_pct", 0.50)),
                "capital_usd": float(getattr(cfg, "capital", 500_000)),
            }
        except Exception:
            logger.debug("config snapshot build failed", exc_info=True)
            return {}

    def set_broker_config(self, config) -> None:
        """Stash a reference to the live config so welcome→hello can
        pack it. Set once at boot from main.py."""
        self._broker_config = config


def _safe_float(x):
    if x is None:
        return None
    try:
        f = float(x)
        if f != f:  # NaN
            return None
        return f
    except (TypeError, ValueError):
        return None


def _safe_int(x):
    if x is None:
        return None
    try:
        i = int(x)
        return i
    except (TypeError, ValueError):
        return None


def is_broker_mode_enabled() -> bool:
    return os.environ.get("CORSAIR_BROKER_MODE", "").strip() == "1"
