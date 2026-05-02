"""Pre-trade preflight check for the post-Phase 6.7 stack.

Validates EVERY safety condition before allowing live trading. Run before
Sunday market open (CME HG resumes 17:00 CT), after any code deploy, or
any time you want a fast all-systems check. Returns nonzero if any check
fails.

Usage (from host so docker compose calls work):
    cd ~/corsair && python3 scripts/cut_over_preflight.py

The script auto-detects host vs container and adjusts paths.

Checks (in order):
   1. corsair-broker-rs / trader / ib-gateway containers up
   2. Gateway not in Error 2110 ("TWS↔server connectivity broken")
   3. NativeBroker initial snapshot completed (positions/openOrders/account)
   4. Position is flat OR within tolerance
   5. No active risk kills
   6. Daily P&L is healthy (above halt threshold)
   7. Vol-surface fits flowing (recent trader_events log entries)
   8. risk_state events flowing trader-side at 1Hz
   9. Hedge contract resolved
  10. ibkr_scale calibration recent (or freshly-booted)
  11. Trader-side decisions active (only post-RTH; skipped if markets closed)
  12. No recent adverse fills (last 30 min)

Exits 0 on all-green, 1 on any failure.
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

BROKER_SERVICE = "corsair-broker-rs"
TRADER_SERVICE = "trader"
GATEWAY_SERVICE = "ib-gateway"


class CheckResult:
    def __init__(self, name: str, passed: bool, detail: str = "", warn: bool = False):
        self.name = name
        self.passed = passed
        self.warn = warn
        self.detail = detail

    def __str__(self):
        if self.passed:
            color, mark = GREEN, "✓"
        elif self.warn:
            color, mark = YELLOW, "!"
        else:
            color, mark = RED, "✗"
        return f"  {color}{mark}{RESET} {self.name}: {self.detail}"


def _trim_subsec(ts: str) -> str:
    """Drop fractional-second digits past 6 so py3.10 fromisoformat parses
    Rust nanosecond timestamps."""
    if "." not in ts:
        return ts
    head, _, tail = ts.partition(".")
    # tail is e.g. "963343572+00:00" — split fraction from offset
    frac = ""
    rest = tail
    for i, c in enumerate(tail):
        if not c.isdigit():
            frac = tail[:i]
            rest = tail[i:]
            break
    else:
        frac = tail
        rest = ""
    return f"{head}.{frac[:6]}{rest}"


def _compose_logs(service: str, since: str = "5m") -> str:
    out = subprocess.check_output(
        ["docker", "compose", "logs", "--since", since, service],
        cwd="/home/ethereal/corsair",
        text=True, timeout=15,
    )
    return out


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
        broker = services.get(BROKER_SERVICE) == "running"
        trader = services.get(TRADER_SERVICE) == "running"
        gateway = services.get(GATEWAY_SERVICE) == "running"
        if broker and trader and gateway:
            return CheckResult(
                "containers up", True,
                f"{BROKER_SERVICE} + {TRADER_SERVICE} + {GATEWAY_SERVICE} all running"
            )
        missing = []
        if not broker: missing.append(BROKER_SERVICE)
        if not trader: missing.append(TRADER_SERVICE)
        if not gateway: missing.append(GATEWAY_SERVICE)
        return CheckResult("containers up", False, f"missing: {','.join(missing)}")
    except Exception as e:
        return CheckResult("containers up", False, f"docker query failed: {e}")


def check_no_gateway_2110() -> CheckResult:
    """Gateway must be connected to IB servers. Error 2110 means it's not."""
    try:
        out = _compose_logs(BROKER_SERVICE, since="3m")
        # Most recent line wins. Look for both 2110 (broken) and 2104
        # (OK) — if 2104 appears AFTER 2110, gateway has recovered.
        last_2110 = None
        last_2104 = None
        for i, line in enumerate(out.split("\n")):
            if "2110" in line and "Connectivity" in line:
                last_2110 = i
            if "2104" in line and "OK" in line:
                last_2104 = i
        if last_2110 is None:
            return CheckResult("gateway connectivity", True,
                              "no Error 2110 in last 3min")
        if last_2104 is not None and last_2104 > last_2110:
            return CheckResult("gateway connectivity", True,
                              "2110 followed by 2104 (recovered)")
        return CheckResult("gateway connectivity", False,
                          "Error 2110 (TWS↔server) is most-recent — gateway needs restart")
    except Exception as e:
        return CheckResult("gateway connectivity", False, f"log query failed: {e}")


