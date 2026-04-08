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
        """Connect to IBKR Gateway. Returns True on success."""
        host = self.config.account.gateway_host
        port = self.config.account.gateway_port
        client_id = self.config.account.client_id

        logger.info("Connecting to IBKR Gateway at %s:%d (client_id=%d)", host, port, client_id)

        try:
            await self.ib.connectAsync(host, port, clientId=client_id, timeout=30)
            self._connected = True
            self._disconnect_fired = False  # arm for the next drop

            # Register disconnect handler (only once across reconnects)
            if not getattr(self, "_disconnect_handler_registered", False):
                self.ib.disconnectedEvent += self._on_disconnect
                self._disconnect_handler_registered = True

            logger.info("Connected to IBKR Gateway. Server version: %s", self.ib.client.serverVersion())
            return True
        except Exception as e:
            logger.error("Failed to connect to IBKR Gateway: %s", e)
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

