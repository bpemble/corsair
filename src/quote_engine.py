"""Quote engine for Corsair v2.

For each quotable strike:
  - Compute bid/ask: penny-jump incumbent, bounded by theo ± buffer
  - Check that a hypothetical fill passes constraints (margin/delta/theta)
  - Send/update or cancel
"""

import logging
import time
from collections import deque
from datetime import datetime, timedelta, timezone
from typing import Deque, Dict, Optional, Tuple

from ib_insync import IB, LimitOrder

from .utils import round_to_tick

LATENCY_RING_SIZE = 500   # rolling window for TTT/RTT percentiles
# Minimum interval between successive amends to the same (strike, right, side).
# Without this, a flapping incumbent can produce multiple in-flight modifies
# that race each other and trip IB error 103 (Duplicate order id). 100ms is
# generous: it lets us react to material moves without bursting amends on
# every micro-tick.
MODIFY_COOLDOWN_MS = 100

logger = logging.getLogger(__name__)

OrderKey = Tuple[float, str, str]  # (strike, right, side)
ORDER_REF_PREFIX = "corsair2"

# Orders auto-cancel after this many seconds if not refreshed. The engine's
# last-resort deadman — bounds how long a quote can sit with stale data if
# a crash, container kill, network outage, or gateway hang slips past every
# other recovery layer. Set to 30s as the equilibrium between API churn
# (refresh frequency) and stale-quote risk; the watchdog handles common-case
# hang detection at ~20s so the GTD only needs to be a backstop. The spec
# nominally calls for "~60s" but we deliberately tightened after building
# the watchdog (the spec was drafted before that discussion).
GTD_EXPIRY_SECONDS = 30
# Re-send the order to refresh GTD when less than this many seconds remain.
# With 30s expiry and 10s threshold, an unchanged order is refreshed every ~20s.
GTD_REFRESH_THRESHOLD_SECONDS = 10


def _gtd_string(seconds_from_now: int = GTD_EXPIRY_SECONDS) -> str:
    """Return an IB-formatted goodTillDate string N seconds in the future."""
    dt = datetime.now(tz=timezone.utc) + timedelta(seconds=seconds_from_now)
    return dt.strftime("%Y%m%d %H:%M:%S") + " UTC"


def should_quote_side(
    portfolio, option, side: str, quote_price: float,
    constraint_checker, sabr, config,
) -> Tuple[bool, str]:
    """Check all constraints and filters for a potential quote.

    Returns (should_quote, rejection_reason).
    """
    # Constraint check (margin + delta + theta)
    passes, reason = constraint_checker.check_constraints(option, side)
    if not passes:
        return False, reason

    # Stale-quote filter (theo buffer is enforced at price construction)
    if config.pricing.sabr_enabled and sabr.last_calibration is not None:
        if sabr.is_quote_stale(option):
            return False, "stale_quote"

    return True, "ok"


