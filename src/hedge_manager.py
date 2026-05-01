"""Delta-hedge manager for Corsair v2 (v1.4 §5).

Maintains the options book at ~zero net delta by trading the product's
front-month underlying future (for HG: the front-month HG copper futures).

Two execution modes:
  - "observe": compute required hedge but log intent only; does NOT
    place orders. Used for Stage 0 bootstrap so induced tests can verify
    the force-hedge path fires without futures execution risk.
  - "execute": place aggressive IOC limit orders to bring net delta inside
    the ±tolerance band.

Tolerance band: ±0.5 contract-deltas per v1.4 §5. No trade if |net_delta|
<= tolerance. Trade size = whole-contract rounding of the breach.

Triggers:
  - rebalance_on_fill(): called by FillHandler after every live fill
  - rebalance_periodic(): called by main loop every rebalance_cadence_sec
  - force_flat(): called by RiskMonitor on delta-kill (hedge to exactly 0)
  - flatten_on_halt(): called on daily-P&L / margin kill (close the hedge)

Hedge MTM (for v1.4 §6.1 daily P&L halt calc):
  hedge_mtm = hedge_qty * (current_F - avg_entry_F) * multiplier

No local state for futures PnL is authoritatively reconciled with IBKR —
that'd require tracking futures fills through a separate event path.
Stage 1 acceptable: the observe-mode log is the audit trail; live execute
mode will need reconciliation via ib.positions() polls on the futures
side (deferred to v1.5 or when flipping to execute).
"""

import logging
import time
from collections import deque
from datetime import date, datetime, timedelta

from ib_insync import Future, LimitOrder

logger = logging.getLogger(__name__)

# Soft cap on _processed_exec_ids dedupe set. FIFO eviction once the
# matching deque hits this size — keeps memory bounded while the dedupe
# window stays comfortably wider than any realistic execDetails replay.
PROCESSED_EXEC_IDS_MAX = 10_000


class HedgeFanout:
    """Multi-product hedge dispatcher.

    Wraps N per-product HedgeManager instances so RiskMonitor can
    invoke a single ``force_flat(reason)`` / ``flatten_on_halt(reason)``
    and have it reach every product's hedge. Also sums MTM across
    products for the daily P&L halt calc.

    Each HedgeManager remains independently responsible for its own
    product; the fanout is strictly a multiplexer with exception
    isolation (a failure in one product's hedge path doesn't block
    the others).
    """

    def __init__(self, managers: list["HedgeManager"]):
        self._managers = list(managers)

    def force_flat(self, reason: str) -> None:
        for m in self._managers:
            try:
                m.force_flat(reason=reason)
            except Exception:
                logger.exception("HedgeFanout.force_flat: %s",
                                 getattr(m, "_product", "?"))

    def flatten_on_halt(self, reason: str) -> None:
        for m in self._managers:
            try:
                m.flatten_on_halt(reason=reason)
            except Exception:
                logger.exception("HedgeFanout.flatten_on_halt: %s",
                                 getattr(m, "_product", "?"))

    def mtm_usd(self) -> float:
        """Sum futures-hedge MTM across every product. RiskMonitor
        includes this in the daily P&L halt calc (v1.4 §6.1)."""
        total = 0.0
        for m in self._managers:
            try:
                total += float(m.hedge_mtm_usd())
            except Exception:
                pass
        return total

    def hedge_qty_for_product(self, product: str) -> int:
        """Return the signed hedge contract-qty for a given product key,
        or 0 if no manager matches. Used by the effective-delta gating
        path in RiskMonitor's per-product loop. CLAUDE.md §10 dependency
        — hedge_qty is local intent, not IBKR-confirmed state."""
        for m in self._managers:
            if getattr(m, "_product", None) == product:
                try:
                    return int(m.hedge_qty)
                except Exception:
                    return 0
        return 0


