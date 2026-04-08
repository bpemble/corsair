"""Runtime monkey-patches for ib_insync.

ib_insync 0.9.86 (the version pinned in our Dockerfile) has a known bug in
``Wrapper.updateMktDepthL2``: when IB sends an "update" operation for a DOM
position that doesn't yet exist locally, ``dom[position] = DOMLevel(...)``
raises ``IndexError``. The exception is caught by the ib_insync decoder and
logged at ERROR level, but each occurrence costs a full traceback format
plus a logger.error call. Under L2 depth subscriptions on busy contracts
this can fire hundreds of times per second and starve the asyncio event
loop, blocking quote processing entirely.

This module replaces the broken method with a safe variant that uses
list.insert() semantics on out-of-range updates (insert clamps position to
len(list), so it never raises). All other behavior — dom mutation order,
MktDepthData tick emission, pendingTickers add — is preserved.

Apply by importing this module once before any ib_insync.IB instance is
created.
"""

import logging

from ib_insync import wrapper as _ib_wrapper
from ib_insync.objects import DOMLevel, MktDepthData

logger = logging.getLogger(__name__)

_PATCHED = False


def apply():
    """Apply the updateMktDepthL2 patch. Idempotent."""
    global _PATCHED
    if _PATCHED:
        return

    def safe_updateMktDepthL2(
        self, reqId, position, marketMaker, operation,
        side, price, size, isSmartDepth=False,
    ):
        # operation: 0 = insert, 1 = update, 2 = delete
        # side: 0 = ask, 1 = bid
        ticker = self.reqId2Ticker.get(reqId)
        if ticker is None:
            return

        dom = ticker.domBids if side else ticker.domAsks
        if operation == 0:
            dom.insert(position, DOMLevel(price, size, marketMaker))
        elif operation == 1:
            # PATCH: original used dom[position] = ... which raises IndexError
            # when position >= len(dom). Use insert() which clamps safely.
            if 0 <= position < len(dom):
                dom[position] = DOMLevel(price, size, marketMaker)
            else:
                dom.insert(position, DOMLevel(price, size, marketMaker))
        elif operation == 2:
            if 0 <= position < len(dom):
                level = dom.pop(position)
                price = level.price
                size = 0

        tick = MktDepthData(
            self.lastTime, position, marketMaker, operation, side, price, size)
        ticker.domTicks.append(tick)
        self.pendingTickers.add(ticker)

    _ib_wrapper.Wrapper.updateMktDepthL2 = safe_updateMktDepthL2
    _PATCHED = True
    logger.info("Applied ib_insync updateMktDepthL2 patch")
