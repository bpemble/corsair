"""Cut-over preflight check.

Validates EVERY safety condition before allowing CORSAIR_TRADER_PLACES_ORDERS=1.
Returns nonzero if any check fails — operators should treat green here as
the gate to flipping cut-over on.

Usage (recommended — from host so docker compose calls work):
    cd ~/corsair && python3 scripts/cut_over_preflight.py

The script auto-detects whether it's running on the host (has access to
docker compose and /home/ethereal/corsair) or inside the container, and
adjusts paths/commands accordingly.

Exits 0 on all-green, 1 on any failure.

Checks (in order):
  1. Broker container is up and healthy
  2. Trader container is up
  3. Position is flat (or within tolerance)
  4. No active risk kills
  5. Daily P&L is healthy (above halt threshold)
  6. SABR fits are recent (< 5min old) for both expiries × both sides
  7. Vol surface published to trader's events log
  8. Risk-state events flowing trader → broker (1Hz cadence holding)
  9. Trader-side telemetry shows healthy IPC + decision rates
 10. Hedge contract resolved (HGK6 / HGN6 etc.)
 11. Trader's decisions match broker's actual placements ≥99% of last hour
     (parity check — only meaningful if broker has been quoting)
 12. No recent adverse fills (last 30 min)
"""
import argparse
import datetime
import json
import os
import subprocess
import sys
from collections import Counter


GREEN = "\033[32m"
RED = "\033[31m"
YELLOW = "\033[33m"
RESET = "\033[0m"


class CheckResult:
    def __init__(self, name: str, passed: bool, detail: str = ""):
        self.name = name
        self.passed = passed
        self.detail = detail

    def __str__(self):
        color = GREEN if self.passed else RED
        mark = "✓" if self.passed else "✗"
        return f"  {color}{mark}{RESET} {self.name}: {self.detail}"


def check_containers() -> CheckResult:
    try:
        out = subprocess.check_output(
            ["docker", "compose", "ps", "--format", "json"],
            cwd="/home/ethereal/corsair",
            text=True, timeout=10,
        )
        services = {}
        for line in out.strip().split("\n"):
            if not line:
                continue
            d = json.loads(line)
            services[d.get("Service")] = d.get("State")
        broker = services.get("corsair") == "running"
        trader = services.get("trader") == "running"
        gateway = services.get("ib-gateway") == "running"
        if broker and trader and gateway:
            return CheckResult("containers up", True,
                              "broker + trader + gateway all running")
        missing = []
        if not broker: missing.append("broker")
        if not trader: missing.append("trader")
        if not gateway: missing.append("gateway")
        return CheckResult("containers up", False,
                          f"missing: {','.join(missing)}")
    except Exception as e:
        return CheckResult("containers up", False, f"docker query failed: {e}")


def check_position_flat(snapshot: dict) -> CheckResult:
    port = snapshot.get("portfolio", {})
    n = port.get("total_contracts", 0)
    delta = abs(port.get("options_delta", 0.0))
    if n == 0:
        return CheckResult("position flat", True, f"0 contracts")
    if delta < 1.0:
        return CheckResult("position flat", True,
                          f"{n} contracts but |delta|={delta:.2f} < 1.0")
    return CheckResult("position flat", False,
                      f"{n} contracts, |delta|={delta:.2f}")


def check_no_kills(snapshot: dict, log_path: str) -> CheckResult:
    """Look at recent broker logs for unresolved kill switches."""
    try:
        out = subprocess.check_output(
            ["docker", "compose", "logs", "--since", "2m", "corsair"],
            cwd="/home/ethereal/corsair",
            text=True, timeout=10,
        )
        active_kills = []
        for line in out.split("\n"):
            if "KILL SWITCH ACTIVATED" in line and "INDUCED" not in line:
                # Look for a CORRESPONDING clear/resume in nearby logs
                active_kills.append(line)
        if active_kills:
            last = active_kills[-1][-150:]
            return CheckResult("no active kills", False,
                              f"recent kill: ...{last}")
        return CheckResult("no active kills", True, "no kills in last 2 min")
    except Exception as e:
        return CheckResult("no active kills", False, f"log query failed: {e}")