class HedgeManager:
    """Delta-hedge manager per v1.4 §5.

    Construct one instance per product (each product has its own
    underlying future). Wire into FillHandler (rebalance_on_fill) and
    RiskMonitor (force_flat, flatten_on_halt).
    """

    def __init__(self, ib, config, market_data, portfolio, csv_logger=None):
        self.ib = ib
        self.config = config
        self.market_data = market_data
        self.portfolio = portfolio
        self.csv_logger = csv_logger

        # Config defaults cover missing fields so a partial config doesn't
        # throw — this class must not be the reason a kill switch fails
        # to fire.
        h = getattr(config, "hedging", None)
        self.enabled: bool = bool(getattr(h, "enabled", False))
        self.mode: str = str(getattr(h, "mode", "observe") or "observe")
        self.tolerance: float = float(getattr(h, "tolerance_deltas", 0.5))
        self.rebalance_on_fill_enabled: bool = bool(
            getattr(h, "rebalance_on_fill", True))
        self.rebalance_cadence_sec: float = float(
            getattr(h, "rebalance_cadence_sec", 30.0))
        self.include_in_daily_pnl: bool = bool(
            getattr(h, "include_in_daily_pnl", True))
        self.flatten_on_halt_enabled: bool = bool(
            getattr(h, "flatten_on_halt", True))

        # Product-scoped key for portfolio.delta_for_product(). For HG,
        # this is the IBKR underlying symbol ("HG").
        self._product = config.product.underlying_symbol

        # Futures tick size for execute-mode aggressive pricing. Prefer
        # an explicit hedge_tick_size override (e.g. ETH futures tick
        # $0.25 while options tick is $0.50); fall back to the options
        # tick (HG uses the same tick for both).
        self._hedge_tick: float = float(
            getattr(h, "hedge_tick_size", None)
            or getattr(config.quoting, "tick_size", 0.0005))
        # IOC limit aggression in ticks past F. 2026-04-27: bumped from
        # 1 → 2 after observing periodic BUY 4 IOCs systematically dying
        # because F+1 tick lands inside the bid-ask spread when the
        # market is wider than 2 ticks (late-RTH / thin-liquidity
        # regime). 2 ticks crosses the ASK in spreads up to 4 ticks
        # wide; configurable so we can tune per-product.
        self._ioc_tick_offset: int = int(
            getattr(h, "ioc_tick_offset", 2))

        # Local hedge state. avg_entry_F is the qty-weighted average
        # futures price at which we're holding this hedge position; used
        # for MTM. Both reset when hedge_qty returns to 0.
        self.hedge_qty: int = 0
        self.avg_entry_F: float = 0.0
        # Cumulative realized P&L from closed hedge cycles (in-memory;
        # resets on restart, not persisted). Surfaced via snapshot so the
        # dashboard's account row can fold in hedge realized — IBKR's
        # `RealizedPnL` account tag does not pick up futures fills on
        # this paper account (verified 2026-04-26: $8.5K of hedge realized
        # invisible in account stream). v1.5 reconciliation will replace
        # this local tracker with execDetailsEvent + ib.positions().
        self.realized_pnl_usd: float = 0.0
        self._last_periodic_ns: int = 0
        # Thread 3 Layer C priority drain deadline (monotonic_ns). When
        # nonzero and now_ns < this value, rebalance_periodic bypasses
        # the cadence gate and runs every loop iteration. Set by
        # request_priority_drain; cleared the first periodic tick after
        # the deadline passes.
        self._priority_drain_until_ns: int = 0
        # Phase 0 of the Thread 3 deployment runbook: skip front-month
        # futures contracts whose expiry is within this window — IBKR's
        # near-expiration physical-delivery lockout rejects retail
        # orders on contracts ~1-2 weeks from expiry (forced execute →
        # observe rollback 2026-04-22 on HGJ6, 6 days from expiry). When
        # set to 0, the lockout skip is disabled and we reuse the
        # options engine's underlying contract (legacy behavior).
        # 2026-05-01: split into two distinct config knobs to avoid
        # the cascade where bumping one forces the options engine to
        # also roll its underlying (which breaks SABR fits + chain
        # subs). hedge_lockout_days is the AGGRESSIVE roll for the
        # hedge subsystem only; near_expiry_lockout_days remains the
        # SHARED-with-market_data knob (still controls underlying
        # selection for options). When hedge_lockout_days is unset,
        # fall back to near_expiry_lockout_days for backward compat.
        shared_default = int(getattr(
            h, "near_expiry_lockout_days", 7))
        self._lockout_days: int = int(getattr(
            h, "hedge_lockout_days", shared_default))
        # Resolved hedge contract (post-lockout-skip). Populated by
        # ``resolve_hedge_contract`` at startup; falls back to
        # ``market_data._underlying_contract`` if resolution fails.
        self._resolved_hedge_contract = None
        # Live ticker for the resolved hedge contract — populated by
        # _subscribe_hedge_market_data after resolve_hedge_contract.
        # Used to price IOC limits against the actual hedge contract's
        # bid/ask, not the options-underlying mid, which can be off by
        # a calendar spread when lockout-skip routes hedge to a back-
        # month contract (observed 2026-04-27: HGJ6 mid=6.024 vs HGK6
        # ask=6.0275 → 7-tick gap, all BUY IOCs died regardless of
        # offset). None until subscribed; falls back to options F.
        self._hedge_ticker = None
        # execDetailsEvent dedupe — second half of CLAUDE.md §10
        # (completed 2026-04-27). Replaces optimistic-on-placement
        # accounting with authoritative-on-fill from IBKR. Keyed by
        # execId so the same fill event firing multiple times never
        # double-counts. Bounded to PROCESSED_EXEC_IDS_MAX via paired
        # FIFO deque; the set membership check stays O(1) and memory
        # caps at ~10K ids regardless of session length.
        self._processed_exec_ids: set[str] = set()
        self._processed_exec_ids_order: deque[str] = deque()
        # Subscribe to fill confirmations. Handler filters for FUT
        # fills on our resolved hedge contract (set later by
        # resolve_hedge_contract). Optimistic-on-placement was removed
        # from _place_or_log execute branch as part of this change;
        # observe mode still uses optimistic since no real order is sent.
        try:
            self.ib.execDetailsEvent += self._on_exec_details
        except Exception as e:
            logger.warning(
                "HEDGE [%s]: execDetailsEvent subscription failed: %s — "
                "fill confirmation will rely on periodic reconcile only",
                self._product, e)
        self._account = config.account.account_id
        # One-time contract-resolution log flag. On first _place_or_log
        # call we dump the futures contract attributes (symbol, secType,
        # exchange, expiry) so Gate 0 verification can confirm we're
        # hedging via the right instrument, not accidentally hitting an
        # options leg or a stale contract. Logged once per session.
        self._resolved_contract_logged = False

        # Cache the futures contract from market_data. The options engine
        # already qualified and subscribed to the front-month underlying;
        # reusing that contract object means we don't duplicate the
        # qualifyContractsAsync round-trip or the market-data request.
        # Accessed lazily so constructor doesn't block on market_data
        # readiness.

    # ── Public API ────────────────────────────────────────────────────
    def hedge_mtm_usd(self) -> float:
        """Current futures-hedge mark-to-market in USD. Used by the
        daily P&L halt calc (v1.4 §6.1).
        """
        if self.hedge_qty == 0:
            return 0.0
        F = self.market_data.state.underlying_price
        if F <= 0:
            return 0.0
        mult = float(self.config.product.multiplier)
        return (F - self.avg_entry_F) * self.hedge_qty * mult

    def rebalance_on_fill(self, strike: float, expiry: str, put_call: str,
                           quantity: int, fill_price: float) -> None:
        """Called by FillHandler after a live option fill.

        No-op if disabled or rebalance_on_fill is off in config. Does NOT
        wait for the periodic timer — the new option position has shifted
        book delta, so we hedge immediately.
        """
        if not self.enabled or not self.rebalance_on_fill_enabled:
            return
        self._maybe_rebalance(reason=f"fill_{put_call}{strike:g}")

    def rebalance_periodic(self) -> None:
        """Called by main loop at rebalance_cadence_sec intervals. Hedges
        delta drift from underlying price moves that didn't involve
        a fill (passive gamma bleed).

        Thread 3 Layer C priority drain: while the priority flag is up
        (set by ``request_priority_drain``), bypass the cadence gate so
        we can re-issue hedge trades on every loop iteration until the
        book is back inside tolerance AND the cooldown deadline has
        passed. The flag clears the first time a periodic tick observes
        both conditions.
        """
        if not self.enabled:
            return
        now_ns = time.monotonic_ns()
        priority_until = getattr(self, "_priority_drain_until_ns", 0)
        priority_active = priority_until > 0 and now_ns < priority_until
        if priority_active:
            # Drain unconditionally — cadence is suspended while the
            # burst-pull cooldown is still in effect. The act of calling
            # _maybe_rebalance is itself idempotent: tolerance check
            # short-circuits if the book is already inside the band.
            self._last_periodic_ns = now_ns
            self._maybe_rebalance(reason="priority_drain_periodic")
            return
        if priority_until > 0 and now_ns >= priority_until:
            # Cooldown expired — clear the flag. Run one final rebalance
            # so any residual delta gets one more shot synchronously
            # before we go back to cadence-gated mode.
            self._priority_drain_until_ns = 0
            self._last_periodic_ns = now_ns
            self._maybe_rebalance(reason="priority_drain_clear")
            return
        if (now_ns - self._last_periodic_ns) / 1e9 < self.rebalance_cadence_sec:
            return
        self._last_periodic_ns = now_ns
        # §10 reconciliation pulse (added 2026-04-27). Catches divergences
        # from non-filling IOCs that updated hedge_qty optimistically on
        # placement. Silent when local already matches IBKR; loud on
        # divergence so the operator sees the auto-correction in logs.
        # Skipped on priority_drain paths above to keep burst-mode latency
        # low; ib.positions() is sync but adds a small RPC round-trip.
        try:
            self.reconcile_from_ibkr(silent=True)
        except Exception:
            logger.exception("HEDGE reconcile_pulse failed [%s]", self._product)
        self._maybe_rebalance(reason="periodic")

    def force_flat(self, reason: str = "delta_kill") -> None:
        """v1.4 §6.2 delta kill: bring net delta to 0 regardless of
        tolerance band. Called by RiskMonitor.kill(kill_type="hedge_flat").

        Skips the tolerance check — we must be at exactly 0, not "within
        ±0.5". Then flattens any residual hedge position so the book
        is entirely flat on both legs.
        """
        if not self.enabled:
            return
        # Bypass tolerance: aim for exactly 0 regardless of where we are.
        self._rebalance(tolerance_override=0.0, reason=reason)

    def request_priority_drain(self, cooldown_until_mono_ns: int,
                               reason: str = "burst_pull") -> None:
        """Thread 3 Layer C hedge-drain signal.

        Called by Layer C (FillHandler) at the moment a burst-pull fires.
        Immediately runs ``_maybe_rebalance`` outside the cadence gate so
        any unhedged delta from the just-recorded burst of fills starts
        being neutralized before the next periodic tick. Then arms a flag
        consumed by ``rebalance_periodic`` so subsequent calls bypass the
        cadence gate until ``cooldown_until_mono_ns`` has passed AND the
        book is back inside tolerance.

        This is the HARD REQUIREMENT for Layer C deployment per the
        Thread 3 brief §3: C alone (cancelling future quotes) does not
        prevent the queued-hedge → unhedged-delta → margin-trip cascade
        that was the actual loss mechanism on 2026-04-20. The drain
        signal closes that gap by ensuring in-flight unhedged delta is
        neutralized synchronously with the burst pull, not on the next
        30-second periodic tick.
        """
        if not self.enabled:
            return
        # Stash the cooldown deadline; rebalance_periodic consults it.
        # Take the longest deadline if multiple bursts overlap.
        prev = getattr(self, "_priority_drain_until_ns", 0)
        self._priority_drain_until_ns = max(int(prev),
                                            int(cooldown_until_mono_ns))
        # Synchronous drain: run the rebalance immediately, regardless of
        # cadence. Mirror the path _maybe_rebalance takes so observe-mode
        # logging stays consistent.
        try:
            self._maybe_rebalance(reason=f"priority_drain_{reason}")
        except Exception:
            logger.exception("hedge priority drain failed")

    def flatten_on_halt(self, reason: str = "flatten") -> None:
        """v1.4 §6.1: on daily P&L halt or SPAN margin kill, close the
        futures hedge along with options. If hedge_qty is already 0,
        no-op.
        """
        if not self.enabled or not self.flatten_on_halt_enabled:
            return
        if self.hedge_qty == 0:
            return
        close_qty = abs(self.hedge_qty)
        close_side = "SELL" if self.hedge_qty > 0 else "BUY"
        # Capture the pre-flatten effective delta for the log — passing a
        # hardcoded 0 would make reconciliation see the hedge trade as
        # originating from zero-delta state, which isn't true. Net
        # options delta at the moment of halt is what drove the book
        # state we're unwinding.
        pre_options = self.portfolio.delta_for_product(self._product)
        self._place_or_log(close_side, close_qty,
                           reason=f"halt_{reason}",
                           net_delta_pre=pre_options + self.hedge_qty,
                           target_qty=0)

    async def resolve_hedge_contract(self) -> bool:
        """Pick the first tradeable futures contract for hedging — i.e.,
        skip front months currently inside IBKR's near-expiration
        physical-delivery lockout window. Awaited from main.py at
        startup; result is cached on ``self._resolved_hedge_contract``.

        Returns True iff a contract was successfully resolved AND
        differs from the options engine's front-month contract (i.e.,
        the lockout skip actually moved us to a back month). Returns
        False on resolution failure or when the front month was already
        outside the lockout window (in which case we keep using the
        options engine's contract — no behavior change).

        No-op when ``_lockout_days <= 0`` (disable knob) or when the
        underlying isn't a HG/HXE futures product. ETH options use a
        different hedge instrument; that path is unchanged.
        """
        if not self.enabled or self._lockout_days <= 0:
            return False
        # Only run for products where the hedge instrument is a future
        # the same as the options engine's underlying. If a future
        # operator wires a non-future hedge (e.g. spot), skip.
        underlying = getattr(self.market_data, "_underlying_contract", None)
        if underlying is None:
            logger.warning(
                "hedge contract resolve [%s]: no options underlying yet — "
                "deferring; legacy fallback will be used until resolved",
                self._product,
            )
            return False
        # Trigger resolution if the front-month underlying expires
        # inside the lockout window. Otherwise no need to walk the chain.
        front_expiry = getattr(
            underlying, "lastTradeDateOrContractMonth", "") or ""
        cutoff = (date.today() + timedelta(days=self._lockout_days)
                  ).strftime("%Y%m%d")
        if front_expiry and front_expiry >= cutoff:
            logger.info(
                "hedge contract resolve [%s]: front-month %s expiry=%s "
                "is outside %d-day lockout window — using as-is",
                self._product, getattr(underlying, "localSymbol", "?"),
                front_expiry, self._lockout_days,
            )
            self._resolved_hedge_contract = underlying
            self._subscribe_hedge_market_data()
            return False
        # Front-month is locked out — enumerate the chain and pick the
        # first contract past the cutoff.
        symbol = getattr(underlying, "symbol", None)
        exchange = getattr(underlying, "exchange", None)
        currency = getattr(underlying, "currency", None)
        if not symbol or not exchange:
            logger.warning(
                "hedge contract resolve [%s]: missing symbol/exchange "
                "on underlying — keeping legacy front-month",
                self._product,
            )
            return False
        try:
            details = await self.ib.reqContractDetailsAsync(Future(
                symbol=symbol, exchange=exchange, currency=currency))
        except Exception as e:
            logger.warning(
                "hedge contract resolve [%s]: reqContractDetailsAsync "
                "failed (%s); keeping legacy front-month", self._product, e,
            )
            return False
        if not details:
            logger.warning(
                "hedge contract resolve [%s]: no contracts returned for "
                "%s — keeping legacy front-month", self._product, symbol,
            )
            return False
        # Sort ascending and pick the first whose expiry passes cutoff.
        candidates = sorted(
            (d.contract for d in details
             if (getattr(d.contract, "lastTradeDateOrContractMonth", "")
                 or "") >= cutoff),
            key=lambda c: c.lastTradeDateOrContractMonth,
        )
        if not candidates:
            logger.warning(
                "hedge contract resolve [%s]: no contract past lockout "
                "cutoff %s — keeping legacy front-month",
                self._product, cutoff,
            )
            return False
        target = candidates[0]
        try:
            qualified = await self.ib.qualifyContractsAsync(target)
        except Exception as e:
            logger.warning(
                "hedge contract resolve [%s]: qualifyContractsAsync "
                "failed (%s) — keeping legacy front-month", self._product, e,
            )
            return False
        if not qualified or qualified[0].conId == 0:
            logger.warning(
                "hedge contract resolve [%s]: failed to qualify %s — "
                "keeping legacy front-month",
                self._product, getattr(target, "localSymbol", "?"),
            )
            return False
        self._resolved_hedge_contract = qualified[0]
        self._subscribe_hedge_market_data()
        logger.warning(
            "HEDGE CONTRACT RESOLVED [%s]: lockout-skip selected %s "
            "(expiry=%s conId=%d) — front-month %s (expiry=%s) is "
            "inside the %d-day near-expiry window",
            self._product,
            getattr(qualified[0], "localSymbol", "?"),
            getattr(qualified[0], "lastTradeDateOrContractMonth", "?"),
            getattr(qualified[0], "conId", 0),
            getattr(underlying, "localSymbol", "?"),
            front_expiry, self._lockout_days,
        )
        return True

    def _on_exec_details(self, trade, fill) -> None:
        """ib.execDetailsEvent handler — process FUT fills matching our
        resolved hedge contract. Closes the second half of CLAUDE.md
        §10 (2026-04-27): hedge_qty + avg_entry_F + realized_pnl_usd
        update from authoritative IBKR fill data instead of optimistic
        on-placement accounting.

        Filtered tightly: only this product's hedge contract is
        processed. execIds are deduped so multiple event firings on
        the same execution don't double-count. Observe mode is
        skipped — no real order placed, so no real fill to process.
        """
        if not self.enabled or self.mode != "execute":
            return
        if self._resolved_hedge_contract is None:
            return
        contract = getattr(fill, "contract", None)
        if contract is None or getattr(contract, "secType", None) != "FUT":
            return
        target_conid = int(getattr(self._resolved_hedge_contract, "conId", 0))
        target_local = getattr(self._resolved_hedge_contract, "localSymbol", None)
        same = ((target_conid
                 and int(getattr(contract, "conId", 0)) == target_conid)
                or getattr(contract, "localSymbol", None) == target_local)
        if not same:
            return
        execution = getattr(fill, "execution", None)
        if execution is None:
            return
        exec_id = getattr(execution, "execId", "")
        if not exec_id or exec_id in self._processed_exec_ids:
            return
        if len(self._processed_exec_ids_order) >= PROCESSED_EXEC_IDS_MAX:
            old = self._processed_exec_ids_order.popleft()
            self._processed_exec_ids.discard(old)
        self._processed_exec_ids.add(exec_id)
        self._processed_exec_ids_order.append(exec_id)
        raw_side = (getattr(execution, "side", "") or "").upper()
        side = "BUY" if raw_side in ("BOT", "BUY", "B") else "SELL"
        try:
            qty = int(getattr(execution, "shares", 0))
            price = float(getattr(execution, "price", 0))
        except (TypeError, ValueError):
            return
        if qty <= 0 or price <= 0:
            return
        pre_qty = self.hedge_qty
        self._apply_local_fill(side, qty, price)
        logger.warning(
            "HEDGE fill confirmed [%s]: %s %d @ %g — hedge_qty %+d → %+d "
            "(execId=%s, oid=%s)",
            self._product, side, qty, price, pre_qty, self.hedge_qty,
            exec_id, getattr(getattr(trade, "order", None), "orderId", "?"))

    def _subscribe_hedge_market_data(self) -> None:
        """Subscribe to live market data on the resolved hedge contract.
        Idempotent — ib_insync caches by contract, so repeated calls
        return the same Ticker. Called from resolve_hedge_contract on
        both legacy and lockout-skip paths.

        Why: IOC pricing in _place_or_log needs the hedge contract's
        actual quote, not the options-engine underlying. Calendar
        spread between front (HGJ6) and back-month (HGK6) was 7 ticks
        on 2026-04-27 — every BUY IOC died because lmt was computed
        relative to the wrong contract. Subscribing here gives
        _place_or_log a live bid/ask to anchor against.
        """
        if self._resolved_hedge_contract is None:
            return
        if self._hedge_ticker is not None:
            return
        try:
            self._hedge_ticker = self.ib.reqMktData(
                self._resolved_hedge_contract, "", False, False)
            logger.info(
                "HEDGE [%s]: subscribed to market data on %s for IOC pricing",
                self._product,
                getattr(self._resolved_hedge_contract, "localSymbol", "?"))
        except Exception as e:
            logger.warning(
                "HEDGE [%s]: reqMktData failed for hedge contract: %s",
                self._product, e)

    def _hedge_quote_mid(self) -> float | None:
        """Live mid of the resolved hedge contract, or None when the
        subscription hasn't populated yet or quotes are stale (IBKR
        returns -1.0 / NaN sentinels during data outages — see CLAUDE.md
        §4 for the ETH options break example, applies here too)."""
        t = self._hedge_ticker
        if t is None:
            return None
        try:
            bid = float(t.bid) if t.bid is not None else 0.0
            ask = float(t.ask) if t.ask is not None else 0.0
        except (TypeError, ValueError):
            return None
        # NaN check (IBKR sometimes returns NaN for thin contracts)
        if bid != bid or ask != ask:
            return None
        if bid <= 0 or ask <= 0:
            return None
        if bid >= ask:
            return None  # crossed or single-side market
        return (bid + ask) / 2.0

    def reconcile_from_ibkr(self, silent: bool = False) -> bool:
        """Reconcile hedge_qty against IBKR's actual futures position
        (CLAUDE.md §10). Called at boot AND on every periodic rebalance
        tick to catch divergences from non-filling IOCs.

        Two failure modes this addresses:
        1. **Boot reset**: on every restart, ``self.hedge_qty`` resets
           to 0 because hedge state is in-memory only. If IBKR holds a
           real futures hedge from a prior session, the first
           risk_monitor.check() would see options + 0 = options-only
           delta and trip delta_kill (observed 2026-04-27: 3 restarts →
           IBKR -10 vs local -5).
        2. **Optimistic fill accounting**: ``_apply_local_fill`` runs on
           order PLACEMENT, not on actual fill. An IOC that doesn't fill
           leaves local hedge_qty out of sync (observed 2026-04-27 BUY 4
           periodic: local +=4 but IBKR position unchanged because the
           IOC limit was below market and never executed).

        Read ib.positions(), find any futures position whose conId or
        localSymbol matches our resolved hedge contract, set hedge_qty
        + avg_entry_F to match. Idempotent. Synchronous (ib.positions()
        is synchronous in ib_insync). Returns True on success.

        ``silent``: when True, suppress the "synced" WARNING log if
        local already matches IBKR — used by the periodic-tick caller
        to avoid spamming the log every 30s when nothing has changed.
        """
        if self._resolved_hedge_contract is None:
            if not silent:
                logger.warning(
                    "HEDGE reconcile [%s]: no resolved contract yet — "
                    "should run AFTER resolve_hedge_contract", self._product)
            return False
        target_local = getattr(self._resolved_hedge_contract, "localSymbol", None)
        target_conid = int(getattr(self._resolved_hedge_contract, "conId", 0))
        if not target_local:
            if not silent:
                logger.warning(
                    "HEDGE reconcile [%s]: resolved contract has no localSymbol",
                    self._product)
            return False
        try:
            positions = self.ib.positions()
        except Exception as e:
            logger.error("HEDGE reconcile [%s]: ib.positions() failed: %s",
                         self._product, e)
            return False
        matched_qty = 0
        matched_cost = 0.0
        for pos in positions:
            c = pos.contract
            if getattr(c, "secType", None) != "FUT":
                continue
            same_conid = (target_conid
                          and int(getattr(c, "conId", 0)) == target_conid)
            same_local = getattr(c, "localSymbol", None) == target_local
            if not (same_conid or same_local):
                continue
            matched_qty += int(pos.position)
            matched_cost += float(pos.avgCost) * abs(int(pos.position))
        mult = float(getattr(self._resolved_hedge_contract, "multiplier", 0)
                     or self.config.product.multiplier)
        # No-op fast path: local already matches IBKR.
        if matched_qty == self.hedge_qty:
            return True
        prior_qty = self.hedge_qty
        if matched_qty == 0:
            self.hedge_qty = 0
            self.avg_entry_F = 0.0
        else:
            self.hedge_qty = matched_qty
            self.avg_entry_F = ((matched_cost / abs(matched_qty)) / mult
                                if mult > 0 else 0.0)
        # Always log on actual divergence — silent flag only suppresses
        # the no-change case (handled above by early return).
        logger.warning(
            "HEDGE reconcile [%s]: divergence — local=%+d → IBKR=%+d "
            "(diff=%+d, avg_F=%.4f). Likely IOC didn't fill or boot reset.",
            self._product, prior_qty, self.hedge_qty,
            self.hedge_qty - prior_qty, self.avg_entry_F)
        return True

    # ── Internal ──────────────────────────────────────────────────────
    def _maybe_rebalance(self, reason: str) -> None:
        """Check tolerance and rebalance if breached."""
        net_delta = self.portfolio.delta_for_product(self._product)
        # Net delta accounts for option positions only. Adjust for the
        # hedge we're already carrying in futures (each future = 1
        # contract-delta in underlying-equivalent terms).
        effective_delta = net_delta + self.hedge_qty

        if abs(effective_delta) <= self.tolerance:
            return  # inside band; no trade

        self._rebalance(tolerance_override=None, reason=reason,
                        effective_delta=effective_delta)

    def _rebalance(self, tolerance_override: float | None,
                   reason: str,
                   effective_delta: float | None = None) -> None:
        """Compute and issue the hedge trade. ``tolerance_override=0.0``
        means bypass the tolerance check (used by force_flat)."""
        if effective_delta is None:
            net_delta = self.portfolio.delta_for_product(self._product)
            effective_delta = net_delta + self.hedge_qty

        # Target futures position: negate the options delta so net = 0
        # (modulo rounding). hedge_qty is in the SAME sign convention as
        # option position (positive = long underlying exposure).
        target_qty = -int(round(effective_delta - self.hedge_qty))
        desired_change = target_qty - self.hedge_qty

        if tolerance_override is not None:
            # force_flat: override to land exactly at the target
            pass
        elif abs(desired_change) < 1:
            # Rounding collapsed — not worth a 1-lot trade
            return

        if desired_change == 0:
            return

        trade_side = "BUY" if desired_change > 0 else "SELL"
        self._place_or_log(
            trade_side, abs(desired_change), reason=reason,
            net_delta_pre=effective_delta, target_qty=target_qty,
        )

    def _place_or_log(self, side: str, qty: int, reason: str,
                      net_delta_pre: float, target_qty: int) -> None:
        """Place the hedge trade (execute mode) or log the intent
        (observe mode). Updates local hedge_qty / avg_entry_F on
        successful order submission.

        Does NOT wait for the fill; IBKR confirms asynchronously. The
        local hedge_qty is a best-effort optimistic update; a periodic
        reconciliation against ib.positions() (not yet implemented)
        would be the authoritative source.
        """
        F = self.market_data.state.underlying_price
        if F <= 0:
            logger.warning("hedge: skip trade — no forward price available "
                           "(reason=%s)", reason)
            return

        # Prefer the post-lockout-skip contract resolved at startup. Fall
        # back to the options engine's underlying if resolution wasn't
        # run or failed (back-compat for existing call sites + observe
        # mode where contract liveness doesn't matter).
        fut_contract = (self._resolved_hedge_contract
                        or getattr(self.market_data,
                                   "_underlying_contract", None))

        # v1.4 Gate 0 §9.1 verification: log the resolved hedge contract
        # once per session so operator can confirm we're trading the
        # correct front-month futures (vs. accidentally an options leg
        # or a stale/mis-qualified contract).
        if fut_contract is not None and not self._resolved_contract_logged:
            try:
                logger.warning(
                    "HEDGE CONTRACT RESOLVED [%s]: symbol=%s secType=%s "
                    "exchange=%s currency=%s localSymbol=%s expiry=%s conId=%d "
                    "multiplier=%s — verify this is the intended hedge leg",
                    self._product,
                    getattr(fut_contract, "symbol", "?"),
                    getattr(fut_contract, "secType", "?"),
                    getattr(fut_contract, "exchange", "?"),
                    getattr(fut_contract, "currency", "?"),
                    getattr(fut_contract, "localSymbol", "?"),
                    getattr(fut_contract, "lastTradeDateOrContractMonth", "?"),
                    getattr(fut_contract, "conId", 0),
                    getattr(fut_contract, "multiplier", "?"),
                )
            except Exception:
                logger.debug("hedge contract log failed", exc_info=True)
            self._resolved_contract_logged = True

        if self.mode == "observe" or fut_contract is None:
            # Observe-only: log intent, no order placed. Local hedge_qty
            # is updated as if the trade filled so hedge_mtm_usd returns
            # a non-zero component for the daily P&L halt check. When
            # flipping to execute mode, live fills drive this state
            # (observe path is skipped).
            order_id = "OBSERVE"
            self._apply_local_fill(side, qty, F)
            logger.info(
                "HEDGE [observe]: %s %d %s@%g (reason=%s, net_delta_pre=%.2f, "
                "target_qty=%d, hedge_qty_post=%d)",
                side, qty, self._product, F, reason,
                net_delta_pre, target_qty, self.hedge_qty,
            )
        else:
            # execute mode: place aggressive IOC limit at hedge_F ±
            # ioc_tick_offset ticks (default 2 — see __init__ note).
            # Prefer the hedge contract's own mid (HGK6 etc.) over the
            # options-engine underlying F (HGJ6) — calendar spread can
            # make the latter wrong by several ticks (observed
            # 2026-04-27: HGJ6 mid=6.024 vs HGK6 ask=6.0275, 7-tick
            # gap). Falls back to F when hedge_ticker hasn't populated.
            tick = self._hedge_tick
            offset = self._ioc_tick_offset * tick
            hedge_F = self._hedge_quote_mid()
            anchor = hedge_F if hedge_F is not None else F
            lmt = (anchor + offset) if side == "BUY" else max(anchor - offset, tick)
            order = LimitOrder(
                action=side,
                totalQuantity=qty,
                lmtPrice=lmt,
                tif="IOC",
                account=self._account,
                orderRef=f"corsair_hedge_{reason}",
            )
            try:
                trade = self.ib.placeOrder(fut_contract, order)
                order_id = str(trade.order.orderId)
                # Optimistic local update REMOVED 2026-04-27 — replaced
                # by _on_exec_details handler that updates hedge_qty +
                # avg_entry_F from authoritative IBKR fill events.
                # Closes second half of CLAUDE.md §10. If the IOC
                # doesn't fill, hedge_qty stays accurate; if it does
                # (or partial-fills), the event fires with the actual
                # qty + price and updates state correctly.
                logger.critical(
                    "HEDGE [execute]: %s %d %s@%g (reason=%s, oid=%s)",
                    side, qty, self._product, lmt, reason, order_id,
                )
            except Exception as e:
                logger.error("hedge placeOrder failed (%s): %s", reason, e)
                return

        # Paper-trading JSONL event (v1.4 §9.5).
        if self.csv_logger is not None:
            try:
                self.csv_logger.log_paper_hedge_trade(
                    side=side, qty=qty, price=F,
                    forward_at_trade=F,
                    net_delta_pre=net_delta_pre,
                    net_delta_post=(net_delta_pre
                                    + (qty if side == "BUY" else -qty)),
                    reason=reason,
                    mode=self.mode,
                    order_id=order_id,
                )
            except Exception:
                logger.debug("hedge_trades.jsonl emit failed", exc_info=True)

    def _apply_local_fill(self, side: str, qty: int, price: float) -> None:
        """Update local hedge_qty / avg_entry_F as if the hedge trade
        filled at ``price``. Best-effort optimistic accounting — real
        reconciliation against IBKR positions is a v1.5 item.

        Realized P&L crystallizes whenever a fill closes part or all of
        the existing position. Sign convention: profit on a long close
        when sell_price > avg_entry_F, profit on a short close when
        buy_price < avg_entry_F. Unified as
        ``sign(hedge_qty) * (price - avg) * |closed_qty| * mult``.
        """
        effect = qty if side == "BUY" else -qty
        new_qty = self.hedge_qty + effect
        mult = float(self.config.product.multiplier)
        if new_qty == 0:
            # Hedge flat — realize the entire prior position.
            sign = 1 if self.hedge_qty > 0 else -1
            self.realized_pnl_usd += (
                sign * (price - self.avg_entry_F) * abs(self.hedge_qty) * mult
            )
            self.hedge_qty = 0
            self.avg_entry_F = 0.0
            return
        # Same-direction: size-weighted average. No realization.
        if (self.hedge_qty >= 0 and effect >= 0) or \
           (self.hedge_qty <= 0 and effect <= 0):
            total_notional = (self.avg_entry_F * self.hedge_qty
                              + price * effect)
            self.hedge_qty = new_qty
            self.avg_entry_F = total_notional / new_qty
            return
        # Partial reverse: close |effect| at current price; remainder
        # of original position keeps its avg_entry_F.
        if abs(effect) < abs(self.hedge_qty):
            sign = 1 if self.hedge_qty > 0 else -1
            self.realized_pnl_usd += (
                sign * (price - self.avg_entry_F) * abs(effect) * mult
            )
            self.hedge_qty = new_qty
            return
        # Full reverse + flip: close the entire original position at
        # ``price``, then open the residual on the opposite side at
        # the same price.
        sign = 1 if self.hedge_qty > 0 else -1
        self.realized_pnl_usd += (
            sign * (price - self.avg_entry_F) * abs(self.hedge_qty) * mult
        )
        self.hedge_qty = new_qty
        self.avg_entry_F = price
