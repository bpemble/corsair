"""Capture the wire bytes ib_insync sends for a place_order.

Used as a reference to compare against our Rust broker's encoder.
ib_insync 0.9.86 caps at server v170 — but the gateway is v178, and
ib_insync still works against it. So whatever ib_insync sends is the
known-good byte sequence for our gateway.

The script:
  1. Connects via ib_insync, normal handshake
  2. Monkey-patches the underlying socket .write to capture frames
  3. Qualifies an HG option contract (HXEM6 C600)
  4. Builds a LMT GTD order with parameters matching what our trader
     uses (qty=1, GTD-30s, account=DUP553657, orderRef=test)
  5. Calls placeOrder
  6. Prints the captured frame bytes (hex + ascii, with field
     boundaries marked at \\0 separators)
  7. Cancels the order so we don't actually leave it resting

Run via:
  docker run --rm --network host -v $PWD/scripts:/app/scripts:ro \\
      --entrypoint sh corsair-corsair -c \\
      "pip install --quiet ib-insync 2>/dev/null && \\
       python3 /app/scripts/capture_place_order_bytes.py"
"""
import asyncio
import os
import sys
from datetime import datetime, timezone, timedelta

from ib_insync import IB, FuturesOption, LimitOrder

CAPTURED = []


def _hex_dump(buf: bytes) -> str:
    """Pretty-print a wire frame: each \\0-separated field on its own line."""
    parts = buf.split(b"\x00")
    # split() leaves an empty trailing if buf ends with \0 — drop it
    if parts and parts[-1] == b"":
        parts = parts[:-1]
    out = []
    for i, p in enumerate(parts):
        try:
            text = p.decode("ascii")
        except UnicodeDecodeError:
            text = repr(p)
        out.append(f"  [{i:3d}] {text!r}")
    return "\n".join(out)


def patch_transport(ib: IB):
    """Hook the underlying transport.write to copy every outbound frame."""
    transport = ib.client.conn.transport
    original_write = transport.write

    def write_capture(data):
        CAPTURED.append(bytes(data))
        return original_write(data)

    transport.write = write_capture


async def main():
    host = os.environ.get("CORSAIR_GATEWAY_HOST", "127.0.0.1")
    port = int(os.environ.get("CORSAIR_GATEWAY_PORT", "4002"))
    account = os.environ.get("IBKR_ACCOUNT", "DUP553657")

    ib = IB()
    print(f"Connecting to {host}:{port} as clientId=99 account={account}")
    await ib.connectAsync(host, port, clientId=99)
    patch_transport(ib)
    print(f"Connected. server_version={ib.client.serverVersion()}")

    # Pick a deep OTM strike so we don't risk a fill — our quote will
    # be miles below the bid.
    contract = FuturesOption(
        symbol="HG", lastTradeDateOrContractMonth="20260526",
        strike=6.50, right="C", exchange="COMEX", currency="USD",
        multiplier="25000", tradingClass="HXE",
    )
    await ib.qualifyContractsAsync(contract)
    print(f"Qualified: conId={contract.conId} localSym={contract.localSymbol}")

    # Match our broker's params:
    # - LMT @ 0.001 (well below market — won't fill)
    # - GTD-30s
    # - account=DUP553657
    # - orderRef=corsair_byte_test
    gtd = (datetime.now(timezone.utc) + timedelta(seconds=30)).strftime(
        "%Y%m%d %H:%M:%S UTC"
    )
    order = LimitOrder(
        "BUY", 1, 0.001,
        account=account,
        tif="GTD",
        orderRef="corsair_byte_test",
    )
    order.goodTillDate = gtd

    print(f"Placing order: {order}")
    print(f"goodTillDate=[{gtd}]")

    # Capture only the bytes we generate from this point. Reset.
    CAPTURED.clear()

    trade = ib.placeOrder(contract, order)

    # Wait briefly for the placeOrder to be sent + status to update.
    for _ in range(50):
        await asyncio.sleep(0.1)
        if trade.orderStatus.status not in ("PendingSubmit",):
            break

    print(f"\nFinal status: {trade.orderStatus.status}")
    print(f"Captured {len(CAPTURED)} frame(s):")
    for i, buf in enumerate(CAPTURED):
        # The first 4 bytes are a big-endian length prefix.
        if len(buf) >= 4:
            length = int.from_bytes(buf[:4], "big")
            print(f"\n--- Frame {i}: 4-byte length={length}, total={len(buf)} bytes ---")
            print(_hex_dump(buf[4:]))
        else:
            print(f"\n--- Frame {i}: {len(buf)} bytes (too short) ---")
            print(repr(buf))

    # Try to cancel so we don't leave the test order live.
    try:
        ib.cancelOrder(trade.order)
        await asyncio.sleep(2)
        print(f"\nCancel result: {trade.orderStatus.status}")
    except Exception as e:
        print(f"Cancel failed: {e}")

    ib.disconnect()


if __name__ == "__main__":
    asyncio.run(main())
