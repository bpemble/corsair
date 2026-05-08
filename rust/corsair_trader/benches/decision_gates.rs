//! Microbench for the §25 trader-centric kill controller's per-tick
//! gates: `compute_risk_gates` and `improving_passes`. Settles whether
//! §25's hot-path additions are below the §21 noise floor (45 ns p50,
//! ±15 ns warm-spread on the production replay harness).
//!
//! These are NOT end-to-end measurements. The production harness
//! (`scripts/run_latency_harness.sh`) measures TTT including IPC and
//! tokio scheduling; this bench isolates pure gate-arithmetic cost.
//! Use criterion's TSC sampling because gate cost is in the
//! single-digit ns range and `Instant::now()` itself costs ~30 ns.
//!
//! Run with:
//!   cargo bench --bench decision_gates --manifest-path rust/corsair_trader/Cargo.toml
//!
//! For an A/B between branches: `criterion` writes baseline data to
//! `target/criterion/`. Run on the baseline branch with `--save-baseline pre`,
//! switch branches, then `--baseline pre` to compare.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use corsair_trader::decision::{compute_risk_gates, improving_passes, RiskGates, Side};
use corsair_trader::state::{DecisionCounters, ScalarSnapshot, TheoGreeks};

/// Plausible production ScalarSnapshot. Values mirror what the trader
/// sees mid-session under normal load — risk_state arrived recently
/// (age=10ms), portfolio greeks within bounds, no breaches.
fn snap_clean(now_ns: u64) -> ScalarSnapshot {
    ScalarSnapshot {
        underlying_price: 6.00,
        risk_effective_delta: Some(0.5),
        risk_margin_pct: Some(0.30),
        risk_theta: Some(-100.0),
        risk_vega: Some(-50.0),
        risk_state_age_monotonic_ns: now_ns.saturating_sub(10_000_000), // 10ms ago
        min_edge_ticks: 2,
        tick_size: 0.0005,
        delta_ceiling: 3.0,
        theta_kill: -500.0,
        vega_kill: 0.0,
        margin_ceiling_pct: 0.50,
        weekend_paused: false,
        gtd_lifetime_s: 30.0,
        gtd_refresh_lead_s: 3.5,
        dead_band_ticks: 1,
        skip_if_spread_over_edge_mul: 6.0,
    }
}

/// ScalarSnapshot in theta breach. theta_kill = -500, theta now -650.
/// Mirrors the live-verification state from §25 deploy (risk_block_buy
/// counter spinning, SELLs allowed).
fn snap_theta_breach(now_ns: u64) -> ScalarSnapshot {
    let mut s = snap_clean(now_ns);
    s.risk_theta = Some(-650.0);
    s
}

/// ScalarSnapshot in delta-high breach (effective_delta > +ceiling).
/// Mirrors a put-overhang scenario.
fn snap_delta_high(now_ns: u64) -> ScalarSnapshot {
    let mut s = snap_clean(now_ns);
    s.risk_effective_delta = Some(3.5);
    s
}

/// ScalarSnapshot at margin breach — block_all path.
fn snap_margin_breach(now_ns: u64) -> ScalarSnapshot {
    let mut s = snap_clean(now_ns);
    s.risk_margin_pct = Some(0.55); // > 0.50 ceiling
    s
}

/// Synthetic per-strike greeks at multiplier=1.0. Matches OTM call
/// parameters in production (strike 6.05 vs spot 6.00).
fn greeks_call() -> TheoGreeks {
    TheoGreeks {
        theo: 0.045,
        delta: 0.40,
        theta: -8.0,
        vega: 4.0,
    }
}

fn greeks_put() -> TheoGreeks {
    TheoGreeks {
        theo: 0.035,
        delta: -0.35,
        theta: -7.0,
        vega: 4.0,
    }
}

// ────────────────────────────────────────────────────────────────────
// Bench 1: compute_risk_gates per breach state
// ────────────────────────────────────────────────────────────────────
//
// Per-tick once. Reads the snapshot, computes age, checks every gate.
// Cost should be ~5-10 ns. The four breach states (clean / theta /
// delta / margin) exercise different early-exit paths.

