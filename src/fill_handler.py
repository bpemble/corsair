"""Fill handler for Corsair v2.

Processes IBKR fill events: records fills to portfolio, logs them,
and triggers immediate quote re-evaluation.
"""

import logging
from datetime import datetime, timezone

from ib_insync import ExecutionFilter

from .discord_notify import send_fill_notification
from .utils import format_hxe_symbol, iso8601ms_utc

logger = logging.getLogger(__name__)


class FillHandler:
    """Handles fill events from IBKR."""

    def __init__(self, ib, portfolio, margin_checker, quote_manager,
                 market_data, csv_logger, config, product_filter=None,
                 risk_monitor=None, hedge_manager=None):
        self.ib = ib
        self.portfolio = portfolio
        self.margin = margin_checker
        self.quotes = quote_manager
        self.market_data = market_data
        self.csv_logger = csv_logger
        self.config = config
        # Optional product filter: when set, only process fills whose
        # contract.symbol matches. Enables multi-product routing where
        # each FillHandler instance handles its own product's fills.
        self._product_filter = product_filter
        # Multi-product portfolio key — the IBKR underlying symbol
        # (e.g. "ETHUSDRR" or "HG") that the portfolio uses to route
        # add_fill into the right multiplier/registry. Distinct from
        # _product_filter (which is option_symbol — what fill events come
        # back tagged with).
        self._product_key = config.product.underlying_symbol
        # v1.4 §6.1: daily P&L halt must sample on every fill, not just
        # at the 5-minute risk check cadence. Holding a risk reference
        # lets us call check_daily_pnl_only() immediately post-fill.
        # Injected by main.py; None in legacy/test paths.
        self.risk_monitor = risk_monitor
        # v1.4 §5: delta hedge rebalance on fill. Optional — None means
        # no hedging wired (Stage 0 bootstrap before HedgeManager is up).
        self.hedge_manager = hedge_manager

        # Register fill callback
        self.ib.execDetailsEvent += self._on_exec_details

        self._seen_exec_ids = set()
        self._MAX_SEEN = 10_000

    def _on_exec_details(self, trade, fill):
        """Called when IBKR reports a fill execution."""
        # Multi-product routing: skip fills for other products.
        if self._product_filter is not None:
            sym = getattr(fill.contract, "symbol", None)
            if sym and sym != self._product_filter:
                return

        exec_id = fill.execution.execId
        if exec_id in self._seen_exec_ids:
            return
        self._seen_exec_ids.add(exec_id)

        # Bound the dedup set
        if len(self._seen_exec_ids) > self._MAX_SEEN:
            self._seen_exec_ids = set(list(self._seen_exec_ids)[-5000:])

        # Only process option fills (ignore any stray futures fills)
        contract = fill.contract
        if not hasattr(contract, 'right') or not contract.right:
            return

        strike = float(contract.strike)
        expiry = contract.lastTradeDateOrContractMonth
        put_call = contract.right
        quantity = int(fill.execution.shares)
        if fill.execution.side == "SLD":
            quantity = -quantity
        fill_price = float(fill.execution.price)

        side = "BOUGHT" if quantity > 0 else "SOLD"

        # Capture place→fill latency BEFORE recording the fill (the order id
        # mapping in QuoteManager survives until we explicitly pop it).
        # Replayed fills (from reqExecutionsAsync after a restart) pass
        # trade=None — for those, latency capture is meaningless because the
        # placeOrder happened in a prior process lifetime.
        fill_latency_ms = None
        if trade is not None:
            try:
                order_id = trade.order.orderId
                if hasattr(self.quotes, "fill_latency_ms"):
                    fill_latency_ms = self.quotes.fill_latency_ms(order_id)
            except Exception:
                pass

        # Compute realized edge two ways:
        #   theo-based: signed distance from our SABR theo (model PnL view)
        #   mid-based:  signed distance from clean-BBO mid (microstructure view)
        # Theo is the headline metric; mid is logged as a reality check.
        # Both are in dollars (multiplier applied) and signed — negatives mean
        # we paid above theo / above mid (bought) or sold below.
        #
        # IMPORTANT: both metrics depend on theo/mid AT THE MOMENT OF FILL.
        # For LIVE fills, "now" ≈ fill time, so reading the current values
        # is correct. For REPLAYED fills (trade is None — backfilled by
        # replay_missed_executions hours after the original execution), the
        # current theo/mid have moved and reading them produces meaningless
        # numbers. Skip spread capture entirely on the replay path so we
        # don't pollute the running totals with a fake number; fills_today
        # still increments so the count is right, only the per-fill edge is
        # absent. Users can backfill manually from fills.csv if needed.
        # Per-product multiplier (FillHandler is per-product; PortfolioState's
        # legacy ``_multiplier`` attribute was removed in the 2026-04-14
        # multi-product refactor — each Position now carries its own).
        # Reading the missing attribute used to raise AttributeError, which
        # the surrounding try/except swallowed silently → every fill was
        # being lost AFTER passing the product filter and dedup.
        mult = self.config.product.multiplier
        sign = 1 if quantity > 0 else -1  # buy: want fill < ref; sell: want fill > ref

        spread_captured_theo = 0.0
        spread_captured_mid = 0.0
        if trade is not None:
            # Multi-expiry fix: both get_theo and get_clean_bbo default to
            # the front month when expiry isn't passed. Without threading
            # the fill's expiry through, a back-month fill gets compared
            # against front-month theo/mid — wildly different strikes' same
            # K/R, wildly different TTE — producing nonsense edge values.
            # Observed 2026-04-09: a back-month C2600 sell at $89.50 was
            # compared against front-month C2600 theo (~$0.50 because
            # deep OTM 15 DTE), booking a fake +$4,450 spread capture and
            # polluting the running total for the session.
            try:
                theo = self.quotes.sabr.get_theo(strike, put_call, expiry=expiry)
                if theo and theo > 0:
                    edge_theo = (theo - fill_price) * sign
                    spread_captured_theo = edge_theo * mult * abs(quantity)
            except Exception:
                pass

            try:
                bid, ask = self.market_data.get_clean_bbo(
                    strike, put_call, expiry=expiry)
                if bid > 0 and ask > 0 and ask > bid:
                    mid = (bid + ask) / 2.0
                    edge_mid = (mid - fill_price) * sign
                    spread_captured_mid = edge_mid * mult * abs(quantity)
            except Exception:
                pass

        # Record the fill. For LIVE fills (trade is not None) we call
        # add_fill which both modifies the position book AND increments
        # analytics counters. For REPLAYED fills (trade is None, called from
        # replay_missed_executions) we ONLY increment analytics counters
        # because the position book was already brought up to the post-fill
        # state by seed_from_ibkr at startup. Calling add_fill on a replayed
        # fill double-counts the position effect (e.g. -1 short becomes -2)
        # and the reconciler immediately kills on the resulting mismatch.
        if trade is not None:
            self.portfolio.add_fill(
                product=self._product_key,
                strike=strike, expiry=expiry, put_call=put_call,
                quantity=quantity, fill_price=fill_price,
                spread_captured=spread_captured_theo,
                spread_captured_mid=spread_captured_mid,
            )
        else:
            self.portfolio._record_fill(
                quantity, spread_captured_theo, spread_captured_mid,
            )
        # Invalidate the synthetic SPAN portfolio cache — the position book
        # just changed, so the cached aggregate is stale.
        if hasattr(self.margin, "invalidate_portfolio"):
            self.margin.invalidate_portfolio()

        # Refresh Greeks immediately so the just-added position contributes
        # real delta/theta/vega to the fill log line and CSV row. Without this
        # the new Position carries zeros until the next 5-minute refresh tick,
        # which makes per-fill decomposition (spread vs theta vs MtM) useless.
        try:
            self.portfolio.refresh_greeks()
        except Exception:
            logger.exception("refresh_greeks after fill failed")

        # Log — adaptive precision for sub-dollar (HG) vs dollar (ETH) strikes/prices
        _strike_str = (f"{strike:g}" if strike != int(strike) else str(int(strike)))
        _px_str = (f"{fill_price:.4f}" if abs(fill_price) < 1 else f"{fill_price:.2f}")
        logger.info(
            "FILL: %s %d %s%s@%s | margin=$%.0f delta=%.2f theta=$%.0f | fills_today=%d",
            side, abs(quantity), put_call, _strike_str, _px_str,
            self.margin.get_current_margin(),
            self.portfolio.net_delta,
            self.portfolio.net_theta,
            self.portfolio.fills_today,
        )

        # Discord notification (live fills only)
        if trade is not None:
            opt = self.market_data.state.get_option(strike, expiry=expiry, right=put_call)
            opt_delta = opt.delta if opt else 0.0
            opt_theta = opt.theta * self.config.product.multiplier if opt else 0.0
            mkt_bid, mkt_ask = 0.0, 0.0
            try:
                mkt_bid, mkt_ask = self.market_data.get_clean_bbo(
                    strike, put_call, expiry=expiry)
            except Exception:
                pass
            theo_val = 0.0
            try:
                theo_val = self.quotes.sabr.get_theo(strike, put_call, expiry=expiry)
            except Exception:
                pass
            send_fill_notification(
                side=side, quantity=abs(quantity), strike=strike,
                expiry=expiry, put_call=put_call, fill_price=fill_price,
                theo=theo_val, spread_captured=spread_captured_theo,
                underlying=self.market_data.state.underlying_price,
                delta=opt_delta, theta=opt_theta,
                market_bid=mkt_bid, market_ask=mkt_ask,
                margin_after=self.margin.get_current_margin(),
                net_delta=self.portfolio.net_delta,
                net_theta=self.portfolio.net_theta,
                fills_today=self.portfolio.fills_today,
            )

        # Underlying price at fill time (for post-trade analysis)
        underlying_at_fill = self.market_data.state.underlying_price

        # CSV log
        self.csv_logger.log_fill(
            strike=strike, expiry=expiry, put_call=put_call,
            side=side, quantity=abs(quantity), fill_price=fill_price,
            spread_captured_theo=spread_captured_theo,
            spread_captured_mid=spread_captured_mid,
            margin_after=self.margin.get_current_margin(),
            delta_after=self.portfolio.net_delta,
            theta_after=self.portfolio.net_theta,
            vega_after=self.portfolio.net_vega,
            fills_today=self.portfolio.fills_today,
            cumulative_spread_theo=self.portfolio.spread_capture_today,
            cumulative_spread_mid=self.portfolio.spread_capture_mid_today,
            fill_latency_ms=fill_latency_ms,
            underlying_price=underlying_at_fill,
        )

        # Hardburst detector shadow-logging (spec Change 3.1). Query the
        # detector at fill time regardless of whether hardburst_skip_enabled
        # is ON. This gives a counterfactual column: when the flag is OFF,
        # detector_was_hardburst==True marks fills the overlay WOULD have
        # suppressed if enabled. Used by the validation protocol to
        # compute suppressed_fill signed_bias before flipping the flag.
        detector_was_hardburst = None
        try:
            import time as _time
            bd = getattr(self.market_data, "burst_detector", None)
            if bd is not None:
                detector_was_hardburst = bool(bd.is_hardburst(_time.time_ns()))
        except Exception:
            # Best-effort — never break the fill path over telemetry.
            logger.debug("burst detector query at fill failed", exc_info=True)

        # Thread 3 Layer C: feed the fill into the burst tracker and (when
        # the master flag is ON) potentially trip C1 / C2 burst-pull. The
        # call returns the any-side burst_1s count INCLUDING this fill,
        # which we surface as fills.burst_1s_at_fill regardless of
        # whether any C-action fired — that field is part of the §6
        # baseline instrumentation and must populate with all flags OFF.
        # Live fills only (replayed fills are historical; including them
        # would corrupt the rolling window).
        burst_1s_at_fill = None
        if trade is not None:
            try:
                import time as _time
                fill_side = "BUY" if quantity > 0 else "SELL"
                burst_1s_at_fill = self.quotes.note_layer_c_fill(
                    strike=strike, expiry=expiry, right=put_call,
                    side=fill_side, ts_ns=_time.monotonic_ns(),
                )
            except Exception:
                logger.exception("Layer C note_layer_c_fill failed")

        # Paper-trading JSONL event — corsair→crowsnest interface, spec
        # hg_spec_v1.3.md §17.1/17.2. Emits per fill; missing recommended
        # fields (theo_at_quote, quoter_count, corsair_bid/ask at quote time,
        # sabr_rmse_at_fill) require upstream plumbing not yet in place.
        try:
            self.csv_logger.log_paper_event({
                "ts": iso8601ms_utc(),
                "event_type": "fill",
                "symbol": format_hxe_symbol(expiry, put_call, strike),
                "side": "BUY" if quantity > 0 else "SELL",
                "size": abs(quantity),
                "price": fill_price,
                "market_bid": mkt_bid,
                "market_ask": mkt_ask,
                "theo_at_fill": theo_val,
                "margin_at_fill": self.margin.get_current_margin(),
                "delta_at_fill": self.portfolio.net_delta,
                "theta_at_fill": self.portfolio.net_theta,
                "vega_at_fill": self.portfolio.net_vega,
                "forward_at_fill": underlying_at_fill,
                # Shadow-logging: True iff hardburst detector was firing
                # at the moment of this fill. None when detector is
                # disabled (e.g. pre-v1.5 deployments).
                "detector_was_hardburst": detector_was_hardburst,
                # Thread 3 §6 instrumentation: count of fills (any side,
                # any strike) within the trailing layer_c_window_sec at
                # the moment of this fill, INCLUDING this fill itself.
                # Populated even with all flags OFF — required for §7
                # baseline measurement.
                "burst_1s_at_fill": burst_1s_at_fill,
            })
        except Exception as e:
            logger.debug("paper fill event emit failed: %s", e)

        # v1.4 §6.1 PRIMARY defense: intraday P&L halt must sample on
        # every fill. Live fills only (replayed fills re-run seed+replay
        # math that the halt already saw at the original fill time). If
        # the halt fires here, risk.kill() flattens via the wired
        # flatten_callback — return BEFORE re-quoting so we don't race
        # the flatten path.
        if trade is not None and self.risk_monitor is not None:
            try:
                if self.risk_monitor.check_daily_pnl_only():
                    return  # halt fired; flatten in progress
            except Exception:
                logger.exception("check_daily_pnl_only failed after fill")

        # When ALREADY killed (halt fired on a prior fill, and THIS fill
        # is one of the flatten IOC closes), skip hedge rebalance and
        # quote update. Rebalancing on close fills during a halt would
        # either emit misleading observe-mode log events or — in execute
        # mode — send a futures order that contradicts the flatten-on-
        # halt intent. Quote update is already a no-op post-kill because
        # cancel_all_quotes emptied active_orders, but we skip explicitly
        # for clarity.
        if (trade is not None and self.risk_monitor is not None
                and self.risk_monitor.killed):
            return

        # v1.4 §5: delta hedge rebalance on every live fill.
        if trade is not None and self.hedge_manager is not None:
            try:
                self.hedge_manager.rebalance_on_fill(
                    strike=strike, expiry=expiry, put_call=put_call,
                    quantity=quantity, fill_price=fill_price,
                )
            except Exception:
                logger.exception("hedge rebalance_on_fill failed")

        # Immediately re-evaluate all quotes (fill changes portfolio state).
        # For replayed fills (trade=None), the quote loop is not yet running
        # — skip this to avoid running update_quotes before the main loop has
        # initialized the per-cycle canonical trade index.
        if trade is not None:
            # Clear margin rejection suppression — the fill changed margin
            # conditions so previously-rejected keys may now be acceptable.
            self.quotes._margin_rejected.clear()
            self.quotes.update_quotes(self.portfolio)

    async def replay_missed_executions(self, session_start_utc: datetime) -> int:
        """Backfill any executions that happened while we were disconnected.

        ib_insync's `execDetailsEvent` only fires for executions that occur
        WHILE WE'RE CONNECTED — IBKR does not replay missed events on
        reconnect. Without this method, every fill that lands during a
        bootstrap window, restart, or watchdog reconnect is silently invisible
        to fill_handler: the position appears (because seed_from_ibkr pulls
        the post-fill book) but `fills_today`, `spread_capture_today`, and
        the realized-P&L attribution are all dark for that fill.

        Called once after FillHandler construction (in main.py) and after
        each successful watchdog reseed (in watchdog.py). Dedup by execId
        against `_seen_exec_ids`, which is itself persisted across restarts
        via daily_state.json — so calling this multiple times within a
        session is safe and idempotent.

        Returns the number of new fills replayed (for logging).
        """
        try:
            fills = await self.ib.reqExecutionsAsync(ExecutionFilter())
        except Exception as e:
            logger.warning("replay_missed_executions: reqExecutions failed: %s", e)
            return 0

        replayed = 0
        for fill in fills:
            try:
                # Filter to current CME session only. ib_insync surfaces the
                # execution time as a tz-aware datetime in UTC.
                exec_time = getattr(fill.execution, "time", None)
                if exec_time is None:
                    continue
                if exec_time.tzinfo is None:
                    exec_time = exec_time.replace(tzinfo=timezone.utc)
                if exec_time < session_start_utc:
                    continue
                # Skip if we already saw this execId in a prior process or
                # via the live event path in the current process.
                if fill.execution.execId in self._seen_exec_ids:
                    continue
                self._on_exec_details(None, fill)
                replayed += 1
            except Exception as e:
                logger.warning("replay_missed_executions: skip one fill: %s", e)
        if replayed:
            logger.warning(
                "replay_missed_executions: backfilled %d missed fill(s) "
                "from current session — these would otherwise be invisible "
                "to fills_today / spread_capture / daily_pnl",
                replayed,
            )
        else:
            logger.info(
                "replay_missed_executions: 0 missed fills (checked %d "
                "executions in current session window)",
                sum(1 for f in fills
                    if getattr(f.execution, "time", None)
                    and (f.execution.time.replace(tzinfo=timezone.utc)
                         if f.execution.time.tzinfo is None
                         else f.execution.time) >= session_start_utc),
            )
        return replayed
