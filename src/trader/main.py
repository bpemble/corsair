"""Trader process entry point.

Phase 1+2 scope:
  - Connect to broker IPC.
  - Receive every event the broker forwards.
  - Maintain a per-symbol price book + active-orders index from order acks.
  - Log every event to ``logs-paper/trader_events-YYYY-MM-DD.jsonl`` with
    a receive-side timestamp, so we can measure IPC latency offline
    (broker stamps ts_ns at emit; trader stamps recv_ns on receipt).
  - Periodic telemetry back to broker (event counts, IPC latency p50/p99).

NOT in this scope:
  - Quoting decisions (Phase 2c)
  - placeOrder commands (Phase 3)

Run: ``python3 -m src.trader.main``. Container command in
``docker-compose.yml``.
"""

import asyncio
import json
import logging
import os
import signal
import sys
import time
from collections import Counter, deque
from datetime import date, datetime, timezone
from typing import Optional

from ..ipc import make_client
from .quote_decision import decide as decide_quote

logging.basicConfig(
    level=os.environ.get("LOG_LEVEL", "INFO"),
    format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
)
logger = logging.getLogger("corsair.trader")

# Where event JSONL is written. Same dir as the broker's paper streams
# so analysis tools find it next to ``fills``, ``hedge_trades`` etc.
EVENTS_LOG_DIR = "/app/logs-paper"
TELEMETRY_INTERVAL_S = 10.0

# Default decision params, used until the broker's hello arrives with
# the real config (mm_service_split cleanup #11). Both processes still
# work standalone; once connected, broker's config replaces these.
DEFAULT_MIN_EDGE_TICKS = 2
DEFAULT_TICK_SIZE = 0.0005

# Phase 3 cut-over: when set, trader doesn't just log decisions — it
# actually sends place_order/cancel_order commands to the broker.
# Toggle via env so we can A/B vs broker-quoting.
PLACES_ORDERS = os.environ.get("CORSAIR_TRADER_PLACES_ORDERS", "").strip() == "1"


class TraderState:
    """Trader's local view of the world, populated from broker events."""

    def __init__(self) -> None:
        # Per-option price book: (strike, expiry, right) -> dict of latest tick
        self.options: dict = {}
        self.underlying_price: float = 0.0
        # Trader's view of OUR resting orders, keyed by orderId.
        self.active_orders: dict = {}
        # Most recent vol surface params per (expiry, side). Cleanup
        # #12 — broker fits surfaces per (expiry, side); we used to
        # store by expiry only and let last-write-wins between C and P
        # quietly clobber. Now stored as the broker emits them.
        self.vol_surface: dict = {}
        # Kill state: source -> reason. Empty means quoting allowed.
        self.kills: dict = {}
        self.weekend_paused: bool = False
        # Decision params from broker hello (cleanup #11). Defaults
        # cover the brief window between connect and hello arrival.
        self.min_edge_ticks: int = DEFAULT_MIN_EDGE_TICKS
        self.tick_size: float = DEFAULT_TICK_SIZE
        # Risk limits from broker hello (cleanup #8). Conservative
        # defaults so trader self-gates even before hello arrives.
        self.delta_ceiling: float = 3.0
        self.delta_kill: float = 5.0
        self.margin_ceiling_pct: float = 0.50
        # Live risk aggregates from periodic risk_state events.
        # None until the first event arrives — trader treats that as
        # "no info, don't quote" rather than "all clear".
        self.risk_options_delta: Optional[float] = None
        self.risk_hedge_delta: Optional[int] = None
        self.risk_effective_delta: Optional[float] = None
        self.risk_margin_pct: Optional[float] = None
        self.risk_total_contracts: Optional[int] = None
        self.risk_state_age_ts: float = 0.0
        # Telemetry
        self.event_counts: Counter = Counter()
        self.ipc_latency_us: deque[int] = deque(maxlen=2000)
        self.last_event_ts: float = 0.0


