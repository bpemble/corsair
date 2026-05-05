#!/usr/bin/env bash
# run_ab_test.sh — run a candidate harness pass and compare to a baseline.
#
# Wraps run_latency_harness.sh with a candidate-run + compare_latency.py
# verdict so each per-item A/B is one command:
#
#   scripts/run_ab_test.sh <name> <baseline.json> [--env KEY=VAL ...] [--duration N]
#
# Saves candidate dump alongside the baseline as <baseline_dir>/<name>.json
# and prints the verdict for both ttt_us and ipc_us metrics.
#
# Example (item 1: mimalloc tail tuning):
#   scripts/run_ab_test.sh mimalloc /tmp/corsair_baseline/baseline_1.json \
#       --env MIMALLOC_PURGE_DELAY=10000 \
#       --env MIMALLOC_RESERVE_HUGE_OS_PAGES=1
set -euo pipefail

NAME=""
BASELINE=""
DURATION=300
HARNESS_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --env)      HARNESS_ARGS+=("--env" "$2"); shift 2;;
        --duration) DURATION="$2"; shift 2;;
        --rate)     HARNESS_ARGS+=("--rate" "$2"); shift 2;;
        --cpuset)   HARNESS_ARGS+=("--cpuset" "$2"); shift 2;;
        --image)    HARNESS_ARGS+=("--image" "$2"); shift 2;;
        --recording) HARNESS_ARGS+=("--recording" "$2"); shift 2;;
        --help|-h)
            sed -n '2,15p' "$0"
            exit 0;;
        -*) echo "unknown flag: $1" >&2; exit 1;;
        *)
            if [[ -z "$NAME" ]]; then NAME="$1"
            elif [[ -z "$BASELINE" ]]; then BASELINE="$1"
            else echo "extra positional: $1" >&2; exit 1
            fi
            shift;;
    esac
done

if [[ -z "$NAME" || -z "$BASELINE" ]]; then
    echo "usage: $0 <name> <baseline.json> [--env KEY=VAL ...] [--duration N]" >&2
    exit 1
fi

if [[ ! -f "$BASELINE" ]]; then
    echo "baseline file missing: $BASELINE" >&2
    exit 1
fi

BASE_DIR=$(dirname "$BASELINE")
OUT="$BASE_DIR/$NAME.json"

echo "ab: candidate '$NAME' for ${DURATION}s"
"$(dirname "$0")/run_latency_harness.sh" "$OUT" \
    --duration "$DURATION" \
    "${HARNESS_ARGS[@]}"

echo
echo "=== TTT (ttt_us) ==="
python3 "$(dirname "$0")/compare_latency.py" "$BASELINE" "$OUT" \
    --metric ttt_us \
    --label-before baseline \
    --label-after "$NAME" || true

echo
echo "=== IPC (ipc_us) ==="
python3 "$(dirname "$0")/compare_latency.py" "$BASELINE" "$OUT" \
    --metric ipc_us \
    --label-before baseline \
    --label-after "$NAME" || true