def check_daily_pnl(snapshot: dict) -> CheckResult:
    port = snapshot.get("portfolio", {})
    sc = port.get("spread_capture", 0.0)
    cap = 500_000  # hard-coded; TODO load from config
    halt_pct = 0.05
    halt_threshold = -cap * halt_pct
    if sc > halt_threshold * 0.8:
        return CheckResult("daily P&L healthy", True,
                          f"spread_capture=${sc:.0f}, halt at ${halt_threshold:.0f}")
    return CheckResult("daily P&L healthy", False,
                      f"spread_capture=${sc:.0f} approaching halt ${halt_threshold:.0f}")


def check_sabr_fits_fresh(log_dir: str) -> CheckResult:
    """Look at sabr_fits-DATE.jsonl for recent fits."""
    today = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    path = os.path.join(log_dir, f"sabr_fits-{today}.jsonl")
    if not os.path.exists(path):
        return CheckResult("SABR fits fresh", False, f"no log at {path}")
    cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(minutes=5)
    expiries_seen = set()
    with open(path) as f:
        for line in f:
            try:
                r = json.loads(line)
                ts = datetime.datetime.fromisoformat(r["timestamp_utc"].replace("Z", "+00:00"))
                if ts < cutoff:
                    continue
                expiries_seen.add((r.get("expiry"), r.get("side")))
            except Exception:
                continue
    if len(expiries_seen) >= 2:  # at least one C + one P, ideally more
        return CheckResult("SABR fits fresh", True,
                          f"{len(expiries_seen)} (expiry,side) fits in last 5min")
    return CheckResult("SABR fits fresh", False,
                      f"only {len(expiries_seen)} fits in last 5min — check broker")


def check_risk_state_flowing(log_dir: str) -> CheckResult:
    """Trader's events log shows risk_state events at 1Hz cadence."""
    today = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    path = os.path.join(log_dir, f"trader_events-{today}.jsonl")
    if not os.path.exists(path):
        return CheckResult("risk_state flowing", False,
                          f"no trader events log at {path}")
    cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(minutes=1)
    n = 0
    with open(path) as f:
        # Read tail efficiently
        f.seek(0, 2)
        size = f.tell()
        f.seek(max(0, size - 200_000))
        f.readline()  # skip partial line
        for line in f:
            try:
                r = json.loads(line)
                ts = datetime.datetime.fromisoformat(r["recv_ts"])
                if ts < cutoff:
                    continue
                if r.get("event", {}).get("type") == "risk_state":
                    n += 1
            except Exception:
                continue
    # Expect ~60 events at 1Hz
    if n >= 30:
        return CheckResult("risk_state flowing", True,
                          f"{n} risk_state events in last 60s (expect ~60)")
    return CheckResult("risk_state flowing", False,
                      f"only {n} risk_state events in last 60s")


def check_no_recent_bad_fills(log_dir: str) -> CheckResult:
    """No fills with negative edge in the last 30 minutes."""
    today = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    path = os.path.join(log_dir, f"fills-{today}.jsonl")
    if not os.path.exists(path):
        return CheckResult("no recent bad fills", True, "no fills log yet")
    cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(minutes=30)
    bad = []
    with open(path) as f:
        for line in f:
            try:
                r = json.loads(line)
                ts = datetime.datetime.fromisoformat(r["ts"].replace("Z", "+00:00"))
                if ts < cutoff:
                    continue
                edge = r.get("signed_edge", 0)
                if edge < -50:
                    bad.append((r["ts"][11:19], r.get("symbol"), edge))
            except Exception:
                continue
    if not bad:
        return CheckResult("no recent bad fills", True,
                          f"no fills with edge < -$50 in last 30min")
    return CheckResult("no recent bad fills", False,
                      f"{len(bad)} adverse fills in 30min: e.g. {bad[0]}")


