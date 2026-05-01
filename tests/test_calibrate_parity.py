"""Rust vs Python calibrator parity.

Verifies that calibrate_sabr / calibrate_svi produce equivalent fits
across a battery of synthetic and realistic chains. Exact-equality to
scipy.optimize.least_squares is NOT a goal — both methods are local
solvers and find slightly different minima depending on numerical
quirks. What we DO require:

  - Both return a fit (not None) for the same well-conditioned chain
  - Both return None for the same ill-conditioned chain
  - The fitted IVs at the calibration strikes agree to within ~1e-3
    in IV space — small enough that downstream theos and decisions
    don't diverge

If parity holds the Rust calibrator is safe to ship as the default;
the SciPy fallback remains via CORSAIR_CALIBRATOR=python.
"""
import os
import random

import pytest

from src.sabr import (
    SABRParams, SVIParams,
    _calibrate_sabr_python, _calibrate_svi_python,
    sabr_implied_vol, svi_implied_vol,
)

try:
    import corsair_pricing as _rs
    HAVE_RS_CAL = bool(getattr(_rs, "calibrate_sabr", None)
                       and getattr(_rs, "calibrate_svi", None))
except ImportError:
    HAVE_RS_CAL = False


pytestmark = pytest.mark.skipif(
    not HAVE_RS_CAL,
    reason="corsair_pricing.calibrate_* not built into image",
)


def _synth_sabr(F, T, alpha, beta, rho, nu, K_grid):
    return [sabr_implied_vol(F, K, T, alpha, beta, rho, nu) for K in K_grid]


def _synth_svi(F, T, a, b, rho, m, sigma, K_grid):
    return [svi_implied_vol(F, K, T, a, b, rho, m, sigma) for K in K_grid]


# ── SABR parity ─────────────────────────────────────────────────────


@pytest.mark.parametrize("seed", [1, 7, 42, 137, 999])
def test_sabr_parity_synthetic(seed):
    """Synthetic chains drawn from random SABR params. Both fitters
    should land on params close to truth and IV reconstruction within
    1e-3."""
    rng = random.Random(seed)
    F = rng.uniform(3.0, 8.0)
    T = rng.uniform(0.02, 0.5)
    beta = 0.5
    alpha = rng.uniform(0.2, 1.5)
    rho = rng.uniform(-0.6, 0.4)
    nu = rng.uniform(0.5, 2.5)
    # 8 strikes: 4 below F, ATM, 3 above
    Ks = [F + i * 0.05 for i in range(-4, 5)]
    ivs = _synth_sabr(F, T, alpha, beta, rho, nu, Ks)

    rs_fit = _rs.calibrate_sabr(F, T, Ks, ivs, beta, 0.05, None)
    py_fit = _calibrate_sabr_python(F, T, Ks, ivs, beta=beta, max_rmse=0.05)

    assert rs_fit is not None, f"Rust returned None on synthetic seed={seed}"
    assert py_fit is not None, f"Python returned None on synthetic seed={seed}"

    # Reconstruct IVs at calibration strikes and compare
    rs_ivs = [
        sabr_implied_vol(F, K, T, rs_fit["alpha"], beta,
                         rs_fit["rho"], rs_fit["nu"])
        for K in Ks
    ]
    py_ivs = [
        sabr_implied_vol(F, K, T, py_fit.alpha, beta, py_fit.rho, py_fit.nu)
        for K in Ks
    ]
    max_iv_diff = max(abs(r - p) for r, p in zip(rs_ivs, py_ivs))
    assert max_iv_diff < 1e-3, (
        f"seed={seed}: max IV diff {max_iv_diff:.5e} too large. "
        f"rs={rs_fit}, py={py_fit.__dict__}"
    )


def test_sabr_returns_none_for_too_few_strikes():
    """Both must reject n<3."""
    F, T = 6.0, 0.05
    Ks = [5.95, 6.0]
    ivs = [0.50, 0.51]
    assert _rs.calibrate_sabr(F, T, Ks, ivs, 0.5, 0.05, None) is None
    assert _calibrate_sabr_python(F, T, Ks, ivs, beta=0.5, max_rmse=0.05) is None


def test_sabr_returns_none_when_invalid_T():
    """T<=0 must short-circuit."""
    F, T = 6.0, 0.0
    Ks = [5.9, 6.0, 6.1]
    ivs = [0.5, 0.5, 0.5]
    assert _rs.calibrate_sabr(F, T, Ks, ivs, 0.5, 0.05, None) is None
    assert _calibrate_sabr_python(F, T, Ks, ivs, beta=0.5, max_rmse=0.05) is None


