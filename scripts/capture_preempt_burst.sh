#!/usr/bin/env bash
# capture_preempt_burst.sh — record scheduler events on the trader's
# hot cpu and post-process to identify what preempts `corsair-hot`.
#
# Diagnoses CLAUDE.md-style p99 stalls (e.g. the 80ms tick→trader_decide
# tail event from 2026-05-07 13:02:57). Uses `perf record` because
# bpftrace isn't installed on the production host; perf IS in
# /usr/bin/perf.
#
# Strategy:
#   1. Run perf record on cpu 8 capturing sched_switch + sched_wakeup
#      + sched_migrate_task + irq_handler_entry + irq_handler_exit.
#   2. Optionally also stamp the trader's monotonic clock at start /
#      end so the operator can correlate with `wire_timing-*.jsonl`
#      tail events that fall within the capture window.
#   3. Post-process with `perf script` to extract preemption events
#      for `corsair-hot`. Emit a summary of which threads preempted
#      it and for how long.
#
# Requires: sudo (perf reads kernel ring buffer + privileged events)
#
# Usage:
#   sudo scripts/capture_preempt_burst.sh [--duration 60] [--cpu 8] [--out /tmp/preempt.data]
#
# Typical session — run during live market hours when the trader is
# placing orders, then stop with Ctrl-C OR let it run for the
# configured duration:
#   sudo scripts/capture_preempt_burst.sh --duration 300 --out /tmp/preempt.data
#
# The post-process step also runs at end and writes a summary to
# /tmp/preempt-summary.txt.

set -euo pipefail

DURATION=60
CPU=8
OUT="/tmp/preempt.data"
SUMMARY="/tmp/preempt-summary.txt"
HOT_THREAD="corsair-hot"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --duration) DURATION="$2"; shift 2;;
        --cpu)      CPU="$2"; shift 2;;
        --out)      OUT="$2"; shift 2;;
        --summary)  SUMMARY="$2"; shift 2;;
        --hot-thread) HOT_THREAD="$2"; shift 2;;
        -h|--help)
            head -33 "$0" | tail -32
            exit 0
            ;;
        *) echo "unknown arg: $1" >&2; exit 2;;
    esac
done

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: must run as root (perf record needs kernel ring buffer access)" >&2
    echo "Try: sudo $0 $@" >&2
    exit 1
fi

if ! command -v perf >/dev/null; then
    echo "ERROR: perf not installed. Try: apt install linux-perf-\$(uname -r | cut -d- -f1-2)" >&2
    exit 1
fi

# Sanity: confirm corsair-hot is currently pinned to $CPU.
PID=$(pgrep -f corsair_trader_rust | head -1 || true)
if [[ -z "$PID" ]]; then
    echo "WARN: no corsair_trader_rust running. Capture will record cpu $CPU activity but you won't have a baseline trader to correlate with."