class JSONLWriter:
    """Append-only JSONL writer with daily file rotation.

    Single-thread, no locks. Caller passes ``prefix`` (e.g. ``"trader_events"``)
    and ``log_dir``; rotated path is ``{log_dir}/{prefix}-YYYY-MM-DD.jsonl``.
    """

    def __init__(self, log_dir: str, prefix: str) -> None:
        self._log_dir = log_dir
        self._prefix = prefix
        self._fp = None
        self._date = None

    def write(self, rec: dict) -> None:
        today = date.today().strftime("%Y-%m-%d")
        if today != self._date:
            self._roll(today)
        if self._fp is None:
            return
        try:
            self._fp.write(json.dumps(rec, default=str) + "\n")
        except Exception:
            logger.exception("%s log write failed", self._prefix)

    def _roll(self, day: str) -> None:
        if self._fp is not None:
            try:
                self._fp.close()
            except Exception:
                pass
        self._date = day
        try:
            os.makedirs(self._log_dir, exist_ok=True)
            path = os.path.join(self._log_dir, f"{self._prefix}-{day}.jsonl")
            self._fp = open(path, "a", buffering=1)
            logger.info("%s log opened: %s", self._prefix, path)
        except Exception:
            logger.exception("%s log open failed", self._prefix)
            self._fp = None

    def close(self) -> None:
        if self._fp is not None:
            try:
                self._fp.close()
            except Exception:
                pass
            self._fp = None


