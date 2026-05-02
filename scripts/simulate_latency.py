"""End-to-end latency simulation: estimate the broker hot-loop cost.

Post Phase 6.7 cutover the entire hot path is Rust. This script
microbenchmarks each component via the corsair_pricing wheel and
extrapolates a back-of-the-envelope TTT (tick-to-trade) figure.

It is NOT a substitute for live measurement — for that, the trader's
10s telemetry log line carries the real ttt_p50 / ttt_p99 from
production traffic. Use this script when:
  - markets are closed and you want a sanity-check estimate
  - you've changed pricing internals and want to verify no regression
  - debugging "why is my hot path slow"

Run inside the corsair container:
    docker run --rm -v $PWD:/work -w /work corsair-corsair \\
        python3 scripts/simulate_latency.py
"""
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

import corsair_pricing as _rs

# Verify the wheel exposes everything we need. If any of these are
# missing the corsair-corsair build is stale.
REQUIRED = ("calibrate_sabr", "compute_greeks", "span_portfolio_margin")
missing = [name for name in REQUIRED if not hasattr(_rs, name)]
if missing:
    raise RuntimeError(
        f"corsair_pricing wheel missing: {missing} — rebuild the corsair image"
    )


def measure(fn, n: int) -> float:
    """Median per-call time in microseconds across 5 batches of n calls."""
    samples = []
    for _ in range(5):
        t0 = time.perf_counter_ns()
        for _ in range(n):
            fn()
        t1 = time.perf_counter_ns()
        samples.append((t1 - t0) / n / 1_000)
    samples.sort()
    return samples[len(samples) // 2]


def hr(s):
    print()
    print(s)
    print("─" * 72)


def main():
    print(f"corsair_pricing: {[m for m in REQUIRED]}")

    hr("Math hot-path microbenchmarks (median, µs/call)")

    rs_greek = measure(
        lambda: _rs.compute_greeks(6.0, 6.0, 0.05, 0.45, r=0.0, right="C", multiplier=25_000),
        n=500,
    )

    positions = [
        (6.00, "C", 0.05, 0.45, -1),
        (6.05, "C", 0.05, 0.42, -1),
        (6.10, "C", 0.05, 0.40, -1),
        (5.95, "P", 0.05, 0.50, -1),
        (5.90, "P", 0.05, 0.55, -1),
        (5.85, "P", 0.05, 0.60, -1),
    ]
    span_cfg = (0.56, 0.49, 0.25, 3.0, 0.33, 500.0, 25_000.0)
    rs_span = measure(
        lambda: _rs.span_portfolio_margin(6.0, positions, *span_cfg),
        n=200,
    )

    F, T, beta = 6.0, 0.05, 0.5
    Ks = [F + i * 0.05 for i in range(-4, 5)]
    # Use the wheel's SABR vol fn to seed test IVs (matches production).
    ivs = [_rs.sabr_implied_vol(F, K, T, 0.6, beta, -0.3, 1.0) for K in Ks]
    rs_cal = measure(
        lambda: _rs.calibrate_sabr(F, T, Ks, ivs, beta, 0.05, None),
        n=20,
    )

    print(f"{'Component':<40} {'Rust':>14}")
    print(f"{'─' * 56}")
    print(f"{'compute_greeks (1 contract)':<40} {rs_greek:>11.2f} µs")
    print(f"{'SPAN portfolio (6-pos book)':<40} {rs_span:>11.2f} µs")
    print(f"{'SABR fit (8 strikes)':<40} {rs_cal:>11.2f} µs")

    hr("refresh_greeks (6-position portfolio): per call")
    rs_refresh = rs_greek * 6
    print(f"  {rs_refresh:.0f} µs  (6 × {rs_greek:.0f} µs)")

    hr("Constraint check on fill: per call")
    rs_check = rs_span * 2
    print(f"  {rs_check:.0f} µs  (2 × SPAN at {rs_span:.0f} µs — current + post-fill)")

    hr("SABR refit cycle (4 calibrations × 2 sides):")
    n_cals = 4
    rs_refit = rs_cal * n_cals
    print(f"  {rs_refit/1000:.2f} ms  ({n_cals} × {rs_cal:.0f} µs)")
    print(f"  Refit cadence (60s default) leaves {60_000_000 / rs_refit:.0f}× headroom.")

    hr("Per-cycle math budget (broker quote refresh):")
    n_strikes = 10
    n_sides = 2
    rs_cycle = rs_refresh + n_strikes * n_sides * rs_check
    print(
        f"  {rs_cycle/1000:.2f} ms / cycle  "
        f"({n_strikes*n_sides} fill checks + 1 refresh_greeks)"
    )
    print(
        f"  At 4 cycles/sec (250ms snapshot cadence): "
        f"{rs_cycle*4/1000:.2f} ms/sec on math = "
        f"{rs_cycle*4/10_000:.2f}% of one core"
    )

    hr("Estimated TTT (tick-to-trade) — broker→trader→broker→wire:")
    # Two scenarios: default FIFO-notify mode and busy-poll mode
    # (CORSAIR_TRADER_BUSY_POLL=1, opt-in, +1 CPU core).
    decide_quote = 1   # corsair_trader::decide_quote, sub-µs
    risk_gate = 1      # 6-layer trader gate stack
    wire_encode = 5    # template-cached place_order, was ~20µs before
    tcp_loopback = 100 # gateway is on localhost
    parse = 1          # measured 193 ns/call via examples/parse_bench
                       # (was estimated at 50 µs — audit was 50x off)

    ipc_fifo = 80      # SHM ring + FIFO notify wakeup
    ipc_busypoll = 15  # SHM ring busy-poll, no FIFO

    ttt_fifo = parse + ipc_fifo + decide_quote + risk_gate + ipc_fifo + wire_encode + tcp_loopback
    ttt_busy = parse + ipc_busypoll + decide_quote + risk_gate + ipc_busypoll + wire_encode + tcp_loopback

    print("  Default mode (FIFO-notify, 0 hot CPUs):")
    print(f"    Tick → broker dispatch (zero-copy): ~{parse} µs")
    print(f"    Broker → trader IPC (FIFO):         ~{ipc_fifo} µs")
    print(f"    Trader decide + gates:              ~{decide_quote + risk_gate} µs")
    print(f"    Trader → broker IPC (FIFO):         ~{ipc_fifo} µs")
    print(f"    Broker place_order encode (cached): ~{wire_encode} µs")
    print(f"    TCP write to gateway:               ~{tcp_loopback} µs")
    print(f"    Estimated TTT p50:                  ~{ttt_fifo} µs")
    print()
    print("  Busy-poll mode (CORSAIR_TRADER_BUSY_POLL=1, 1 CPU hot):")
    print(f"    SHM busy-poll IPC ×2:               ~{2 * ipc_busypoll} µs")
    print(f"    Estimated TTT p50:                  ~{ttt_busy} µs")
    print()
    print(f"  (For live numbers, watch trader telemetry: ttt_p50 / ttt_p99)")

    hr("Sustained quote-rate headroom:")
    rs_per_quote = rs_check + decide_quote + ipc_busypoll + wire_encode
    print(
        f"  {rs_per_quote:.0f} µs/quote → max "
        f"~{1_000_000/rs_per_quote:.0f} quotes/sec on a single trader thread"
    )

    print()
    print("=" * 72)
    print("Summary (post Phase 6.13 / Rust hot path + latency optimizations)")
    print("=" * 72)
    print(f"  Per-cycle math:    {rs_cycle/1000:.2f} ms")
    print(f"  Refit cycle:       {rs_refit/1000:.2f} ms")
    print(f"  Per-fill check:    {rs_check:.0f} µs")
    print(f"  Refresh greeks:    {rs_refresh:.0f} µs")
    print(f"  Estimated TTT (FIFO mode):       ~{ttt_fifo} µs")
    print(f"  Estimated TTT (busy-poll):       ~{ttt_busy} µs")
    print()


if __name__ == "__main__":
    main()
