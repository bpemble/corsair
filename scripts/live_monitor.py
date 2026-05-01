"""Live monitor — watches fills + risk + active orders.

Polls the most recent state every 5s and alerts on:
  - Negative-edge fills (>$50 of negative edge)
  - Position drift past delta_ceiling/2
  - Daily P&L approaching halt
  - Position concentration (one strike > 20 contracts)

Output lines suitable for piping to Discord / Slack / log.

Usage:
  docker compose exec corsair python3 /app/scripts/live_monitor.py
  # Or with --once for single-shot.
"""
import argparse
import datetime
import json
import os
import time
from collections import Counter


def _read_snapshot(path: str):
    try:
        with open(path) as f:
            return json.load(f)
    except Exception:
        return None


def _tail_fills(path: str, since_iso: str) -> list:
    """Return fills with ts >= since_iso."""
    if not os.path.exists(path):
        return []
    out = []
    with open(path) as f:
        # Read tail; full file scan acceptable for our volume
        for line in f:
            try:
                r = json.loads(line)
                if r.get("ts", "") >= since_iso:
                    out.append(r)
            except Exception:
                continue
    return out


def check_fills(state: dict) -> list:
    """Return alert messages for adverse fills since last poll."""
    alerts = []
    last_seen = state.setdefault("last_fill_ts", "")
    today = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    path = os.path.join(state["logs_dir"], f"fills-{today}.jsonl")
    new_fills = _tail_fills(path, last_seen)
    new_fills = [f for f in new_fills if f.get("ts", "") > last_seen]
    if new_fills:
        state["last_fill_ts"] = max(f["ts"] for f in new_fills)
    for f in new_fills:
        edge = f.get("signed_edge", 0)
        if edge < -50:
            alerts.append(
                f"ALERT adverse fill: {f.get('ts','')[11:19]} "
                f"{f.get('side','?')} {f.get('size',0)} {f.get('symbol','?')} "
                f"@{f.get('price',0):.4f} bid={f.get('market_bid','?')} "
                f"ask={f.get('market_ask','?')} edge=${edge:.0f}"
            )
    return alerts


def check_position(snapshot: dict) -> list:
    alerts = []
    port = snapshot.get("portfolio", {})
    eff_delta = abs(port.get("effective_delta", 0))
    options_delta = port.get("options_delta", 0)
    n_contracts = port.get("total_contracts", 0)
    if eff_delta > 4.0:
        alerts.append(
            f"WARN effective_delta={eff_delta:.2f} approaching delta_kill (5.0)"
        )
    elif eff_delta > 2.5:
        alerts.append(
            f"INFO effective_delta={eff_delta:.2f} past delta_ceiling (3.0)"
        )
    if abs(options_delta) > 4.0 and abs(eff_delta) < 1.0:
        alerts.append(
            f"INFO options_delta={options_delta:.2f} (hedge effective)"
        )
    # Concentration check
    positions = port.get("positions", [])
    for p in positions:
        if abs(p.get("qty", 0)) >= 20:
            alerts.append(
                f"WARN concentration: {p.get('strike')}{p.get('right')} "
                f"qty={p.get('qty')}"
            )
    return alerts


def check_daily_pnl(snapshot: dict) -> list:
    alerts = []
    sc = snapshot.get("portfolio", {}).get("spread_capture", 0)
    threshold = -25_000
    pct = (sc / threshold) if threshold else 0
    if pct > 0.5:
        alerts.append(
            f"WARN daily P&L ${sc:.0f} / halt ${threshold:.0f} ({pct*100:.0f}% to halt)"
        )
    return alerts


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--snapshot", default="/app/data/hg_chain_snapshot.json")
    p.add_argument("--logs-dir", default="/app/logs-paper")
    p.add_argument("--interval", type=int, default=5)
    p.add_argument("--once", action="store_true",
                   help="Run a single check and exit")
    args = p.parse_args()

    state = {"logs_dir": args.logs_dir, "last_fill_ts": ""}
    # On first run, set last_fill_ts to NOW so we don't replay old fills
    today = datetime.datetime.now(datetime.timezone.utc)
    state["last_fill_ts"] = today.isoformat()

    print(f"[{today.isoformat()}] live_monitor started; interval={args.interval}s")
    while True:
        snapshot = _read_snapshot(args.snapshot)
        ts = datetime.datetime.now(datetime.timezone.utc).strftime("%H:%M:%S")
        if snapshot:
            alerts = []
            alerts += check_fills(state)
            alerts += check_position(snapshot)
            alerts += check_daily_pnl(snapshot)
            if alerts:
                for a in alerts:
                    print(f"[{ts}] {a}")
            else:
                # Heartbeat every 12 polls (= 1 min default) so we know
                # it's alive
                if int(time.time()) % 60 < args.interval:
                    port = snapshot.get("portfolio", {})
                    print(f"[{ts}] OK: pos={port.get('total_contracts',0)} "
                          f"delta={port.get('effective_delta',0):.2f} "
                          f"sc=${port.get('spread_capture',0):.0f}")
        else:
            print(f"[{ts}] WARN: no snapshot at {args.snapshot}")
        if args.once:
            break
        time.sleep(args.interval)
    return 0


if __name__ == "__main__":
    import sys
    sys.exit(main())