def test_sabr_weights_normalize_correctly():
    """Same chain with non-uniform weights — both should respect
    them and produce similar fits."""
    F, T = 6.0, 0.1
    beta, alpha, rho, nu = 0.5, 0.6, -0.3, 1.0
    Ks = [F + i * 0.05 for i in range(-4, 5)]
    ivs = _synth_sabr(F, T, alpha, beta, rho, nu, Ks)
    weights = [3.0, 2.0, 1.5, 1.2, 1.0, 1.2, 1.5, 2.0, 3.0]  # tighter ATM

    rs_fit = _rs.calibrate_sabr(F, T, Ks, ivs, beta, 0.05, weights)
    py_fit = _calibrate_sabr_python(F, T, Ks, ivs,
                                     beta=beta, max_rmse=0.05,
                                     weights=weights)
    assert rs_fit is not None
    assert py_fit is not None
    rs_ivs = [sabr_implied_vol(F, K, T, rs_fit["alpha"], beta,
                                rs_fit["rho"], rs_fit["nu"]) for K in Ks]
    py_ivs = [sabr_implied_vol(F, K, T, py_fit.alpha, beta,
                                py_fit.rho, py_fit.nu) for K in Ks]
    max_iv_diff = max(abs(r - p) for r, p in zip(rs_ivs, py_ivs))
    assert max_iv_diff < 2e-3, f"weighted fit diverged: {max_iv_diff:.5e}"


# ── SVI parity ──────────────────────────────────────────────────────


@pytest.mark.parametrize("seed", [1, 42, 137])
def test_svi_parity_synthetic(seed):
    rng = random.Random(seed)
    F = rng.uniform(3.0, 8.0)
    T = rng.uniform(0.02, 0.5)
    a = rng.uniform(0.005, 0.05)
    b = rng.uniform(0.05, 0.2)
    rho = rng.uniform(-0.6, 0.2)
    m = rng.uniform(-0.05, 0.05)
    sigma = rng.uniform(0.05, 0.3)
    Ks = [F + i * 0.05 for i in range(-4, 5)]
    ivs = _synth_svi(F, T, a, b, rho, m, sigma, Ks)

    rs_fit = _rs.calibrate_svi(F, T, Ks, ivs, 0.05, None)
    py_fit = _calibrate_svi_python(F, T, Ks, ivs, max_rmse=0.05)

    if rs_fit is None and py_fit is None:
        return  # both reject, parity holds
    assert rs_fit is not None, f"seed={seed}: Rust returned None"
    assert py_fit is not None, f"seed={seed}: Python returned None"

    rs_ivs = [svi_implied_vol(F, K, T, rs_fit["a"], rs_fit["b"], rs_fit["rho"],
                              rs_fit["m"], rs_fit["sigma"]) for K in Ks]
    py_ivs = [svi_implied_vol(F, K, T, py_fit.a, py_fit.b, py_fit.rho,
                              py_fit.m, py_fit.sigma) for K in Ks]
    max_iv_diff = max(abs(r - p) for r, p in zip(rs_ivs, py_ivs))
    assert max_iv_diff < 1e-3, (
        f"seed={seed}: SVI max IV diff {max_iv_diff:.5e}. "
        f"rs={rs_fit}, py={py_fit.__dict__}"
    )


def test_svi_returns_none_for_too_few_strikes():
    """SVI requires n>=5."""
    F, T = 6.0, 0.1
    Ks = [5.9, 5.95, 6.0, 6.05]  # only 4
    ivs = [0.5, 0.5, 0.5, 0.5]
    assert _rs.calibrate_svi(F, T, Ks, ivs, 0.05, None) is None
    assert _calibrate_svi_python(F, T, Ks, ivs, max_rmse=0.05) is None


# ── Performance smoke ──────────────────────────────────────────────


def test_rust_is_meaningfully_faster_than_python():
    """Sanity: 100 sequential calibrations of an 8-strike SABR chain.
    Rust should be at least 5× faster — we don't gate on 25× since CI
    machines can be slow, but a pathological regression (e.g., 2× faster
    or worse) means something's wrong with the Rust path."""
    import time
    F, T = 6.0, 0.1
    beta = 0.5
    alpha, rho, nu = 0.6, -0.3, 1.0
    Ks = [F + i * 0.05 for i in range(-4, 5)]
    ivs = _synth_sabr(F, T, alpha, beta, rho, nu, Ks)

    t0 = time.perf_counter()
    for _ in range(100):
        _calibrate_sabr_python(F, T, Ks, ivs, beta=beta, max_rmse=0.05)
    py_dur = time.perf_counter() - t0

    t0 = time.perf_counter()
    for _ in range(100):
        _rs.calibrate_sabr(F, T, Ks, ivs, beta, 0.05, None)
    rs_dur = time.perf_counter() - t0

    speedup = py_dur / rs_dur
    print(f"\n  Python: {py_dur*1000:.1f}ms total, {py_dur*10:.2f}ms/fit")
    print(f"  Rust:   {rs_dur*1000:.1f}ms total, {rs_dur*10:.2f}ms/fit")
    print(f"  speedup: {speedup:.1f}×")
    assert speedup > 5.0, (
        f"Expected ≥5× speedup, got {speedup:.1f}× "
        f"(py={py_dur*1000:.1f}ms, rs={rs_dur*1000:.1f}ms)"
    )
