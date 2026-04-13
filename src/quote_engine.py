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

from .discord_notify import send_alert
from ib_insync.order import OrderStatus
from ib_insync.util import UNSET_DOUBLE, UNSET_INTEGER

from .utils import ceil_to_tick, floor_to_tick, round_to_tick

LATENCY_RING_SIZE = 500   # rolling window for TTT/RTT/AMEND percentiles
# Soft cap on the in-flight tracking dicts (_pending_rtt, _placed_at_ns,
# _pending_amend, _rtt_captured_oids). When any of these grows past this
# size, we drop the oldest half. Prevents slow leaks when an order goes
# terminal without observation (e.g., GTD-expired before next snapshot).
TRACKING_DICT_MAX = 4_000

# Minimum lifetime before a freshly-placed order is eligible for cancellation.
# IBKR takes ~50-300ms to ack a new order; if we cancel inside that window the
# order goes PendingSubmit → PendingCancel without ever visiting Submitted, so
# the order is invisible on the book and we burn a place/cancel pair. 750ms
# covers the long-tail ack latency with headroom.
MIN_ORDER_LIFETIME_MS = 750
MIN_ORDER_LIFETIME_NS = MIN_ORDER_LIFETIME_MS * 1_000_000

logger = logging.getLogger(__name__)


class TokenBucket:
    """Simple monotonic-clock token bucket for outbound API rate limiting.

    Capacity = max burst, refill = sustained rate (tokens/sec). `try_consume`
    refills lazily on each call (no background task), returns True if a
    token was available and consumed, False if the bucket was empty.

    Single-asyncio-loop only — no thread safety. That matches our runtime
    (ib_insync runs everything on one event loop).

    Ported from corsair v1's order_manager.py rate limiter. Used to keep us
    under IBKR's documented ~50 msg/sec API throttle.
    """

    __slots__ = ("_capacity", "_refill", "_tokens", "_last_ns", "_drops")

    def __init__(self, capacity: float, refill_per_sec: float):
        self._capacity = float(capacity)
        self._refill = float(refill_per_sec)
        self._tokens = float(capacity)
        self._last_ns = time.monotonic_ns()
        self._drops = 0

    def try_consume(self, n: float = 1.0) -> bool:
        now_ns = time.monotonic_ns()
        elapsed = (now_ns - self._last_ns) / 1e9
        self._last_ns = now_ns
        self._tokens = min(self._capacity, self._tokens + elapsed * self._refill)
        if self._tokens >= n:
            self._tokens -= n
            return True
        self._drops += 1
        return False

    @property
    def drops(self) -> int:
        return self._drops

    @property
    def tokens(self) -> float:
        return self._tokens

OrderKey = Tuple[float, str, str, str]  # (strike, expiry, right, side)
ORDER_REF_PREFIX = "corsair2"


