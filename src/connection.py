"""IBKR Gateway connection management for Corsair v2."""

import logging
from typing import Callable, Optional

from ib_insync import IB

logger = logging.getLogger(__name__)


class IBKRConnection:
    """Manages the IBKR Gateway connection lifecycle."""

    def __init__(self, config):
        self.config = config
        self.ib = IB()
        self._on_disconnect_callback: Optional[Callable] = None
        self._connected = False
        self._disconnect_fired = False
        self._disconnect_handler_registered = False

    @property
    def connected(self) -> bool:
        return self._connected and self.ib.isConnected()

    def set_disconnect_callback(self, callback: Callable):
        self._on_disconnect_callback = callback

    async def connect(self) -> bool:
        """Connect to IBKR Gateway with a minimal bootstrap.

        ib_insync's stock IB.connectAsync issues a long list of initializing
        requests in parallel after the API handshake (positions, open orders,
        completed orders, executions, account updates, *and* per-sub-account
        multi-account updates for every account on the login). On a paper
        login with 6 sub-accounts and a heavy overnight order history this
        bootstrap consistently times out — completed orders alone can take
        60+ seconds because IB Gateway is also processing them internally.

        We replace it with a hand-rolled bootstrap that issues only the four
        requests we actually need:

          1. client.connectAsync       — TCP/API handshake
          2. reqPositionsAsync         — to seed our position book
          3. reqOpenOrdersAsync        — to know what's resting from prior runs
          4. reqAccountUpdatesAsync    — for cash/margin/balance state

        Skipped vs the stock bootstrap:
          - reqCompletedOrdersAsync         (we never read completed orders;
                                             openTrades comes from reqOpenOrders)
          - reqExecutionsAsync              (we use execDetailsEvent for new fills)
          - reqAccountUpdatesMultiAsync × N (we trade in exactly one account)
          - reqAutoOpenOrders               (we match orders by orderRef, not bind)

        This brings the connect from ~33-90s down to ~3-5s in the steady state
        and eliminates the failure modes where any single bloat request times
        out and breaks the whole gather.
        """
        import asyncio
        from ib_insync.util import getLoop  # noqa: F401

        host = self.config.account.gateway_host
        port = self.config.account.gateway_port
        client_id = self.config.account.client_id
        account_id = self.config.account.account_id
        TIMEOUT = 30  # per-request budget; lean bootstrap should never need more

        logger.info(
            "Connecting to IBKR Gateway at %s:%d (client_id=%d)",
            host, port, client_id,
        )

        ib = self.ib
        try:
            # 1. API handshake
            await ib.client.connectAsync(host, port, client_id, TIMEOUT)

            # clientId=0 has special semantics in the TWS API: it's the
            # "master" client that receives order status messages for orders
            # placed by ANY client on the connection, AND it's the only mode
            # that works correctly with FA (Financial Advisor) accounts. On
            # an FA login, IBKR rewrites the routing of orderStatus messages
            # so they come back tagged with the FA master's clientId — non-
            # zero clients miss every status update because the wrapper looks
            # up trades by (clientId, orderId) and the lookup silently fails.
            # The order is actually live on IBKR; we just never see the ack.
            # reqAutoOpenOrders(True) binds master orders to this session and
            # is REQUIRED for clientId=0; ib_insync's stock connectAsync calls
            # this automatically but our hand-rolled lean bootstrap must do
            # it explicitly.
            if client_id == 0:
                ib.reqAutoOpenOrders(True)

            # 2-4. Minimal initializing requests, run concurrently. Each gets
            #      its own timeout so a single slow one doesn't block the rest.
            reqs = {
                "positions": ib.reqPositionsAsync(),
                "open orders": ib.reqOpenOrdersAsync(),
                "account updates": ib.reqAccountUpdatesAsync(account_id),
            }
            tasks = [asyncio.wait_for(coro, TIMEOUT) for coro in reqs.values()]
            results = await asyncio.gather(*tasks, return_exceptions=True)
            timeouts = 0
            for name, result in zip(reqs, results):
                if isinstance(result, asyncio.TimeoutError):
                    logger.warning("Bootstrap '%s' timed out", name)
                    timeouts += 1
                elif isinstance(result, BaseException):
                    logger.warning("Bootstrap '%s' failed: %s", name, result)
                    timeouts += 1
            # If any of the lean bootstrap calls timed out, the gateway is
            # in the half-dead "IBC session corrupted" state — accepts TCP,
            # rejects/hangs API calls. Continuing to declare "Connected"
            # downstream guarantees a wasted chain-discovery attempt later
            # and another 30s before the watchdog can re-escalate. Fail
            # fast so the watchdog escalation counter can advance.
            if timeouts >= 1:
                logger.error(
                    "Bootstrap failed: %d/%d requests timed out — gateway is "
                    "half-dead, reporting connect failure",
                    timeouts, len(reqs),
                )
                try:
                    ib.disconnect()
                except Exception:
                    pass
                self._connected = False
                return False

            # Force a fresh nextValidId sync. IBKR sends one automatically
            # during the API handshake, but if a prior session on this clientId
            # left orphaned orders, the counter we got at handshake can collide
            # with already-used IDs. reqIds(-1) makes IBKR re-emit nextValidId
            # using its current high-water mark, which is what we actually want.
            try:
                ib.client.reqIds(-1)
                await asyncio.sleep(0.5)  # let nextValidId callback land
            except Exception:
                pass

            # Final socket sanity check (mirrors ib_insync's own guard).
            if not ib.client.isReady():
                raise ConnectionError("Socket connection broken during bootstrap")

            self._connected = True
            self._disconnect_fired = False  # arm for the next drop

            # Register disconnect handler (only once across reconnects)
            if not getattr(self, "_disconnect_handler_registered", False):
                ib.disconnectedEvent += self._on_disconnect
                ib.errorEvent += self._on_error
                self._disconnect_handler_registered = True

            # ib_insync normally emits this from connectAsync; emit it ourselves
            # so any code listening on connectedEvent still fires.
            ib.connectedEvent.emit()

            logger.info(
                "Connected to IBKR Gateway. Server version: %s",
                ib.client.serverVersion(),
            )
            return True
        except Exception as e:
            logger.error("Failed to connect to IBKR Gateway: %s", e)
            try:
                ib.disconnect()
            except Exception:
                pass
            self._connected = False
            return False

    async def disconnect(self):
        """Gracefully disconnect from IBKR Gateway."""
        # Mark as already-fired so the disconnectedEvent that ib_insync emits
        # during ib.disconnect() doesn't re-trigger our user callback (the
        # caller is initiating this teardown deliberately).
        self._disconnect_fired = True
        if self.ib.isConnected():
            self.ib.disconnect()
        self._connected = False
        logger.info("Disconnected from IBKR Gateway")

    def _on_error(self, *args, **kwargs):
        """Treat IBKR connectivity errors as soft disconnects.

        ib_insync's errorEvent signature has varied across versions
        ((reqId, errorCode, errorString) → +contract → +errorTime), and
        a signature mismatch in eventkit dispatch silently swallows the
        callback. Accept *args/**kwargs to be version-agnostic and
        unpack defensively.
        """
        if len(args) < 2:
            return
        # args: (reqId, errorCode, errorString, [contract], [errorTime])
        try:
            errorCode = int(args[1])
        except (TypeError, ValueError):
            return
        errorString = args[2] if len(args) >= 3 else ""
        return self._handle_error_code(errorCode, errorString)

    def _handle_error_code(self, errorCode: int, errorString: str):
        """Treat IBKR connectivity errors as soft disconnects.

        Defense vector A: ib_insync's `disconnectedEvent` only fires on hard
        socket close. When IBKR's upstream link dies (Error 1100), the TCP
        socket stays alive but no data flows — we'd otherwise wait for the
        watchdog tick-staleness threshold (30s) before reacting. By the
        time we did, our orders had been sitting on the book for 30+s of
        bad-fill exposure.

        Codes:
          1100 — Connectivity between IBKR and TWS has been lost
          1300 — TWS socket port has been reset
        Both mean: stop trading, cancel everything, wait for restoration.

          1101 — Connectivity restored, data lost (need to resubscribe)
          1102 — Connectivity restored, data maintained
        These will be picked up by the watchdog reconnect path; we don't
        need to do anything special here beyond noting them.
        """
        if errorCode in (1100, 1300):
            if not getattr(self, "_disconnect_fired", False):
                logger.critical(
                    "IBKR Error %d: %s — treating as soft disconnect",
                    errorCode, errorString,
                )
                self._on_disconnect()
        elif errorCode in (1101, 1102):
            logger.warning("IBKR Error %d: %s", errorCode, errorString)

    def _on_disconnect(self):
        """Called when gateway connection drops. Idempotent within a session
        — only the first invocation per connection actually fires the user
        callback. ib_insync's disconnectedEvent can fire multiple times for a
        single drop (and once more from a deliberate teardown), so we guard
        with a per-connection flag that resets on each successful connect."""
        if getattr(self, "_disconnect_fired", False):
            return
        self._disconnect_fired = True
        self._connected = False
        logger.critical("IBKR Gateway connection lost")
        if self._on_disconnect_callback:
            self._on_disconnect_callback()

