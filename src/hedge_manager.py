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

from ib_insync import LimitOrder

logger = logging.getLogger(__name__)


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

        # Local hedge state. avg_entry_F is the qty-weighted average
        # futures price at which we're holding this hedge position; used
        # for MTM. Both reset when hedge_qty returns to 0.
        self.hedge_qty: int = 0
        self.avg_entry_F: float = 0.0
        self._last_periodic_ns: int = 0
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
        """
        if not self.enabled:
            return
        now_ns = time.monotonic_ns()
        if (now_ns - self._last_periodic_ns) / 1e9 < self.rebalance_cadence_sec:
            return
        self._last_periodic_ns = now_ns
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

        fut_contract = getattr(self.market_data, "_underlying_contract", None)

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
            # execute mode: place aggressive IOC limit at market ± 1 tick.
            tick = self._hedge_tick
            lmt = (F + tick) if side == "BUY" else max(F - tick, tick)
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
                # Optimistic local update. Actual fill reconciliation
                # would arrive via execDetailsEvent — not wired in this
                # scaffold since the fill_handler is option-only.
                self._apply_local_fill(side, qty, F)
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
        """
        effect = qty if side == "BUY" else -qty
        new_qty = self.hedge_qty + effect
        if new_qty == 0:
            # Hedge flat — reset avg entry to 0 so the next open starts
            # fresh.
            self.hedge_qty = 0
            self.avg_entry_F = 0.0
            return
        # Same-direction: size-weighted average.
        if (self.hedge_qty >= 0 and effect >= 0) or \
           (self.hedge_qty <= 0 and effect <= 0):
            total_notional = (self.avg_entry_F * self.hedge_qty
                              + price * effect)
            self.hedge_qty = new_qty
            self.avg_entry_F = total_notional / new_qty
            return
        # Partial reverse: reduce existing position at unchanged avg,
        # any residual flips direction and resets avg.
        if abs(effect) < abs(self.hedge_qty):
            self.hedge_qty = new_qty
            # avg_entry_F unchanged — we're closing part of the position
            return
        # Full reverse + flip: new position at current price
        self.hedge_qty = new_qty
        self.avg_entry_F = price
