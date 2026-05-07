#!/usr/bin/env bash
# run_latency_harness.sh — drive the trader against a fixed JSONL recording
# and capture an in-memory histogram dump for A/B comparison.
#
# Spins up two short-lived containers on an isolated SHM-base path
# (production /app/data/corsair_ipc is untouched):
#   1. corsair_tick_replay — fakes the broker, publishes ticks at the
#      recording's original timing (or --rate if given).
#   2. corsair_trader_rust — connects, busy-polls, dumps histograms
#      to a shared volume on SIGTERM.
#
# Output: a JSON file with {"ipc_ns": [...], "ttt_ns": [...]} that
# scripts/compare_latency.py can ingest. Samples are nanoseconds.
#
# IMPORTANT: by default the harness pins both containers to cpuset
# 4,5 (free P-cores between gateway and broker on i9-13900K). The
# production trader on cpuset 8,9,10,11 is undisturbed. For tightest
# production-equivalence (matching frequency + L1/L2 layout), pass
# `--cpuset 8,9,10,11` AND stop the production trader first
# (`docker compose stop trader`) — markets-closed only.
#
# Usage:
#   scripts/run_latency_harness.sh <out.json> \
#       [--duration 180] [--rate <hz>] \
#       [--recording logs-paper/trader_events-2026-05-05.jsonl] \
#       [--cpuset 4,5] [--image corsair-corsair:latest]
set -euo pipefail

OUT=""
DURATION=180
RATE=""
RECORDING="logs-paper/trader_events-2026-05-05.jsonl"
CPUSET="4,5"
IMAGE="corsair-corsair:latest"
EXTRA_ENVS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --duration) DURATION="$2"; shift 2;;
        --rate)     RATE="$2"; shift 2;;
        --recording) RECORDING="$2"; shift 2;;
        --cpuset)   CPUSET="$2"; shift 2;;
        --image)    IMAGE="$2"; shift 2;;
        --env)      EXTRA_ENVS+=("-e" "$2"); shift 2;;
        --help|-h)
            sed -n '2,30p' "$0"
            exit 0;;
        -*) echo "unknown flag: $1" >&2; exit 1;;
        *)
            if [[ -z "$OUT" ]]; then
                OUT="$1"
            else
                echo "extra positional arg: $1" >&2; exit 1
            fi
            shift;;
    esac
done

if [[ -z "$OUT" ]]; then
    echo "usage: $0 <out.json> [--duration N] [--rate HZ] [--recording PATH] [--cpuset LIST] [--image IMG]" >&2
    exit 1
fi

if [[ ! -f "$RECORDING" ]]; then
    echo "recording not found: $RECORDING" >&2
    exit 1
fi

# Workspace and unique container names so concurrent invocations don't collide.
WORK=$(mktemp -d /tmp/corsair_harness.XXXXXX)
TAG=$(basename "$WORK" | tr 'A-Z' 'a-z')
REPLAY_NAME="corsair-replay-$TAG"
TRADER_NAME="corsair-trader-harness-$TAG"
DUMP_PATH="$WORK/hist.json"

cleanup() {
    docker rm -f "$REPLAY_NAME" "$TRADER_NAME" 2>/dev/null >/dev/null || true
}
trap 'cleanup; rm -rf "$WORK" 2>/dev/null || true' EXIT

# Recording must be readable by docker; bind-mount the absolute path.
ABS_REC=$(readlink -f "$RECORDING")
ABS_OUT=$(readlink -f "$(dirname "$OUT")")/$(basename "$OUT")

echo "harness: recording=$RECORDING  duration=${DURATION}s  cpuset=$CPUSET"
echo "harness: workspace=$WORK"

RATE_FLAG=()
if [[ -n "$RATE" ]]; then
    RATE_FLAG=(--rate "$RATE")
fi