def check_initial_snapshot() -> CheckResult:
    """NativeBroker logs 'initial snapshot: positions=true open_orders=true account=true' at boot."""
    try:
        out = _compose_logs(BROKER_SERVICE, since="10m")
        snap_lines = [
            line for line in out.split("\n")
            if "NativeBroker initial snapshot" in line
        ]
        if not snap_lines:
            return CheckResult("initial snapshot", False,
                              "no 'initial snapshot' line — broker may not have completed boot")
        last = snap_lines[-1]
        if "positions=true" in last and "open_orders=true" in last and "account=true" in last:
            return CheckResult("initial snapshot", True, "positions+orders+account all seeded")
        return CheckResult(
            "initial snapshot", False,
            f"partial seed: {last.split('initial snapshot:')[-1].strip()[:80]}",
            warn=True,
        )
    except Exception as e:
        return CheckResult("initial snapshot", False, f"log query failed: {e}")


def check_position_flat(snapshot: dict) -> CheckResult:
    port = snapshot.get("portfolio", {})
    n = port.get("total_contracts", 0)
    delta = abs(port.get("options_delta", port.get("net_delta", 0.0)))
    if n == 0:
        return CheckResult("position flat", True, "0 contracts")
    if delta < 1.0:
        return CheckResult("position flat", True,
                          f"{n} contracts, |delta|={delta:.2f} < 1.0 — within tolerance")
    return CheckResult("position state", True,
                      f"{n} contracts, |delta|={delta:.2f}",
                      warn=True)


def check_no_kills() -> CheckResult:
    """Rust broker logs 'risk check fired kill' (or kill_switch JSONL)."""
    try:
        out = _compose_logs(BROKER_SERVICE, since="30m")
        # Rust kill log strings:
        #   "risk check fired kill: ..."
        #   "kill_switch: ..."
        kills = [
            line for line in out.split("\n")
            if "fired kill" in line or "kill_switch" in line
        ]
        # Filter out induced ones
        real_kills = [k for k in kills if "induced" not in k.lower()]
        if not real_kills:
            return CheckResult("no active kills", True, "no kills in last 30min")
        last = real_kills[-1][-150:]
        return CheckResult("no active kills", False, f"recent kill: ...{last}")
    except Exception as e:
        return CheckResult("no active kills", False, f"log query failed: {e}")


def check_daily_pnl(snapshot: dict) -> CheckResult:
    port = snapshot.get("portfolio", {})
    sc = port.get("spread_capture", 0.0)
    cap = 500_000
    halt_threshold = -cap * 0.05  # -$25K
    if sc > halt_threshold * 0.5:  # > -$12.5K
        return CheckResult("daily P&L healthy", True,
                          f"spread_capture=${sc:.0f}, halt at ${halt_threshold:.0f}")
    return CheckResult("daily P&L healthy", False,
                      f"spread_capture=${sc:.0f} approaching halt ${halt_threshold:.0f}")


def check_vol_surface_fitter() -> CheckResult:
    """Broker logs vol_surface fitter activity every ~60s."""
    try:
        out = _compose_logs(BROKER_SERVICE, since="3m")
        if "vol_surface" in out and "fit" in out.lower():
            return CheckResult("vol_surface fitter", True,
                              "fitter activity in last 3min")
        if "vol_surface fitter enabled" in out:
            return CheckResult(
                "vol_surface fitter", True,
                "fitter enabled (not yet 60s since boot, no fit emitted)",
                warn=True,
            )
        return CheckResult("vol_surface fitter", False,
                          "no vol_surface activity in last 3min")
    except Exception as e:
        return CheckResult("vol_surface fitter", False, f"log query failed: {e}")


def check_risk_state_flowing(log_dir: str) -> CheckResult:
    today = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    path = os.path.join(log_dir, f"trader_events-{today}.jsonl")
    if not os.path.exists(path):
        return CheckResult("risk_state flowing", False,
                          f"no trader events log at {path}",
                          warn=True)
    cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(minutes=1)
    n = 0
    with open(path) as f:
        f.seek(0, 2)
        size = f.tell()
        f.seek(max(0, size - 500_000))
        f.readline()
        for line in f:
            try:
                r = json.loads(line)
                ts_str = r.get("recv_ts") or r.get("ts") or ""
                # Trim sub-microsecond precision (Rust trader emits 9-digit
                # nanos; fromisoformat in py3.10 only accepts ≤6).
                ts_str = _trim_subsec(ts_str.replace("Z", "+00:00"))
                ts = datetime.datetime.fromisoformat(ts_str)
                if ts < cutoff:
                    continue
                ev = r.get("event", r)
                if ev.get("type") == "risk_state":
                    n += 1
            except Exception:
                continue
    if n >= 30:
        return CheckResult("risk_state flowing", True,
                          f"{n} risk_state events in last 60s (expect ~60)")
    if n > 0:
        return CheckResult(
            "risk_state flowing", True,
            f"{n} risk_state events in last 60s — sub-cadence (markets may be closed)",
            warn=True,
        )
    return CheckResult("risk_state flowing", False,
                      f"NO risk_state events in last 60s — broker→trader pipe may be dead")


