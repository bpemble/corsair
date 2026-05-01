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
from .quote_decision import decide as decide_quote, compute_theo
from ..sabr import time_to_expiry_years

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

# Staleness check cadence + threshold. Resting orders whose price is
# more than STALENESS_TICKS off current theo get cancelled. This is
# the trader-side equivalent of broker's quote_engine.is_quote_stale.
# Without it, theo drift between order placement and the GTD-30s
# expiry can adversely fill us (2026-05-01: 20 such fills, ~$2.4K
# negative edge, before this guard landed).
STALENESS_INTERVAL_S = 0.10       # 10Hz scan
STALENESS_TICKS = 1               # cancel when off-by-more-than 1 tick vs theo

# Per-key place cooldown (cleanup pass 4, 2026-05-01). After the trader
# sends a place_order for a (strike, expiry, right, side) key, refuse
# to re-place at that key for COOLDOWN_NS nanoseconds. Bounds the
# re-placement rate at ~4Hz/key under the current 250ms default; without
# this, theo bouncing 1 tick on every option update produced ~80
# places/sec total and pushed IPC p99 to 700-1900ms.
COOLDOWN_NS = 250_000_000         # 250 ms

# Dead-band + GTD-refresh logic (cleanup pass 5, 2026-05-01). Mirrors
# the broker's _send_or_update pattern. The broker has been doing this
# for ages; the trader was using strict same-price equality which let
# 1-tick theo wiggle drive 50 places/sec to IBKR (place_rtt_us p50 ~1s).
#
# Three rules, in priority order:
#   1. If the new target price is within DEAD_BAND_TICKS of the resting
#      order's price AND the resting order is fresh (GTD won't expire
#      soon), skip — no re-place needed.
#   2. If the resting order's GTD is within GTD_REFRESH_LEAD_S of
#      expiry, force a re-place even within the dead-band — that's the
#      "keep the quote alive" branch.
#   3. Outside the dead-band → always re-place (price actually moved).
DEAD_BAND_TICKS = 1               # ≥1-tick price move re-fires
GTD_LIFETIME_S = 5.0              # must match broker's _dispatch_place_order GTD
GTD_REFRESH_LEAD_S = 1.5          # re-place when ≤1.5s remains

# Cleanup pass 3 (2026-05-01) defensive constants. Trader will refuse
# to quote outside this band of ATM strikes — wing extrapolation of
# the SVI surface is least reliable there, and that's where the
# fit-forward bug bit hardest. ATM is computed from current spot;
# strikes whose offset from spot exceeds MAX_STRIKE_OFFSET_USD are
# silently skipped.
MAX_STRIKE_OFFSET_USD = 0.30      # ±30 ticks at 0.0005 = ±$0.30, ~5% of HG underlying

# Risk-state freshness. If broker stops sending risk_state events for
# this long, treat the cached values as suspect and gate ALL placements.
# Broker sends at 1Hz; threshold is generous to absorb transient lag.
RISK_STATE_STALE_S = 5.0