class Trader:
    def __init__(self) -> None:
        self.client = make_client()
        self.state = TraderState()
        self.event_log = JSONLWriter(EVENTS_LOG_DIR, "trader_events")
        # Separate JSONL stream for quote decisions — kept distinct from
        # raw events so parity comparison joins are obvious.
        self.decision_log = JSONLWriter(EVENTS_LOG_DIR, "trader_decisions")
        # Counters for the telemetry payload — parity gap surfaces in
        # logs but operator should also see it on the broker dashboard.
        self.decisions_made: Counter = Counter()
        # Phase 3: track our just-sent place_order requests so the
        # next tick on the same key doesn't churn-amend if the price
        # didn't change. Cleared on terminal order_ack so a Cancelled
        # / Filled order doesn't leave the key permanently locked at
        # a stale price.
        self._our_orders_by_key: dict = {}
        # orderId → key map for the inverse lookup on order_ack.
        # Populated when broker's order_ack returns matching orderRef.
        # Bounded by the number of orders we've ever placed in the
        # session (cleaned up on terminal status).
        self._orderid_to_key: dict = {}

    async def on_event(self, msg: dict) -> None:
        recv_ns = time.monotonic_ns()
        ev_type = msg.get("type", "?")
        self.state.event_counts[ev_type] += 1
        self.state.last_event_ts = time.monotonic()

        # IPC latency: broker stamps "ts_ns" at emit (loop monotonic_ns on
        # the broker side); trader's receive monotonic_ns is on a different
        # process so the two clocks aren't directly comparable. Wall-clock
        # delta from the broker's "ts" field is comparable across processes.
        broker_ts_ns = msg.get("ts_ns")
        if isinstance(broker_ts_ns, int):
            wall_recv_ns = time.time_ns()
            lat_us = max(0, (wall_recv_ns - broker_ts_ns) // 1000)
            if 0 <= lat_us < 5_000_000:  # cap at 5s; ignore clock skew
                self.state.ipc_latency_us.append(lat_us)

        # Update local state per type.
        try:
            self._apply(msg)
        except Exception:
            logger.exception("apply failed for %s", ev_type)

        self.event_log.write({
            "recv_ts": datetime.now(timezone.utc).isoformat(),
            "recv_ns": recv_ns,
            "event": msg,
        })

        # On every option tick, run the v2 quote decision for both sides
        # and emit to trader_decisions JSONL. Skips are logged too — the
        # parity-comparison harness compares both with broker's actuals.
        if ev_type == "tick":
            self._decide_on_tick(msg, recv_ns)

    def _decide_on_tick(self, tick_msg: dict, recv_ns: int) -> None:
        """Emit v2 quote decisions for the (strike, expiry, right) of a
        tick event. Both sides (BUY+SELL) are evaluated independently.
        When CORSAIR_TRADER_PLACES_ORDERS=1, also send place_order
        commands to the broker."""
        forward = self.state.underlying_price
        if forward <= 0:
            return
        strike = tick_msg.get("strike")
        expiry = tick_msg.get("expiry")
        right = tick_msg.get("right")
        if strike is None or expiry is None or right is None:
            return
        if self.state.kills:
            # Don't quote into a halt
            return
        if self.state.weekend_paused:
            return

        # Cleanup #8: risk gates. Block new placements when broker's
        # risk aggregates indicate we're at or near hard limits.
        # Without this, trader-driven cut-over can rebuild adverse
        # positions silently (the 2026-05-01 -14.48 delta_kill
        # incident). Only gates new placements; trader still RECEIVES
        # ticks and runs decision logic for the JSONL parity log.
        risk_blocks_buy = False
        risk_blocks_sell = False
        risk_blocks_all = False
        eff = self.state.risk_effective_delta
        if eff is None:
            # No risk_state seen yet — refuse to place orders. Decision
            # log will show a "skip" with reason "risk_state_unknown"
            # so the operator can see how often this fires during boot.
            risk_blocks_all = True
        else:
            # Within delta_ceiling: a BUY would push delta up by ~1
            # contract-delta; if that would breach ceiling, block buys.
            # Similarly for sells. Use a 1.0-delta safety margin so we
            # don't dance right at the limit.
            if eff + 1.0 >= self.state.delta_ceiling:
                risk_blocks_buy = True
            if eff - 1.0 <= -self.state.delta_ceiling:
                risk_blocks_sell = True
            # Hard kill: anywhere near the kill threshold, stop placing.
            if abs(eff) >= self.state.delta_kill - 1.0:
                risk_blocks_all = True
            # Margin ceiling.
            if (self.state.risk_margin_pct is not None
                    and self.state.risk_margin_pct >= self.state.margin_ceiling_pct):
                risk_blocks_all = True
        # Vol surface lookup: prefer the side matching the option's
        # right (broker fits per (expiry, side); call options use the
        # call surface, puts use the put surface). Fall back to whichever
        # side is available during warmup.
        bid = tick_msg.get("bid")
        ask = tick_msg.get("ask")
        vp_msg = (
            self.state.vol_surface.get((expiry, right))
            or self.state.vol_surface.get((expiry, "C"))
            or self.state.vol_surface.get((expiry, "P"))
        )
        vol_params = (vp_msg or {}).get("params")

        for side in ("BUY", "SELL"):
            d = decide_quote(
                forward=forward,
                strike=float(strike),
                expiry=expiry,
                right=right,
                side=side,
                vol_params=vol_params,
                market_bid=bid,
                market_ask=ask,
                min_edge_ticks=self.state.min_edge_ticks,
                tick_size=self.state.tick_size,
            )
            self.decisions_made[d.get("action", "?")] += 1
            self.decision_log.write({
                "recv_ts": datetime.now(timezone.utc).isoformat(),
                "recv_ns": recv_ns,
                "trigger_ts_ns": tick_msg.get("ts_ns"),
                "forward": forward,
                "decision": d,
            })

            # Phase 3 cut-over: if we own order flow, send the command.
            # v3 keeps it simple — every "place" decision sends a fresh
            # order. Phase 3b will add incumbency tracking so we amend
            # rather than churn.
            if PLACES_ORDERS and d.get("action") == "place" and d.get("price"):
                # Cleanup #8 risk gate.
                if risk_blocks_all:
                    self.decisions_made["risk_block"] += 1
                    continue
                if side == "BUY" and risk_blocks_buy:
                    self.decisions_made["risk_block_buy"] += 1
                    continue
                if side == "SELL" and risk_blocks_sell:
                    self.decisions_made["risk_block_sell"] += 1
                    continue
                key = (float(strike), expiry, right, side)
                # Skip if we already have an order at exactly this price
                # for this key (prevents amend churn). Strict equality
                # check; price comparison after tick-quantization.
                existing = self._our_orders_by_key.get(key)
                if existing is not None and existing.get("price") == d["price"]:
                    continue
                self.client.send({
                    "type": "place_order",
                    "ts_ns": time.time_ns(),
                    "strike": float(strike),
                    "expiry": expiry,
                    "right": right,
                    "side": side,
                    "qty": 1,
                    "price": d["price"],
                    "orderRef": "corsair_trader",
                })
                self._our_orders_by_key[key] = {
                    "price": d["price"], "ts_ns": time.time_ns(),
                    "orderId": None,  # filled in by first non-terminal order_ack
                }

    def _apply(self, msg: dict) -> None:
        t = msg.get("type")
        if t == "tick":
            key = (msg["strike"], msg["expiry"], msg["right"])
            self.state.options[key] = msg
        elif t == "underlying_tick":
            self.state.underlying_price = float(msg.get("price", 0.0))
        elif t == "order_ack":
            oid = msg.get("orderId")
            if oid is not None:
                terminal = msg.get("status") in (
                    "Filled", "Cancelled", "ApiCancelled", "Inactive",
                )
                if terminal:
                    self.state.active_orders.pop(oid, None)
                    # Phase 3 incumbency cleanup: drop the (strike,
                    # expiry, right, side) → price entry so the next
                    # tick at that price re-fires a place_order.
                    # Without this, the trader treats the dead order's
                    # last price as still resting and skips re-quoting.
                    key = self._orderid_to_key.pop(oid, None)
                    if key is not None:
                        self._our_orders_by_key.pop(key, None)
                else:
                    self.state.active_orders[oid] = msg
                    # First non-terminal ack for one of our orders:
                    # learn its key so we can clean up on terminal.
                    if (msg.get("orderRef") or "").startswith("corsair_trader") and oid not in self._orderid_to_key:
                        # Reverse-lookup against last few sent commands.
                        # We sent {strike, expiry, right, side, price};
                        # ack carries (orderId, side, lmtPrice, ...).
                        # Match by side + price (within tick tolerance)
                        # against _our_orders_by_key entries with no
                        # orderId yet.
                        ack_price = float(msg.get("lmtPrice", 0) or 0)
                        ack_side = msg.get("side", "")
                        for k, v in list(self._our_orders_by_key.items()):
                            if v.get("orderId") is not None:
                                continue
                            if k[3] != ack_side:
                                continue
                            if abs(v["price"] - ack_price) < 1e-6:
                                v["orderId"] = oid
                                self._orderid_to_key[oid] = k
                                break
        elif t == "fill":
            # No state mutation here in Phase 2 — broker handles position
            # bookkeeping. Just log via on_event's caller.
            pass
        elif t == "vol_surface":
            # Cleanup #12: key by (expiry, side) so call+put surfaces
            # don't clobber each other.
            key = (msg.get("expiry"), msg.get("side"))
            self.state.vol_surface[key] = msg
        elif t == "risk_state":
            # Cleanup #8: ingest broker's risk aggregates so
            # _decide_on_tick can gate new orders.
            self.state.risk_options_delta = float(msg.get("options_delta", 0.0))
            self.state.risk_hedge_delta = int(msg.get("hedge_delta", 0))
            self.state.risk_effective_delta = float(msg.get("effective_delta", 0.0))
            self.state.risk_margin_pct = float(msg.get("margin_pct", 0.0))
            self.state.risk_total_contracts = int(msg.get("total_contracts", 0))
            self.state.risk_state_age_ts = time.monotonic()
        elif t == "kill":
            self.state.kills[msg.get("source", "?")] = msg.get("reason", "?")
        elif t == "resume":
            self.state.kills.pop(msg.get("source", "?"), None)
        elif t == "weekend_pause":
            self.state.weekend_paused = bool(msg.get("paused", False))
        elif t == "snapshot":
            # Initial state seed from broker on (re)connect. Phase 3+.
            pass
        elif t == "hello":
            logger.warning("broker hello: %s", msg)
            # Cleanup #11: pull config from the broker rather than
            # using the hard-coded defaults.
            cfg = msg.get("config") or {}
            if "min_edge_ticks" in cfg:
                self.state.min_edge_ticks = int(cfg["min_edge_ticks"])
            if "tick_size" in cfg:
                self.state.tick_size = float(cfg["tick_size"])
            # Cleanup #8: risk limits.
            if "delta_ceiling" in cfg:
                self.state.delta_ceiling = float(cfg["delta_ceiling"])
            if "delta_kill" in cfg:
                self.state.delta_kill = float(cfg["delta_kill"])
            if "margin_ceiling_pct" in cfg:
                self.state.margin_ceiling_pct = float(cfg["margin_ceiling_pct"])
            logger.warning(
                "trader config from broker: min_edge_ticks=%d tick_size=%g "
                "delta_ceiling=%.1f delta_kill=%.1f margin_ceiling_pct=%.2f",
                self.state.min_edge_ticks, self.state.tick_size,
                self.state.delta_ceiling, self.state.delta_kill,
                self.state.margin_ceiling_pct,
            )
        else:
            logger.debug("unknown event type: %s", t)

    async def telemetry_loop(self) -> None:
        """Send periodic stats back to the broker."""
        while True:
            await asyncio.sleep(TELEMETRY_INTERVAL_S)
            if not self.client.connected:
                continue
            lats = sorted(self.state.ipc_latency_us)
            n = len(lats)
            p50 = lats[n // 2] if n else None
            p99 = lats[int(n * 0.99)] if n else None
            payload = {
                "type": "telemetry",
                "ts_ns": time.time_ns(),
                "events": dict(self.state.event_counts.most_common()),
                "decisions": dict(self.decisions_made),
                "ipc_p50_us": p50,
                "ipc_p99_us": p99,
                "ipc_n": n,
                "n_options": len(self.state.options),
                "n_active_orders": len(self.state.active_orders),
                "n_vol_expiries": len(self.state.vol_surface),
                "killed": list(self.state.kills.keys()),
                "weekend_paused": self.state.weekend_paused,
            }
            self.client.send(payload)
            logger.info(
                "telemetry: events=%d ipc_p50=%sus ipc_p99=%sus opts=%d "
                "orders=%d decisions=%s vol_expiries=%d",
                sum(self.state.event_counts.values()),
                p50, p99,
                len(self.state.options),
                len(self.state.active_orders),
                dict(self.decisions_made),
                len(self.state.vol_surface),
            )

    async def run(self) -> None:
        # Send welcome handshake when first connected.
        async def _on_event(msg):
            await self.on_event(msg)

        # Register a startup hook that fires after first connection. Simplest:
        # poll connection state in a small wrapper task and send when up.
        async def _send_welcome_when_connected():
            while not self.client.connected:
                await asyncio.sleep(0.05)
            self.client.send({
                "type": "welcome",
                "ts_ns": time.time_ns(),
                "trader_version": "v1",
            })
            logger.warning("trader sent welcome")

        client_task = asyncio.create_task(self.client.run(_on_event))
        welcome_task = asyncio.create_task(_send_welcome_when_connected())
        telemetry_task = asyncio.create_task(self.telemetry_loop())

        try:
            await asyncio.gather(client_task, telemetry_task, welcome_task)
        finally:
            self.event_log.close()
            self.decision_log.close()


async def main() -> None:
    transport = os.environ.get("CORSAIR_IPC_TRANSPORT", "socket")
    logger.info("Trader starting; transport=%s", transport)
    trader = Trader()
    stop = asyncio.Event()

    def _signal_handler(*_):
        logger.info("shutdown requested")
        stop.set()

    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, _signal_handler)

    runner = asyncio.create_task(trader.run(), name="trader-run")
    await stop.wait()
    runner.cancel()
    try:
        await runner
    except asyncio.CancelledError:
        pass


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        sys.exit(0)
