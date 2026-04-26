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

from ib_insync import IB, LimitOrder

from .discord_notify import send_alert
from .sabr import delta_adjust_theo
from ib_insync.order import OrderStatus
from ib_insync.util import UNSET_DOUBLE, UNSET_INTEGER

from .utils import (
    ceil_to_tick, days_to_expiry, floor_to_tick,
    format_hxe_symbol, round_to_tick,
)

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

OrderKey = tuple[float, str, str, str]  # (strike, expiry, right, side)
ORDER_REF_PREFIX = "corsair"


class FillBurstTracker:
    """Thread 3 Layer C rolling fill-burst window.

    Maintains a deque of (ts_ns, side) for fills landed within the
    trailing ``window_sec``. ``record_and_evaluate`` appends the new
    fill, evicts everything older than the window, and returns the
    same-side and any-side counts (inclusive of the just-added fill).

    Side semantics: caller passes ``"BUY"`` or ``"SELL"`` per fill;
    same-side counts match strict equality. Mixed C/P fills on the same
    side count together — that matches the brief's "trigger_C1 = side ==
    this_side, on any strike" definition (cross-strike same-side burst
    is the canonical P1 signature).

    O(1) amortized on the hot path: ``_evict`` trims the deque head each
    call against ``now_ns − window_ns``; total work is bounded by
    lifetime fill count.
    """

    __slots__ = ("_window_ns", "_events")

    def __init__(self, window_sec: float):
        if window_sec <= 0:
            raise ValueError(f"window_sec must be > 0, got {window_sec}")
        self._window_ns = int(window_sec * 1_000_000_000)
        self._events: deque = deque()

    def _evict(self, now_ns: int) -> None:
        cutoff = now_ns - self._window_ns
        ev = self._events
        while ev and ev[0][0] <= cutoff:
            ev.popleft()

    def record_and_evaluate(self, ts_ns: int, side: str
                            ) -> tuple[int, int]:
        """Append one fill, evict stale entries, return
        ``(same_side_count, any_side_count)`` over the trailing window
        INCLUDING the fill just appended."""
        self._events.append((ts_ns, side))
        self._evict(ts_ns)
        same = sum(1 for _, s in self._events if s == side)
        return same, len(self._events)

    def count_in_window(self, now_ns: int) -> int:
        """Read-only any-side count over the trailing window. Used to
        emit ``burst_1s_at_fill`` after the fill has been recorded.
        Caller is responsible for ordering: query AFTER record_and_evaluate
        so the just-added fill is included."""
        self._evict(now_ns)
        return len(self._events)


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
) -> tuple[bool, str]:
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
        self.active_orders: dict[OrderKey, int] = {}  # {(strike, expiry, right, side): order_id}
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
        self._last_sabr_attempt: datetime | None = None
        self._last_sabr_forward: float = 0.0  # underlying at last calibration
        # Latency rings (microseconds). TTT = tick→placeOrder. RTT = placeOrder→Submitted ack.
        self._ttt_us: deque[int] = deque(maxlen=LATENCY_RING_SIZE)
        # Place RTT: time from placeOrder() send to the first openOrder
        # echo from IBKR. Measured against openOrderEvent. We verified
        # 2026-04-09 that openOrder and orderStatus(Submitted/PreSubmitted)
        # arrive within ~100µs of each other on this paper gateway, so
        # measuring against openOrder is equivalent to v1's status-based
        # measurement. (Diagnostic ran a parallel rtt_status_us ring for
        # several minutes; both rings produced identical numbers and the
        # parallel ring was removed.)
        self._place_rtt_us: deque[int] = deque(maxlen=LATENCY_RING_SIZE)
        # Amend RTT — separate from _place_rtt_us because modifies dominate steady
        # state and they have a different latency profile than fresh places
        # (no order-id allocation, no permission validation, just price update).
        self._amend_us: deque[int] = deque(maxlen=LATENCY_RING_SIZE)
        self._pending_rtt: dict[int, int] = {}  # order_id -> placeOrder ns
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
        self._pending_amend: dict[int, int] = {}
        # Order placement time per order_id, for fill latency (place→fill).
        # Cleared on fill_handler when the fill arrives. Distinct from
        # _pending_rtt which clears on Submitted ack.
        self._placed_at_ns: dict[int, int] = {}
        # Underlying price at last place/modify per order_id. Used by
        # delta-retreat to detect when F has moved enough that the resting
        # order should be shifted by delta × ΔF instead of penny-jumping.
        self._order_underlying: dict[int, float] = {}
        # Most recent tick_received_ns per (strike, expiry, right) at decision time.
        self._decision_tick_ns: dict[tuple[float, str, str], int] = {}
        # Resolved enabled_expiries cache. Refreshed each cycle in update_quotes
        # from the live state.expiries list. Tokens from config.quoting.enabled_expiries
        # are resolved via resolve_enabled_expiries().
        self._enabled_expiries_resolved: list = []
        # Consecutive theo_edge skip count per key — used for hysteresis so a
        # single tick of theo_edge violation doesn't drop a resting order
        # that would re-qualify on the next tick. Reset on any non-theo path
        # through _process_side (place or other gate).
        self._theo_edge_streak: dict[OrderKey, int] = {}
        # orderIds we've already extracted RTT for (so we don't double-count
        # on subsequent snapshot passes).
        self._rtt_captured_oids: set = set()
        # Modify-storm guard (defense vector #14): per-(strike,expiry,right,side)
        # rolling window of recent modify send timestamps (monotonic ns). Keyed
        # on the side, not orderId, so cancel-replace doesn't reset the throttle
        # and let us churn out orderIds at the same effective message rate.
        # Old single-order behavior is preserved by virtue of being a strict
        # subset (one orderId per side at a time).
        self._modify_times_per_side: dict[OrderKey, deque[int]] = {}
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
        self._canonical_idx: dict[int, object] = {}

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
        # Margin rejection suppression. Keys that IBKR rejected with Error 201
        # (insufficient margin) are tracked here so we don't re-submit every
        # cycle. Cleared on any fill (margin conditions may have changed).
        self._margin_rejected: set = set()
        self._margin_rejected_last_clear: float = time.monotonic()
        # v1.4 §9.5 skip-stream throttle: last-emitted reason per
        # (strike, expiry, right, side) so skips.jsonl only captures
        # transitions. Cleared on successful placement.
        self._last_skip_reason: dict[OrderKey, str] = {}
        # priority_v1 refresh policy state. Last send time per key drives
        # the max-age override that prevents far strikes from going stale
        # indefinitely under the per-tick amend cap. Per-cycle counters
        # reset at the top of update_quotes; the cap is the binding
        # constraint on how many modify messages leave per tick.
        self._last_amend_ns: dict[OrderKey, int] = {}
        self._tick_amend_count: int = 0
        self._tick_amend_deferred: int = 0
        # Diagnostic counters for distinguishing max-age force-refresh
        # amends from genuine price-change amends. Reset at process start;
        # surfaced via get_latency_snapshot() so the tuning question "is
        # max_age_s firing too often?" can be answered from the live
        # snapshot instead of adding ad-hoc logging.
        self._amend_force_count: int = 0
        self._amend_price_count: int = 0
        # Requote-cycle instrumentation (logs-paper/requote_cycles-*.jsonl).
        # Each update_quotes() call opens a cycle; acks arrive async via
        # _on_open_order_ack and close the cycle when the last sent order
        # has acknowledged. _prev_cycle_deferred_end seeds the next cycle's
        # deferred_queue_depth_at_start so compounding deferrals are visible.
        self._cycle_seq: int = 0
        self._cycle: dict | None = None
        self._cycles_pending_emit: dict[int, dict] = {}
        self._cycle_by_oid: dict[int, int] = {}
        self._last_cycle_forward: float | None = None
        self._prev_cycle_deferred_end: int = 0

        # Hardburst-skip overlay telemetry. Counters reset at session
        # rollover via reset_daily_hardburst_counters(). Populated by the
        # gate at the top of update_quotes when hardburst_skip_enabled is
        # True. _hardburst_update_attempts counts ALL reached-the-gate
        # cycles (so the ratio .../attempts is the suppression rate).
        self._hardburst_skip_count: int = 0
        self._hardburst_update_attempts: int = 0
        self._last_hardburst_skip_ts: int = 0
        # Runtime kill-override from operational_kills when suppression
        # rate breaches the daily ceiling. Takes precedence over the
        # config flag; cleared at the next daily counter reset.
        self._hardburst_skip_disabled_by_kill: bool = False

        # Thread 3 Layer C burst-pull state. Tracker is constructed lazily
        # from config so the window_sec knob is honored even if the
        # master flag is OFF (we keep tracker active so we can populate
        # fills.burst_1s_at_fill regardless — that field is part of §6
        # baseline instrumentation and must work with all flags OFF).
        thread3 = getattr(config.quoting, "thread3", None)
        burst_window_sec = float(getattr(
            thread3, "layer_c_window_sec", 1.0)) if thread3 else 1.0
        self._fill_burst_tracker: FillBurstTracker = FillBurstTracker(
            window_sec=burst_window_sec)
        # Cooldown deadlines (monotonic_ns). 0 = inactive.
        # _burst_cooldown_until_ns_per_side maps "BUY" / "SELL" → deadline;
        # _burst_cooldown_global_until_ns is the C2 global lock. Both are
        # consulted by the placement gate at the top of _process_side.
        self._burst_cooldown_per_side_until_ns: dict[str, int] = {
            "BUY": 0, "SELL": 0,
        }
        self._burst_cooldown_global_until_ns: int = 0
        # Diagnostic counters surfaced via get_latency_snapshot when a
        # burst-pull dashboard is wired up. Reset at session rollover.
        self._burst_pull_c1_count: int = 0
        self._burst_pull_c2_count: int = 0
        # Hedge manager handle for the priority-drain signal. Wired by
        # main.py after construction; None when no hedge is wired (legacy
        # / test paths).
        self.hedge_manager = None

        self.ib.errorEvent += self._on_ib_error

    def _begin_requote_cycle(self, trigger: str) -> None:
        """Open a new requote cycle. Called at the top of update_quotes()
        after early-return gates. Cycle closes when the last amended order
        has been acked (or immediately if no amends were sent)."""
        self._cycle_seq += 1
        cid = self._cycle_seq
        fwd = self.market_data.state.underlying_price
        prev_fwd = self._last_cycle_forward
        tick_size = float(getattr(self.config.product, "tick_size", 0.0005))
        fwd_delta_usd = (fwd - prev_fwd) if prev_fwd is not None else 0.0
        fwd_delta_ticks = (int(round(fwd_delta_usd / tick_size))
                           if tick_size > 0 else 0)
        self._last_cycle_forward = fwd
        self._cycle = {
            'cycle_id': cid,
            'trigger': trigger,
            'refit_expiries': [],
            'start_ns': time.monotonic_ns(),
            'forward': fwd,
            'forward_delta_usd': fwd_delta_usd,
            'forward_delta_ticks': fwd_delta_ticks,
            'tick_size': tick_size,
            'n_evaluated': 0,
            'skip_reasons': {},
            'deferred_depth_start': self._prev_cycle_deferred_end,
            'sent_order_ids': set(),
            'ack_us': [],
            'ready_to_emit': False,
        }
        self._cycles_pending_emit[cid] = self._cycle

    def _note_cycle_eval(self, skip_reason: str) -> None:
        c = self._cycle
        if c is None:
            return
        c['n_evaluated'] += 1
        reason = skip_reason or "amended_or_placed"
        c['skip_reasons'][reason] = c['skip_reasons'].get(reason, 0) + 1

    def _note_cycle_amend_send(self, order_id: int) -> None:
        c = self._cycle
        if c is None:
            return
        c['sent_order_ids'].add(order_id)
        self._cycle_by_oid[order_id] = c['cycle_id']

    def _note_cycle_amend_ack(self, order_id: int, ack_us: int) -> None:
        cid = self._cycle_by_oid.pop(order_id, None)
        if cid is None:
            return
        c = self._cycles_pending_emit.get(cid)
        if c is None:
            return
        c['ack_us'].append(ack_us)
        c['sent_order_ids'].discard(order_id)
        if c['ready_to_emit'] and not c['sent_order_ids']:
            self._emit_requote_cycle(cid)

    def _end_requote_cycle(self) -> None:
        """Finalize the open cycle. If amends were sent and some acks are
        still pending, emission is deferred to the last ack. Otherwise
        emits immediately (or drops the row if no amends fired)."""
        c = self._cycle
        if c is None:
            return
        c['n_amended'] = self._tick_amend_count
        c['n_deferred'] = self._tick_amend_deferred
        c['deferred_depth_end'] = self._tick_amend_deferred
        c['ready_to_emit'] = True
        self._prev_cycle_deferred_end = self._tick_amend_deferred
        self._cycle = None
        if c['n_amended'] == 0:
            # No-op cycle — nothing to measure. Drop to avoid noise.
            self._cycles_pending_emit.pop(c['cycle_id'], None)
            return
        if not c['sent_order_ids']:
            self._emit_requote_cycle(c['cycle_id'])

    def _emit_requote_cycle(self, cycle_id: int) -> None:
        c = self._cycles_pending_emit.pop(cycle_id, None)
        if c is None or self.csv_logger is None:
            return
        ack = sorted(c['ack_us'])

        def _pct(p: float):
            if not ack:
                return None
            k = min(len(ack) - 1,
                    max(0, int(round((p / 100.0) * (len(ack) - 1)))))
            return ack[k]

        total_us = (time.monotonic_ns() - c['start_ns']) // 1000
        try:
            self.csv_logger.log_paper_requote_cycle(
                cycle_id=c['cycle_id'],
                trigger=c['trigger'],
                refit_expiries=c['refit_expiries'],
                forward=c['forward'],
                forward_delta_usd=c['forward_delta_usd'],
                forward_delta_ticks=c['forward_delta_ticks'],
                tick_size=c['tick_size'],
                orders_evaluated=c['n_evaluated'],
                orders_amended=c['n_amended'],
                orders_deferred=c['n_deferred'],
                orders_skipped_reasons=c['skip_reasons'],
                deferred_queue_depth_at_start=c['deferred_depth_start'],
                deferred_queue_depth_at_end=c['deferred_depth_end'],
                first_ack_us=ack[0] if ack else None,
                last_ack_us=ack[-1] if ack else None,
                p50_ack_us=_pct(50),
                p99_ack_us=_pct(99),
                cycle_total_us=total_us,
            )
        except Exception as e:
            logger.debug("requote_cycle emit failed: %s", e)

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

    def _build_our_prices_index(self) -> dict[tuple[float, str, str, str], set]:
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
        canonical: dict[int, object] = {}
        for t in self.ib.openTrades():
            ref = getattr(t.order, "orderRef", "") or ""
            if ref.startswith(ORDER_REF_PREFIX):
                canonical[t.order.orderId] = t
        self._canonical_idx = canonical  # cache for _canonical_trade reads
        out: dict[tuple[float, str, str, str], set] = {}
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

        Thread 3 Layer A (additive, OR with elapsed-time): when the master
        flag and lever_a_f_tick_refit_enabled are both ON, also trigger
        a refit when |F_now − F_at_last_fit| > N_ticks × tick_size. The
        N_ticks knob (default 5 HG ticks = $0.0025) is configured under
        config.quoting.thread3.layer_a_n_ticks and lives separately from
        the sabr_fast_recal_dollars knob (a coarser, far-larger threshold).
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

        # Layer A: F-tick trigger. Master + per-layer flag, both default OFF.
        thread3 = getattr(config.quoting, "thread3", None)
        layer_a_on = (thread3 is not None
                      and bool(getattr(thread3, "master_enabled", False))
                      and bool(getattr(
                          thread3, "lever_a_f_tick_refit_enabled", False)))
        layer_a_fired = False
        if layer_a_on and self._last_sabr_attempt is not None:
            n_ticks = float(getattr(thread3, "layer_a_n_ticks", 5))
            tick_size = float(getattr(config.quoting, "tick_size", 0.0005))
            if n_ticks > 0 and tick_size > 0:
                threshold_usd = n_ticks * tick_size
                if forward_move > threshold_usd:
                    layer_a_fired = True

        elapsed_fired = (
            self._last_sabr_attempt is None
            or elapsed >= config.pricing.sabr_recalibrate_seconds
            or forward_move >= fast_recal_dollars
        )
        should_recal = elapsed_fired or layer_a_fired
        if should_recal:
            # trigger_reason: prefer "F_tick" when ONLY the Layer A path
            # fired (elapsed/fast_recal didn't bite). When both fired,
            # call it "elapsed" — that path would have run anyway and the
            # sabr_fits row reflects the existing cadence.
            if layer_a_fired and not elapsed_fired:
                trigger_reason = "F_tick"
            else:
                trigger_reason = "elapsed"
            # Only advance the "attempted" clock when we actually submit.
            # calibrate_async returns False if a prior recal is still in
            # flight — in that case we want the next cycle to try again
            # rather than waiting another 60s.
            self.sabr.set_expiries(state.expiries)
            submitted = self.sabr.calibrate_async(
                state.underlying_price, state.options,
                trigger_reason=trigger_reason)
            if submitted:
                self._last_sabr_attempt = now
                self._last_sabr_forward = state.underlying_price

    def update_quotes(self, portfolio, dirty: set | None = None):
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

        # Periodic clear of margin rejection suppression so keys get a
        # fresh attempt every 60s. Adapts to changing margin conditions
        # (position closes, cash deposited, market moves) without restart.
        _now_mono = time.monotonic()
        if _now_mono - self._margin_rejected_last_clear > 300.0:
            self._margin_rejected.clear()
            self._margin_rejected_last_clear = _now_mono

        # Hardburst-skip overlay. When the detector sees front-month futs
        # BBO moved ≥threshold_ticks inside the trailing window_ms, skip
        # this update_quotes invocation entirely. Existing resting quotes
        # are LEFT IN PLACE — cancelling them adds cancel-RTT risk that
        # the mechanism argument doesn't justify (spec Change 2, step 4).
        # Flag defaults OFF; detector-always-on gives us the shadow
        # counter-factual via fill_handler's detector_was_hardburst column.
        # _hardburst_skip_disabled_by_kill shorts the gate when the
        # operational suppression-rate kill has tripped for the day.
        if (getattr(config.quoting, "hardburst_skip_enabled", False)
                and not self._hardburst_skip_disabled_by_kill):
            self._hardburst_update_attempts += 1
            detector = getattr(self.market_data, "burst_detector", None)
            if detector is not None:
                now_ns = time.time_ns()
                if detector.is_hardburst(now_ns):
                    self._hardburst_skip_count += 1
                    self._last_hardburst_skip_ts = now_ns
                    logger.debug(
                        "hardburst_skip: window=%s",
                        detector.snapshot(now_ns),
                    )
                    return  # skip this update cycle entirely; no cycle opened

        # Thread 3 Layer B: cancel resting orders that have drifted by
        # more than M_ticks since placement. Runs before the cycle opens
        # so cancelled orders aren't double-evaluated below. Cheap O(N)
        # scan over active_orders. No-op when flag is OFF.
        self._check_layer_b()

        # priority_v1: reset per-cycle amend counters. Iteration order is
        # |delta|-desc (see sort below), so the first N amends to fire are
        # the highest-priority; anything past the cap defers to next tick.
        self._tick_amend_count = 0
        self._tick_amend_deferred = 0
        # Open a requote cycle for instrumentation. Trigger refinement (refit
        # detection) happens after consume_refit_pending() below.
        self._begin_requote_cycle(
            "dirty_event" if dirty is not None else "periodic")

        # SABR recalibration is hoisted out of this hot path and runs from
        # main.py's loop via maybe_recal_sabr() — calibration takes several ms
        # and was inflating TTT (tick→placeOrder) by landing between the tick
        # handler and the per-strike send. Theo for this cycle uses whatever
        # surface was last calibrated.

        enabled_tokens = getattr(config.quoting, "enabled_expiries", ["front"])
        self._enabled_expiries_resolved = resolve_enabled_expiries(
            enabled_tokens, state.expiries or []
        )
        # max_dte filter (hg_spec_v1.3 §2.3): skip contracts with DTE above
        # the configured cap. Front-month HG cycles bi-monthly (H/K/N/U/Z),
        # so a newly-rolled front can have DTE ~60; the spec quotes only
        # during the final ≤35 days.
        max_dte = getattr(config.product, "max_dte", None)
        if max_dte is not None:
            self._enabled_expiries_resolved = [
                exp for exp in self._enabled_expiries_resolved
                if days_to_expiry(exp) <= max_dte
            ]
        if not self._enabled_expiries_resolved:
            return  # nothing to quote (config misconfigured or pre-discovery)

        # Full set of (strike, expiry, right) tuples we're eligible to quote
        # this cycle, unioned across all enabled expiries. Used for the
        # end-of-cycle stale sweep.
        full_quotable: set = set()
        # Per-expiry quotable lists for iteration
        per_expiry_quotable: dict[str, list] = {}
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

        # Force full re-eval for any expiry whose surface refitted since the
        # last cycle. Without this, a resting order placed against the
        # pre-refit theo can stay below the new min_edge floor for the
        # entire ~250-500ms amend RTT window.
        refit_expiries = self.sabr.consume_refit_pending()
        if refit_expiries and self._cycle is not None:
            self._cycle['refit_expiries'] = sorted(refit_expiries)
            # "refit" takes priority over the generic trigger label — it's
            # the more interesting signal (a material surface change forced
            # a full re-eval of those expiries).
            self._cycle['trigger'] = "refit"

        for exp, pairs in per_expiry_quotable.items():
            if exp in refit_expiries:
                iter_pairs = pairs
            elif dirty is not None:
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
            # spot_at_fit (not cal_forward) — bridges theo by spot drift,
            # not spot-vs-forward gap. See sabr.delta_adjust_theo docstring.
            _exp_spot_at_fit = (self.sabr.get_spot_at_fit(exp)
                                if _exp_cal_ready else 0.0)
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
                                   _exp_spot_at_fit, _exp_underlying)
                self._process_side(portfolio, option, strike, exp, right, "SELL",
                                   inc_ask_info, theo,
                                   _exp_spot_at_fit, _exp_underlying)

        # ── v1.4 OOR inventory close pass ─────────────────────────────
        # For positions outside the sym_5 quote window or on back-month
        # expiries (which aren't opened but ARE subscribed), post a
        # CLOSING-only order using the same penny-jump-the-incumbent
        # logic as in-window quotes. Reduces margin drag and cleans up
        # inventory as ATM re-centers or DTE decays. Runs only on full-
        # refresh cycles; the tick-driven dirty path skips this loop
        # because OOR positions rarely need sub-second repricing.
        # `allowed_order_keys` tracks (strike, exp, right, side) tuples
        # permitted to rest after this cycle — union of in-window both
        # sides + OOR close single sides. The stale sweep below cancels
        # anything outside this set (side-aware, so a stale SELL on a
        # strike where we now only post BUY-to-close gets cancelled).
        allowed_order_keys: set = set()
        for (sk, ex, rt) in full_quotable:
            allowed_order_keys.add((sk, ex, rt, "BUY"))
            allowed_order_keys.add((sk, ex, rt, "SELL"))

        if dirty is None:
            product_key = config.product.underlying_symbol
            all_subscribed_expiries = set(state.expiries or [])
            in_window_by_exp: dict[str, set] = {
                ex: set(pairs) for ex, pairs in per_expiry_quotable.items()
            }
            for pos in portfolio.positions:
                if pos.product != product_key or pos.quantity == 0:
                    continue
                if pos.expiry not in all_subscribed_expiries:
                    continue  # no market data subscription; skip
                key_sr = (pos.strike, pos.put_call)
                if key_sr in in_window_by_exp.get(pos.expiry, set()):
                    continue  # in-window loop already handled both sides
                # Closing side: BUY to close short, SELL to close long.
                close_side = "BUY" if pos.quantity < 0 else "SELL"
                pos_option = state.get_option(
                    pos.strike, expiry=pos.expiry, right=pos.put_call)
                if pos_option is None:
                    continue
                # Theo for this expiry (back-month uses its own fit).
                oor_theo = None
                if (config.pricing.sabr_enabled
                        and self.sabr.get_last_calibration(pos.expiry) is not None):
                    try:
                        oor_theo = self.sabr.get_theo(
                            pos.strike, pos.put_call, expiry=pos.expiry)
                    except Exception:
                        oor_theo = None
                oor_spot_at_fit = (self.sabr.get_spot_at_fit(pos.expiry)
                                   if oor_theo is not None else 0.0)
                # Incumbent on closing side only.
                our_side_prices = our_prices_idx.get(
                    (pos.strike, pos.expiry, pos.put_call, close_side),
                    _empty_set)
                oor_inc = self.market_data.find_incumbent(
                    pos.strike, close_side, our_side_prices,
                    right=pos.put_call, expiry=pos.expiry)
                self._process_side(
                    portfolio, pos_option, pos.strike, pos.expiry,
                    pos.put_call, close_side, oor_inc, oor_theo,
                    oor_spot_at_fit, state.underlying_price,
                    override_qty=abs(pos.quantity),
                )
                allowed_order_keys.add(
                    (pos.strike, pos.expiry, pos.put_call, close_side))

        # Side-aware stale sweep (full-refresh only). Cancels any
        # resting order not in allowed_order_keys: strikes that fell
        # outside the quote window AND aren't backed by an OOR
        # closing-side entry get cleared. Preserves OOR close orders.
        if dirty is None:
            for (strike, exp, right, side) in list(self.active_orders.keys()):
                if (strike, exp, right, side) not in allowed_order_keys:
                    self._cancel_quote(strike, exp, right, side)

        # priority_v1 observability: when the per-tick cap bit this cycle,
        # log so deployment smoke-tests can confirm the policy is active.
        # Silent when no deferrals — avoids log spam in calm markets or
        # when running under legacy policy.
        if self._tick_amend_deferred > 0:
            logger.info(
                "priority_v1 cycle: amends_sent=%d amends_deferred=%d",
                self._tick_amend_count, self._tick_amend_deferred,
            )

        # Close the requote cycle. Emits immediately if no amends were
        # sent OR all acks have already arrived; otherwise _emit_requote_cycle
        # fires from the last async ack in _on_open_order_ack.
        self._end_requote_cycle()

    def reset_daily_hardburst_counters(self) -> None:
        """Reset hardburst skip counters at session rollover. Called from
        main.py's session-boundary hook along with other daily-reset state.

        We intentionally DON'T reset _last_hardburst_skip_ts — a consumer
        wanting "was the last skip recent?" needs to compare vs now itself.
        Also clears the operational-kill override so the next session
        starts fresh.
        """
        self._hardburst_skip_count = 0
        self._hardburst_update_attempts = 0
        self._hardburst_skip_disabled_by_kill = False

    def disable_hardburst_skip_for_session(self, reason: str) -> None:
        """Runtime override — operational_kills calls this when daily
        suppression rate exceeds config's kill rate. Force the gate off
        until reset_daily_hardburst_counters() runs at session rollover.
        Idempotent; safe to call repeatedly."""
        if not self._hardburst_skip_disabled_by_kill:
            self._hardburst_skip_disabled_by_kill = True
            logger.warning(
                "hardburst_skip disabled by operational kill until session "
                "rollover — reason: %s (skips=%d, attempts=%d, rate=%.3f)",
                reason, self._hardburst_skip_count,
                self._hardburst_update_attempts,
                self.get_hardburst_suppression_rate(),
            )

    def get_hardburst_suppression_rate(self) -> float:
        """Fraction of update_quotes invocations that were skipped by the
        hardburst gate since the last daily reset. Returns 0.0 when the
        gate has never been reached (flag OFF or boot with no ticks).
        Used by operational_kills to enforce the ≤20% ceiling."""
        attempts = self._hardburst_update_attempts
        if attempts <= 0:
            return 0.0
        return self._hardburst_skip_count / attempts

    def _priority_bucket(self, option) -> str:
        """Classify a strike by |delta| into near/mid/far bucket.

        Returns 'near' (|delta| ≥ near_threshold, default 0.30), 'far'
        (|delta| < far_threshold, default 0.10), or 'mid' otherwise.
        None / missing delta falls back to 'near' — we'd rather overrun
        the cap on an unclassified strike than under-refresh it.
        """
        if option is None:
            return "near"
        d = getattr(option, "delta", None)
        if d is None:
            return "near"
        ad = abs(d)
        near = float(getattr(self.config.quoting, "delta_threshold_near", 0.30))
        far = float(getattr(self.config.quoting, "delta_threshold_far", 0.10))
        if ad >= near:
            return "near"
        if ad < far:
            return "far"
        return "mid"

    def _process_side(self, portfolio, option, strike: float, expiry: str,
                      right: str, side: str, inc_info: dict,
                      theo: float | None,
                      spot_at_fit: float = 0.0,
                      current_underlying: float = 0.0,
                      override_qty: int | None = None) -> None:
        """Apply skip-reason gate, theo-edge gate, constraint check, and
        order placement for a single (strike, expiry, right, side).

        ``override_qty`` (v1.4 OOR close pass): when set, uses this qty
        instead of config.product.quote_size × backmonth_size_mul. Lets
        the OOR inventory loop post closing orders sized to the exact
        position quantity rather than the standard quote size.
        """
        # Skip if IBKR previously rejected this key for margin — but
        # never suppress closing orders (they reduce margin, not increase it).
        if (strike, expiry, right, side) in self._margin_rejected:
            is_closing = False
            for _p in portfolio.positions:
                if (_p.strike == strike and _p.expiry == expiry
                        and _p.put_call == right):
                    is_closing = ((_p.quantity > 0 and side == "SELL")
                                  or (_p.quantity < 0 and side == "BUY"))
                    break
            if not is_closing:
                return
        config = self.config
        tick = config.quoting.tick_size

        # SABR calibration gate per hg_spec_v1.3.md §3.3 (fourth/fifth
        # bullets): skip strikes where fit quality is unacceptable or the
        # fit hasn't run for this expiry. Cancels any existing resting
        # order on this side to avoid quoting against stale theo.
        if config.pricing.sabr_enabled:
            cal_ok, cal_reason = self.sabr.is_strike_calibrated(strike, expiry)
            if not cal_ok:
                self._cancel_quote(strike, expiry, right, side)
                self._log_quote_telemetry(
                    strike, right, side, None,
                    {**(inc_info or {}), "skip_reason": cal_reason},
                    theo=theo, expiry=expiry,
                )
                return

        # Back-month dampeners (crowsnest 2026-04-18 hotfix). Anything that
        # isn't the front month has thin SVI support (7-10 strikes per side)
        # so theo is extrapolated with wide uncertainty. Widen the quoted
        # edge (via effective_min_edge) and shrink size to cap the dollar
        # damage per fill. Defaults to no-op (mul = 1.0) when config knobs
        # are absent.
        _is_back = (expiry != self.market_data.state.front_month_expiry)
        _width_mul = (float(getattr(config.quoting, "backmonth_width_mul", 1.0))
                      if _is_back else 1.0)
        _size_mul = (float(getattr(config.quoting, "backmonth_size_mul", 1.0))
                     if _is_back else 1.0)
        effective_min_edge = float(config.pricing.min_edge_points or 0.0) * _width_mul
        if override_qty is not None:
            effective_quote_size = max(1, int(override_qty))
        else:
            effective_quote_size = max(
                1, int(round(config.product.quote_size * _size_mul)))

        if theo is not None and option is not None:
            theo = delta_adjust_theo(theo, option.delta,
                                     spot_at_fit, current_underlying)

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
            self._log_quote_telemetry(strike, right, side, None, inc_info,
                                      theo=theo, expiry=expiry)
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

            edge_floor = effective_min_edge
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
        if theo is not None and effective_min_edge > 0:

            edge_floor = effective_min_edge
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

        # Peer-consensus cap (2026-04-22, opt-in via
        # peer_consensus_cap_ticks). Brackets our quote against NBBO mid
        # so a SABR theo that disagrees with market consensus can't park
        # us far below mid (for a SELL) or above mid (for a BUY).
        # Motivated by the 2026-04-22 14:07 C6.10 fill where theo sat
        # 3.5 ticks under mid in a 9-tick-wide market and our resting
        # SELL was picked off on a rally print. Defaults to 0 (disabled)
        # to preserve baseline quoting; positive N enforces the bracket.
        # Skipped under _bypass_wide_market since that path is already
        # mid-anchored by design (see ~line 1069).
        peer_cap_ticks = int(
            getattr(config.quoting, "peer_consensus_cap_ticks", 0) or 0)
        if peer_cap_ticks > 0 and not _bypass_wide_market:
            mkt_bid, mkt_ask = self.market_data.get_clean_bbo(
                strike, right, expiry=expiry)
            if mkt_bid > 0 and mkt_ask > 0 and mkt_ask > mkt_bid:
                mid = (mkt_bid + mkt_ask) / 2.0
                pre_cap = adj
                if side == "BUY":
                    # Don't bid more than peer_cap_ticks above mid.
                    ceiling = floor_to_tick(
                        mid + peer_cap_ticks * tick, tick)
                    if adj > ceiling:
                        adj = ceiling
                else:  # SELL
                    # Don't offer more than peer_cap_ticks below mid.
                    floor_px = ceil_to_tick(
                        mid - peer_cap_ticks * tick, tick)
                    if adj < floor_px:
                        adj = floor_px
                # Instrumentation: emit to skips JSONL whenever cap
                # actually changes the price so we can measure fire rate,
                # price improvement distribution, and for sampling the
                # "SABR-was-right counterfactual" cases.
                if adj != pre_cap and self.csv_logger is not None:
                    try:
                        improvement_ticks = (
                            abs(adj - pre_cap) / tick if tick else 0.0
                        )
                        self.csv_logger.log_paper_stream("skips", {
                            "event_type": "cap_engaged",
                            "symbol": format_hxe_symbol(expiry, right, strike),
                            "cp": right,
                            "strike": float(strike),
                            "expiry": expiry,
                            "side": "BUY" if side == "BUY" else "SELL",
                            "skip_reason": "capped_peer_consensus",
                            "pre_cap_price": float(pre_cap),
                            "post_cap_price": float(adj),
                            "improvement_ticks": float(improvement_ticks),
                            "mid": float(mid),
                            "bbo_bid": float(mkt_bid),
                            "bbo_ask": float(mkt_ask),
                            "theo": float(theo) if theo is not None else None,
                            "forward": self.market_data.state.underlying_price,
                            "cap_ticks": peer_cap_ticks,
                        })
                    except Exception:
                        logger.debug("cap_engaged log failed", exc_info=True)

        # Behind-incumbent gate: if the theo cap pushed our price behind
        # the incumbent, the order has little queue value. The strict
        # semantics (tie_is_behind=True) also skips ties — reasoning was
        # "match the incumbent with worse time priority = dead weight."
        #
        # Relaxed semantics (tie_is_behind=False) treat tie as OK and
        # place at NBBO (we sit behind incumbent in time, but fill on any
        # overflow, and also become the best quote if incumbent cancels).
        # 2026-04-22 experiment B: analysis showed ~37% of behind_incumbent
        # events are ties at 4dp precision; flipping tie_is_behind=False
        # is predicted to eliminate those skips at zero per-fill edge cost
        # (target is unchanged). If fill rate rises without adverse-
        # selection deterioration, permanent change is justified.
        tie_is_behind = bool(getattr(config.quoting, "tie_is_behind", True))
        if inc_info["price"] is not None and not _bypass_wide_market:
            if tie_is_behind:
                behind = ((side == "BUY" and adj <= inc_info["price"])
                          or (side == "SELL" and adj >= inc_info["price"]))
            else:
                behind = ((side == "BUY" and adj < inc_info["price"])
                          or (side == "SELL" and adj > inc_info["price"]))
            if behind:
                self._cancel_quote(strike, expiry, right, side)
                self._log_quote_telemetry(
                    strike, right, side, None,
                    {**inc_info, "skip_reason": "behind_incumbent"},
                    theo=theo, expiry=expiry,
                )
                return

        # Theo edge gate: reject if our price wouldn't sit min_edge_points
        # away from theo on the favorable side. Hysteresis: require N
        # consecutive violations on the same key before cancelling so a
        # single boundary tick doesn't churn a resting order. Default N=1
        # (no hysteresis); set theo_edge_hysteresis_ticks: 2 in config to
        # smooth at the cost of slightly higher behind% on stale quotes.
        key = (strike, expiry, right, side)
        if theo is not None and effective_min_edge > 0:
            edge = effective_min_edge
            hyst = int(getattr(config.pricing, "theo_edge_hysteresis_ticks", 1))
            violates = (adj > theo - edge) if side == "BUY" else (adj < theo + edge)
            if violates:
                streak = self._theo_edge_streak.get(key, 0) + 1
                self._theo_edge_streak[key] = streak
                if streak >= hyst:
                    self._cancel_quote(strike, expiry, right, side)
                self._log_quote_telemetry(strike, right, side, None,
                                          {**inc_info, "skip_reason": "theo_edge"},
                                          theo=theo, expiry=expiry)
                return
            else:
                self._theo_edge_streak.pop(key, None)

        if adj <= 0:
            self._cancel_quote(strike, expiry, right, side)
            self._log_quote_telemetry(strike, right, side, None,
                                      {**inc_info, "skip_reason": "invalid_price"},
                                      theo=theo, expiry=expiry)
            return

        can_quote, reason = should_quote_side(
            option, side,
            self.constraint_checker, self.sabr, config,
        )
        if can_quote:
            self._send_or_update(strike, expiry, right, side, adj,
                                 effective_quote_size, option, theo=theo)
            self._log_quote_telemetry(strike, right, side, adj, inc_info,
                                      theo=theo, expiry=expiry)
        else:
            self._cancel_quote(strike, expiry, right, side)
            self._log_rejection(strike, right, side, reason)
            self._log_quote_telemetry(strike, right, side, None,
                                      {**inc_info, "skip_reason": reason},
                                      theo=theo, expiry=expiry)

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
                        price: float, qty: int, option, theo: float = None):
        """Send new order or modify existing if price changed.

        ``theo`` enables a dead-band bypass: if the resting price violates
        the min_edge floor against current theo, modify even when the
        price delta is below min_modify_ticks. Without this the SABR refit
        can drop theo by 1 tick and our 1-tick price correction gets
        suppressed by the noise filter.

        Thread 3 Layer C cooldown gate: when the master + lever_c flags
        are ON and a burst-pull cooldown is active for ``side`` (or
        globally for C2), this method is a no-op. Modifies on existing
        resting orders are also blocked — the Layer C action already
        cancelled all matching orders, so the active_orders entry
        shouldn't exist; but if it does (race or stale state), don't
        modify into a quote that was just defensively pulled.
        """
        key = (strike, expiry, right, side)
        contract = option.contract

        if contract is None:
            logger.warning("No contract for %s %s%s, cannot send order",
                           expiry, int(strike), right)
            return

        # Layer C cooldown check. Cheap O(1); short-circuit before any
        # other work (incumbent lookup, theo math, modify-storm tracking).
        if self._layer_c_cooldown_active(side=side):
            self._note_cycle_eval("layer_c_cooldown")
            self._last_skip_reason[key] = "layer_c_cooldown"
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
                # Modify-storm guard (defense vector #14). Per (strike, expiry,
                # right, side) so cancel-replace doesn't reset and let us hit
                # the same effective message rate under a fresh orderId. When
                # the window saturates we skip the modify entirely (instead of
                # cancel-replace) — the next quote cycle re-evaluates and
                # prior cancel-replace was already churning oids at IBKR.
                now_ns_storm = time.monotonic_ns()
                window = self._modify_times_per_side.get(key)
                if window is None:
                    window = deque(maxlen=16)
                    self._modify_times_per_side[key] = window
                cutoff = now_ns_storm - 1_000_000_000
                while window and window[0] < cutoff:
                    window.popleft()
                if len(window) >= self._max_modifies_per_sec_per_oid:
                    logger.warning(
                        "Modify storm on %s%s %s oid=%d (%d modifies/sec) — "
                        "skip",
                        int(strike), right, side, order_id, len(window),
                    )
                    return
                window.append(now_ns_storm)
                # Dead-band: skip a re-price unless the new target moves
                # the resting order by at least min_modify_ticks. This
                # suppresses 1-tick noise from a flickering BBO without
                # giving up the leading position.
                #
                # Bypass the dead-band when the *current* resting price
                # violates min_edge against current theo — otherwise a
                # 1-tick theo drop after our last modify leaves the order
                # below the floor, and the very correction that would clear
                # it gets suppressed as "noise."
                tick = self.config.quoting.tick_size
                edge_violation = False
                if theo is not None and self.config.pricing.min_edge_points > 0:
                    edge = self.config.pricing.min_edge_points
                    cur = trade.order.lmtPrice
                    edge_violation = (
                        (side == "BUY" and cur > theo - edge)
                        or (side == "SELL" and cur < theo + edge)
                    )
                # Dead-band + per-tick cap (see `refresh_policy` config).
                # legacy: single min_modify_ticks band, no cap, no max-age.
                # priority_v1: tiered bands by |delta| (near/mid/far),
                # per-cycle amend cap, and a max-age floor that overrides
                # the dead-band so deferred strikes can't stay stale forever.
                # Edge-violation bypasses BOTH the band and the cap — the
                # correctness floor for when current price violates min_edge.
                # Declared up front so the post-success counter path can
                # read it regardless of which policy branch was taken.
                force_refresh = False
                policy = getattr(self.config.quoting, "refresh_policy", "legacy")
                if policy == "priority_v1":
                    cq = self.config.quoting
                    bucket = self._priority_bucket(option)
                    fallback_ticks = float(getattr(cq, "min_modify_ticks", 1))
                    if bucket == "near":
                        band_ticks = float(getattr(
                            cq, "dead_band_near_ticks", fallback_ticks))
                        max_age_s = float(getattr(cq, "max_age_s_near", 1.0))
                    elif bucket == "mid":
                        band_ticks = float(getattr(
                            cq, "dead_band_mid_ticks", fallback_ticks))
                        max_age_s = float(getattr(cq, "max_age_s_mid", 3.0))
                    else:  # far
                        band_ticks = float(getattr(
                            cq, "dead_band_far_ticks", fallback_ticks))
                        max_age_s = float(getattr(cq, "max_age_s_far", 5.0))
                    last_ns = self._last_amend_ns.get(key, 0)
                    age_s = ((time.monotonic_ns() - last_ns) / 1e9
                             if last_ns else float("inf"))
                    force_refresh = age_s > max_age_s
                    if (not edge_violation and not force_refresh
                            and abs(price - trade.order.lmtPrice)
                            < (band_ticks * tick - 1e-9)):
                        return
                    cap = int(getattr(
                        cq, "max_concurrent_amends_per_tick", 4))
                    if (not edge_violation
                            and self._tick_amend_count >= cap):
                        self._tick_amend_deferred += 1
                        return  # defer to next tick; max-age keeps honest
                else:
                    min_dt = float(getattr(
                        self.config.quoting, "min_modify_ticks", 1))
                    if (not edge_violation
                            and abs(price - trade.order.lmtPrice)
                            < (min_dt * tick - 1e-9)):
                        return
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
                        # priority_v1 tracking: record successful modify send
                        # time + count against per-cycle cap. Harmless under
                        # legacy (no reader consults them).
                        self._last_amend_ns[key] = now_ns
                        self._tick_amend_count += 1
                        self._note_cycle_amend_send(order_id)
                        # Classify the amend for diagnostic tuning: a
                        # max-age-driven "touch" where the price wasn't
                        # changing is a force_refresh; anything else
                        # (price moved past dead-band, or edge_violation
                        # correctness fix) counts as a price-driven amend.
                        if force_refresh and not edge_violation:
                            self._amend_force_count += 1
                        else:
                            self._amend_price_count += 1
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
        # priority_v1: start the max-age clock for this key on first place.
        # New places are intentionally exempt from the per-tick amend cap —
        # a missing resting order is higher priority than throttling churn.
        self._last_amend_ns[key] = time.monotonic_ns()
        # Thread 3 §6 instrumentation: per-order placement event. F_at
        # is the underlying price at placement, used by Layer B tuning
        # (CDF of |ΔF since placement| at cancel/fill time).
        if self.csv_logger is not None:
            try:
                self.csv_logger.log_paper_order_lifecycle(
                    order_id=trade.order.orderId, event="placed",
                    strike=strike, expiry=expiry, right=right, side=side,
                    price=price,
                    forward=self.market_data.state.underlying_price,
                    reason="",
                )
            except Exception:
                logger.debug("order_lifecycle emit failed", exc_info=True)

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

    def fill_latency_ms(self, order_id: int) -> float | None:
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
                    self._note_cycle_amend_ack(oid, amend_us)
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

        # Error 201: margin rejection. Track the key so we don't re-submit
        # the same order every cycle. Cleared on fills (margin may change).
        # Also invalidate the cross-product margin cache: IBKR's view of
        # used capital just moved, so any sibling engine's cached portfolio
        # (e.g. HG while this rejection came from ETH) is now stale and
        # could pass a check that the shared budget won't actually allow.
        if errorCode == 201:
            for key, oid in list(self.active_orders.items()):
                if oid == reqId:
                    self._margin_rejected.add(key)
                    self.active_orders.pop(key, None)
                    self._order_underlying.pop(oid, None)
                    break
            mc = self.constraint_checker.margin
            coord = getattr(mc, "coordinator", None)
            if coord is not None:
                coord.invalidate_portfolio()
            else:
                mc.invalidate_portfolio()
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
        """Return rolling percentiles (microseconds) for TTT, RTT, and amend."""
        def stats(buf):
            if not buf:
                return {"n": 0, "p10": None, "p25": None, "p50": None,
                        "p75": None, "p90": None, "p99": None, "min": None, "max": None}
            s = sorted(buf)
            n = len(s)
            def pct(q):
                return s[min(n - 1, int(n * q))]
            return {
                "n": n, "min": s[0], "max": s[-1],
                "p10": pct(0.10), "p25": pct(0.25), "p50": pct(0.50),
                "p75": pct(0.75), "p90": pct(0.90), "p99": pct(0.99),
            }
        total_amends = self._amend_force_count + self._amend_price_count
        force_pct = (
            100.0 * self._amend_force_count / total_amends
            if total_amends > 0 else 0.0
        )
        return {
            "ttt_us": stats(self._ttt_us),
            "place_rtt_us": stats(self._place_rtt_us),
            "amend_us": stats(self._amend_us),
            "amend_reasons": {
                "force_refresh": self._amend_force_count,
                "price_change": self._amend_price_count,
                "force_pct": round(force_pct, 1),
                "total": total_amends,
            },
        }

    def _cancel_quote(self, strike: float, expiry: str, right: str,
                      side: str, reason: str = "requote"):
        """Cancel a quote at a specific (strike, expiry, right, side).

        Skips cancellation when the order was placed less than
        MIN_ORDER_LIFETIME_MS ago AND is still in PendingSubmit (i.e., IBKR
        hasn't acked yet). Cancelling in that window produces a
        PendingSubmit → PendingCancel transition that never visits Submitted,
        so the order is invisible on the book and we burn a place/cancel pair
        for nothing. The next quote tick will re-evaluate and cancel then if
        the skip condition still holds.

        ``reason`` is passed through to the order_lifecycle stream so
        downstream analysis can distinguish requote-driven cancels (the
        normal hot-path) from layer_B / layer_C / manual cancels.
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
        # Thread 3 §6 instrumentation. Drop tracking dicts so the next
        # placement on this key starts clean. Also emits the cancel row.
        self._order_underlying.pop(order_id, None)
        if self.csv_logger is not None:
            try:
                self.csv_logger.log_paper_order_lifecycle(
                    order_id=order_id, event="cancelled",
                    strike=strike, expiry=expiry, right=right, side=side,
                    price=getattr(getattr(trade, "order", None),
                                  "lmtPrice", None) if trade else None,
                    forward=self.market_data.state.underlying_price,
                    reason=reason,
                )
            except Exception:
                logger.debug("order_lifecycle emit failed", exc_info=True)

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

    # ── Thread 3 Layer B: cancel-on-ΔF since placement ────────────────
    def _check_layer_b(self) -> None:
        """Cancel any resting order whose underlying price has drifted by
        more than ``M_ticks`` since that order's last placement/amend.
        ``_order_underlying[order_id]`` is the anchor — set on placement
        (line ~1700 in _send_or_update) and refreshed on every modify.

        Spec: do NOT auto-replace; let the next requote cycle re-evaluate.
        That avoids replacing a cancelled-stale order with a still-stale
        one in the same handler tick.
        """
        thread3 = getattr(self.config.quoting, "thread3", None)
        if thread3 is None:
            return
        if not bool(getattr(thread3, "master_enabled", False)):
            return
        if not bool(getattr(
                thread3, "lever_b_cancel_on_df_enabled", False)):
            return
        m_ticks = float(getattr(thread3, "layer_b_m_ticks", 3))
        tick_size = float(getattr(self.config.quoting, "tick_size", 0.0005))
        if m_ticks <= 0 or tick_size <= 0:
            return
        threshold_usd = m_ticks * tick_size
        F_now = self.market_data.state.underlying_price
        if F_now <= 0:
            return  # no live forward; nothing to compare against
        for key in list(self.active_orders.keys()):
            order_id = self.active_orders.get(key)
            if order_id is None:
                continue
            F_at_placement = self._order_underlying.get(order_id)
            if F_at_placement is None or F_at_placement <= 0:
                continue
            dF = abs(F_now - F_at_placement)
            if dF <= threshold_usd:
                continue
            strike, expiry, right, side = key
            trade = self._canonical_trade(order_id)
            # Same MIN_ORDER_LIFETIME_NS guard as _cancel_quote: if IBKR
            # hasn't acked, don't cancel — the place/cancel pair would
            # never visit Submitted and we'd burn a round-trip. The next
            # F-tick will catch it once acked.
            if trade is not None:
                placed_ns = self._placed_at_ns.get(order_id)
                status = getattr(getattr(trade, "orderStatus", None),
                                 "status", "")
                if (placed_ns is not None
                        and status in ("PendingSubmit", "ApiPending")
                        and (time.monotonic_ns() - placed_ns)
                                < MIN_ORDER_LIFETIME_NS):
                    continue
                self._try_cancel_order(trade.order)
            self.active_orders.pop(key, None)
            self._pending_amend.pop(order_id, None)
            self._order_underlying.pop(order_id, None)
            if self.csv_logger is not None:
                try:
                    self.csv_logger.log_paper_order_lifecycle(
                        order_id=order_id, event="cancelled",
                        strike=strike, expiry=expiry, right=right,
                        side=side,
                        price=getattr(getattr(trade, "order", None),
                                      "lmtPrice", None) if trade else None,
                        forward=F_now, reason="layer_B",
                    )
                except Exception:
                    logger.debug("order_lifecycle emit failed", exc_info=True)

    # ── Thread 3 Layer C: burst-rate quote pull ───────────────────────
    def note_layer_c_fill(self, strike: float, expiry: str, right: str,
                          side: str, ts_ns: int | None = None) -> int:
        """Layer C entry point. Called by FillHandler immediately after a
        live fill is recorded. Returns the any-side burst_1s count
        INCLUDING the just-recorded fill (for fills.burst_1s_at_fill).

        Side note: ``side`` is the FILL side ("BUY" if we bought, "SELL"
        if we sold), which is the side of the resting order that was
        lifted/hit. Same-side cluster semantics in the brief refer to
        the resting-order side: aggressors lifting a row of stale asks
        produce SELL fills (we were selling, they were buying); aggressors
        hitting a row of stale bids produce BUY fills.

        Per Thread 3 deployment runbook Phase 3 instrumentation
        correction: threshold-crossing always emits a burst_events row
        regardless of master/sub-flag state. The flag gates the
        ACTION (cancel quotes + cooldown + hedge drain), not the
        OBSERVATION. Observational rows have ``action_fired=False`` and
        empty ``pulled_order_ids``; downstream tooling uses them to
        detect P1/P2-shaped patterns under flag-OFF baseline (§7).
        """
        if ts_ns is None:
            ts_ns = time.monotonic_ns()
        # 1. Record + read counts (always, regardless of flags). The
        # tracker's window matches config.quoting.thread3.layer_c_window_sec
        # — typically 1.0s.
        same, any_count = self._fill_burst_tracker.record_and_evaluate(
            ts_ns, side)

        # 2. Always evaluate thresholds (observational logging is
        # independent of action eligibility).
        thread3 = getattr(self.config.quoting, "thread3", None)
        if thread3 is None:
            return any_count
        k1 = int(getattr(thread3, "layer_c1_k1_threshold", 2))
        k2 = int(getattr(thread3, "layer_c2_k2_threshold", 3))
        c1_crossed = same >= k1
        c2_crossed = any_count >= k2
        if not (c1_crossed or c2_crossed):
            return any_count

        # 3. Determine action eligibility. C2 takes precedence over C1
        # when both cross AND both sub-flags are enabled.
        master_on = bool(getattr(thread3, "master_enabled", False))
        layer_c_on = (master_on
                      and bool(getattr(thread3,
                                       "lever_c_burst_pull_enabled", False)))
        c1_enabled = (layer_c_on
                      and bool(getattr(thread3,
                                       "lever_c1_same_side_enabled", False)))
        c2_enabled = (layer_c_on
                      and bool(getattr(thread3,
                                       "lever_c2_any_side_enabled", False)))
        fire_c2 = c2_crossed and c2_enabled
        fire_c1 = c1_crossed and c1_enabled and not fire_c2

        # 4. Execute and emit. ALWAYS emit a row per crossed trigger; set
        # action_fired only on the trigger whose action actually ran.
        if fire_c2:
            self._fire_burst_pull_c2(
                side=side, strike=strike,
                same_count=same, any_count=any_count, ts_ns=ts_ns)
            if c1_crossed:
                # C1 also crossed but C2 superseded — emit observational.
                self._emit_burst_event_observation(
                    trigger="C1", same_count=same, any_count=any_count,
                    ts_ns=ts_ns, fill_strike=strike, fill_side=side)
        elif fire_c1:
            self._fire_burst_pull_c1(
                side=side, strike=strike,
                same_count=same, any_count=any_count, ts_ns=ts_ns)
            if c2_crossed:
                # C2 crossed but its sub-flag is off (otherwise C2 would
                # have fired by precedence). Observational only.
                self._emit_burst_event_observation(
                    trigger="C2", same_count=same, any_count=any_count,
                    ts_ns=ts_ns, fill_strike=strike, fill_side=side)
        else:
            # Pure observation — neither action eligible.
            if c2_crossed:
                self._emit_burst_event_observation(
                    trigger="C2", same_count=same, any_count=any_count,
                    ts_ns=ts_ns, fill_strike=strike, fill_side=side)
            if c1_crossed:
                self._emit_burst_event_observation(
                    trigger="C1", same_count=same, any_count=any_count,
                    ts_ns=ts_ns, fill_strike=strike, fill_side=side)
        return any_count

    def _emit_burst_event_observation(self, *, trigger: str,
                                      same_count: int, any_count: int,
                                      ts_ns: int, fill_strike: float,
                                      fill_side: str) -> None:
        """Emit a burst_events row for an observational threshold cross
        (master OFF, sub-flag OFF, or superseded by another trigger).
        Reports the as-if cooldown deadline so downstream can reconstruct
        the counterfactual window without recomputing thresholds."""
        thread3 = getattr(self.config.quoting, "thread3", None)
        if thread3 is None:
            return
        if trigger == "C1":
            cooldown_sec = float(getattr(
                thread3, "layer_c1_cooldown_sec", 2.0))
        else:
            cooldown_sec = float(getattr(
                thread3, "layer_c2_cooldown_sec", 3.0))
        deadline = ts_ns + int(cooldown_sec * 1_000_000_000)
        self._emit_burst_event(
            trigger=trigger,
            same_count=same_count, any_count=any_count,
            cooldown_sec=cooldown_sec, cooldown_until_mono_ns=deadline,
            pulled=[], fill_strike=fill_strike, fill_side=fill_side,
            action_fired=False)

    def _fire_burst_pull_c1(self, *, side: str, strike: float,
                            same_count: int, any_count: int,
                            ts_ns: int) -> None:
        """C1 (same-side burst): cancel all resting same-side quotes
        across all expiries; arm a per-side cooldown; raise hedge-drain
        priority signal.
        """
        thread3 = getattr(self.config.quoting, "thread3", None)
        cooldown_sec = float(getattr(
            thread3, "layer_c1_cooldown_sec", 2.0)) if thread3 else 2.0
        cooldown_ns = int(cooldown_sec * 1_000_000_000)
        deadline = ts_ns + cooldown_ns
        # Take the larger deadline if a prior C1 hasn't expired.
        prev = self._burst_cooldown_per_side_until_ns.get(side, 0)
        self._burst_cooldown_per_side_until_ns[side] = max(prev, deadline)

        pulled = self._pull_resting_orders(
            scope="same_side", side=side, reason="layer_C1")
        self._burst_pull_c1_count += 1

        # Hedge drain. Pass the cooldown deadline so the hedge engine
        # bypasses cadence for at least the cooldown window.
        self._raise_hedge_drain(deadline, reason="layer_C1")

        # Telemetry.
        self._emit_burst_event(
            trigger="C1",
            same_count=same_count, any_count=any_count,
            cooldown_sec=cooldown_sec, cooldown_until_mono_ns=deadline,
            pulled=pulled, fill_strike=strike, fill_side=side,
            action_fired=True,
        )
        logger.warning(
            "Layer C1 fired: side=%s same=%d any=%d pulled=%d cooldown=%.1fs",
            side, same_count, any_count, len(pulled), cooldown_sec,
        )

    def _fire_burst_pull_c2(self, *, side: str, strike: float,
                            same_count: int, any_count: int,
                            ts_ns: int) -> None:
        """C2 (any-side burst): cancel ALL resting quotes (both sides,
        all strikes, all expiries); arm a global cooldown; raise hedge-
        drain priority signal. Spec §3 hard requirement: hedge-drain is
        coupled to C2.
        """
        thread3 = getattr(self.config.quoting, "thread3", None)
        cooldown_sec = float(getattr(
            thread3, "layer_c2_cooldown_sec", 3.0)) if thread3 else 3.0
        cooldown_ns = int(cooldown_sec * 1_000_000_000)
        deadline = ts_ns + cooldown_ns
        # C2 cooldown supersedes any C1 cooldown.
        self._burst_cooldown_global_until_ns = max(
            self._burst_cooldown_global_until_ns, deadline)
        # Also extend per-side cooldowns up to the global deadline so
        # the per-side gate doesn't unlock earlier than the global one.
        for s in ("BUY", "SELL"):
            prev = self._burst_cooldown_per_side_until_ns.get(s, 0)
            self._burst_cooldown_per_side_until_ns[s] = max(prev, deadline)

        pulled = self._pull_resting_orders(
            scope="all", side=side, reason="layer_C2")
        self._burst_pull_c2_count += 1

        self._raise_hedge_drain(deadline, reason="layer_C2")

        self._emit_burst_event(
            trigger="C2",
            same_count=same_count, any_count=any_count,
            cooldown_sec=cooldown_sec, cooldown_until_mono_ns=deadline,
            pulled=pulled, fill_strike=strike, fill_side=side,
            action_fired=True,
        )
        logger.critical(
            "Layer C2 fired: side=%s same=%d any=%d pulled=%d cooldown=%.1fs",
            side, same_count, any_count, len(pulled), cooldown_sec,
        )

    def _pull_resting_orders(self, *, scope: str, side: str,
                             reason: str) -> list[int]:
        """Cancel the orders matched by ``scope`` and emit per-order
        lifecycle rows. Returns the list of cancelled orderIds.

        ``scope == "same_side"``: cancel every active_orders entry whose
            (strike, expiry, right, side) tuple's side equals ``side``.
        ``scope == "all"``: cancel every active_orders entry.
        """
        pulled: list[int] = []
        forward = self.market_data.state.underlying_price
        for key in list(self.active_orders.keys()):
            _, _, _, key_side = key
            if scope == "same_side" and key_side != side:
                continue
            order_id = self.active_orders.get(key)
            if order_id is None:
                continue
            strike, expiry, right, ks = key
            trade = self._canonical_trade(order_id)
            # Use the underlying cancel primitive (token-bucketed). Skip
            # the MIN_ORDER_LIFETIME guard from _cancel_quote because
            # the burst-pull is a defensive panic action — we'd rather
            # burn a place/cancel pair than leave a stale order live
            # through the cooldown window.
            if trade is not None:
                self._try_cancel_order(trade.order)
            self.active_orders.pop(key, None)
            self._pending_amend.pop(order_id, None)
            self._order_underlying.pop(order_id, None)
            pulled.append(order_id)
            if self.csv_logger is not None:
                try:
                    self.csv_logger.log_paper_order_lifecycle(
                        order_id=order_id, event="cancelled",
                        strike=strike, expiry=expiry, right=right,
                        side=ks,
                        price=getattr(getattr(trade, "order", None),
                                      "lmtPrice", None) if trade else None,
                        forward=forward, reason=reason,
                    )
                except Exception:
                    logger.debug("order_lifecycle emit failed", exc_info=True)
        return pulled

    def _raise_hedge_drain(self, cooldown_until_mono_ns: int,
                           reason: str) -> None:
        """Hedge-drain priority signal (spec §3 HARD REQUIREMENT).
        Synchronous: returns once the hedge manager has observed the
        flag. Wired as a soft handle (``self.hedge_manager``) injected
        by main.py after construction."""
        hm = self.hedge_manager
        if hm is None:
            return
        try:
            hm.request_priority_drain(
                cooldown_until_mono_ns=cooldown_until_mono_ns,
                reason=reason)
        except Exception:
            logger.exception("hedge priority-drain signal failed")

    def _emit_burst_event(self, *, trigger: str,
                          same_count: int, any_count: int,
                          cooldown_sec: float,
                          cooldown_until_mono_ns: int,
                          pulled: list[int], fill_strike: float,
                          fill_side: str,
                          action_fired: bool) -> None:
        if self.csv_logger is None:
            return
        try:
            self.csv_logger.log_paper_burst_event(
                trigger=trigger,
                K1_count=same_count,
                K2_count=any_count,
                window_sec=float(getattr(getattr(self.config.quoting,
                                                  "thread3", None),
                                          "layer_c_window_sec", 1.0)),
                pulled_order_ids=pulled,
                cooldown_sec=cooldown_sec,
                cooldown_until_mono_ns=cooldown_until_mono_ns,
                action_fired=action_fired,
                forward=self.market_data.state.underlying_price,
                fill_strike=fill_strike,
                fill_side=fill_side,
            )
        except Exception:
            logger.debug("burst_events emit failed", exc_info=True)

    def _layer_c_cooldown_active(self, side: str | None = None,
                                 now_ns: int | None = None) -> bool:
        """True if a Layer C cooldown blocks placement on ``side`` (or
        any side, if ``side`` is None — used by the global gate at the
        top of update_quotes). Cheap O(1) lookup."""
        if now_ns is None:
            now_ns = time.monotonic_ns()
        if self._burst_cooldown_global_until_ns > now_ns:
            return True
        if side is not None:
            until = self._burst_cooldown_per_side_until_ns.get(side, 0)
            if until > now_ns:
                return True
        else:
            for until in self._burst_cooldown_per_side_until_ns.values():
                if until > now_ns:
                    return True
        return False

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

    def flatten_all_positions(self, portfolio, reason: str = "flatten") -> int:
        """Emergency flatten: cancel every resting order and send
        aggressive IOC limit orders to close every open option position
        in this product.

        v1.4 §6.1 (daily P&L halt) and §6.2 (SPAN margin kill) require
        this behavior — not just quote cancel. Book should be empty of
        both resting orders AND positions when this returns from IBKR's
        perspective within 1-2 seconds.

        Aggressive-limit approach per spec: IOC limit at market ± 1 tick
        (buy-to-close above the ask, sell-to-close below the bid).
        Bounded slippage vs. a MKT order; IOC tif so residuals don't
        rest if the counterparty-side depth moves.

        Returns the number of close orders submitted. Does NOT wait for
        fills — caller is responsible for sequencing (e.g. kill() should
        log the flatten intent before the fills arrive asynchronously).
        """
        # Step 1: clear any resting orders first.
        self.panic_cancel()

        # Step 2: filter portfolio positions to this product only.
        product_key = self.config.product.underlying_symbol
        to_flatten = [
            p for p in portfolio.positions
            if p.product == product_key and p.quantity != 0
        ]
        if not to_flatten:
            logger.info("flatten_all_positions: no open positions for %s",
                        product_key)
            return 0

        state = self.market_data.state
        tick = self.config.quoting.tick_size
        sent = 0

        for pos in to_flatten:
            # Close = opposite side of current qty.
            close_qty = abs(pos.quantity)
            close_side = "BUY" if pos.quantity < 0 else "SELL"

            # Resolve the option contract. Prefer the cached market_data
            # contract (already qualified); fall back to the discovered
            # chain.
            option = state.get_option(pos.strike, pos.expiry, pos.put_call)
            if option is None or option.contract is None:
                logger.warning(
                    "flatten: no contract for %s %s %s%s %s — skipping",
                    product_key, pos.expiry, f"{pos.strike:g}",
                    pos.put_call, close_side,
                )
                continue

            # Aggressive price: ± 1 tick past the opposite-side BBO so
            # the order crosses immediately. If BBO is missing/stale,
            # use SABR theo with a 20% crossing cushion (theo tracks
            # current F/IV, whereas avg_fill_price can be days old on
            # a position held across underlying moves). Fall back to
            # avg_fill_price only if theo is also unavailable — this
            # is a tail-event-only path where the flatten must fire
            # somehow on a degraded feed.
            bbo_bid = getattr(option, "bid", 0.0) or 0.0
            bbo_ask = getattr(option, "ask", 0.0) or 0.0
            if close_side == "BUY" and bbo_ask > 0:
                price = ceil_to_tick(bbo_ask + tick, tick)
            elif close_side == "SELL" and bbo_bid > 0:
                price = floor_to_tick(max(bbo_bid - tick, tick), tick)
            else:
                # BBO missing — try SABR theo first (fresh vs. avg_fill_price).
                theo = None
                try:
                    theo = self.sabr.get_theo(pos.strike, pos.put_call,
                                               expiry=pos.expiry)
                except Exception:
                    theo = None
                if theo and theo > 0:
                    # Cross with a 20% buffer. Bounded slippage vs a MKT
                    # order, reliably crosses any reasonable resting
                    # counterparty.
                    if close_side == "BUY":
                        price = ceil_to_tick(theo * 1.20, tick)
                    else:
                        price = floor_to_tick(
                            max(theo * 0.80, tick), tick)
                    logger.warning(
                        "flatten %s%s %s: BBO missing, using SABR theo "
                        "%.4f × %s crossing buffer → %g",
                        pos.put_call, f"{pos.strike:g}", close_side,
                        theo, "1.20" if close_side == "BUY" else "0.80",
                        price,
                    )
                else:
                    # Final fallback — avg_fill_price with tick cushion.
                    # Strictly worse than theo-based, but the flatten
                    # must fire somehow.
                    fallback = max(pos.avg_fill_price, tick)
                    if close_side == "BUY":
                        price = ceil_to_tick(fallback + 2 * tick, tick)
                    else:
                        price = floor_to_tick(max(fallback - 2 * tick, tick), tick)
                    logger.warning(
                        "flatten %s%s %s: BBO AND theo missing, falling "
                        "back to avg_fill_price %.4f → %g (may not cross)",
                        pos.put_call, f"{pos.strike:g}", close_side,
                        pos.avg_fill_price, price,
                    )

            # orderRef includes "flatten" and the reason so post-mortem
            # log grep-ability is clean (distinguishes halt flattens from
            # normal closes).
            order = LimitOrder(
                action=close_side,
                totalQuantity=close_qty,
                lmtPrice=price,
                tif="IOC",
                account=self._account,
                orderRef=f"{ORDER_REF_PREFIX}_flatten_{reason}_"
                         f"{pos.expiry[-4:]}_{pos.strike:g}{pos.put_call}",
            )
            try:
                trade = self.ib.placeOrder(option.contract, order)
                sent += 1
                logger.critical(
                    "FLATTEN: %s %d %s%s@%g (reason=%s, qty_before=%d)",
                    close_side, close_qty, pos.put_call,
                    f"{pos.strike:g}", price, reason, pos.quantity,
                )
            except Exception as e:
                logger.error("flatten: placeOrder failed for %s %s %s%s: %s",
                             product_key, pos.expiry, pos.strike, pos.put_call, e)

        logger.critical(
            "FLATTEN submitted: %d/%d IOC close orders (reason=%s)",
            sent, len(to_flatten), reason,
        )
        return sent

    def _log_quote_telemetry(self, strike: float, right: str, side: str,
                             our_price: float | None, info: dict,
                             theo: float | None = None,
                             expiry: str | None = None):
        """Emit per-quote telemetry row."""
        skip_reason = info.get("skip_reason", "")
        # Cycle instrumentation: count every evaluation (including skips)
        # regardless of csv_logger presence so tests see the right counts.
        self._note_cycle_eval(skip_reason)
        if self.csv_logger is None:
            return
        if self.config.logging.log_quotes:
            try:
                self.csv_logger.log_quote(
                    strike=strike, side=side, our_price=our_price,
                    incumbent_price=info.get("price"),
                    incumbent_level=info.get("level"),
                    incumbent_size=info.get("size"),
                    incumbent_age_ms=info.get("age_ms"),
                    bbo_width=info.get("bbo_width"),
                    skip_reason=skip_reason,
                    theo=theo,
                    put_call=right,
                )
            except Exception as e:
                logger.debug("quote telemetry log failed: %s", e)

        # v1.4 §9.5 skips.jsonl — emit to the dedicated paper skip stream,
        # but only on REASON-CHANGE per (strike, expiry, right, side) to
        # avoid drowning the JSONL in 4Hz × 44-orders-worth of rows that
        # repeat the same reason. The CSV quotes.csv still carries every
        # tick for fine-grained analysis; skips.jsonl is the summary
        # stream reconciliation ingests.
        if skip_reason and expiry is not None:
            key = (strike, expiry, right, side)
            prev = self._last_skip_reason.get(key)
            if prev != skip_reason:
                self._last_skip_reason[key] = skip_reason
                try:
                    self.csv_logger.log_paper_skip(
                        symbol=format_hxe_symbol(expiry, right, strike),
                        side="BUY" if side == "BUY" else "SELL",
                        skip_reason=skip_reason,
                        strike=float(strike),
                        cp=right,
                        expiry=expiry,
                        theo=theo,
                        bbo_bid=info.get("bid"),
                        bbo_ask=info.get("ask"),
                        forward=self.market_data.state.underlying_price,
                    )
                except Exception as e:
                    logger.debug("paper skip event emit failed: %s", e)
        elif not skip_reason and expiry is not None:
            # Placed successfully — clear the "last skip" so a future skip
            # on this key re-emits the transition.
            self._last_skip_reason.pop(
                (strike, expiry, right, side), None)

    def _log_rejection(self, strike: float, right: str, side: str, reason: str):
        """Log a quote rejection."""
        if self.config.logging.log_rejections:
            logger.info("REJECT %s %d%s: %s", side, int(strike), right, reason)

    @property
    def active_quote_count(self) -> int:
        return len(self.active_orders)

    def get_active_quotes(self) -> dict:
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


