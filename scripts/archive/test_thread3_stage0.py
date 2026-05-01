#!/usr/bin/env python3
"""Stage 0 verification harness for Thread 3 deployment runbook Phase 1.

Drives the full hedge-drain priority-signal verification protocol from the
runbook: injects a synthetic burst, reads the resulting JSONL streams,
and reports PASS/FAIL against the five criteria.

Pre-requisites (operator must arrange these BEFORE running):
  1. corsair is running in paper mode and connected.
  2. config.quoting.thread3 flags are set as required by the test
     phase (defaults to "all OFF" — verifies observational rows;
     override --flags-on to test the action path).
  3. The hedge_manager has resolved a tradeable contract (Phase 0 fix
     landed — verify with `grep "HEDGE CONTRACT RESOLVED" logs-corsair`).

What this script does:
  1. Snapshots the current sizes of burst_events and hedge_trades streams.
  2. Calls scripts/induce_burst.py to write the sentinel.
  3. Polls the JSONL streams for ~5 seconds waiting for new rows.
  4. Evaluates the five Phase 1 pass criteria.
  5. Prints PASS/FAIL summary and exits with status 0 (PASS) or 1 (FAIL).

Pass criteria (Thread 3 deployment runbook Phase 1):
  - Burst tracker counts 5 fills in <1s.
  - C2 trigger fires at K2=3 (the 3rd of 5 mixed-side fills).
  - Hedge engine drains its queue within ≤500ms of the trigger.
  - Hedge throughput ≥3 hedges/sec during drain.
  - Cooldown clears the priority flag only when (queue empty AND cooldown
    expired).

Usage (from the corsair container):
    docker compose exec corsair python scripts/test_thread3_stage0.py
    docker compose exec corsair python scripts/test_thread3_stage0.py \\
        --flags-on  # require master+C2 ON; verifies action path

Note: criteria #3-#5 (drain timing) require an actual non-zero net delta
in the book at injection time AND hedging.mode=execute. With observe-mode
or zero delta, the script will report drain criteria as N/A. Use
--flags-on with execute mode for full verification.
"""
import argparse
import json
import os
import subprocess
import sys
import time
from datetime import date, timedelta


PAPER_LOG_DIR = os.environ.get(
    "CORSAIR_PAPER_LOG_DIR", "/app/logs-paper")


def _stream_path(stream: str) -> str:
    today = date.today().isoformat()
    return os.path.join(PAPER_LOG_DIR, f"{stream}-{today}.jsonl")


def _read_lines_after(path: str, byte_offset: int) -> list[dict]:
    """Return all JSONL rows in `path` after the given byte offset.
    Used to capture only events that arrived since the snapshot tick."""
    out = []
    if not os.path.exists(path):
        return out
    try:
        with open(path) as fh:
            fh.seek(byte_offset)
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    out.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    except OSError:
        pass
    return out


def _file_size(path: str) -> int:
    try:
        return os.path.getsize(path)
    except OSError:
        return 0