fn bench_compute_risk_gates(c: &mut Criterion) {
    let now_ns: u64 = 1_000_000_000_000;
    let counters = DecisionCounters::default();
    let mut group = c.benchmark_group("compute_risk_gates");

    for (name, snap) in [
        ("clean", snap_clean(now_ns)),
        ("theta_breach", snap_theta_breach(now_ns)),
        ("delta_high", snap_delta_high(now_ns)),
        ("margin_breach", snap_margin_breach(now_ns)),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(name), &snap, |b, snap| {
            b.iter(|| {
                let g = compute_risk_gates(
                    black_box(snap),
                    black_box(now_ns),
                    black_box(&counters),
                );
                black_box(g);
            });
        });
    }
    group.finish();
}

// ────────────────────────────────────────────────────────────────────
// Bench 2: improving_passes per (right, side, breach) combination
// ────────────────────────────────────────────────────────────────────
//
// Per-side ×2 per tick (BUY + SELL) when reached. Should be 1-3 ns
// each — 1 mul + 1-2 cmps + branch.

fn bench_improving_passes(c: &mut Criterion) {
    let mut group = c.benchmark_group("improving_passes");

    let no_breach = RiskGates::default();
    let theta_b = RiskGates {
        theta_breach: true,
        ..RiskGates::default()
    };
    let delta_hi = RiskGates {
        delta_breach_dir: Some(1),
        ..RiskGates::default()
    };

    let g_call = greeks_call();
    let g_put = greeks_put();

    for (name, gates, right, side, gx) in [
        ("no_breach_C_BUY", no_breach, 'C', Side::Buy, g_call),
        ("theta_C_BUY_blocked", theta_b, 'C', Side::Buy, g_call),
        ("theta_C_SELL_passes", theta_b, 'C', Side::Sell, g_call),
        ("delta_hi_P_BUY_passes", delta_hi, 'P', Side::Buy, g_put),
        ("delta_hi_P_SELL_blocked", delta_hi, 'P', Side::Sell, g_put),
    ] {
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(gates, right, side, gx),
            |b, &(gates, right, side, gx)| {
                b.iter(|| {
                    let r = improving_passes(
                        black_box(&gates),
                        black_box(right),
                        black_box(side),
                        black_box(gx),
                    );
                    black_box(r);
                });
            },
        );
    }
    group.finish();
}

// ────────────────────────────────────────────────────────────────────
// Bench 3: combined per-tick gate cost
// ────────────────────────────────────────────────────────────────────
//
// What `decide_on_tick` actually pays per-tick when not block_all'd:
// one compute_risk_gates + two improving_passes (BUY+SELL). This is
// the §25 hot-path bill in the steady state.

fn bench_combined_per_tick(c: &mut Criterion) {
    let now_ns: u64 = 1_000_000_000_000;
    let counters = DecisionCounters::default();
    let mut group = c.benchmark_group("per_tick_combined");

    for (name, snap, right, gx) in [
        ("clean_C", snap_clean(now_ns), 'C', greeks_call()),
        ("clean_P", snap_clean(now_ns), 'P', greeks_put()),
        ("theta_breach_C", snap_theta_breach(now_ns), 'C', greeks_call()),
        ("theta_breach_P", snap_theta_breach(now_ns), 'P', greeks_put()),
        ("delta_hi_C", snap_delta_high(now_ns), 'C', greeks_call()),
        ("delta_hi_P", snap_delta_high(now_ns), 'P', greeks_put()),
    ] {
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(snap, right, gx),
            |b, (snap, right, gx)| {
                b.iter(|| {
                    let g = compute_risk_gates(
                        black_box(snap),
                        black_box(now_ns),
                        black_box(&counters),
                    );
                    let buy_ok =
                        improving_passes(&g, black_box(*right), Side::Buy, black_box(*gx));
                    let sell_ok =
                        improving_passes(&g, black_box(*right), Side::Sell, black_box(*gx));
                    black_box((g, buy_ok, sell_ok));
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_compute_risk_gates,
    bench_improving_passes,
    bench_combined_per_tick
);
criterion_main!(benches);