else
    HOT_TID=$(grep -l "^${HOT_THREAD}$" /proc/$PID/task/*/comm 2>/dev/null \
              | sed -n 's|.*/task/\([0-9]*\)/.*|\1|p' | head -1 || true)
    if [[ -z "$HOT_TID" ]]; then
        echo "WARN: trader is running (PID=$PID) but no thread named '$HOT_THREAD' found."
    else
        HOT_CPUS=$(grep Cpus_allowed_list /proc/$PID/task/$HOT_TID/status | awk '{print $2}')
        echo "  trader PID=$PID, $HOT_THREAD TID=$HOT_TID, allowed cpus=$HOT_CPUS"
        if [[ "$HOT_CPUS" != "$CPU" ]]; then
            echo "  WARN: $HOT_THREAD's allowed cpus is $HOT_CPUS, not $CPU. Capture may not see the right thread."
        fi
    fi
fi

# Record correlation timestamp so the operator can match tail events
# in wire_timing-*.jsonl with the capture window.
T_START_NS=$(date +%s%N)
echo "  T_START_NS=$T_START_NS  ($(date --date=@$((T_START_NS/1000000000)) --iso-8601=seconds))"

echo
echo "Recording $DURATION seconds of scheduler events on cpu $CPU → $OUT"
echo "  Events: sched:sched_switch, sched:sched_wakeup, sched:sched_migrate_task,"
echo "          irq:irq_handler_entry/exit, irq:softirq_entry/exit"
echo "  Press Ctrl-C to stop early."
echo

# `-C $CPU` filters to cpu only. `-a` collects from all cpus to also
# see what woke up on cpu 8 (caller cpu). Use `-C` for tighter scope —
# we explicitly want preempters of cpu 8.
perf record \
    -C "$CPU" \
    -e sched:sched_switch \
    -e sched:sched_wakeup \
    -e sched:sched_migrate_task \
    -e irq:irq_handler_entry \
    -e irq:irq_handler_exit \
    -e irq:softirq_entry \
    -e irq:softirq_exit \
    --output "$OUT" \
    -- sleep "$DURATION" || true

T_END_NS=$(date +%s%N)
echo
echo "  T_END_NS=$T_END_NS"
echo "Capture complete: $OUT ($(stat -c%s "$OUT") bytes)"

# Post-process. perf script emits one line per event. We grep for
# sched_switch lines where prev_comm is corsair-hot AND prev_state is
# 'R' (runnable) or 'R+' (running) — those indicate involuntary
# preemption (the thread didn't go to sleep, it was kicked off).
echo
echo "Post-processing → $SUMMARY"
{
    echo "Capture summary"
    echo "  duration:      ${DURATION}s on cpu $CPU"
    echo "  T_START_NS:    $T_START_NS"
    echo "  T_END_NS:      $T_END_NS"
    echo "  perf data:     $OUT"
    echo "  hot thread:    $HOT_THREAD (TID=${HOT_TID:-unknown})"
    echo
    echo "════════════════════════════════════════════════════════════════════"
    echo "Top threads that preempted $HOT_THREAD:"
    echo "════════════════════════════════════════════════════════════════════"
    perf script -i "$OUT" --no-demangle 2>/dev/null \
        | awk -v hot="$HOT_THREAD" '
            /sched_switch/ {
                # Lines look like:
                #   corsair-hot  4567 [008] 12345.6789: sched:sched_switch: prev_comm=... prev_pid=... prev_state=R ==> next_comm=ksoftirqd/8 next_pid=42
                # We want events where prev_comm is the hot thread and prev_state is R or R+
                # (not S/D, which mean voluntary sleep).
                if (match($0, /prev_comm=[^ ]+ prev_pid=[0-9]+ prev_prio=[0-9]+ prev_state=[A-Za-z+]+/)) {
                    s = substr($0, RSTART, RLENGTH)
                    gsub(/.*prev_comm=/, "", s); split(s, a, " "); prev_comm = a[1]
                    s2 = substr($0, RSTART, RLENGTH)
                    gsub(/.*prev_state=/, "", s2); split(s2, b, " "); prev_state = b[1]
                }
                if (match($0, /next_comm=[^ ]+ next_pid=[0-9]+/)) {
                    s = substr($0, RSTART, RLENGTH)
                    gsub(/.*next_comm=/, "", s); split(s, a, " "); next_comm = a[1]
                }
                # Involuntary preemption: hot thread was Runnable, switched out
                if (prev_comm ~ hot && prev_state ~ /^R/) {
                    preempt[next_comm]++
                }
            }
            END {
                for (c in preempt) printf "%6d   %s\n", preempt[c], c | "sort -rn | head -20"
            }
        '
    echo
    echo "════════════════════════════════════════════════════════════════════"
    echo "Long preemption episodes (>1ms off-cpu for $HOT_THREAD):"
    echo "════════════════════════════════════════════════════════════════════"
    # For each sched_switch where corsair-hot went off-cpu, find the
    # next sched_switch where it comes back on-cpu, and report the
    # delta if > 1ms.
    perf script -i "$OUT" --no-demangle 2>/dev/null \
        | awk -v hot="$HOT_THREAD" '
            /sched_switch/ {
                # Extract timestamp from field 4 (e.g. 12345.6789:)
                ts_raw = $4; gsub(":", "", ts_raw)
                ts_ns = int(ts_raw * 1e9)
                # Extract prev_comm / next_comm
                if (match($0, /prev_comm=[^ ]+/)) {
                    s = substr($0, RSTART, RLENGTH); gsub(/.*=/, "", s); prev_comm = s
                }
                if (match($0, /next_comm=[^ ]+/)) {
                    s = substr($0, RSTART, RLENGTH); gsub(/.*=/, "", s); next_comm = s
                }
                if (match($0, /prev_state=[A-Za-z+]+/)) {
                    s = substr($0, RSTART, RLENGTH); gsub(/.*=/, "", s); prev_state = s
                }
                if (prev_comm ~ hot && prev_state ~ /^R/) {
                    off_ts = ts_ns; off_to = next_comm
                }
                if (next_comm ~ hot && off_ts > 0) {
                    dur = ts_ns - off_ts
                    if (dur > 1000000) {
                        printf "%9.3f ms off-cpu, replaced by %s\n", dur/1e6, off_to
                    }
                    off_ts = 0
                }
            }
        ' | sort -rn | head -20
    echo
    echo "════════════════════════════════════════════════════════════════════"
    echo "IRQ counts on cpu $CPU during capture window:"
    echo "════════════════════════════════════════════════════════════════════"
    perf script -i "$OUT" --no-demangle 2>/dev/null \
        | awk '/irq_handler_entry/ {
            if (match($0, /name=[^ ]+/)) {
                s = substr($0, RSTART, RLENGTH); gsub(/.*=/, "", s); irq_count[s]++
            }
        }
        END { for (c in irq_count) printf "%6d   %s\n", irq_count[c], c }' \
        | sort -rn | head -20
    echo
    echo "════════════════════════════════════════════════════════════════════"
    echo "Correlate with wire_timing tail events:"
    echo "════════════════════════════════════════════════════════════════════"
    DAY=$(date --date=@$((T_START_NS/1000000000)) +%Y-%m-%d)
    LOG="/home/ethereal/corsair/logs-paper/wire_timing-${DAY}.jsonl"
    if [[ -f "$LOG" ]]; then
        echo "  Log: $LOG"
        python3 -c "
import json, sys
t0 = $T_START_NS
t1 = $T_END_NS
tail_events = []
for line in open('$LOG'):
    try:
        d = json.loads(line)
        if d.get('outcome') != 'ack': continue
        td = d.get('trader_decide_ts_ns'); tt = d.get('triggering_tick_broker_recv_ns')
        if not td or not tt: continue
        if t0 <= td <= t1:
            lat = (td - tt) / 1000.0
            if lat > 1000:
                tail_events.append((td, lat, d.get('order_id'), d.get('side'), d.get('strike')))
    except: pass
tail_events.sort()
print(f'  Tail events (>1ms) inside capture window: {len(tail_events)}')
for td, lat, oid, side, strike in tail_events:
    import datetime
    dt = datetime.datetime.fromtimestamp(td/1e9)
    side_s = side if side is not None else '?'
    strike_s = f'{strike:.3f}' if strike is not None else '?'
    print(f'    {dt.strftime(\"%H:%M:%S.%f\")[:-3]}  oid={oid}  {side_s} K={strike_s}  tick→decide={lat:.0f}us')
"
    else
        echo "  (no wire_timing-${DAY}.jsonl found)"
    fi
} | tee "$SUMMARY"

echo
echo "Done. Summary saved to $SUMMARY"
echo
echo "For deep dive: perf script -i $OUT | less"
echo "(filter to a specific time window with: perf script -i $OUT --time 12345.6:12346.0)"