# 1. Start the replay container in background. Loops the recording so
#    the trader doesn't run out of input even on long durations.
# Note: NOT using --rm so we can `docker logs` after exit. Cleanup
# trap removes the container.
docker run -d \
    --name "$REPLAY_NAME" \
    --cpuset-cpus "$CPUSET" \
    --network none \
    -v "$ABS_REC:/recording.jsonl:ro" \
    -v "$WORK:/work" \
    --entrypoint /usr/local/bin/corsair_tick_replay \
    "$IMAGE" \
    /recording.jsonl --ipc-base /work/ipc --loop "${RATE_FLAG[@]}" \
    >/dev/null

# Wait for SHM rings to materialize.
for i in $(seq 1 50); do
    if [[ -f "$WORK/ipc.events" && -f "$WORK/ipc.commands" ]]; then
        break
    fi
    sleep 0.1
done
if [[ ! -f "$WORK/ipc.events" ]]; then
    echo "harness: replay failed to create rings" >&2
    docker logs "$REPLAY_NAME" >&2 || true
    exit 2
fi
echo "harness: rings ready"

# 2. Start the trader container. Same cpuset so it competes with
#    replay (mirrors the production layout where broker + trader
#    share adjacent P-cores). Histogram caps bumped to capture
#    the entire run; dump path set so SIGTERM produces output.
docker run -d \
    --name "$TRADER_NAME" \
    --cpuset-cpus "$CPUSET" \
    --network none \
    --ulimit memlock=-1 \
    -v "$WORK:/work" \
    -e CORSAIR_IPC_BASE=/work/ipc \
    -e CORSAIR_LOGS_DIR=/work/logs \
    -e CORSAIR_TRADER_BUSY_POLL=1 \
    -e CORSAIR_TRADER_HIST_DUMP_PATH=/work/hist.json \
    -e CORSAIR_TRADER_HIST_TTT_CAP=200000 \
    -e CORSAIR_TRADER_HIST_IPC_CAP=200000 \
    -e CORSAIR_TRADER_PLACES_ORDERS=1 \
    -e RUST_LOG=info \
    "${EXTRA_ENVS[@]}" \
    --entrypoint /usr/local/bin/corsair_trader_rust \
    "$IMAGE" \
    >/dev/null

echo "harness: trader running for ${DURATION}s"
sleep "$DURATION"

# 3. SIGTERM the trader; the graceful_shutdown handler dumps
#    histograms to /work/hist.json before exit.
echo "harness: sending SIGTERM (triggers histogram dump)"
docker kill --signal=TERM "$TRADER_NAME" >/dev/null

# Wait up to 10s for shutdown to complete.
for i in $(seq 1 20); do
    if [[ ! -f "$WORK/hist.json" ]]; then
        sleep 0.5
        continue
    fi
    if ! docker ps --format '{{.Names}}' | grep -q "^${TRADER_NAME}$"; then
        break
    fi
    sleep 0.5
done

if [[ ! -f "$DUMP_PATH" ]]; then
    echo "harness: dump file missing after SIGTERM" >&2
    docker logs "$TRADER_NAME" 2>&1 | tail -40 >&2 || true
    exit 3
fi

mkdir -p "$(dirname "$ABS_OUT")"
cp "$DUMP_PATH" "$ABS_OUT"
# Snapshot trader log alongside the dump so the operator can inspect
# decisions counters / kill events / boot timing if a verdict is
# unexpected. Lives next to the histogram with the same prefix.
TRADER_LOG="${ABS_OUT%.json}.trader.log"
docker logs "$TRADER_NAME" > "$TRADER_LOG" 2>&1 || true
REPLAY_LOG="${ABS_OUT%.json}.replay.log"
docker logs "$REPLAY_NAME" > "$REPLAY_LOG" 2>&1 || true
N_TTT=$(python3 -c "import json; print(len(json.load(open('$ABS_OUT')).get('ttt_ns',[])))")
N_IPC=$(python3 -c "import json; print(len(json.load(open('$ABS_OUT')).get('ipc_ns',[])))")

echo
echo "harness: histograms saved to $OUT"
echo "  ttt_ns samples: $N_TTT"
echo "  ipc_ns samples: $N_IPC"
