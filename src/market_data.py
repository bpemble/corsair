"""Market data subscription and state management for Corsair v2.

Subscribes to IBKR market data for:
- ETH futures (underlying) via reqMktData
- Option chain discovery via reqSecDefOptParams
- Individual option quotes via reqMktData

Maintains a live MarketState object with all current quotes.
"""

import asyncio
import logging
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import Dict, List, Optional, Tuple

import numpy as np

from ib_insync import IB, Future, FuturesOption, Ticker

from .pricing import PricingEngine
from .utils import days_to_expiry

logger = logging.getLogger(__name__)

OptionKey = Tuple[float, str, str]  # (strike, expiry, right)


@dataclass
class OptionQuote:
    """Live quote data for a single option."""
    strike: float
    expiry: str             # YYYYMMDD
    put_call: str           # "C" or "P"
    bid: float = 0.0
    ask: float = 0.0
    bid_size: int = 0
    ask_size: int = 0
    last: float = 0.0
    volume: int = 0
    iv: float = 0.0         # Implied vol (from IBKR or computed)
    open_interest: int = 0
    delta: float = 0.0
    gamma: float = 0.0
    theta: float = 0.0      # Raw per-unit theta (not yet multiplied)
    vega: float = 0.0
    last_update: datetime = field(default_factory=datetime.now)
    contract: Optional[FuturesOption] = field(default=None, repr=False)
    # Depth book: list of (price, size) tuples, level 0 = best
    dom_bids: List[Tuple[float, int]] = field(default_factory=list)
    dom_asks: List[Tuple[float, int]] = field(default_factory=list)
    dom_last_update: datetime = field(default_factory=datetime.now)


@dataclass
class MarketState:
    """Aggregated live market state."""
    underlying_price: float = 0.0
    underlying_bid: float = 0.0
    underlying_ask: float = 0.0
    underlying_last_update: datetime = field(default_factory=datetime.now)

    options: Dict[OptionKey, OptionQuote] = field(default_factory=dict)

    atm_strike: float = 0.0
    front_month_expiry: str = ""
    strike_increment: float = 25.0  # Will be set from discovered chain

    def get_option(self, strike: float, expiry: str = None, right: str = "C") -> Optional[OptionQuote]:
        """Get option quote by strike. Uses front_month_expiry if expiry not specified."""
        if expiry is None:
            expiry = self.front_month_expiry
        return self.options.get((strike, expiry, right))

    def get_all_strikes(self) -> List[float]:
        """Return sorted list of all available strikes."""
        strikes = set()
        for (strike, _, _) in self.options:
            strikes.add(strike)
        return sorted(strikes)