class QuoteManager:
    """Manages quote lifecycle: send, update, cancel orders on IBKR."""

    def __init__(self, ib: IB, config, market_data, sabr, constraint_checker,
                 csv_logger=None):
        self.ib = ib
        self.config = config
        self.market_data = market_data
        self.sabr = sabr
        self.constraint_checker = constraint_checker
        self.csv_logger = csv_logger
        self.active_orders: Dict[OrderKey, int] = {}  # {(strike, right, side): order_id}
        self._order_id_to_trade: Dict[int, object] = {}  # order_id -> Trade object
        self._account = config.account.account_id
        self._last_sabr_attempt: Optional[datetime] = None
        self._last_sabr_forward: float = 0.0  # underlying at last calibration
        # Last-known meaningful incumbent (for snapshot/dashboard), per (strike, right).
        self._incumbent_bid: Dict[Tuple[float, str], float] = {}
        self._incumbent_ask: Dict[Tuple[float, str], float] = {}
        # Latency rings (microseconds). TTT = tick→placeOrder. RTT = placeOrder→Submitted ack.
        self._ttt_us: Deque[int] = deque(maxlen=LATENCY_RING_SIZE)
        self._rtt_us: Deque[int] = deque(maxlen=LATENCY_RING_SIZE)
        self._pending_rtt: Dict[int, int] = {}  # order_id -> placeOrder ns
        # Order placement time per order_id, for fill latency (place→fill).
        # Cleared on fill_handler when the fill arrives. Distinct from
        # _pending_rtt which clears on Submitted ack.
        self._placed_at_ns: Dict[int, int] = {}
        # Most recent tick_received_ns per (strike, right) at decision time.
        self._decision_tick_ns: Dict[Tuple[float, str], int] = {}
        # Last successful amend time per (strike, right, side) — for cooldown.
        self._last_modify_ns: Dict[OrderKey, int] = {}

    def _our_prices_at(self, strike: float, right: str) -> set:
        """Return set of our resting limit prices at this (strike, right)
        for self-filter use in find_incumbent."""
        out = set()
        for side in ("BUY", "SELL"):
            oid = self.active_orders.get((strike, right, side))
            if oid:
                t = self._order_id_to_trade.get(oid)
                if t and self._is_order_live(t):
                    out.add(float(t.order.lmtPrice))
        return out

    def update_quotes(self, portfolio, dirty: Optional[set] = None):
        """Reprice and re-issue quotes.

        If `dirty` is provided, only those (strike, right) pairs are evaluated
        (event-driven path: caller has identified which options received ticks
        since the last cycle). If None, every quotable (strike, right) is
        evaluated (periodic full-refresh path).
        """
        state = self.market_data.state
        config = self.config

        # Recalibrate SABR if needed.
        # Two triggers: (a) periodic interval since last fit, (b) underlying
        # has moved more than `sabr_fast_recal_dollars` since last fit. The
        # fast trigger keeps theo fresh during sharp moves where the periodic
        # interval would lag the market by tens of seconds.
        if config.pricing.sabr_enabled and state.underlying_price > 0:
            now = datetime.now()
            elapsed = 0
            forward_move = 0.0
            if self._last_sabr_attempt is not None:
                elapsed = (now - self._last_sabr_attempt).total_seconds()
                forward_move = abs(state.underlying_price - self._last_sabr_forward)
            fast_recal_dollars = float(getattr(
                config.pricing, "sabr_fast_recal_dollars", 10.0))
            should_recal = (
                self._last_sabr_attempt is None
                or elapsed >= config.pricing.sabr_recalibrate_seconds
                or forward_move >= fast_recal_dollars
            )
            if should_recal:
                self._last_sabr_attempt = now
                self._last_sabr_forward = state.underlying_price
                self.sabr.set_expiry(state.front_month_expiry)
                self.sabr.calibrate(state.underlying_price, state.options)

        quotable = self.market_data.get_quotable_strikes()  # list of (strike, right)
        quotable_set = set(quotable)

        if dirty is not None:
            iter_pairs = [pair for pair in quotable if pair in dirty]
        else:
            iter_pairs = quotable

        for strike, right in iter_pairs:
            option = state.get_option(strike, right=right)
            if option is None:
                continue

            self._decision_tick_ns[(strike, right)] = option.tick_received_ns

            # Theo from SABR (per right)
            theo = None
            if config.pricing.sabr_enabled and self.sabr.last_calibration is not None:
                try:
                    theo = self.sabr.get_theo(strike, right)
                except Exception:
                    theo = None

            our_prices = self._our_prices_at(strike, right)
            inc_bid_info = self.market_data.find_incumbent(strike, "BUY", our_prices, right=right)
            inc_ask_info = self.market_data.find_incumbent(strike, "SELL", our_prices, right=right)

            if inc_bid_info["price"] is not None:
                self._incumbent_bid[(strike, right)] = inc_bid_info["price"]
            if inc_ask_info["price"] is not None:
                self._incumbent_ask[(strike, right)] = inc_ask_info["price"]

            self._process_side(portfolio, option, strike, right, "BUY",
                               inc_bid_info, theo)
            self._process_side(portfolio, option, strike, right, "SELL",
                               inc_ask_info, theo)

        # On full refresh, sweep stale orders for any (strike, right, side) that
        # is no longer quotable.
        if dirty is None:
            for (strike, right, side) in list(self.active_orders.keys()):
                if (strike, right) not in quotable_set:
                    self._cancel_quote(strike, right, side)

    def _process_side(self, portfolio, option, strike: float, right: str,
                      side: str, inc_info: dict, theo: Optional[float]) -> None:
        """Apply skip-reason gate, theo-edge gate, constraint check, and
        order placement for a single (strike, right, side). Centralizes the
        bid/ask logic that used to be duplicated in update_quotes."""
        config = self.config
        tick = config.quoting.tick_size

        # Skip-reason gate from market_data
        if inc_info["skip_reason"]:
            # self_only = our order is the only level on this side; leave it
            # in place rather than cancelling and losing queue priority.
            if inc_info["skip_reason"] != "self_only":
                self._cancel_quote(strike, right, side)
            self._log_quote_telemetry(strike, right, side, None, inc_info, theo=theo)
            return

        # Penny-jump the incumbent
        if side == "BUY":
            jumped = inc_info["price"] + (tick * config.quoting.penny_jump_ticks)
        else:
            jumped = inc_info["price"] - (tick * config.quoting.penny_jump_ticks)
        adj = round_to_tick(jumped, tick)

        # Theo edge gate: reject if our price wouldn't sit min_edge_points
        # away from theo on the favorable side.
        if theo is not None and config.pricing.min_edge_points > 0:
            edge = config.pricing.min_edge_points
            violates = (adj > theo - edge) if side == "BUY" else (adj < theo + edge)
            if violates:
                self._cancel_quote(strike, right, side)
                self._log_quote_telemetry(strike, right, side, None,
                                          {**inc_info, "skip_reason": "theo_edge"},
                                          theo=theo)
                return

        if adj <= 0:
            self._cancel_quote(strike, right, side)
            self._log_quote_telemetry(strike, right, side, None,
                                      {**inc_info, "skip_reason": "invalid_price"},
                                      theo=theo)
            return

        can_quote, reason = should_quote_side(
            portfolio, option, side, adj,
            self.constraint_checker, self.sabr, config,
        )
        if can_quote:
            self._send_or_update(strike, right, side, adj,
                                 config.product.quote_size, option)
            self._log_quote_telemetry(strike, right, side, adj, inc_info, theo=theo)
        else:
            self._cancel_quote(strike, right, side)
            self._log_rejection(strike, right, side, reason)
            self._log_quote_telemetry(strike, right, side, None,
                                      {**inc_info, "skip_reason": reason},
                                      theo=theo)

    def _is_order_live(self, trade) -> bool:
        """Check if an order is still active (not dead/cancelled/filled)."""
        if trade is None:
            return False
        status = trade.orderStatus.status
        return status in ("PendingSubmit", "PreSubmitted", "Submitted")

    def _send_or_update(self, strike: float, right: str, side: str,
                        price: float, qty: int, option):
        """Send new order or modify existing if price changed."""
        key = (strike, right, side)
        contract = option.contract

        if contract is None:
            logger.warning("No contract for %s%s, cannot send order", int(strike), right)
            return

        if key in self.active_orders:
            order_id = self.active_orders[key]
            trade = self._order_id_to_trade.get(order_id)

            if not self._is_order_live(trade):
                self.active_orders.pop(key, None)
                self._order_id_to_trade.pop(order_id, None)
            elif trade.order.lmtPrice != price:
                # Modify cooldown: refuse to amend the same key more than
                # once per MODIFY_COOLDOWN_MS. Prevents in-flight modifies
                # from racing each other and tripping IB error 103.
                now_ns = time.monotonic_ns()
                last_ns = self._last_modify_ns.get(key, 0)
                if last_ns and (now_ns - last_ns) < MODIFY_COOLDOWN_MS * 1_000_000:
                    return
                trade.order.lmtPrice = price
                trade.order.totalQuantity = qty
                trade.order.goodTillDate = _gtd_string()
                self._record_send_latency(strike, right, trade.order.orderId)
                self.ib.placeOrder(trade.contract, trade.order)
                self._last_modify_ns[key] = now_ns
                return
            else:
                gtd_str = trade.order.goodTillDate or ""
                remaining = float("inf")
                try:
                    gtd_clean = gtd_str.replace(" UTC", "")
                    gtd_dt = datetime.strptime(gtd_clean, "%Y%m%d %H:%M:%S").replace(tzinfo=timezone.utc)
                    remaining = (gtd_dt - datetime.now(tz=timezone.utc)).total_seconds()
                except Exception:
                    remaining = 0
                if remaining < GTD_REFRESH_THRESHOLD_SECONDS:
                    trade.order.goodTillDate = _gtd_string()
                    self.ib.placeOrder(trade.contract, trade.order)
                return

        # Place a new order
        action = "BUY" if side == "BUY" else "SELL"
        order = LimitOrder(
            action=action,
            totalQuantity=qty,
            lmtPrice=price,
            tif="GTD",
            goodTillDate=_gtd_string(),
            account=self._account,
            orderRef=f"{ORDER_REF_PREFIX}_{int(strike)}{right}_{side}",
        )
        trade = self.ib.placeOrder(contract, order)
        self.active_orders[key] = trade.order.orderId
        self._order_id_to_trade[trade.order.orderId] = trade
        self._record_send_latency(strike, right, trade.order.orderId)
        trade.statusEvent += self._on_order_status

    def _record_send_latency(self, strike: float, right: str, order_id: int):
        """Capture TTT (tick→placeOrder) and arm RTT/fill timers for this order.

        TTT is only sampled when the underlying tick is fresh (<50ms old).
        Older ticks come from the periodic 1s fallback refresh, where the
        recorded delta is data age rather than compute latency.

        Two timers are armed:
          - _pending_rtt: cleared on Submitted ack (used for RTT histogram)
          - _placed_at_ns: cleared on fill (used for fill latency in fills.csv)
        """
        now_ns = time.monotonic_ns()
        tick_ns = self._decision_tick_ns.get((strike, right), 0)
        if tick_ns > 0:
            ttt_us = (now_ns - tick_ns) // 1000
            if 0 <= ttt_us < 50_000:
                self._ttt_us.append(ttt_us)
        self._pending_rtt[order_id] = now_ns
        # Only record the FIRST place time per order — modifies don't reset
        # the fill clock, since the same order id can fill at any moment
        # after the original submission.
        self._placed_at_ns.setdefault(order_id, now_ns)

    def fill_latency_ms(self, order_id: int) -> Optional[float]:
        """Return milliseconds from first placement to now for an order, or
        None if we don't have a record. Caller is responsible for invoking
        this in the fill handler at the moment a fill arrives."""
        placed_ns = self._placed_at_ns.pop(order_id, None)
        if placed_ns is None:
            return None
        return (time.monotonic_ns() - placed_ns) / 1_000_000.0

    def _on_order_status(self, trade):
        """Capture RTT when an order first reaches Submitted/PreSubmitted."""
        try:
            status = trade.orderStatus.status
            if status not in ("Submitted", "PreSubmitted"):
                return
            oid = trade.order.orderId
            sent_ns = self._pending_rtt.pop(oid, None)
            if sent_ns is None:
                return
            rtt_us = (time.monotonic_ns() - sent_ns) // 1000
            if 0 <= rtt_us < 5_000_000:
                self._rtt_us.append(rtt_us)
        except Exception:
            pass

    def get_latency_snapshot(self) -> dict:
        """Return rolling p50/p90/p99 in microseconds for TTT and RTT."""
        def pct(buf, p):
            if not buf:
                return None
            s = sorted(buf)
            idx = min(len(s) - 1, int(len(s) * p))
            return s[idx]
        return {
            "ttt_us": {"n": len(self._ttt_us),
                       "p50": pct(self._ttt_us, 0.50),
                       "p90": pct(self._ttt_us, 0.90),
                       "p99": pct(self._ttt_us, 0.99)},
            "rtt_us": {"n": len(self._rtt_us),
                       "p50": pct(self._rtt_us, 0.50),
                       "p90": pct(self._rtt_us, 0.90),
                       "p99": pct(self._rtt_us, 0.99)},
        }

    def _cancel_quote(self, strike: float, right: str, side: str):
        """Cancel a quote at a specific (strike, right, side)."""
        key = (strike, right, side)
        if key in self.active_orders:
            order_id = self.active_orders[key]
            trade = self._order_id_to_trade.get(order_id)
            if trade is not None:
                self.ib.cancelOrder(trade.order)
            del self.active_orders[key]
            self._order_id_to_trade.pop(order_id, None)

    def cancel_all_quotes(self):
        """Kill switch: cancel everything immediately."""
        for key, order_id in list(self.active_orders.items()):
            trade = self._order_id_to_trade.get(order_id)
            if trade is not None:
                try:
                    self.ib.cancelOrder(trade.order)
                except Exception as e:
                    logger.warning("Failed to cancel order %d: %s", order_id, e)
        self.active_orders.clear()
        self._order_id_to_trade.clear()
        logger.info("All quotes cancelled")

    def _log_quote_telemetry(self, strike: float, right: str, side: str,
                             our_price: Optional[float], info: dict,
                             theo: Optional[float] = None):
        """Emit per-quote telemetry row."""
        if self.csv_logger is None or not self.config.logging.log_quotes:
            return
        try:
            self.csv_logger.log_quote(
                strike=strike, side=side, our_price=our_price,
                incumbent_price=info.get("price"),
                incumbent_level=info.get("level"),
                incumbent_size=info.get("size"),
                incumbent_age_ms=info.get("age_ms"),
                bbo_width=info.get("bbo_width"),
                skip_reason=info.get("skip_reason", ""),
                theo=theo,
                put_call=right,
            )
        except Exception as e:
            logger.debug("quote telemetry log failed: %s", e)

    def _log_rejection(self, strike: float, right: str, side: str, reason: str):
        """Log a quote rejection."""
        if self.config.logging.log_rejections:
            logger.info("REJECT %s %d%s: %s", side, int(strike), right, reason)

    @property
    def active_quote_count(self) -> int:
        return len(self.active_orders)

    def get_active_quotes(self) -> Dict:
        """Return dict keyed by (strike, right, side) -> live quote info."""
        quotes = {}
        for (strike, right, side), order_id in self.active_orders.items():
            trade = self._order_id_to_trade.get(order_id)
            if trade is not None:
                quotes[(strike, right, side)] = {
                    "order_id": order_id,
                    "price": trade.order.lmtPrice,
                    "qty": trade.order.totalQuantity,
                    "status": trade.orderStatus.status if hasattr(trade, 'orderStatus') else "unknown",
                }
        return quotes

    def get_incumbent(self, strike: float, right: str = "C") -> Tuple[float, float]:
        """Return tracked incumbent (bid, ask) for a (strike, right)."""
        return (
            self._incumbent_bid.get((strike, right), 0.0),
            self._incumbent_ask.get((strike, right), 0.0),
        )