def resolve_enabled_expiries(tokens, subscribed: list) -> list:
    """Resolve config.quoting.enabled_expiries tokens to concrete YYYYMMDD
    strings using the live subscribed expiry list (front-first, sorted).

    Accepted token formats:
      - "front"       → subscribed[0]
      - "front+N"     → subscribed[N] (if present)
      - "back1"..     → subscribed[1]
      - explicit "YYYYMMDD" → passed through if present in subscribed

    Tokens that don't resolve are logged and skipped. Returns the resolved
    list in the order given. Deduped preserving order.
    """
    if not subscribed:
        return []
    out: list = []
    seen: set = set()
    for tok in tokens or []:
        resolved = None
        t = str(tok).strip().lower()
        if t == "front":
            resolved = subscribed[0]
        elif t.startswith("front+"):
            try:
                idx = int(t.split("+", 1)[1])
                if 0 <= idx < len(subscribed):
                    resolved = subscribed[idx]
            except ValueError:
                pass
        elif len(t) == 8 and t.isdigit():
            if t.upper() in subscribed:
                resolved = t.upper()
            else:
                # original casing
                if str(tok) in subscribed:
                    resolved = str(tok)
        if resolved is None:
            logger.warning(
                "enabled_expiries: token %r did not resolve against subscribed=%s",
                tok, subscribed,
            )
            continue
        if resolved in seen:
            continue
        seen.add(resolved)
        out.append(resolved)
    return out

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
    option, side: str,
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
        self.active_orders: Dict[OrderKey, int] = {}  # {(strike, expiry, right, side): order_id}
        # Consecutive update_quotes() exception counter — incremented by
        # main.py's catch block, reset on a successful cycle. The
        # watchdog reads this to detect quote-loop exception storms
        # (the failure mode where update_quotes throws every cycle but
        # the snapshot writer still runs, so the docker healthcheck
        # never trips and the system silently zombies).
        self.consecutive_quote_errors: int = 0
        # Outbound API token bucket — see TokenBucket docstring and
        # config.quoting.api_bucket_*. Wraps placeOrder and cancelOrder
        # only; reqGlobalCancel (panic path) is intentionally NOT gated
        # so the kill switch always reaches the wire.
        bucket_cap = float(getattr(config.quoting, "api_bucket_capacity", 250))
        bucket_refill = float(getattr(config.quoting, "api_bucket_refill_per_sec", 250))
        self._tb = TokenBucket(bucket_cap, bucket_refill)
        self._account = config.account.account_id
        self._last_sabr_attempt: Optional[datetime] = None
        self._last_sabr_forward: float = 0.0  # underlying at last calibration
        # Latency rings (microseconds). TTT = tick→placeOrder. RTT = placeOrder→Submitted ack.
        self._ttt_us: Deque[int] = deque(maxlen=LATENCY_RING_SIZE)
        # Place RTT: time from placeOrder() send to the first openOrder
        # echo from IBKR. Measured against openOrderEvent. We verified
        # 2026-04-09 that openOrder and orderStatus(Submitted/PreSubmitted)
        # arrive within ~100µs of each other on this paper gateway, so
        # measuring against openOrder is equivalent to v1's status-based
        # measurement. (Diagnostic ran a parallel rtt_status_us ring for
        # several minutes; both rings produced identical numbers and the
        # parallel ring was removed.)
        self._place_rtt_us: Deque[int] = deque(maxlen=LATENCY_RING_SIZE)
        # Amend RTT — separate from _place_rtt_us because modifies dominate steady
        # state and they have a different latency profile than fresh places
        # (no order-id allocation, no permission validation, just price update).
        self._amend_us: Deque[int] = deque(maxlen=LATENCY_RING_SIZE)
        self._pending_rtt: Dict[int, int] = {}  # order_id -> placeOrder ns
        # Pending amend tracking: orderId -> most-recent sent_ns.
        # Keyed by orderId only. We tried (oid, price) to make rapid
        # back-to-back modifies independent, but openOrder ack messages
        # don't echo the price we just sent — they carry whatever IBKR
        # currently has, which lags by one or two modifies on a busy
        # order. The (oid, price) key never matched, the stash leaked,
        # and surviving samples were biased to the rare quiet acks. With
        # oid-only, the LATEST send wins: each ack pops the most recent
        # timestamp and we measure send→ack on whatever was last in
        # flight. Slightly overcounts on contiguous bursts (multiple
        # modifies before any ack collapse to one sample) but that's
        # honest — the user-visible ack-to-send delay IS the latest one.
        self._pending_amend: Dict[int, int] = {}
        # Order placement time per order_id, for fill latency (place→fill).
        # Cleared on fill_handler when the fill arrives. Distinct from
        # _pending_rtt which clears on Submitted ack.
        self._placed_at_ns: Dict[int, int] = {}
        # Underlying price at last place/modify per order_id. Used by
        # delta-retreat to detect when F has moved enough that the resting
        # order should be shifted by delta × ΔF instead of penny-jumping.
        self._order_underlying: Dict[int, float] = {}
        # Most recent tick_received_ns per (strike, expiry, right) at decision time.
        self._decision_tick_ns: Dict[Tuple[float, str, str], int] = {}
        # Resolved enabled_expiries cache. Refreshed each cycle in update_quotes
        # from the live state.expiries list. Tokens from config.quoting.enabled_expiries
        # are resolved via resolve_enabled_expiries().
        self._enabled_expiries_resolved: list = []
        # Consecutive theo_edge skip count per key — used for hysteresis so a
        # single tick of theo_edge violation doesn't drop a resting order
        # that would re-qualify on the next tick. Reset on any non-theo path
        # through _process_side (place or other gate).
        self._theo_edge_streak: Dict[OrderKey, int] = {}
        # orderIds we've already extracted RTT for (so we don't double-count
        # on subsequent snapshot passes).
        self._rtt_captured_oids: set = set()
        # Modify-storm guard (defense vector #14): per-orderId rolling
        # window of recent modify send timestamps (monotonic ns). When a
        # single order receives more than max_modifies_per_sec_per_oid
        # modifies in 1s we cancel-replace instead of issuing more amends.
        # Without this guard a tight reprice loop on a flickering BBO can
        # generate Error 103 cascades and zombie the order.
        self._modify_times_per_oid: Dict[int, Deque[int]] = {}
        self._max_modifies_per_sec_per_oid = int(getattr(
            config.quoting, "max_modifies_per_sec_per_oid", 5))
        # Global per-side resting-order cap (defense vector #11). Hard
        # ceiling on total active BUY or SELL orders across all strikes.
        # Belt-and-suspenders against a logic bug that would queue orders
        # without bound; the rate-limit token bucket bounds messages/sec
        # but not steady-state outstanding count.
        self._max_resting_per_side = int(getattr(
            config.quoting, "max_resting_per_side", 60))
        # Theo-vs-incumbent sanity gate (defense vector #3): if SABR theo
        # disagrees with the market mid by more than this many ticks, treat
        # Per-cycle canonical trade index — populated by _build_our_prices_index
        # at the top of each update_quotes / get_active_quotes call. Reading
        # from this dict is O(1) vs an O(N) walk of openTrades, which matters
        # at 4Hz × ~50 active orders. Cleared opportunistically; readers fall
        # back to walking openTrades when the cache is empty (cold path).
        self._canonical_idx: Dict[int, object] = {}

        # Event-driven RTT/AMEND measurement: subscribe once to the IB-level
        # openOrderEvent, which fires whenever IBKR sends an openOrder message
        # in response to a place or modify. The handler pops _pending_rtt /
        # _pending_amend and records latency in their respective rings. This
        # replaces the snapshot-quantized _capture_rtt_from_log path that was
        # adding 0-250ms uniform quantization noise (4Hz polling cadence) on
        # top of real ~1ms amend latency, making the metric meaningless.
        self.ib.openOrderEvent += self._on_open_order_ack
        # Error 104 ("Cannot modify a filled order") cleanup. ib_insync
        # surfaces every TWS error via errorEvent(reqId, code, msg, contract).
        # Without this hook, after a fill races our modify the active_orders
        # entry keeps pointing at the dead orderId and every subsequent quote
        # cycle re-attempts the same modify, which IBKR rejects with another
        # 104 — generating tens of thousands of log lines per session and
        # leaving stale state for that strike/right/side until something else
        # cleans it. Drop the entry on the first 104 so the next cycle places
        # a fresh order. Counter is for telemetry / sanity-check.
        self._error_104_count: int = 0
        # Market data blackout flag — set on Error 10197 (competing session).
        # While set, update_quotes is a no-op and all resting orders have been
        # cancelled. Cleared only when fresh option ticks arrive.
        self._market_data_blackout: bool = False
        self._blackout_cancel_sent: bool = False
        self.ib.errorEvent += self._on_ib_error

    def _canonical_trade(self, order_id: int):
        """Return the canonical (latest) Trade object for an orderId, or
        None if it's no longer open.

        Hot path: reads from `self._canonical_idx`, which is populated once
        per cycle by `_build_our_prices_index`. The cached entry is the same
        Trade *instance* that ib_insync mutates in place, so reading its
        `.orderStatus.status` always returns the latest value even between
        cache rebuilds.

        Cold path: when the cache is empty (e.g., a caller outside the
        update_quotes / get_active_quotes flow), fall back to walking
        openTrades. This preserves correctness if the call site changes.

        Why we don't cache the placeOrder return value: ib_insync sometimes
        constructs a NEW Trade object when an openOrder callback fires
        (notably after reqAutoOpenOrders adopts the order on clientId=0).
        The Trade returned by placeOrder becomes an orphan that nobody
        updates — it'd stay at PendingSubmit forever. The fix is to always
        re-resolve from the canonical store.

        Subtlety: openTrades() can return MULTIPLE Trade objects with the
        same orderId — the original (stale) Trade and a fresh one from the
        openOrder callback. We MUST return the LAST match in the iteration
        (the canonical one with up-to-date status), not the first.
        """
        cached = self._canonical_idx.get(order_id)
        if cached is not None:
            return cached
        latest = None
        for t in self.ib.openTrades():
            if t.order.orderId == order_id:
                latest = t
        return latest

    def _build_our_prices_index(self) -> Dict[Tuple[float, str, str, str], set]:
        """One-pass build of {(strike, expiry, right, side): set(prices)} for
        self-filter use across an entire update_quotes cycle. Walks
        openTrades exactly once per cycle instead of N times.

        Keyed by side (BUY/SELL) so the self-filter for the bid incumbent
        only sees our resting BUY prices, not our SELL prices (and vice
        versa). Without this, a SELL at $X causes is_self($X) to fire on
        the BUY path, falling back to a stale _last_clean_bid cache and
        penny-jumping an old price — leaving the bid many ticks behind.

        Two-step to handle ib_insync's multi-Trade-per-orderId quirk: first
        build {orderId: canonical_trade} (last-write-wins picks the canonical
        non-orphan instance), then bucket by (strike, expiry, right, side).

        Side effect: populates `self._canonical_idx` so `_canonical_trade()`
        can do O(1) lookups during the rest of the cycle instead of walking
        openTrades on every call.
        """
        # Last-write-wins per CLAUDE.md §2: when openTrades() returns
        # multiple Trades for the same orderId, the canonical (live)
        # instance is whichever one ib_insync's openOrder callback
        # constructed most recently — that's the one receiving real
        # status updates. The placeOrder-return Trade is the orphan and
        # gets stuck at PendingSubmit. Don't try to be clever with a
        # clientId tiebreak: on FA logins the canonical entry can carry
        # clientId=-1 (master adoption), so a "prefer my clientId"
        # tiebreak would pick exactly the orphan we want to avoid.
        canonical: Dict[int, object] = {}
        for t in self.ib.openTrades():
            ref = getattr(t.order, "orderRef", "") or ""
            if ref.startswith(ORDER_REF_PREFIX):
                canonical[t.order.orderId] = t
        self._canonical_idx = canonical  # cache for _canonical_trade reads
        out: Dict[Tuple[float, str, str, str], set] = {}
        for t in canonical.values():
            if not self._is_order_live(t):
                continue
            c = t.contract
            if c.symbol != self.config.product.underlying_symbol:
                continue
            exp = getattr(c, "lastTradeDateOrContractMonth", "") or ""
            side = "BUY" if t.order.action == "BUY" else "SELL"
            key = (float(c.strike), exp, c.right, side)
            out.setdefault(key, set()).add(float(t.order.lmtPrice))
        return out

    def maybe_recal_sabr(self) -> None:
        """Recalibrate SABR if the periodic interval has elapsed OR the
        underlying has moved more than sabr_fast_recal_dollars since the
        last fit. Called from the main loop, NOT from update_quotes — see
        the comment in update_quotes for why.
        """
        config = self.config
        state = self.market_data.state
        if not (config.pricing.sabr_enabled and state.underlying_price > 0):
            return
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
            self.sabr.set_expiries(state.expiries)
            self.sabr.calibrate(state.underlying_price, state.options)

    def update_quotes(self, portfolio, dirty: Optional[set] = None):
        """Reprice and re-issue quotes.

        Iterates every enabled expiry × (strike, right) in the subscribed
        quoting window. If `dirty` is provided, only those (strike, expiry,
        right) triples are evaluated (event-driven path: caller has
        identified which options received ticks since the last cycle). If
        None, every quotable combo is evaluated (periodic full-refresh).

        Multi-expiry: resolves config.quoting.enabled_expiries tokens
        against state.expiries each cycle (cheap, captures reconnects and
        expiry rollover). Quoting is a no-op when no tokens resolve.
        """
        state = self.market_data.state
        config = self.config

        # Market data blackout guard: if Error 10197 fired, refuse to place
        # or modify any orders until fresh option ticks confirm the feed is
        # back. Check the freshest option tick age — if any option ticked
        # within the last 2 seconds, the feed has recovered.
        if self._market_data_blackout:
            if state.options:
                from datetime import datetime
                freshest = min(
                    (datetime.now() - opt.last_update).total_seconds()
                    for opt in state.options.values()
                )
                if freshest < 2.0:
                    logger.critical(
                        "MARKET DATA BLACKOUT cleared: fresh option tick "
                        "received (age=%.1fs) — resuming quoting", freshest
                    )
                    self._market_data_blackout = False
                    self._blackout_cancel_sent = False
                    send_alert(
                        "BLACKOUT CLEARED",
                        "Option data feed restored. Resuming quoting.",
                        color=0x2ECC71,
                    )
                else:
                    return  # still dark
            else:
                return  # no options at all

        # SABR recalibration is hoisted out of this hot path and runs from
        # main.py's loop via maybe_recal_sabr() — calibration takes several ms
        # and was inflating TTT (tick→placeOrder) by landing between the tick
        # handler and the per-strike send. Theo for this cycle uses whatever
        # surface was last calibrated.

        enabled_tokens = getattr(config.quoting, "enabled_expiries", ["front"])
        self._enabled_expiries_resolved = resolve_enabled_expiries(
            enabled_tokens, state.expiries or []
        )
        if not self._enabled_expiries_resolved:
            return  # nothing to quote (config misconfigured or pre-discovery)

        # Full set of (strike, expiry, right) tuples we're eligible to quote
        # this cycle, unioned across all enabled expiries. Used for the
        # end-of-cycle stale sweep.
        full_quotable: set = set()
        # Per-expiry quotable lists for iteration
        per_expiry_quotable: Dict[str, list] = {}
        for exp in self._enabled_expiries_resolved:
            pairs = self.market_data.get_quotable_strikes(expiry=exp)
            per_expiry_quotable[exp] = pairs
            for strike, right in pairs:
                full_quotable.add((strike, exp, right))

        # Precompute our resting prices once per cycle. Avoids walking
        # ib.openTrades() twice per (strike, expiry, right) inside the loop.
        our_prices_idx = self._build_our_prices_index()
        _empty_set: set = set()

        # Cache margin state once per cycle for the wide_market bypass gate
        # (avoids a full SPAN portfolio walk per-strike).
        self._cycle_margin_ceiling = (float(config.constraints.capital)
                                      * float(config.constraints.margin_ceiling_pct))
        self._cycle_cur_margin = self.constraint_checker.margin.get_current_margin()

        for exp, pairs in per_expiry_quotable.items():
            if dirty is not None:
                # Dirty may be either (strike, right) or (strike, exp, right).
                # Support both shapes so event-driven callers that are not
                # expiry-aware keep working.
                iter_pairs = [
                    (s, r) for (s, r) in pairs
                    if (s, r) in dirty or (s, exp, r) in dirty
                ]
            else:
                iter_pairs = pairs

            # Sort by absolute delta descending so near-ATM (high-delta)
            # strikes get processed first, minimising TTT where it matters
            # most. Wings with |delta| near 0 go last.
            iter_pairs = sorted(
                iter_pairs,
                key=lambda sr: abs(
                    getattr(state.get_option(sr[0], expiry=exp, right=sr[1]),
                            'delta', 0) or 0),
                reverse=True,
            )

            # Per-expiry constants (avoid re-fetching per-strike)
            _exp_cal_ready = (config.pricing.sabr_enabled
                              and self.sabr.get_last_calibration(exp) is not None)
            _exp_cal_forward = self.sabr.get_forward(exp) if _exp_cal_ready else 0.0
            _exp_underlying = state.underlying_price

            for strike, right in iter_pairs:
                option = state.get_option(strike, expiry=exp, right=right)
                if option is None:
                    continue

                self._decision_tick_ns[(strike, exp, right)] = option.tick_received_ns

                # Theo from SVI/SABR (per expiry × right)
                theo = None
                if _exp_cal_ready:
                    try:
                        theo = self.sabr.get_theo(strike, right, expiry=exp)
                    except Exception:
                        theo = None

                our_bid_prices = our_prices_idx.get((strike, exp, right, "BUY"), _empty_set)
                our_ask_prices = our_prices_idx.get((strike, exp, right, "SELL"), _empty_set)
                inc_bid_info = self.market_data.find_incumbent(
                    strike, "BUY", our_bid_prices, right=right, expiry=exp)
                inc_ask_info = self.market_data.find_incumbent(
                    strike, "SELL", our_ask_prices, right=right, expiry=exp)

                self._process_side(portfolio, option, strike, exp, right, "BUY",
                                   inc_bid_info, theo,
                                   _exp_cal_forward, _exp_underlying)
                self._process_side(portfolio, option, strike, exp, right, "SELL",
                                   inc_ask_info, theo,
                                   _exp_cal_forward, _exp_underlying)

        # On full refresh, sweep stale orders for any (strike, expiry, right, side)
        # whose (strike, expiry, right) is no longer in the full quotable set.
        # This also cancels orders from expiries that are no longer enabled.
        if dirty is None:
            for (strike, exp, right, side) in list(self.active_orders.keys()):
                if (strike, exp, right) not in full_quotable:
                    self._cancel_quote(strike, exp, right, side)

    def _process_side(self, portfolio, option, strike: float, expiry: str,
                      right: str, side: str, inc_info: dict,
                      theo: Optional[float],
                      cal_forward: float = 0.0,
                      current_underlying: float = 0.0) -> None:
        """Apply skip-reason gate, theo-edge gate, constraint check, and
        order placement for a single (strike, expiry, right, side)."""
        config = self.config
        tick = config.quoting.tick_size

        # Delta-based instant reprice: adjust theo for underlying movement
        # since the last SVI/SABR calibration. First-order Taylor expansion
        # collapses repricing latency from ~300ms (full recal) to <1ms.
        if theo is not None and option is not None:
            delta = option.delta
            if delta != 0 and cal_forward > 0 and current_underlying > 0:
                theo = max(theo + delta * (current_underlying - cal_forward), 0.01)

        # Skip-reason gate from market_data
        #
        # Margin-escape bypass (defense vector #16, 2026-04-09):
        # when the incumbent gate returns `wide_market` for a strike
        # where we hold a position AND closing it would reduce margin
        # AND margin is currently breached, bypass the skip and post a
        # mid-anchored passive order. Without this the system wedges —
        # constraint checker says "close to free margin," market_data
        # says "spread too wide," and nothing happens.
        #
        # Gated on margin breach: if margin is fine, let positions sit.
        # Posting closing orders into wide markets when there's no
        # urgency to unwind just generates adverse fills for no benefit.
        _bypass_wide_market = False
        if (inc_info["skip_reason"] == "wide_market"
                and theo is not None and theo > 0):
            if self._cycle_cur_margin > self._cycle_margin_ceiling:
                pos_qty = 0
                for _p in portfolio.positions:
                    if (_p.strike == strike and _p.expiry == expiry
                            and _p.put_call == right):
                        pos_qty = _p.quantity
                        break
                is_closing = ((pos_qty > 0 and side == "SELL")
                              or (pos_qty < 0 and side == "BUY"))
                if is_closing:
                    _bypass_wide_market = True
                    logger.info(
                        "wide_market bypass: closing %+d %s %s%.0f via mid anchor "
                        "(margin=$%.0f > $%.0f ceiling, spread=%.2f, theo=%.2f)",
                        pos_qty, expiry[-4:], right, strike,
                        self._cycle_cur_margin, self._cycle_margin_ceiling,
                        inc_info.get("bbo_width") or 0.0, theo,
                    )

        if inc_info["skip_reason"] and not _bypass_wide_market:
            # self_only = our order is the only level on this side; leave it
            # in place rather than cancelling and losing queue priority.
            if inc_info["skip_reason"] != "self_only":
                self._cancel_quote(strike, expiry, right, side)
            self._log_quote_telemetry(strike, right, side, None, inc_info, theo=theo)
            return

        # Penny-jump the incumbent — or, in the wide_market bypass
        # case, synthesize a mid-anchored price to close the position.
        # Uses market mid (not theo) as the anchor because theo can be
        # $5+ off mid on wide-spread near-ATM strikes, leading to
        # adverse fills where we think we have edge but are actually
        # on the wrong side of fair.
        #
        # Delta-retreat: when F has moved more than delta_retreat_threshold
        # since the order was last placed/modified, shift the resting price
        # by delta × ΔF instead of penny-jumping the incumbent. This
        # protects against AS during fast moves where the incumbent is
        # also stale. Falls through to penny-jump in calm markets.
        _retreated = False
        if _bypass_wide_market:

            edge_floor = float(config.pricing.min_edge_points or 0.0)
            mkt_bid, mkt_ask = self.market_data.get_clean_bbo(
                strike, right, expiry=expiry)
            if mkt_bid > 0 and mkt_ask > 0:
                mid = (mkt_bid + mkt_ask) / 2.0
            else:
                mid = theo  # fall back to theo if no BBO
            if side == "BUY":
                anchor = mid - edge_floor
                adj = floor_to_tick(anchor, tick)
            else:  # SELL
                anchor = mid + edge_floor
                adj = ceil_to_tick(anchor, tick)
        else:
            retreat_threshold = float(
                getattr(config.quoting, "delta_retreat_threshold", 0))
            key = (strike, expiry, right, side)
            if (retreat_threshold > 0
                    and key in self.active_orders
                    and option is not None
                    and current_underlying > 0):
                oid = self.active_orders[key]
                f_at_modify = self._order_underlying.get(oid)
                if f_at_modify is not None and f_at_modify > 0:
                    f_move = current_underlying - f_at_modify
                    if abs(f_move) > retreat_threshold:
                        trade = self._canonical_trade(oid)
                        if trade is not None:
                            old_price = trade.order.lmtPrice
                            delta = option.delta
                            adj = round_to_tick(
                                old_price + delta * f_move, tick)
                            _retreated = True

            if not _retreated:
                if side == "BUY":
                    jumped = inc_info["price"] + (tick * config.quoting.penny_jump_ticks)
                else:
                    jumped = inc_info["price"] - (tick * config.quoting.penny_jump_ticks)
                adj = round_to_tick(jumped, tick)

        # Theo-aware aggression cap (defense vector #15, added 2026-04-09).
        # Never place a bid above (theo − min_edge) or an offer below
        # (theo + min_edge), regardless of what the incumbent is doing.
        #
        # Why: during fast underlying moves, other MMs are slow to reprice
        # their option bids/asks. If we penny-jump a stale incumbent in a
        # wide market, we become the best bid at a price that's now above
        # true fair — and the next natural seller lifts us. The theo-edge
        # gate below is supposed to catch this, but it fires as an all-or-
        # nothing CANCEL and only after the violation is strong enough to
        # clear the hysteresis. This cap is a softer intervention: slide
        # the quote *down* to the edge of fair instead of cancelling the
        # strike entirely, so we stay resting at a safer price and retain
        # queue priority.
        #
        # Uses math.floor/ceil to round directionally: for a BUY cap we
        # want the highest tick at or below (theo − edge); for a SELL cap
        # we want the lowest tick at or above (theo + edge). round_to_tick
        # is nearest-tick and could land on the wrong side by one tick.
        if theo is not None and config.pricing.min_edge_points > 0:

            edge_floor = config.pricing.min_edge_points
            if side == "BUY":
                cap_raw = theo - edge_floor
                capped = floor_to_tick(cap_raw, tick)
                if adj > capped:
                    adj = capped
            else:  # SELL
                cap_raw = theo + edge_floor
                capped = ceil_to_tick(cap_raw, tick)
                if adj < capped:
                    adj = capped

        # Behind-incumbent gate: if the theo cap pushed our price to or
        # behind the incumbent (bid at/below best bid, ask at/above best
        # ask), the order has zero queue value — we'd match the incumbent
        # with worse time priority. Cancel rather than rest dead weight.
        if inc_info["price"] is not None and not _bypass_wide_market:
            behind = ((side == "BUY" and adj <= inc_info["price"])
                      or (side == "SELL" and adj >= inc_info["price"]))
            if behind:
                self._cancel_quote(strike, expiry, right, side)
                self._log_quote_telemetry(
                    strike, right, side, None,
                    {**inc_info, "skip_reason": "behind_incumbent"},
                    theo=theo,
                )
                return

        # Theo edge gate: reject if our price wouldn't sit min_edge_points
        # away from theo on the favorable side. Hysteresis: require N
        # consecutive violations on the same key before cancelling so a
        # single boundary tick doesn't churn a resting order. Default N=1
        # (no hysteresis); set theo_edge_hysteresis_ticks: 2 in config to
        # smooth at the cost of slightly higher behind% on stale quotes.
        key = (strike, expiry, right, side)
        if theo is not None and config.pricing.min_edge_points > 0:
            edge = config.pricing.min_edge_points
            hyst = int(getattr(config.pricing, "theo_edge_hysteresis_ticks", 1))
            violates = (adj > theo - edge) if side == "BUY" else (adj < theo + edge)
            if violates:
                streak = self._theo_edge_streak.get(key, 0) + 1
                self._theo_edge_streak[key] = streak
                if streak >= hyst:
                    self._cancel_quote(strike, expiry, right, side)
                self._log_quote_telemetry(strike, right, side, None,
                                          {**inc_info, "skip_reason": "theo_edge"},
                                          theo=theo)
                return
            else:
                self._theo_edge_streak.pop(key, None)

        if adj <= 0:
            self._cancel_quote(strike, expiry, right, side)
            self._log_quote_telemetry(strike, right, side, None,
                                      {**inc_info, "skip_reason": "invalid_price"},
                                      theo=theo)
            return

        can_quote, reason = should_quote_side(
            option, side,
            self.constraint_checker, self.sabr, config,
        )
        if can_quote:
            self._send_or_update(strike, expiry, right, side, adj,
                                 config.product.quote_size, option)
            self._log_quote_telemetry(strike, right, side, adj, inc_info, theo=theo)
        else:
            self._cancel_quote(strike, expiry, right, side)
            self._log_rejection(strike, right, side, reason)
            self._log_quote_telemetry(strike, right, side, None,
                                      {**inc_info, "skip_reason": reason},
                                      theo=theo)

    def _try_place_order(self, contract, order):
        """Token-bucketed wrapper around `ib.placeOrder`.

        Returns the Trade on success, None if the bucket was empty (call
        was dropped). Callers that need to track Trade state must handle
        the None case — typically by leaving `active_orders` untouched
        so the next quote cycle re-attempts naturally.
        """
        if not self._tb.try_consume(1.0):
            if self._tb.drops % 100 == 1:
                logger.warning(
                    "API token bucket empty — dropping placeOrder (drops=%d, tokens=%.1f)",
                    self._tb.drops, self._tb.tokens,
                )
            return None
        return self.ib.placeOrder(contract, order)

    def _try_cancel_order(self, order_obj) -> bool:
        """Token-bucketed wrapper around `ib.cancelOrder`. Returns True if
        the cancel was sent, False if the bucket was empty (dropped)."""
        if not self._tb.try_consume(1.0):
            if self._tb.drops % 100 == 1:
                logger.warning(
                    "API token bucket empty — dropping cancelOrder (drops=%d, tokens=%.1f)",
                    self._tb.drops, self._tb.tokens,
                )
            return False
        try:
            self.ib.cancelOrder(order_obj)
            return True
        except Exception as e:
            logger.warning("cancelOrder failed: %s", e)
            return False

    @staticmethod
    def _strip_contamination_fields(order) -> None:
        """Reset VOL / delta-neutral / algo / reference fields to ib_insync's
        UNSET sentinels before a modify-path placeOrder.

        Background: ib_insync's wrapper mutates `trade.order` in place from
        every `openOrder` callback. If a TWS client logs into the same
        account, IBKR sends openOrder messages that include populated VOL
        fields, which contaminate our reference. The next placeOrder then
        looks like a malformed VOL order to IBKR and we get Error 321
        ("VOL order requires non-negative floating point value for
        volatility") plus an AssertionError on the way out.

        The clean-LimitOrder rebuild approach (commits 3a391d2 / fa2fb16)
        avoided contamination but produced Error 103 cascades on FA logins
        (see d54e60d for the symptom list). The right answer is in-place
        mutation — which preserves ib_insync's clientId association in
        wrapper.trades — combined with explicit field stripping.
        """
        # VOL order fields
        order.volatility = UNSET_DOUBLE
        order.volatilityType = UNSET_INTEGER
        order.continuousUpdate = False
        order.referencePriceType = UNSET_INTEGER
        # Delta-neutral fields
        order.deltaNeutralOrderType = ""
        order.deltaNeutralAuxPrice = UNSET_DOUBLE
        order.deltaNeutralConId = 0
        order.deltaNeutralOpenClose = ""
        order.deltaNeutralShortSale = False
        order.deltaNeutralShortSaleSlot = 0
        order.deltaNeutralDesignatedLocation = ""
        order.deltaNeutralSettlingFirm = ""
        order.deltaNeutralClearingAccount = ""
        order.deltaNeutralClearingIntent = ""
        # Algo / reference contract fields
        order.algoStrategy = ""
        order.algoParams = []
        order.algoId = ""
        order.referenceContractId = 0
        order.referenceExchangeId = ""
        order.referenceChangeAmount = 0.0

    def _is_order_live(self, trade) -> bool:
        """Check if an order is still active (not dead/cancelled/filled).

        Reads `trade.orderStatus.status` directly. The caller is expected
        to have resolved `trade` via `_canonical_trade` (last-write-wins
        over openTrades) so this is the same instance ib_insync mutates
        on every status event. We can't safely indirect through
        `wrapper.trades[(my_cid, oid)]`: on FA logins the canonical entry
        is keyed under clientId=-1 (master adoption), not under our own.
        """
        if trade is None:
            return False
        return trade.orderStatus.status not in OrderStatus.DoneStates

    def _send_or_update(self, strike: float, expiry: str, right: str, side: str,
                        price: float, qty: int, option):
        """Send new order or modify existing if price changed."""
        key = (strike, expiry, right, side)
        contract = option.contract

        if contract is None:
            logger.warning("No contract for %s %s%s, cannot send order",
                           expiry, int(strike), right)
            return

        if key in self.active_orders:
            order_id = self.active_orders[key]
            trade = self._canonical_trade(order_id)

            if not self._is_order_live(trade):
                self.active_orders.pop(key, None)
            elif getattr(trade.orderStatus, "filled", 0) > 0:
                # Defense vector: orderStatus.filled increments on every
                # partial/full fill BEFORE the status field necessarily
                # transitions to "Filled" and BEFORE execDetails has
                # propagated to fill_handler. _is_order_live alone misses
                # this race window because it only checks the status string.
                # Treating any non-zero filled count as terminal here is
                # what stopped the Error 104 storm.
                self.active_orders.pop(key, None)
                self._pending_amend.pop(order_id, None)
            elif trade.order.lmtPrice != price:
                # Modify-storm guard (defense vector #14). If we've already
                # sent more than N modifies on this orderId in the last
                # second, stop amending and force a cancel-replace on the
                # next cycle. Bounds blast radius of any future regression
                # in the modify hot path (the area touched by fa2fb16 /
                # 3a391d2 / d54e60d) without needing to reason about why
                # the loop is hot.
                now_ns_storm = time.monotonic_ns()
                window = self._modify_times_per_oid.get(order_id)
                if window is None:
                    window = deque(maxlen=16)
                    self._modify_times_per_oid[order_id] = window
                cutoff = now_ns_storm - 1_000_000_000
                while window and window[0] < cutoff:
                    window.popleft()
                if len(window) >= self._max_modifies_per_sec_per_oid:
                    logger.warning(
                        "Modify storm on %s%s %s oid=%d (%d modifies/sec) — "
                        "cancel-replace",
                        int(strike), right, side, order_id, len(window),
                    )
                    self._cancel_quote(strike, expiry, right, side)
                    self._modify_times_per_oid.pop(order_id, None)
                    return
                window.append(now_ns_storm)
                # Dead-band: skip a re-price unless the new target moves
                # the resting order by at least min_modify_ticks. This
                # suppresses 1-tick noise from a flickering BBO without
                # giving up the leading position. Combined with the
                # token bucket, this is the main lever cutting our API
                # message rate below the IBKR throttle. Ported from v1.
                tick = self.config.quoting.tick_size
                min_dt = float(getattr(self.config.quoting, "min_modify_ticks", 1))
                if abs(price - trade.order.lmtPrice) < (min_dt * tick - 1e-9):
                    return  # within dead-band, leave the resting order alone
                # Modify-too-soon guard: don't amend an order that IBKR
                # hasn't acknowledged yet. If we send a modify while the
                # original placeOrder is still in flight, the server sees
                # two placeOrders for the same orderId and rejects the
                # second with Error 103 (Duplicate order id), which then
                # cascades into a Cancelled order. Same MIN_ORDER_LIFETIME
                # window we already enforce on cancels (see _cancel_quote).
                placed_ns = self._placed_at_ns.get(order_id)
                status = trade.orderStatus.status
                if (placed_ns is not None
                        and status in ("PendingSubmit", "ApiPending")
                        and (time.monotonic_ns() - placed_ns)
                                < MIN_ORDER_LIFETIME_NS):
                    return  # let IBKR ack first; next cycle will retry
                # In-place mutation of the canonical trade.order. The
                # clean-LimitOrder rebuild approach (commits 3a391d2 /
                # fa2fb16) breaks ib_insync's clientId association in
                # wrapper.trades on FA logins and produces Error 103
                # (Duplicate order id) cascades — see d54e60d for the
                # symptom list. In-place mutation preserves the wrapper
                # state IBKR's modify path needs.
                #
                # Defensively strip VOL / delta-neutral / algo fields
                # before placeOrder: ib_insync's wrapper picks those up
                # from openOrder callbacks (e.g. when a TWS client logs
                # into the same account), which would otherwise turn the
                # next placeOrder into a malformed VOL order (Error 321).
                # That's the failure mode the clean-rebuild was trying
                # to avoid; stripping the contaminated fields gives us
                # the same protection without losing wrapper routing.
                now_ns = time.monotonic_ns()
                self._strip_contamination_fields(trade.order)
                trade.order.lmtPrice = price
                trade.order.totalQuantity = qty
                trade.order.tif = "GTD"
                trade.order.goodTillDate = _gtd_string()
                self._record_send_latency(strike, expiry, right, order_id)
                self._pending_amend[order_id] = now_ns
                if len(self._pending_amend) > TRACKING_DICT_MAX:
                    self._evict_oldest_half(self._pending_amend)
                try:
                    if self._try_place_order(trade.contract, trade.order) is None:
                        # Bucket dropped the modify; back out the amend
                        # tracking entry so the latency ring isn't
                        # poisoned. The next cycle will retry.
                        self._pending_amend.pop(order_id, None)
                    else:
                        self._order_underlying[order_id] = self.market_data.state.underlying_price
                except AssertionError:
                    # Race: wrapper.trades flipped to a DoneState between
                    # our liveness check and placeOrder. Drop tracking
                    # and let the next cycle place a fresh order.
                    logger.warning(
                        "placeOrder DoneState race on modify %s%s %s oid=%d — dropping",
                        int(strike), right, side, order_id,
                    )
                    self.active_orders.pop(key, None)
                    self._pending_amend.pop(order_id, None)
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
                    # Same in-place + strip rationale as the modify path.
                    self._strip_contamination_fields(trade.order)
                    trade.order.goodTillDate = _gtd_string()
                    try:
                        self._try_place_order(trade.contract, trade.order)
                    except AssertionError:
                        logger.warning(
                            "placeOrder DoneState race on GTD refresh %s%s %s oid=%d — dropping",
                            int(strike), right, side, order_id,
                        )
                        self.active_orders.pop(key, None)
                return

        # Per-side resting-order cap (defense vector #11). Belt-and-
        # suspenders ceiling on total active orders for this side. The
        # token bucket rate-limits API messages but not steady-state
        # outstanding count, so a logic bug that kept calling
        # update_quotes against an ever-growing strike set could otherwise
        # accumulate unbounded resting size.
        resting_on_side = sum(1 for (_s, _e, _r, sd) in self.active_orders if sd == side)
        if resting_on_side >= self._max_resting_per_side:
            logger.warning(
                "Per-side resting cap reached (%s=%d ≥ %d) — refusing new "
                "order on %s%s",
                side, resting_on_side, self._max_resting_per_side,
                int(strike), right,
            )
            return

        # Place a new order
        # account= is REQUIRED on multi-account logins (DFP/DUP paper sub-
        # accounts) — IBKR returns Error 436 "You must specify an allocation"
        # if it's missing. Verified 2026-04-08.
        # orderRef includes expiry (last 4 digits of YYYYMMDD — MMDD) for
        # trace-friendly per-expiry filtering in logs/csv when multi-expiry
        # quoting is active. The ORDER_REF_PREFIX prefix stays intact so the
        # canonical-trade filter in _build_our_prices_index still matches.
        exp_tag = (expiry or "")[-4:] or "xxxx"
        order = LimitOrder(
            action=side,
            totalQuantity=qty,
            lmtPrice=price,
            tif="GTD",
            goodTillDate=_gtd_string(),
            account=self._account,
            orderRef=f"{ORDER_REF_PREFIX}_{exp_tag}_{int(strike)}{right}_{side}",
        )
        # Defensive wrap: ib_insync.placeOrder asserts on
        # `trade.orderStatus.status not in OrderStatus.DoneStates`. After a
        # watchdog reconnect/escalation cycle, ib_insync's wrapper.trades
        # dict can still hold stale entries from the previous session in
        # Cancelled state, with orderIds that get recycled by IBKR's
        # nextValidId on the new session. The next placeOrder for one of
        # those recycled IDs trips the assertion and — without this guard
        # — bubbles up to the quote loop's exception storm path and
        # sticky-kills the engine. Drop the key, log, and let the next
        # cycle allocate a fresh orderId. (The modify path at line 613
        # already has the same guard for analogous reasons.)
        try:
            trade = self._try_place_order(contract, order)
        except AssertionError:
            logger.warning(
                "placeOrder DoneState race on NEW order %s%s %s — "
                "stale wrapper.trades entry; dropping and retrying next cycle",
                int(strike), right, side,
            )
            return
        if trade is None:
            return  # bucket dropped — next cycle re-attempts
        self.active_orders[key] = trade.order.orderId
        self._order_underlying[trade.order.orderId] = self.market_data.state.underlying_price
        self._record_send_latency(strike, expiry, right, trade.order.orderId)

    @staticmethod
    def _evict_oldest_half(d: dict) -> None:
        """Drop the oldest half of a dict in place. Cheap stand-in for an
        LRU eviction policy on the latency tracking dicts. Relies on Python
        3.7+ insertion-order semantics: keys() returns them in insert order,
        so the first half is the oldest. O(N) per eviction, called at most
        once every TRACKING_DICT_MAX inserts → amortized O(1)."""
        keys = list(d.keys())
        for k in keys[: len(keys) // 2]:
            d.pop(k, None)

    def _record_send_latency(self, strike: float, expiry: str, right: str, order_id: int):
        """Capture TTT (tick→placeOrder) and arm RTT/fill timers for this order.

        TTT is only sampled when the underlying tick is fresh (<50ms old).
        Older ticks come from the periodic 1s fallback refresh, where the
        recorded delta is data age rather than compute latency.

        Two timers are armed:
          - _pending_rtt: cleared on Submitted ack (used for RTT histogram)
          - _placed_at_ns: cleared on fill (used for fill latency in fills.csv)

        Both dicts are capped at TRACKING_DICT_MAX so they can't leak when
        an order goes terminal without an observation (GTD-expired before
        the next snapshot, cancelled by the broker, etc).
        """
        now_ns = time.monotonic_ns()
        tick_ns = self._decision_tick_ns.get((strike, expiry, right), 0)
        if tick_ns > 0:
            ttt_us = (now_ns - tick_ns) // 1000
            if 0 <= ttt_us < 50_000:
                self._ttt_us.append(ttt_us)
        # Place RTT is measured ONCE per orderId — from the original
        # placeOrder to the first openOrder ack. Modifies don't overwrite
        # this; they have their own _pending_amend stash measured per
        # modify cycle.
        if order_id not in self._pending_rtt and order_id not in self._rtt_captured_oids:
            self._pending_rtt[order_id] = now_ns
            if len(self._pending_rtt) > TRACKING_DICT_MAX:
                self._evict_oldest_half(self._pending_rtt)
        # Fill latency clock — also once per orderId.
        if order_id not in self._placed_at_ns:
            self._placed_at_ns[order_id] = now_ns
            if len(self._placed_at_ns) > TRACKING_DICT_MAX:
                self._evict_oldest_half(self._placed_at_ns)

    def fill_latency_ms(self, order_id: int) -> Optional[float]:
        """Return milliseconds from first placement to now for an order, or
        None if we don't have a record. Caller is responsible for invoking
        this in the fill handler at the moment a fill arrives."""
        placed_ns = self._placed_at_ns.pop(order_id, None)
        if placed_ns is None:
            return None
        return (time.monotonic_ns() - placed_ns) / 1_000_000.0

    def _on_open_order_ack(self, trade):
        """Event-driven handler for IBKR's openOrder messages.

        Fired by ib_insync.IB.openOrderEvent whenever IBKR sends an openOrder
        message in response to a place or modify. Records:
          - place-RTT: time from the FIRST placeOrder for this orderId to
            the first openOrder ack we receive (one sample per orderId).
          - amend-RTT: time from each modify placeOrder to the corresponding
            ack (one sample per modify cycle).

        Single-threaded asyncio means our `_pending_amend[oid] = ...` stash
        in _send_or_update always lands BEFORE this callback fires for the
        same modify, because the IBKR response can only arrive after the
        send completes via the writer task and the reader task dispatches.
        No race.

        Direct event dispatch (~sub-ms wrapper overhead) replaces the prior
        snapshot-quantized polling that was adding 0-250ms uniform noise on
        top of real ~1ms amend latency.
        """
        try:
            oid = trade.order.orderId
            now_ns = time.monotonic_ns()

            # ── place-RTT (one sample per orderId) ────────────────
            if oid not in self._rtt_captured_oids:
                sent_ns = self._pending_rtt.pop(oid, None)
                if sent_ns is not None:
                    rtt_us = (now_ns - sent_ns) // 1000
                    if 0 <= rtt_us < 5_000_000:
                        self._place_rtt_us.append(rtt_us)
                    self._rtt_captured_oids.add(oid)
                    if len(self._rtt_captured_oids) > TRACKING_DICT_MAX:
                        self._rtt_captured_oids.clear()

            # ── amend-RTT (one sample per modify) ─────────────────
            # Match on (oid, price) so rapid modifies are independent.
            sent_ns = self._pending_amend.pop(oid, None)
            if sent_ns is not None:
                amend_us = (now_ns - sent_ns) // 1000
                if 0 <= amend_us < 5_000_000:
                    self._amend_us.append(amend_us)
        except Exception:
            pass

    def _on_ib_error(self, *args, **kwargs):
        """Handle Error 104 (Cannot modify a filled order) by dropping the
        dead orderId from active_orders / _pending_amend so the next quote
        cycle places a fresh order instead of looping on the same modify.

        Other error codes are handled (or ignored) elsewhere — ib_insync's
        wrapper logs them and the per-event paths (orderStatus, openOrder)
        handle the state transitions. We only special-case 104 because it's
        the one error code that, without intervention, generates an unbounded
        retry storm because nothing in the existing code path clears the
        active_orders entry on it.

        Signature-agnostic (*args) for the same reason as the Error 1100
        handler in connection.py (commit 7de42c8): ib_insync's errorEvent
        dispatches with different arity across versions — newer releases
        add an `advancedOrderRejectJson` positional before `contract`. A
        fixed 4-arg signature silently fails on version mismatch because
        ib_insync's Event class swallows TypeError from callbacks, so the
        handler never runs and 104s accumulate uncleared. Parse reqId and
        errorCode positionally from args[:2].
        """
        if len(args) < 2:
            return
        reqId, errorCode = args[0], args[1]

        # Error 10197: "No market data during competing live session."
        # Another session (TWS, second gateway) stole our market data feed.
        # Orders are still resting but we're blind to price changes.
        # Immediately cancel everything and block new quotes until fresh
        # option ticks confirm the feed is back.
        if errorCode == 10197:
            if not self._blackout_cancel_sent:
                logger.critical(
                    "MARKET DATA BLACKOUT (Error 10197): competing session "
                    "detected — panic cancelling all orders"
                )
                self.panic_cancel()
                self._blackout_cancel_sent = True
                send_alert(
                    "MARKET DATA BLACKOUT",
                    "Error 10197: competing live session detected. "
                    "All orders cancelled. Quoting suspended until "
                    "option data feed resumes.",
                )
            self._market_data_blackout = True
            return

        if errorCode != 104:
            return
        try:
            self._error_104_count += 1
            # active_orders is keyed by (strike, expiry, right, side); the
            # value is the orderId. Reverse-lookup is O(N) but N <= ~300
            # with 3-expiry quoting, so this is cheap and only fires on
            # actual races.
            dead_key = None
            for key, oid in self.active_orders.items():
                if oid == reqId:
                    dead_key = key
                    break
            if dead_key is not None:
                self.active_orders.pop(dead_key, None)
            self._pending_amend.pop(reqId, None)
            # Log the first few and then every 100th to keep the noise
            # bounded but still surface the rate.
            if self._error_104_count <= 5 or self._error_104_count % 100 == 0:
                logger.info(
                    "Error 104 cleanup: oid=%d key=%s (total 104s seen: %d)",
                    reqId, dead_key, self._error_104_count,
                )
        except Exception as e:
            logger.warning("_on_ib_error cleanup failed: %s", e)

    def get_latency_snapshot(self) -> dict:
        """Return rolling p50/p90/p99 in microseconds for TTT, RTT, and amend."""
        def stats(buf):
            if not buf:
                return {"n": 0, "p50": None, "p90": None, "p99": None}
            s = sorted(buf)
            n = len(s)
            return {
                "n": n,
                "p50": s[min(n - 1, int(n * 0.50))],
                "p90": s[min(n - 1, int(n * 0.90))],
                "p99": s[min(n - 1, int(n * 0.99))],
            }
        return {
            "ttt_us": stats(self._ttt_us),
            "place_rtt_us": stats(self._place_rtt_us),
            "amend_us": stats(self._amend_us),
        }

    def _cancel_quote(self, strike: float, expiry: str, right: str, side: str):
        """Cancel a quote at a specific (strike, expiry, right, side).

        Skips cancellation when the order was placed less than
        MIN_ORDER_LIFETIME_MS ago AND is still in PendingSubmit (i.e., IBKR
        hasn't acked yet). Cancelling in that window produces a
        PendingSubmit → PendingCancel transition that never visits Submitted,
        so the order is invisible on the book and we burn a place/cancel pair
        for nothing. The next quote tick will re-evaluate and cancel then if
        the skip condition still holds.
        """
        key = (strike, expiry, right, side)
        if key not in self.active_orders:
            return
        order_id = self.active_orders[key]
        trade = self._canonical_trade(order_id)
        if trade is not None:
            placed_ns = self._placed_at_ns.get(order_id)
            status = trade.orderStatus.status
            if (placed_ns is not None
                    and status in ("PendingSubmit", "ApiPending")
                    and (time.monotonic_ns() - placed_ns) < MIN_ORDER_LIFETIME_NS):
                # Too young to cancel — let IBKR ack first.
                return
            # Bucket-gated. If the cancel is dropped here we still pop
            # from active_orders so we don't keep retrying — the next
            # quote cycle will see no resting order and re-place if it
            # still wants to. Worst case: a stale order rests until its
            # GTD expires, which is the same outcome as v1's drop.
            self._try_cancel_order(trade.order)
        del self.active_orders[key]

    def cancel_all_quotes(self):
        """Soft kill switch: cancel everything via per-order cancels.

        For HARD kill switch / panic paths use `panic_cancel()` instead —
        it sends one `reqGlobalCancel` (server-side cancel-all in one
        message) and bypasses the token bucket so it always reaches the
        wire. Use this method only on graceful shutdown / cycle-end.
        """
        for key, order_id in list(self.active_orders.items()):
            trade = self._canonical_trade(order_id)
            if trade is not None:
                self._try_cancel_order(trade.order)
        self.active_orders.clear()
        self._order_underlying.clear()
        logger.info("All quotes cancelled")

    def panic_cancel(self) -> None:
        """Fast cancel-all for graceful degradation paths.

        Sends a single `reqGlobalCancel` to IBKR — server-side it nukes
        every working order on the connection in one message, which is
        both faster and more reliable than walking active_orders one by
        one (especially when the API is sluggish, half-disconnected, or
        the local view is desynced from the book). Then clears local
        state so a recovery cycle starts from a clean slate.

        Use this for: socket disconnect, exception storms, watchdog
        fast-cancel tier, anywhere "the book is unsafe and I want it
        empty *now*". For nominal shutdown / cycle-end cancels keep
        using cancel_all_quotes / _cancel_quote.
        """
        try:
            self.ib.reqGlobalCancel()
        except Exception as e:
            logger.error("PANIC CANCEL: reqGlobalCancel failed: %s", e)
            # Fall back to per-order cancel walk so we still try to
            # clear the book even if the global path is broken.
            try:
                self.cancel_all_quotes()
            except Exception as ee:
                logger.error("PANIC CANCEL: fallback cancel_all_quotes failed: %s", ee)
        # Clear local state regardless of API success — the next quote
        # cycle (if any) should not believe it has live orders.
        n = len(self.active_orders)
        self.active_orders.clear()
        self._pending_amend.clear()
        self._order_underlying.clear()
        logger.critical("PANIC CANCEL: reqGlobalCancel sent, cleared %d local order refs", n)

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
        """Return dict keyed by (strike, expiry, right, side) -> live quote info.

        Refreshes the canonical-trade index up front so the per-order lookups
        below are O(1) instead of O(N) per call. This is the snapshot writer's
        4Hz hot path; without the refresh we'd walk openTrades once per
        active order per snapshot (~50 × 200 = 10K iterations every 250ms).
        """
        # Side-effect: populates self._canonical_idx that _canonical_trade reads.
        self._build_our_prices_index()
        quotes = {}
        for (strike, expiry, right, side), order_id in self.active_orders.items():
            trade = self._canonical_trade(order_id)
            if trade is not None:
                quotes[(strike, expiry, right, side)] = {
                    "order_id": order_id,
                    "price": trade.order.lmtPrice,
                    "qty": trade.order.totalQuantity,
                    "status": trade.orderStatus.status if hasattr(trade, 'orderStatus') else "unknown",
                }
        return quotes