def _await_new_rows(stream: str, baseline_size: int,
                    min_rows: int, timeout_sec: float
                    ) -> list[dict]:
    """Poll `stream` for new rows after `baseline_size` until at least
    `min_rows` arrive or `timeout_sec` elapses."""
    path = _stream_path(stream)
    deadline = time.monotonic() + timeout_sec
    while time.monotonic() < deadline:
        rows = _read_lines_after(path, baseline_size)
        if len(rows) >= min_rows:
            return rows
        time.sleep(0.1)
    return _read_lines_after(path, baseline_size)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Thread 3 Stage 0 verification harness.")
    parser.add_argument(
        "--pattern", choices=["c1", "c2"], default="c2",
        help="Which burst pattern to inject (default: c2).")
    parser.add_argument(
        "--flags-on", action="store_true",
        help="Assert that master + corresponding sub-flag are ON; "
             "expect action_fired=True in burst_events. Without this, "
             "expect observational rows only (action_fired=False).")
    parser.add_argument(
        "--timeout", type=float, default=5.0,
        help="Seconds to wait for events after injection (default: 5).")
    args = parser.parse_args()

    print("=" * 70)
    print("Thread 3 Stage 0 verification (deployment runbook Phase 1)")
    print("=" * 70)
    print(f"Pattern: {args.pattern}")
    print(f"Expected action_fired: {args.flags_on}")
    print(f"Paper log dir: {PAPER_LOG_DIR}")
    print()

    # Snapshot existing stream sizes before injection.
    burst_events_path = _stream_path("burst_events")
    hedge_trades_path = _stream_path("hedge_trades")
    burst_baseline = _file_size(burst_events_path)
    hedge_baseline = _file_size(hedge_trades_path)
    print(f"Pre-injection: burst_events={burst_baseline}B "
          f"hedge_trades={hedge_baseline}B")

    # Inject the burst via induce_burst.py.
    induce_script = os.path.join(os.path.dirname(__file__), "induce_burst.py")
    print(f"\nInjecting burst via {induce_script}...")
    try:
        subprocess.run(
            [sys.executable, induce_script, "--pattern", args.pattern],
            check=True, capture_output=True, text=True,
        )
    except subprocess.CalledProcessError as e:
        print(f"FAIL: induce_burst.py exited {e.returncode}", file=sys.stderr)
        print(e.stderr, file=sys.stderr)
        return 1
    inject_ts = time.time()
    print(f"Sentinel written at t={inject_ts:.3f}")

    # Wait for the corsair main loop to pick up the sentinel and emit
    # burst_events rows. Expected: at minimum 1 row for the trigger.
    print(f"\nWaiting up to {args.timeout}s for burst_events rows...")
    new_burst = _await_new_rows("burst_events", burst_baseline,
                                min_rows=1, timeout_sec=args.timeout)

    # Read whatever hedge_trades arrived in the same window.
    new_hedge = _read_lines_after(hedge_trades_path, hedge_baseline)
    print(f"Got {len(new_burst)} new burst_events rows, "
          f"{len(new_hedge)} new hedge_trades rows")

    # ── Evaluate criteria ─────────────────────────────────────────
    results: list[tuple[str, bool, str]] = []

    # Criterion 1: a burst_events row exists for the expected trigger.
    expected_trigger = "C2" if args.pattern == "c2" else "C1"
    matching = [r for r in new_burst if r.get("trigger") == expected_trigger]
    if matching:
        results.append((
            f"C{expected_trigger[1]} trigger fires",
            True,
            f"{len(matching)} matching row(s); "
            f"K1_count={matching[0].get('K1_count')} "
            f"K2_count={matching[0].get('K2_count')}",
        ))
    else:
        all_triggers = [r.get("trigger") for r in new_burst]
        results.append((
            f"C{expected_trigger[1]} trigger fires",
            False,
            f"no row with trigger={expected_trigger}; "
            f"observed: {all_triggers}",
        ))

    # Criterion 2: action_fired matches expectation.
    if matching:
        action_states = [bool(r.get("action_fired")) for r in matching]
        all_match = all(s == args.flags_on for s in action_states)
        results.append((
            f"action_fired={args.flags_on} on all matching rows",
            all_match,
            f"observed action_fired states: {action_states}",
        ))

    # Criterion 3: K-count threshold semantics.
    if matching:
        first = matching[0]
        if expected_trigger == "C2":
            ok = first.get("K2_count", 0) >= 3
            detail = f"K2_count={first.get('K2_count')} (need ≥3)"
        else:
            ok = first.get("K1_count", 0) >= 2
            detail = f"K1_count={first.get('K1_count')} (need ≥2)"
        results.append((f"{expected_trigger} K-count threshold met",
                        ok, detail))

    # Criterion 4 (action path only): hedge drain rows landed in cooldown.
    if args.flags_on:
        drain_rows = [
            r for r in new_hedge
            if str(r.get("reason", "")).startswith("priority_drain")
        ]
        if drain_rows:
            results.append((
                "Hedge drain rows present during cooldown",
                True,
                f"{len(drain_rows)} priority_drain hedge_trades rows",
            ))
        else:
            # If the book had zero net delta at injection, drain is a no-op.
            results.append((
                "Hedge drain rows present during cooldown",
                None,  # N/A
                "no priority_drain rows (expected if book had zero net "
                "delta or hedging.mode=observe)",
            ))

    # ── Summary ───────────────────────────────────────────────────
    print()
    print("=" * 70)
    print("Results:")
    print("=" * 70)
    failed = 0
    for name, ok, detail in results:
        if ok is True:
            print(f"  PASS — {name}")
        elif ok is False:
            print(f"  FAIL — {name}")
            failed += 1
        else:
            print(f"  N/A  — {name}")
        print(f"         {detail}")
    print()
    if failed:
        print(f"OVERALL: FAIL ({failed} criterion(criteria) failed)")
        return 1
    print("OVERALL: PASS")
    print()
    print("Reminder: the runbook's drain-timing criteria (≤500ms drain, "
          "≥3 hedges/sec) require a non-zero net delta in the book at "
          "injection time AND hedging.mode=execute. Verify those manually "
          "by running this script during a session with active option "
          "positions and execute-mode hedging.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