# BBO size requirement — paper IBKR matches against displayed depth
# even when size is 1; refusing to quote into thin books eliminates
# that vector. Override-able by env if a thinner-book product needs it.
# 2026-05-01: tightening from 5 → 1 after live test showed 5 was too
# strict for HG options at the wings (legitimate 1-3 size books were
# being rejected). 1 still blocks fully-empty-on-one-side states.
MIN_BBO_SIZE = int(os.environ.get("CORSAIR_TRADER_MIN_BBO_SIZE", "1"))


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

    def __init__(self, log_dir: str, prefix: str,
                 max_bytes: int = 256 * 1024 * 1024) -> None:
        self._log_dir = log_dir
        self._prefix = prefix
        self._fp = None
        self._date = None
        self._max_bytes = max_bytes
        self._bytes_written = 0
        self._part = 0

    def write(self, rec: dict) -> None:
        today = date.today().strftime("%Y-%m-%d")
        if today != self._date:
            self._roll(today)
        # Size-based rotation backstop. trader_decisions/events write
        # at ~30Hz which produces 500+MB/day at v1.4 cadences. Without
        # size rotation a multi-day session fills the disk. Roll to
        # .1, .2, .3, ... when hot file exceeds max_bytes.
        if self._fp is not None and self._bytes_written >= self._max_bytes:
            self._part += 1
            self._roll(today)
        if self._fp is None:
            return
        try:
            line = json.dumps(rec, default=str) + "\n"
            self._fp.write(line)
            self._bytes_written += len(line)
        except Exception:
            logger.exception("%s log write failed", self._prefix)

    def _roll(self, day: str) -> None:
        if self._fp is not None:
            try:
                self._fp.close()
            except Exception:
                pass
        if day != self._date:
            self._part = 0  # new day starts fresh part numbering
        self._date = day
        try:
            os.makedirs(self._log_dir, exist_ok=True)
            suffix = f".{self._part}" if self._part > 0 else ""
            path = os.path.join(
                self._log_dir, f"{self._prefix}-{day}.jsonl{suffix}")
            self._fp = open(path, "a", buffering=1)
            self._bytes_written = os.path.getsize(path) if os.path.exists(path) else 0
            logger.info("%s log opened: %s (existing %d bytes)",
                        self._prefix, path, self._bytes_written)
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
        # Trader-side TTT histogram (cleanup gap #2). During cut-over,
        # broker's _decision_tick_ns isn't populated, so the broker's
        # TTT histogram is empty. Trader measures its own
        # tick-arrive → place_order-emit latency instead, reports via
        # telemetry. Bounded ring of recent samples.
        self.ttt_us: deque[int] = deque(maxlen=500)
        # Per-key place cooldown timestamps (cleanup pass 4, 2026-05-01).
        # Maps (strike, expiry, right, side) → time.monotonic_ns of last
        # place attempt. Used to enforce COOLDOWN_NS between re-places.
        self._last_place_ns: dict = {}

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
        # NEW: risk_state freshness. Once seen, treat stale data as
        # if not seen at all — broker may have crashed or stopped
        # publishing. Fail closed.
        risk_age_s = (time.monotonic() - self.state.risk_state_age_ts
                      if self.state.risk_state_age_ts > 0
                      else float('inf'))
        if eff is None or risk_age_s > RISK_STATE_STALE_S:
            # No risk_state seen yet OR stale — refuse to place orders.
            # Decision log will show a "skip" with reason "risk_state_unknown"
            # / "risk_state_stale" so the operator can see how often this
            # fires.
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
        # CRITICAL: use the fit-time forward, not current spot. SVI's
        # `m` is anchored on log-moneyness relative to the fit forward;
        # broker's get_theo uses surface.forward (fit-time), so we must
        # too. Falls back to current spot when no fit yet — decision
        # would be skipped on no_vol_surface anyway.
        fit_forward = (vp_msg or {}).get("forward")
        decision_forward = float(fit_forward) if fit_forward else forward

        # ATM-window restriction. Don't quote deep wings where SVI
        # extrapolation is least reliable — that's where 2026-05-01
        # adverse fills concentrated. Use current spot (not fit_F) for
        # the ATM anchor; we want recently-tracked strikes.
        if abs(float(strike) - forward) > MAX_STRIKE_OFFSET_USD:
            self.decisions_made["skip_off_atm"] += 1
            return

        # OTM-ONLY restriction (cleanup pass 4b, 2026-05-01). Spec §12
        # in CLAUDE.md: asymmetric scope — calls quote only K >= F,
        # puts only K <= F. ITM strikes have thin flow + wide spreads;
        # quoting them consumes order budget without generating fill
        # volume. Live observation (cut-over): trader was placing both
        # BUY+SELL on K=5.6 even with F=5.96 — that's a deep ITM call
        # we shouldn't have been quoting. Tolerance is one half-tick on
        # the strike grid ($0.025 = half a strike-tick at HG's $0.05
        # strike spacing) so the ATM strike still quotes both rights
        # when F falls slightly above/below it.
        atm_tol = 0.025
        if right == "C" and float(strike) < forward - atm_tol:
            self.decisions_made["skip_itm"] += 1
            return
        if right == "P" and float(strike) > forward + atm_tol:
            self.decisions_made["skip_itm"] += 1
            return

        # NEW: extract bid/ask sizes for the thin-book guard.
        bid_size = tick_msg.get("bid_size")
        ask_size = tick_msg.get("ask_size")

        for side in ("BUY", "SELL"):
            d = decide_quote(
                forward=decision_forward,
                strike=float(strike),
                expiry=expiry,
                right=right,
                side=side,
                vol_params=vol_params,
                market_bid=bid,
                market_ask=ask,
                market_bid_size=bid_size,
                market_ask_size=ask_size,
                min_bbo_size=MIN_BBO_SIZE,
                fit_forward=decision_forward,
                current_forward=forward,
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
                existing = self._our_orders_by_key.get(key)
                send_ns_now = time.monotonic_ns()
                last_place_ns = self._last_place_ns.get(key, 0)

                # Dead-band + GTD-refresh logic (cleanup pass 5,
                # 2026-05-01). Replaces the strict same-price equality
                # check with a tick-band check, plus a force-refresh
                # branch when the resting order's GTD is about to
                # expire. Mirrors broker's _send_or_update — the
                # standard market-maker pattern of "only send when the
                # price actually moved meaningfully OR the existing
                # order is about to expire". Big rate reduction vs the
                # earlier strict-equality + cooldown stack: at quiet
                # markets, only ~1 place per (5-1.5)s = 3.5s per key
                # (the GTD refresh) instead of every theo wiggle.
                if existing is not None:
                    rest_price = existing.get("price")
                    age_s = ((send_ns_now - last_place_ns) / 1e9
                             if last_place_ns else float("inf"))
                    in_band = (rest_price is not None
                               and abs(d["price"] - rest_price)
                               < DEAD_BAND_TICKS * self.state.tick_size)
                    needs_gtd_refresh = age_s > (
                        GTD_LIFETIME_S - GTD_REFRESH_LEAD_S
                    )
                    if in_band and not needs_gtd_refresh:
                        # Quote price is close enough AND GTD has time
                        # left → no need to re-place.
                        self.decisions_made["skip_in_band"] += 1
                        continue

                # Hard cooldown floor as a defensive backstop. With
                # dead-band catching the common "1-tick wiggle" case,
                # cooldown rarely fires now — but keep it to bound any
                # pathological edge case. 250ms < GTD_REFRESH_LEAD_S so
                # GTD-refresh is never blocked by cooldown.
                if send_ns_now - last_place_ns < COOLDOWN_NS:
                    self.decisions_made["skip_cooldown"] += 1
                    continue
                # NEW: dark-book ON-PLACE guard — re-check the latest
                # tick state for this option. The decide_quote check
                # used the ticks at decision time. The book may have
                # gone dark while we were processing. Refuse if either
                # side has zero size or zero price now.
                latest_tick = self.state.options.get((float(strike), expiry, right))
                if latest_tick:
                    bid_now = latest_tick.get("bid")
                    ask_now = latest_tick.get("ask")
                    bsz = latest_tick.get("bid_size") or 0
                    asz = latest_tick.get("ask_size") or 0
                    if (not bid_now or bid_now <= 0 or
                            not ask_now or ask_now <= 0 or
                            bsz <= 0 or asz <= 0):
                        self.decisions_made["skip_dark_at_place"] += 1
                        continue
                # Cancel-before-replace (cleanup pass 6, 2026-05-01).
                # Without this, every re-place leaves the OLD orderId
                # at IBKR pending GTD-expiry (5s). Multiple stale orders
                # at the same key churn IBKR's order book and bloat
                # place_rtt_us (1+s p50). Sending an explicit cancel
                # immediately before the new place tells IBKR to drop
                # the stale order, freeing the slot.
                #
                # Skip cancel if old orderId unknown (place_ack hasn't
                # arrived yet) — the old order will GTD-expire on its
                # own. Skip if existing entry was already an unacked
                # request (orderId=None) since we have nothing to
                # cancel yet anyway.
                old_oid = existing.get("orderId") if existing else None
                if old_oid is not None:
                    self.client.send({
                        "type": "cancel_order",
                        "ts_ns": time.time_ns(),
                        "orderId": old_oid,
                    })
                    # Drop the orderId→key mapping; the cancel ack
                    # would do this too but we want to short-circuit
                    # the staleness loop from trying to re-cancel.
                    self._orderid_to_key.pop(old_oid, None)
                    self.decisions_made["replace_cancel"] += 1

                # Cleanup gap #2 — trader-side TTT instrumentation.
                # Measure broker-tick-emit (wall clock) → trader-place-
                # order-send (wall clock). Cross-process delta captures
                # full IPC + trader decision compute. Report via
                # telemetry; broker's TTT histogram stays empty during
                # cut-over because broker's _decision_tick_ns isn't
                # populated when its quote engine is gated off.
                send_ns = time.time_ns()
                tick_ns = tick_msg.get("ts_ns")
                if isinstance(tick_ns, int) and tick_ns > 0:
                    ttt_us = max(0, (send_ns - tick_ns) // 1000)
                    if 0 <= ttt_us < 5_000_000:  # cap at 5s sanity
                        self.ttt_us.append(ttt_us)
                self.client.send({
                    "type": "place_order",
                    "ts_ns": send_ns,
                    "strike": float(strike),
                    "expiry": expiry,
                    "right": right,
                    "side": side,
                    "qty": 1,
                    "price": d["price"],
                    "orderRef": "corsair_trader",
                })
                self._our_orders_by_key[key] = {
                    "price": d["price"], "ts_ns": send_ns,
                    "orderId": None,  # filled in by first non-terminal order_ack
                }
                self._last_place_ns[key] = send_ns_now

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
        elif t == "place_ack":
            # Broker confirms ib.placeOrder dispatched with this
            # orderId. Populate _orderid_to_key + _our_orders_by_key
            # immediately so the staleness loop can cancel even
            # fast-fill orders that go PendingSubmit → Filled with no
            # intermediate Submitted ack.
            try:
                oid = int(msg["orderId"])
                key = (
                    float(msg["strike"]), msg["expiry"],
                    msg["right"], msg["side"],
                )
                self._orderid_to_key[oid] = key
                if key in self._our_orders_by_key:
                    self._our_orders_by_key[key]["orderId"] = oid
                else:
                    self._our_orders_by_key[key] = {
                        "price": float(msg.get("price", 0.0)),
                        "ts_ns": time.time_ns(),
                        "orderId": oid,
                    }
            except Exception:
                pass
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

    async def staleness_loop(self) -> None:
        """Periodically cancel resting orders whose price has drifted
        too far from current theo. Mirrors broker's is_quote_stale.

        Without this, an order placed at ``theo - edge`` and left to
        rest 30s (GTD) can be matched adversely when theo drifts and
        our limit becomes the wrong-side-of-fair price. The guard fires
        at STALENESS_INTERVAL_S cadence and uses STALENESS_TICKS as the
        absolute-distance-from-theo threshold.
        """
        while True:
            await asyncio.sleep(STALENESS_INTERVAL_S)
            if not PLACES_ORDERS:
                continue
            if not self._our_orders_by_key:
                continue
            forward = self.state.underlying_price
            if forward <= 0:
                continue
            threshold = STALENESS_TICKS * self.state.tick_size

            # Snapshot keys to avoid mutating-during-iteration. Cancels
            # mutate _our_orders_by_key in _cancel_stale_order.
            for key, order_info in list(self._our_orders_by_key.items()):
                strike, expiry, right, side = key
                order_price = order_info.get("price")
                order_id = order_info.get("orderId")
                if order_price is None or order_id is None:
                    # Either no price set (shouldn't happen) or order
                    # not yet acked — can't cancel something the broker
                    # doesn't know about yet. Wait for next cycle.
                    continue
                # NEW (cleanup pass 4): dark-book ON-REST guard. Cancel
                # if the latest tick state shows the underlying market
                # has gone one-sided / dark / size-zero. The decide_quote
                # check ran at decision time only; this catches markets
                # that thinned out *while* our order was resting — which
                # is exactly when paper-IBKR's matching sweeps resting
                # orders for adverse fills (2026-05-01 15:06 burst:
                # 17 fills in 11 seconds, all at bid=0/ask=0).
                latest = self.state.options.get((strike, expiry, right))
                if latest:
                    bid_now = latest.get("bid")
                    ask_now = latest.get("ask")
                    bsz = latest.get("bid_size") or 0
                    asz = latest.get("ask_size") or 0
                    market_dark = (
                        not bid_now or bid_now <= 0
                        or not ask_now or ask_now <= 0
                        or bsz <= 0 or asz <= 0
                    )
                    if market_dark:
                        self._cancel_stale_order(
                            order_id, key, order_price, 0.0, 0.0,
                        )
                        self.decisions_made["staleness_cancel_dark"] = (
                            self.decisions_made.get("staleness_cancel_dark", 0) + 1
                        )
                        continue
                # Pick vol surface for this option's right; fall back to
                # the other side during bootstrap.
                vp_msg = (
                    self.state.vol_surface.get((expiry, right))
                    or self.state.vol_surface.get((expiry, "C"))
                    or self.state.vol_surface.get((expiry, "P"))
                )
                if not vp_msg:
                    continue
                vol_params = vp_msg.get("params")
                if not vol_params:
                    continue
                # Use fit-time forward (same reasoning as _decide_on_tick)
                fit_forward = vp_msg.get("forward")
                eval_forward = float(fit_forward) if fit_forward else forward
                try:
                    tte = time_to_expiry_years(expiry)
                    res = compute_theo(eval_forward, strike, tte, right, vol_params)
                except Exception:
                    continue
                if res is None:
                    continue
                _iv, theo = res
                # Stale if our price is too unfavorable vs current theo.
                # BUY: bad when we'd pay above theo (price > theo).
                # SELL: bad when we'd sell below theo (price < theo).
                if side == "BUY":
                    drift = order_price - theo
                else:
                    drift = theo - order_price
                if drift > threshold:
                    self._cancel_stale_order(
                        order_id, key, order_price, theo, drift,
                    )

    def _cancel_stale_order(self, order_id: int, key: tuple,
                            order_price: float, theo: float,
                            drift: float) -> None:
        """Send cancel_order to broker; drop from local maps."""
        if not self.client.connected:
            return
        self.client.send({
            "type": "cancel_order",
            "ts_ns": time.time_ns(),
            "orderId": order_id,
        })
        # Optimistic local cleanup. The order_ack with terminal status
        # would do the same, but doing it here means the next tick on
        # this key won't re-place against a stale dedupe entry.
        self._our_orders_by_key.pop(key, None)
        self._orderid_to_key.pop(order_id, None)
        self.decisions_made["staleness_cancel"] += 1
        if self.decisions_made["staleness_cancel"] % 10 == 1:
            logger.info(
                "staleness cancel: oid=%d %.2f%s %s @ %.4f vs theo %.4f "
                "(drift %.4f, threshold %.4f); total cancels=%d",
                order_id, key[0], key[2], key[3],
                order_price, theo, drift,
                STALENESS_TICKS * self.state.tick_size,
                self.decisions_made["staleness_cancel"],
            )

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
            # Trader-side TTT histogram (gap #2 fix).
            ttt = sorted(self.ttt_us)
            ttt_n = len(ttt)
            ttt_p50 = ttt[ttt_n // 2] if ttt_n else None
            ttt_p99 = ttt[int(ttt_n * 0.99)] if ttt_n else None
            payload = {
                "type": "telemetry",
                "ts_ns": time.time_ns(),
                "events": dict(self.state.event_counts.most_common()),
                "decisions": dict(self.decisions_made),
                "ipc_p50_us": p50,
                "ipc_p99_us": p99,
                "ipc_n": n,
                "ttt_p50_us": ttt_p50,
                "ttt_p99_us": ttt_p99,
                "ttt_n": ttt_n,
                "n_options": len(self.state.options),
                "n_active_orders": len(self.state.active_orders),
                "n_vol_expiries": len(self.state.vol_surface),
                "killed": list(self.state.kills.keys()),
                "weekend_paused": self.state.weekend_paused,
            }
            self.client.send(payload)
            logger.info(
                "telemetry: events=%d ipc_p50=%sus ipc_p99=%sus "
                "ttt_p50=%sus ttt_p99=%sus opts=%d "
                "orders=%d decisions=%s vol_expiries=%d",
                sum(self.state.event_counts.values()),
                p50, p99, ttt_p50, ttt_p99,
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
        staleness_task = asyncio.create_task(self.staleness_loop())

        try:
            await asyncio.gather(client_task, telemetry_task, welcome_task,
                                 staleness_task)
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
