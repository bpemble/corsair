"""IPC wire protocol — msgpack frames, length-prefixed.

Frame format:
    [4-byte big-endian uint32 length][msgpack body]

Body is a dict with at minimum:
    {"type": <str>, "ts_ns": <int>, ...}

The type string identifies which message it is. See EVENT_TYPES (broker→
trader) and COMMAND_TYPES (trader→broker). Forward-compat rule: receivers
ignore unknown fields and unknown types (log + drop).

Why msgpack and not JSON: ~3-5× faster encode + decode for our payload
shape, ~50% smaller wire size, native handling of binary types. Drop-in
replacement; we'd reach for it anyway in Phase 5 SHM rings.
"""

import struct
from typing import Iterator

import msgpack


DEFAULT_SOCKET_PATH = "/app/data/corsair_ipc.sock"

# Broker → trader. Anything coming off the gateway plus internal kill state.
EVENT_TYPES = frozenset({
    "hello",            # handshake on connect, broker → trader (after welcome)
    "tick",             # option ticker update
    "underlying_tick",  # underlying futures price update
    "order_ack",        # orderStatusEvent (Submitted, Filled, Cancelled, ...)
    "fill",             # execDetailsEvent — single execution
    "vol_surface",      # SABR/SVI fit params, per expiry
    "kill",             # risk/op-kill fired; trader must stop quoting
    "resume",           # kill cleared (e.g. daily_halt at session rollover)
    "weekend_pause",    # paused: bool — Friday close / Sunday reopen
    "snapshot",         # full state seed sent on (re)connect
    "risk_state",       # periodic broker → trader risk aggregates
                        # (cleanup #8): margin, Greeks, position counts
                        # — gates trader's new-order placement
})

# Trader → broker.
COMMAND_TYPES = frozenset({
    "welcome",          # handshake on connect, trader → broker
    "place_order",
    "cancel_order",
    "subscribe_chain",  # request market-data subs (idempotent)
    "telemetry",        # ttt/p50/p99 etc. for dashboard / logs
    "ping",             # liveness — broker echoes back as `pong` event
})

_LEN_HDR = struct.Struct(">I")
_MAX_FRAME_BYTES = 4 * 1024 * 1024  # 4 MiB hard cap; protects against malformed input


def pack_frame(msg: dict) -> bytes:
    """Encode a message as a length-prefixed msgpack frame.

    Caller's responsibility: include "type" and "ts_ns" fields. The
    encoder doesn't validate; receivers do.
    """
    body = msgpack.packb(msg, use_bin_type=True)
    if len(body) > _MAX_FRAME_BYTES:
        raise ValueError(
            f"frame too large: {len(body)} > {_MAX_FRAME_BYTES} bytes"
        )
    return _LEN_HDR.pack(len(body)) + body


def unpack_frames(buffer: bytearray) -> Iterator[dict]:
    """Decode all complete frames in the front of ``buffer`` and yield them.

    Mutates ``buffer`` in place: consumed bytes are removed. Partial frames
    at the tail are left in place for the next read. Caller appends new
    bytes to ``buffer`` and re-invokes.
    """
    while len(buffer) >= 4:
        (size,) = _LEN_HDR.unpack_from(buffer, 0)
        if size > _MAX_FRAME_BYTES:
            raise ValueError(
                f"oversize frame on wire: {size} > {_MAX_FRAME_BYTES}"
            )
        if len(buffer) < 4 + size:
            return  # incomplete; wait for more bytes
        body = bytes(buffer[4 : 4 + size])
        del buffer[: 4 + size]
        try:
            yield msgpack.unpackb(body, raw=False)
        except Exception:
            # Skip malformed frame; caller logs.
            continue