class MarketDataManager:
    """Manages IBKR market data subscriptions and maintains MarketState."""

    STALE_THRESHOLD = timedelta(seconds=60)

    def __init__(self, ib: IB, config):
        self.ib = ib
        self.config = config
        self.state = MarketState()
        self._option_tickers: Dict[OptionKey, Ticker] = {}
        self._underlying_ticker: Optional[Ticker] = None
        self._underlying_contract: Optional[Future] = None
        self._option_contracts: Dict[OptionKey, FuturesOption] = {}
        # Rotating depth subscriptions (IBKR caps at ~5 concurrent).
        self._depth_tickers: Dict[OptionKey, Ticker] = {}
        self._depth_rotation_idx: int = 0
        # Last-known clean (non-self) BBO per strike. Used by find_incumbent
        # as a fallback when the live top-of-book is just our own resting
        # order — prevents self-quote feedback loops.
        self._last_clean_bid: Dict[float, float] = {}
        self._last_clean_ask: Dict[float, float] = {}
        # Event-driven tick queue. Created lazily in discover_and_subscribe()
        # so it binds to the running asyncio loop. Items are tuples of
        #   ("option", OptionKey)  or  ("underlying", None)
        # The quoter loop awaits this queue and reprices only the strikes
        # whose ticks have been pushed since the last cycle.
        self.tick_queue: Optional[asyncio.Queue] = None

    async def discover_and_subscribe(self):
        """Discover option chain and subscribe to all relevant market data."""
        # Create the tick queue on the running event loop
        if self.tick_queue is None:
            self.tick_queue = asyncio.Queue(maxsize=5000)

        # Step 1: Find and subscribe to the underlying futures contract
        await self._subscribe_underlying()

        # Step 2: Wait for underlying price (so ATM is set before we narrow strikes)
        for _ in range(20):
            if self.state.underlying_price > 0:
                break
            await asyncio.sleep(0.5)
        if self.state.underlying_price <= 0:
            logger.warning("No underlying price after 10s — subscribing to all strikes")

        # Step 3: Discover the option chain (now ATM is known)
        await self._discover_option_chain()

        # Step 4: Subscribe to option quotes
        await self._subscribe_options()

        logger.info(
            "Market data ready: underlying=%s, %d options subscribed, front_month=%s",
            self.state.underlying_price, len(self._option_tickers),
            self.state.front_month_expiry,
        )

    async def _subscribe_underlying(self):
        """Subscribe to the underlying ETH futures contract (front month)."""
        p = self.config.product

        # Request all contract details to find front month
        contract = Future(
            symbol=p.underlying_symbol,
            exchange=p.exchange,
            currency=p.currency,
        )
        details_list = await self.ib.reqContractDetailsAsync(contract)
        if not details_list:
            logger.error("No contract details for %s", p.underlying_symbol)
            return

        # Sort by expiry, pick the nearest one that hasn't expired
        now_str = datetime.now().strftime("%Y%m%d")
        valid = [d for d in details_list
                 if d.contract.lastTradeDateOrContractMonth >= now_str]
        if not valid:
            logger.error("No valid future expiries found for %s", p.underlying_symbol)
            return

        valid.sort(key=lambda d: d.contract.lastTradeDateOrContractMonth)
        front = valid[0].contract

        # Qualify the specific contract
        qualified = await self.ib.qualifyContractsAsync(front)
        if not qualified or qualified[0].conId == 0:
            logger.error("Failed to qualify front-month contract %s", front.localSymbol)
            return

        self._underlying_contract = qualified[0]
        logger.info(
            "Qualified underlying: %s (conId=%d) expiry=%s localSymbol=%s",
            qualified[0].symbol, qualified[0].conId,
            qualified[0].lastTradeDateOrContractMonth, qualified[0].localSymbol,
        )

        # Subscribe to market data
        ticker = self.ib.reqMktData(self._underlying_contract, genericTickList="", snapshot=False)
        self._underlying_ticker = ticker

        # Set up callback for underlying price updates
        ticker.updateEvent += self._on_underlying_tick

    def _on_underlying_tick(self, ticker: Ticker):
        """Process underlying futures price update."""
        if ticker.bid and ticker.bid > 0:
            self.state.underlying_bid = ticker.bid
        if ticker.ask and ticker.ask > 0:
            self.state.underlying_ask = ticker.ask

        # Prefer mid (more responsive than last in fast markets, never stale).
        # Fall back to last/close if bid/ask are unavailable.
        if self.state.underlying_bid > 0 and self.state.underlying_ask > 0:
            self.state.underlying_price = (
                self.state.underlying_bid + self.state.underlying_ask
            ) / 2.0
        elif ticker.last and ticker.last > 0:
            self.state.underlying_price = ticker.last
        elif ticker.close and ticker.close > 0:
            self.state.underlying_price = ticker.close

        self.state.underlying_last_update = datetime.now()

        # Update ATM strike if underlying has moved significantly
        self._update_atm_strike()

        # Notify the quoter — underlying move means every option needs reprice
        self._push_tick(("underlying", None))

    def _update_atm_strike(self):
        """Recalculate ATM strike when underlying moves > half a strike width."""
        if self.state.underlying_price <= 0:
            return

        inc = self.state.strike_increment
        if inc <= 0:
            return

        new_atm = round(self.state.underlying_price / inc) * inc

        if self.state.atm_strike == 0 or abs(new_atm - self.state.atm_strike) >= inc / 2:
            old_atm = self.state.atm_strike
            self.state.atm_strike = new_atm
            if old_atm != new_atm:
                logger.info("ATM strike updated: %.0f -> %.0f (underlying=%.2f)",
                           old_atm, new_atm, self.state.underlying_price)

    async def _discover_option_chain(self):
        """Discover available strikes and expirations for the option chain."""
        p = self.config.product

        if self._underlying_contract is None:
            logger.error("Cannot discover chain: no underlying contract")
            return

        params_list = await self.ib.reqSecDefOptParamsAsync(
            underlyingSymbol=self._underlying_contract.symbol,
            futFopExchange=p.exchange,
            underlyingSecType="FUT",
            underlyingConId=self._underlying_contract.conId,
        )

        if not params_list:
            logger.error("No option chain parameters returned for %s", p.symbol)
            return

        # Find the right param set (matching exchange)
        params = None
        for param in params_list:
            if param.exchange == p.exchange:
                params = param
                break

        if params is None:
            params = params_list[0]
            logger.warning("No exact exchange match, using %s", params.exchange)

        # Determine front month expiry
        sorted_expiries = sorted(params.expirations)
        now_str = datetime.now().strftime("%Y%m%d")
        front_month = None
        for exp in sorted_expiries:
            dte = days_to_expiry(exp)
            if dte > self.config.product.min_dte:
                front_month = exp
                break

        if front_month is None:
            logger.error("No valid expiry found with DTE > %d", self.config.product.min_dte)
            return

        self.state.front_month_expiry = front_month

        # Determine strike increment from available strikes
        sorted_strikes = sorted(params.strikes)
        if len(sorted_strikes) >= 2:
            # Find most common increment
            increments = [sorted_strikes[i+1] - sorted_strikes[i]
                         for i in range(min(20, len(sorted_strikes) - 1))]
            if increments:
                self.state.strike_increment = min(increments)

        # Filter strikes to our range (if we have an underlying price)
        self._update_atm_strike()

        inc = self.state.strike_increment
        if self.state.atm_strike > 0:
            low_bound = self.state.atm_strike + (self.config.product.strike_range_low * inc)
            high_bound = self.state.atm_strike + (self.config.product.strike_range_high * inc)
            relevant_strikes = [s for s in sorted_strikes if low_bound <= s <= high_bound]
        else:
            # Markets closed — subscribe to all strikes, will filter at quote time
            relevant_strikes = sorted_strikes
            low_bound = relevant_strikes[0] if relevant_strikes else 0
            high_bound = relevant_strikes[-1] if relevant_strikes else 0

        logger.info(
            "Option chain: expiry=%s, %d strikes in range [%.0f, %.0f], increment=%.0f, ATM=%.0f",
            front_month, len(relevant_strikes), low_bound, high_bound, inc,
            self.state.atm_strike,
        )

        # Build option contracts
        option_types = []
        opt_type = self.config.product.option_type
        if opt_type in ("calls_only", "both"):
            option_types.append("C")
        if opt_type in ("puts_only", "both"):
            option_types.append("P")

        for strike in relevant_strikes:
            for right in option_types:
                contract = FuturesOption(
                    symbol=p.option_symbol,
                    lastTradeDateOrContractMonth=front_month,
                    strike=strike,
                    right=right,
                    exchange=p.exchange,
                    currency=p.currency,
                    tradingClass=p.trading_class,
                )
                key = (strike, front_month, right)
                self._option_contracts[key] = contract

        # Qualify all option contracts
        contracts_to_qualify = list(self._option_contracts.values())
        if contracts_to_qualify:
            qualified = await self.ib.qualifyContractsAsync(*contracts_to_qualify)
            qualified_count = sum(1 for c in qualified if c.conId > 0)
            logger.info("Qualified %d/%d option contracts", qualified_count, len(contracts_to_qualify))

    async def _subscribe_options(self):
        """Subscribe to market data for all discovered option contracts."""
        for key, contract in self._option_contracts.items():
            if contract.conId == 0:
                continue  # Skip unqualified contracts

            strike, expiry, right = key

            # Initialize the quote
            quote = OptionQuote(
                strike=strike, expiry=expiry, put_call=right,
                contract=contract,
            )
            self.state.options[key] = quote

            # Subscribe
            ticker = self.ib.reqMktData(contract, genericTickList="100,101,106", snapshot=False)
            self._option_tickers[key] = ticker

            # Set up callback
            ticker.updateEvent += lambda t, k=key: self._on_option_tick(t, k)

        logger.info("Subscribed to %d option contracts", len(self._option_tickers))

    def rotate_depth_subscriptions(self):
        """Cycle the depth-book window forward by one batch.

        IBKR caps concurrent market-depth requests (~5). Rotate through all
        quotable option contracts so each gets fresh L2 data periodically.
        Strikes with no current depth subscription fall back to top-of-book
        in find_incumbent.
        """
        max_active = int(getattr(self.config.quoting, "max_active_depth_subs", 5))
        num_rows = int(getattr(self.config.quoting, "depth_levels", 5))

        # Build the full ordered list of subscribable option keys (qualified only)
        keys = [k for k, c in self._option_contracts.items() if c.conId > 0]
        if not keys:
            return

        # Pick the next batch starting at the rotation cursor
        n = len(keys)
        batch = [keys[(self._depth_rotation_idx + i) % n] for i in range(min(max_active, n))]
        self._depth_rotation_idx = (self._depth_rotation_idx + max_active) % n

        # Cancel existing subs not in the new batch; clear their stale dom books
        batch_set = set(batch)
        for old_key in list(self._depth_tickers.keys()):
            if old_key in batch_set:
                continue
            old_ticker = self._depth_tickers.pop(old_key)
            try:
                self.ib.cancelMktDepth(old_ticker.contract, isSmartDepth=False)
            except Exception:
                pass
            opt = self.state.options.get(old_key)
            if opt is not None:
                opt.dom_bids = []
                opt.dom_asks = []

        # Subscribe to anything new in the batch
        for key in batch:
            if key in self._depth_tickers:
                continue
            contract = self._option_contracts[key]
            try:
                t = self.ib.reqMktDepth(contract, numRows=num_rows, isSmartDepth=False)
                t.updateEvent += lambda tk, k=key: self._on_depth_tick(tk, k)
                self._depth_tickers[key] = t
            except Exception as e:
                logger.warning("reqMktDepth rotate failed for %s: %s", key, e)

    def _on_option_tick(self, ticker: Ticker, key: OptionKey):
        """Process an option quote update."""
        quote = self.state.options.get(key)
        if quote is None:
            return

        prev_bid, prev_ask = quote.bid, quote.ask
        if ticker.bid is not None and ticker.bid > 0:
            quote.bid = ticker.bid
        if ticker.ask is not None and ticker.ask > 0:
            quote.ask = ticker.ask
        bid_ask_changed = (quote.bid != prev_bid) or (quote.ask != prev_ask)
        if ticker.bidSize is not None and not np.isnan(ticker.bidSize):
            quote.bid_size = int(ticker.bidSize)
        if ticker.askSize is not None and not np.isnan(ticker.askSize):
            quote.ask_size = int(ticker.askSize)
        if ticker.last is not None and ticker.last > 0:
            quote.last = ticker.last
        if ticker.volume is not None and not np.isnan(ticker.volume):
            quote.volume = int(ticker.volume)
        # Open interest comes via call/putOpenInterest depending on right
        oi = None
        if quote.put_call == "C" and getattr(ticker, "callOpenInterest", None):
            if not np.isnan(ticker.callOpenInterest):
                oi = int(ticker.callOpenInterest)
        elif quote.put_call == "P" and getattr(ticker, "putOpenInterest", None):
            if not np.isnan(ticker.putOpenInterest):
                oi = int(ticker.putOpenInterest)
        if oi is not None:
            quote.open_interest = oi

        # Model Greeks from IBKR — use delta only (their IV uses a different
        # reference forward and is systematically off; we compute IV ourselves)
        if ticker.modelGreeks is not None:
            mg = ticker.modelGreeks
            if mg.delta is not None:
                quote.delta = mg.delta
            if mg.gamma is not None:
                quote.gamma = mg.gamma
            if mg.theta is not None:
                quote.theta = mg.theta
            if mg.vega is not None:
                quote.vega = mg.vega

        # Compute IV from market mid using our forward. Skip when bid/ask
        # haven't changed — brentq inversion is the most expensive part of
        # the tick path (~200-500us per call) and pure waste on volume/last-only
        # ticks. IBKR's modelGreeks.impliedVol uses a different reference and
        # produces ~2% inflated values, so we always compute our own.
        if (bid_ask_changed and quote.bid > 0 and quote.ask > 0
                and self.state.underlying_price > 0):
            mid = (quote.bid + quote.ask) / 2
            tte = days_to_expiry(quote.expiry) / 365.0
            if tte > 0:
                iv = PricingEngine.implied_vol(
                    mid, self.state.underlying_price, quote.strike, tte,
                    right=quote.put_call,
                )
                if iv is not None:
                    quote.iv = iv

        quote.last_update = datetime.now()
        # Notify the quoter
        self._push_tick(("option", key))

    def _push_tick(self, item):
        """Best-effort enqueue. Drops the oldest tick if the queue is full."""
        q = self.tick_queue
        if q is None:
            return
        try:
            q.put_nowait(item)
        except asyncio.QueueFull:
            try:
                q.get_nowait()
            except asyncio.QueueEmpty:
                pass
            try:
                q.put_nowait(item)
            except asyncio.QueueFull:
                pass

    def _on_depth_tick(self, ticker: Ticker, key: OptionKey):
        """Process a market depth update — copy domBids/domAsks into the quote."""
        quote = self.state.options.get(key)
        if quote is None:
            return
        try:
            quote.dom_bids = [(float(d.price), int(d.size))
                              for d in (ticker.domBids or [])
                              if d.price and d.price > 0]
            quote.dom_asks = [(float(d.price), int(d.size))
                              for d in (ticker.domAsks or [])
                              if d.price and d.price > 0]
            quote.dom_last_update = datetime.now()
        except Exception:
            pass

    def find_incumbent(self, strike: float, side: str,
                       our_prices: Optional[set] = None):
        """Scan the depth book for the first 'meaningful' incumbent level.

        Returns dict with keys:
            price, level (1-based), size, age_ms, bbo_width, skip_reason

        skip_reason is non-empty when no quote should be placed this cycle.
        """
        opt = self.state.get_option(strike)
        if opt is None:
            return {"price": None, "level": None, "size": None,
                    "age_ms": None, "bbo_width": None, "skip_reason": "no_option"}

        q = self.config.quoting
        min_size = int(getattr(q, "min_incumbent_size", 2))
        max_width = float(getattr(q, "max_bbo_width_dollars", 5.00))
        max_stale = float(getattr(q, "max_quote_staleness_sec", 5.0))
        filter_self = bool(getattr(q, "filter_self_orders", True))
        tick = float(q.tick_size)

        # Use depth book if fresh; otherwise fall back to top-of-book.
        # (Depth subs rotate across many strikes, so most strikes have no
        # current L2 — top-of-book from the always-on price feed is still
        # accurate, just without the phantom-level safety scan.)
        depth_age_ms = int((datetime.now() - opt.dom_last_update).total_seconds() * 1000)
        depth_fresh = bool(opt.dom_bids) and depth_age_ms <= max_stale * 1000
        if depth_fresh:
            bids = opt.dom_bids
            asks = opt.dom_asks
            age_ms = depth_age_ms
        else:
            # Top-of-book fallback. If our own resting order IS the BBO,
            # substitute the last-known clean (non-self) price so we don't
            # penny-jump ourselves into oblivion.
            tob_bid = opt.bid
            tob_ask = opt.ask
            if our_prices:
                if tob_bid > 0 and any(abs(tob_bid - op) < tick * 0.5 for op in our_prices):
                    tob_bid = self._last_clean_bid.get(strike, 0.0)
                if tob_ask > 0 and any(abs(tob_ask - op) < tick * 0.5 for op in our_prices):
                    tob_ask = self._last_clean_ask.get(strike, 0.0)
            bids = [(tob_bid, opt.bid_size)] if tob_bid > 0 else []
            asks = [(tob_ask, opt.ask_size)] if tob_ask > 0 else []
            tick_age = (datetime.now() - opt.last_update).total_seconds() * 1000
            age_ms = int(tick_age)
            if tick_age > max_stale * 1000:
                return {"price": None, "level": None, "size": None,
                        "age_ms": age_ms, "bbo_width": None, "skip_reason": "stale"}

        # Cache the most recent clean (non-self) top-of-book prices so we
        # can fall back to them next cycle if our order becomes the BBO.
        if opt.bid > 0 and not (our_prices and any(abs(opt.bid - op) < tick * 0.5 for op in our_prices)):
            self._last_clean_bid[strike] = opt.bid
        if opt.ask > 0 and not (our_prices and any(abs(opt.ask - op) < tick * 0.5 for op in our_prices)):
            self._last_clean_ask[strike] = opt.ask

        best_bid = bids[0][0] if bids else 0.0
        best_ask = asks[0][0] if asks else 0.0
        bbo_width = (best_ask - best_bid) if (best_bid > 0 and best_ask > 0) else None

        # Crossed/locked
        if best_bid > 0 and best_ask > 0 and best_bid >= best_ask:
            return {"price": None, "level": None, "size": None,
                    "age_ms": age_ms, "bbo_width": bbo_width, "skip_reason": "crossed"}

        # Wide market
        if bbo_width is not None and bbo_width > max_width + 1e-9:
            return {"price": None, "level": None, "size": None,
                    "age_ms": age_ms, "bbo_width": bbo_width, "skip_reason": "wide_market"}

        levels = bids if side == "BUY" else asks
        if not levels:
            return {"price": None, "level": None, "size": None,
                    "age_ms": age_ms, "bbo_width": bbo_width, "skip_reason": "empty_side"}

        # Scan for first meaningful level
        for idx, (px, sz) in enumerate(levels):
            if filter_self and our_prices and any(abs(px - op) < tick * 0.5 for op in our_prices):
                continue
            if sz >= min_size:
                return {"price": px, "level": idx + 1, "size": sz,
                        "age_ms": age_ms, "bbo_width": bbo_width, "skip_reason": ""}

        # All-phantom fallback: penny-jump the deepest non-self level
        deepest = None
        for idx, (px, sz) in enumerate(levels):
            if filter_self and our_prices and any(abs(px - op) < tick * 0.5 for op in our_prices):
                continue
            deepest = (px, idx + 1, sz)
        if deepest is not None:
            px, lvl, sz = deepest
            return {"price": px, "level": lvl, "size": sz,
                    "age_ms": age_ms, "bbo_width": bbo_width, "skip_reason": ""}

        return {"price": None, "level": None, "size": None,
                "age_ms": age_ms, "bbo_width": bbo_width, "skip_reason": "self_only"}

    def get_quotable_strikes(self) -> List[float]:
        """Return list of strikes eligible for quoting."""
        config = self.config
        state = self.state
        quotable = []

        inc = state.strike_increment
        if inc <= 0 or state.atm_strike <= 0:
            return quotable

        low = state.atm_strike + (config.product.quote_range_low * inc)
        high = state.atm_strike + (config.product.quote_range_high * inc)
        tick = config.quoting.tick_size

        for strike in state.get_all_strikes():
            if strike < low or strike > high:
                continue

            option = state.get_option(strike)
            if option is None:
                continue

            dte = days_to_expiry(option.expiry)
            if dte <= config.product.min_dte:
                continue

            # Must have valid incumbent quote
            if option.bid <= 0 or option.ask <= 0:
                continue

            # Spread must be wide enough to penny-jump
            if option.ask - option.bid < 2 * tick:
                continue

            quotable.append(strike)

        return quotable

    def cancel_all_subscriptions(self):
        """Cancel all market data subscriptions."""
        for ticker in self._option_tickers.values():
            self.ib.cancelMktData(ticker.contract)
        if self._underlying_ticker:
            self.ib.cancelMktData(self._underlying_ticker.contract)
        self._option_tickers.clear()
        logger.info("All market data subscriptions cancelled")