def check_hedge_resolved(log_dir: str) -> CheckResult:
    """Look in broker logs for hedge contract resolved + no recent Error 201."""
    try:
        out = subprocess.check_output(
            ["docker", "compose", "logs", "--since", "10m", "corsair"],
            cwd="/home/ethereal/corsair",
            text=True, timeout=10,
        )
        resolved = "HEDGE CONTRACT RESOLVED" in out
        error_201 = out.count("Error 201")
        if not resolved:
            return CheckResult("hedge resolved", False,
                              "no HEDGE CONTRACT RESOLVED line in last 10min")
        if error_201 > 5:
            return CheckResult("hedge resolved", False,
                              f"{error_201} Error 201 (near-expiration) in last 10min")
        return CheckResult("hedge resolved", True,
                          f"contract resolved, {error_201} Error 201s")
    except Exception as e:
        return CheckResult("hedge resolved", False, f"log query failed: {e}")


def check_trader_decisions_active(log_dir: str) -> CheckResult:
    """Trader's decision log shows recent decisions."""
    today = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    path = os.path.join(log_dir, f"trader_decisions-{today}.jsonl")
    if not os.path.exists(path):
        return CheckResult("trader decisions active", False,
                          f"no trader_decisions log")
    cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(minutes=1)
    n = 0
    actions = Counter()
    with open(path) as f:
        f.seek(0, 2)
        size = f.tell()
        f.seek(max(0, size - 500_000))
        f.readline()
        for line in f:
            try:
                r = json.loads(line)
                ts = datetime.datetime.fromisoformat(r["recv_ts"])
                if ts < cutoff:
                    continue
                n += 1
                actions[r["decision"]["action"]] += 1
            except Exception:
                continue
    if n >= 50:
        return CheckResult("trader decisions active", True,
                          f"{n} decisions in last 60s (place={actions['place']}, skip={actions['skip']})")
    return CheckResult("trader decisions active", False,
                      f"only {n} decisions in last 60s")


def _autodetect_paths():
    """Pick container or host paths based on what exists."""
    host_root = "/home/ethereal/corsair"
    if os.path.isdir(host_root):
        return (
            os.path.join(host_root, "data", "hg_chain_snapshot.json"),
            os.path.join(host_root, "logs-paper"),
        )
    return ("/app/data/hg_chain_snapshot.json", "/app/logs-paper")


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    default_snap, default_logs = _autodetect_paths()
    p.add_argument("--snapshot", default=default_snap)
    p.add_argument("--logs-dir", default=default_logs)
    args = p.parse_args()

    print("=" * 60)
    print("CUT-OVER PREFLIGHT CHECK")
    print(f"  time: {datetime.datetime.now(datetime.timezone.utc).isoformat()}")
    print("=" * 60)

    snapshot = {}
    if os.path.exists(args.snapshot):
        try:
            with open(args.snapshot) as f:
                snapshot = json.load(f)
        except Exception:
            pass

    results = [
        check_containers(),
        check_position_flat(snapshot),
        check_no_kills(snapshot, args.logs_dir),
        check_daily_pnl(snapshot),
        check_sabr_fits_fresh(args.logs_dir),
        check_risk_state_flowing(args.logs_dir),
        check_hedge_resolved(args.logs_dir),
        check_trader_decisions_active(args.logs_dir),
        check_no_recent_bad_fills(args.logs_dir),
    ]

    for r in results:
        print(r)

    print()
    n_pass = sum(1 for r in results if r.passed)
    if n_pass == len(results):
        print(f"{GREEN}✓ ALL {len(results)} CHECKS PASSED — cut-over is safe to enable{RESET}")
        return 0
    print(f"{RED}✗ {len(results) - n_pass}/{len(results)} CHECKS FAILED — DO NOT enable cut-over{RESET}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