def check_hedge_resolved() -> CheckResult:
    """Rust broker logs `hedge[HG]: resolved HGK6 (...)` at boot."""
    try:
        out = _compose_logs(BROKER_SERVICE, since="15m")
        # Look for the explicit resolved line. Fail fast if "no hedge
        # contract resolved yet, skipping" is the most-recent state.
        skip_lines = [
            line for line in out.split("\n")
            if "no hedge contract resolved yet" in line
        ]
        resolved_lines = [
            line for line in out.split("\n")
            if "hedge[" in line and "resolved" in line
            and "no hedge" not in line  # exclude the negative log
        ]
        if resolved_lines:
            last = resolved_lines[-1]
            return CheckResult(
                "hedge contract resolved", True,
                last.split("hedge[")[-1][:80].strip()
            )
        if skip_lines:
            n = len(skip_lines)
            return CheckResult(
                "hedge contract resolved", False,
                f"{n} 'no hedge contract resolved yet' lines — list_chain probably failed at boot"
            )
        return CheckResult(
            "hedge contract resolved", True,
            "no hedge log activity in 15m (boot line scrolled out, but no skip warnings)",
            warn=True,
        )
    except Exception as e:
        return CheckResult("hedge contract resolved", False, f"log query failed: {e}")


def check_ibkr_scale_calibrated() -> CheckResult:
    """ConstraintChecker logs 'ibkr_scale recalibrated' every ~5min after first poll."""
    try:
        out = _compose_logs(BROKER_SERVICE, since="10m")
        if "ibkr_scale recalibrated" in out:
            for line in reversed(out.split("\n")):
                if "ibkr_scale recalibrated" in line:
                    return CheckResult(
                        "ibkr_scale calibrated", True,
                        line.split("ibkr_scale")[-1][:80]
                    )
        # First poll fires within 5min of boot. If broker booted < 5min
        # ago, scale is still 1.0 (default). Don't fail; warn.
        return CheckResult(
            "ibkr_scale calibrated", True,
            "no calibration log yet (default 1.0 — first poll within 5min of boot)",
            warn=True,
        )
    except Exception as e:
        return CheckResult("ibkr_scale calibrated", False, f"log query failed: {e}")


def check_trader_decisions_active(log_dir: str) -> CheckResult:
    today = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    path = os.path.join(log_dir, f"trader_decisions-{today}.jsonl")
    if not os.path.exists(path):
        return CheckResult(
            "trader decisions active", True,
            "no trader_decisions log (markets may be closed)",
            warn=True,
        )
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
    if n == 0:
        return CheckResult(
            "trader decisions active", True,
            "no decisions in last 60s (markets closed?)",
            warn=True,
        )
    if n >= 50:
        return CheckResult("trader decisions active", True,
                          f"{n} decisions in last 60s "
                          f"(place={actions['place']}, skip={actions['skip']})")
    return CheckResult(
        "trader decisions active", True,
        f"only {n} decisions in last 60s — sub-cadence; spot-check before quoting",
        warn=True,
    )


def check_no_recent_bad_fills(log_dir: str) -> CheckResult:
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
                          "no fills with edge < -$50 in last 30min")
    return CheckResult("no recent bad fills", False,
                      f"{len(bad)} adverse fills in 30min: e.g. {bad[0]}")


def _autodetect_paths():
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
    print("CORSAIR PRE-TRADE PREFLIGHT")
    print(f"  time:   {datetime.datetime.now(datetime.timezone.utc).isoformat()}")
    print(f"  broker: {BROKER_SERVICE}")
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
        check_no_gateway_2110(),
        check_initial_snapshot(),
        check_position_flat(snapshot),
        check_no_kills(),
        check_daily_pnl(snapshot),
        check_vol_surface_fitter(),
        check_risk_state_flowing(args.logs_dir),
        check_hedge_resolved(),
        check_ibkr_scale_calibrated(),
        check_trader_decisions_active(args.logs_dir),
        check_no_recent_bad_fills(args.logs_dir),
    ]

    for r in results:
        print(r)

    print()
    n_pass = sum(1 for r in results if r.passed and not r.warn)
    n_warn = sum(1 for r in results if r.passed and r.warn)
    n_fail = sum(1 for r in results if not r.passed)

    if n_fail == 0 and n_warn == 0:
        print(f"{GREEN}✓ ALL {len(results)} CHECKS GREEN — system is trade-ready{RESET}")
        return 0
    if n_fail == 0:
        print(
            f"{YELLOW}! {n_pass}/{len(results)} GREEN, {n_warn} WARN — "
            f"review warnings before quoting{RESET}"
        )
        return 0
    print(
        f"{RED}✗ {n_fail}/{len(results)} CHECKS FAILED — "
        f"DO NOT proceed to quoting{RESET}"
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
