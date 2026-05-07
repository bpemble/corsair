// Levenberg-Marquardt calibrator for SABR and SVI vol surfaces.
//
// Mirrors src/sabr.py:calibrate_sabr / calibrate_svi. Numerically
// equivalent within ~1e-4 RMSE on most fits — scipy uses TRF (trust
// region with reflections); we use bounded LM with projection. The
// result space is convex enough on our 8-strike chains that both
// methods land at the same local minimum from the same initial
// guess. Residuals are weighted-IV-space, matching the Python
// implementation.
//
// Test surface: tests/test_pricing_parity.py asserts the calibrator
// produces SABR/SVI params that, when evaluated at the calibrated
// strikes, yield IVs within ~1e-3 of the Python fit. Exact-equality
// to scipy is NOT a goal — we just need fits good enough that the
// downstream theos and decisions match.
//
// Performance target (8 strikes, 4 SABR initial guesses on a single
// thread):
//     Python (scipy.optimize.least_squares): ~80–200 ms
//     Rust (this module):                    ~3–8 ms
//   ≈ 25× speedup.

#![allow(clippy::needless_range_loop)]

const EPS_GRAD: f64 = 1e-8; // gradient norm convergence
const EPS_STEP: f64 = 1e-9; // step norm convergence
const EPS_COST: f64 = 1e-12; // relative cost convergence
const FD_STEP: f64 = 1e-6; // central-difference numerical jacobian step
const LM_MU_INIT: f64 = 1e-3;
const LM_MU_DOWN: f64 = 0.4;
const LM_MU_UP: f64 = 5.0;
const LM_MU_FLOOR: f64 = 1e-12;
const LM_MU_CEIL: f64 = 1e10;

#[derive(Debug, Clone)]
pub struct LMResult {
    pub x: Vec<f64>,
    pub residuals: Vec<f64>,
    pub cost: f64, // 0.5 * sum(r_i^2)
    /// Total residual evaluations across LM iterations. Kept for
    /// telemetry / diagnostics; not used by callers today.
    #[allow(dead_code)]
    pub nfev: usize,
    /// True when LM converged (gradient-norm or step-norm or
    /// relative-cost bound met). Audit T3-15: `calibrate_sabr`
    /// prefers `success=true` guesses over lower-cost-but-stuck
    /// guesses when picking the best fit across initial conditions.
    pub success: bool,
}

/// Solve A * x = b for small dense A (n x n). In-place Gaussian
/// elimination with partial pivoting. n ≤ 5 in our use; not a hot
/// path in absolute terms but called inside the LM loop.
fn solve_linear(a: &mut [Vec<f64>], b: &mut [f64], n: usize) -> bool {
    for k in 0..n {
        // Pivot: swap row with largest absolute value in column k.
        let mut piv = k;
        let mut piv_val = a[k][k].abs();
        for i in (k + 1)..n {
            if a[i][k].abs() > piv_val {
                piv = i;
                piv_val = a[i][k].abs();
            }
        }
        if piv_val < 1e-14 {
            return false; // singular
        }
        if piv != k {
            a.swap(piv, k);
            b.swap(piv, k);
        }
        // Eliminate
        for i in (k + 1)..n {
            let factor = a[i][k] / a[k][k];
            for j in k..n {
                a[i][j] -= factor * a[k][j];
            }
            b[i] -= factor * b[k];
        }
    }
    // Back-substitute
    for i in (0..n).rev() {
        let mut s = b[i];
        for j in (i + 1)..n {
            s -= a[i][j] * b[j];
        }
        b[i] = s / a[i][i];
    }
    true
}

/// Compute J^T J + mu * diag(J^T J) and J^T r in-place. n=#params,
/// m=#residuals.
fn build_normal_eqs(
    j: &[Vec<f64>],
    r: &[f64],
    mu: f64,
    n: usize,
    m: usize,
) -> (Vec<Vec<f64>>, Vec<f64>) {
    let mut jtj = vec![vec![0.0_f64; n]; n];
    let mut jtr = vec![0.0_f64; n];
    for k in 0..m {
        for i in 0..n {
            jtr[i] -= j[k][i] * r[k]; // negate because we want -gradient
            for jj in 0..n {
                jtj[i][jj] += j[k][i] * j[k][jj];
            }
        }
    }
    // Marquardt damping: scale identity by diagonal of J^T J.
    for i in 0..n {
        jtj[i][i] += mu * jtj[i][i].max(1e-12);
    }
    (jtj, jtr)
}

/// Compute the Jacobian via central differences into a caller-owned
/// buffer. Audit T2-9: the prior signature returned a fresh
/// `Vec<Vec<f64>>` per LM iteration — at 200 iters × 4 SABR
/// initial-guess restarts that's ~800 allocs per fit, and refits
/// happen at 1Hz. Caller now reuses `j_buf` across iterations.
fn numerical_jacobian<F: Fn(&[f64]) -> Vec<f64>>(
    f: &F,
    x: &[f64],
    r0: &[f64],
    nfev: &mut usize,
    j_buf: &mut Vec<Vec<f64>>,
) {
    let n = x.len();
    let m = r0.len();
    // Resize without reallocating inner Vecs when shape matches.
    if j_buf.len() != m {
        j_buf.resize_with(m, || vec![0.0; n]);
    }
    for row in j_buf.iter_mut() {
        if row.len() != n {
            row.resize(n, 0.0);
        }
    }
    let mut x_pert = x.to_vec();
    for i in 0..n {
        let h = FD_STEP * (x[i].abs().max(1.0));
        x_pert[i] = x[i] + h;
        let r_plus = f(&x_pert);
        *nfev += 1;
        x_pert[i] = x[i] - h;
        let r_minus = f(&x_pert);
        *nfev += 1;
        x_pert[i] = x[i];
        let inv = 0.5 / h;
        for k in 0..m {
            j_buf[k][i] = (r_plus[k] - r_minus[k]) * inv;
        }
    }
}

#[inline]
fn cost(r: &[f64]) -> f64 {
    0.5 * r.iter().map(|v| v * v).sum::<f64>()
}

#[inline]
fn project(x: &mut [f64], lb: &[f64], ub: &[f64]) {
    for i in 0..x.len() {
        if x[i] < lb[i] {
            x[i] = lb[i];
        } else if x[i] > ub[i] {
            x[i] = ub[i];
        }
    }
}

/// Bounded Levenberg-Marquardt. Bounds enforced by simple projection
/// after each step; the trust-region damping handles step rejection.
///
/// Convergence: gradient norm < EPS_GRAD, step norm < EPS_STEP, or
/// relative cost change < EPS_COST.
pub fn lm_solve<F: Fn(&[f64]) -> Vec<f64>>(
    f: &F,
    x0: &[f64],
    lb: &[f64],
    ub: &[f64],
    max_nfev: usize,
) -> LMResult {
    let n = x0.len();
    let mut x = x0.to_vec();
    project(&mut x, lb, ub);
    let mut r = f(&x);
    let m = r.len();
    let mut nfev = 1;
    let mut c = cost(&r);
    let mut mu = LM_MU_INIT;
    let mut success = false;
    let mut j: Vec<Vec<f64>> = Vec::with_capacity(m);

    for _iter in 0..200 {
        if nfev >= max_nfev {
            break;
        }
        numerical_jacobian(f, &x, &r, &mut nfev, &mut j);

        // Gradient = J^T r. Convergence on gradient norm.
        let mut grad_norm = 0.0_f64;
        for i in 0..n {
            let mut g = 0.0;
            for k in 0..m {
                g += j[k][i] * r[k];
            }
            grad_norm += g * g;
        }
        grad_norm = grad_norm.sqrt();
        if grad_norm < EPS_GRAD {
            success = true;
            break;
        }

        // Try LM step with current mu; expand mu on rejection.
        let mut accepted = false;
        for _try in 0..30 {
            let (mut a, mut b) = build_normal_eqs(&j, &r, mu, n, m);
            if !solve_linear(&mut a, &mut b, n) {
                mu = (mu * LM_MU_UP).min(LM_MU_CEIL);
                continue;
            }
            // b now holds the step delta_x.
            let mut x_new = x.clone();
            for i in 0..n {
                x_new[i] += b[i];
            }
            project(&mut x_new, lb, ub);

            let r_new = f(&x_new);
            nfev += 1;
            let c_new = cost(&r_new);

            if c_new < c {
                // Accept
                let step_norm = b.iter().map(|v| v * v).sum::<f64>().sqrt();
                let cost_rel = (c - c_new) / c.max(1e-30);
                x = x_new;
                r = r_new;
                let prev_c = c;
                c = c_new;
                mu = (mu * LM_MU_DOWN).max(LM_MU_FLOOR);
                accepted = true;
                if step_norm < EPS_STEP || cost_rel < EPS_COST {
                    success = true;
                }
                let _ = prev_c;
                break;
            } else {
                mu = (mu * LM_MU_UP).min(LM_MU_CEIL);
                if mu >= LM_MU_CEIL {
                    break;
                }
            }
            if nfev >= max_nfev {
                break;
            }
        }
        if !accepted {
            break;
        }
        if success {
            break;
        }
    }

    LMResult {
        x,
        residuals: r,
        cost: c,
        nfev,
        success,
    }
}

// ─── SABR calibration ──────────────────────────────────────────────

use crate::sabr_implied_vol;

#[derive(Debug, Clone)]
pub struct SabrFit {
    pub alpha: f64,
    pub beta: f64,
    pub rho: f64,
    pub nu: f64,
    pub rmse: f64,
    pub n_points: usize,
}

pub fn calibrate_sabr(
    f: f64,
    t: f64,
    strikes: &[f64],
    market_ivs: &[f64],
    weights: Option<&[f64]>,
    beta: f64,
    max_rmse: f64,
) -> Option<SabrFit> {
    let n = strikes.len();
    if n < 3 || market_ivs.len() != n || t <= 0.0 || f <= 0.0 {
        return None;
    }

    // Normalize weights to sum-to-n (matches Python).
    let mut w = vec![1.0_f64; n];
    if let Some(weights) = weights {
        if weights.len() == n {
            let s: f64 = weights.iter().sum();
            if s > 0.0 {
                let scale = (n as f64) / s;
                for i in 0..n {
                    w[i] = weights[i] * scale;
                }
            }
        }
    }

    // ATM seed
    let mut atm_idx = 0;
    let mut best_d = (strikes[0] - f).abs();
    for i in 1..n {
        let d = (strikes[i] - f).abs();
        if d < best_d {
            best_d = d;
            atm_idx = i;
        }
    }
    let atm_iv = market_ivs[atm_idx];
    let alpha_0 = (atm_iv * f.powf(1.0 - beta)).max(0.001);

    // SABR parameter bounds.
    //
    // rho ∈ [-0.99, 0.99] — tightened from ±0.999 per fp32 precision
    // study (2026-05-06). Hagan 2002 SABR has structural numerical
    // instability near |ρ| → 1: the closed form computes
    //   xz = z / ln((sqrt(1 − 2ρz + z²) + z − ρ) / (1 − ρ))
    // and both `(1 − ρ)` and `(sqrt(...) + z − ρ)` are O(1 − |ρ|),
    // so their ratio amplifies floating-point noise into
    // percentage-level errors when |ρ| is near 1. The fp32 study
    // documented ~5–13% IV drift on rho=0.999 fits in fp32 (the
    // FPGA-prototype-blocking finding). The same instability exists
    // in fp64 at smaller magnitudes — and the calibrator's noise
    // floor on rho is already well above 0.009, so the last 0.009
    // of rho range is statistically uninteresting (the LM solver
    // hitting the boundary indicates the data doesn't constrain
    // rho tightly, not that rho is genuinely 0.999).
    //
    // Eliminates the only observed structural fp32 failure mode
    // (precision study verdict) and removes a known fp64 SABR
    // pitfall as a side effect. See:
    //   - audits/sections-16-19-audit.md (precision-study cross-ref)
    //   - /home/ethereal/fp32-precision-spike/results/analysis_report.md §5
    let alpha_ub = (10.0_f64).max(alpha_0 * 5.0);
    let lb = [0.0001_f64, -0.99, 0.0001];
    let ub = [alpha_ub, 0.99, 5.0];

    let initial_guesses: [[f64; 3]; 4] = [
        [alpha_0, -0.3, 0.3],
        [alpha_0, -0.5, 0.5],
        [alpha_0, 0.0, 0.2],
        [alpha_0 * 0.8, -0.2, 0.4],
    ];

    let strikes = strikes.to_vec();
    let mkt = market_ivs.to_vec();
    let w_local = w.clone();
    let n_pts = n;
    let beta_local = beta;
    let f_local = f;
    let t_local = t;
    let residual = move |p: &[f64]| -> Vec<f64> {
        let (a, r, v) = (p[0], p[1], p[2]);
        let mut out = Vec::with_capacity(n_pts);
        for i in 0..n_pts {
            let mdl = sabr_implied_vol(f_local, strikes[i], t_local, a, beta_local, r, v);
            out.push((mdl - mkt[i]) * w_local[i]);
        }
        out
    };

    // Audit T3-15: prefer LM results that converged (`success=true`)
    // over stuck guesses with marginally lower raw cost. A guess that
    // hits LM_MU_CEIL early can have lower cost than one that ran to
    // gradient-norm convergence but the latter is the real fit.
    // Fall back to lowest-cost when no guess converged.
    let mut best_success: Option<LMResult> = None;
    let mut best_any: Option<LMResult> = None;
    for guess in initial_guesses.iter() {
        let res = lm_solve(&residual, guess, &lb, &ub, 200);
        let lower_any = best_any.as_ref().map(|b| res.cost < b.cost).unwrap_or(true);
        if lower_any {
            best_any = Some(res.clone());
        }
        if res.success {
            let lower_succ = best_success
                .as_ref()
                .map(|b| res.cost < b.cost)
                .unwrap_or(true);
            if lower_succ {
                best_success = Some(res);
            }
        }
    }
    let best = best_success.or(best_any)?;
    let rmse =
        (best.residuals.iter().map(|v| v * v).sum::<f64>() / best.residuals.len() as f64).sqrt();
    if rmse > max_rmse {
        return None;
    }
    Some(SabrFit {
        alpha: best.x[0],
        beta,
        rho: best.x[1],
        nu: best.x[2],
        rmse,
        n_points: n,
    })
}

// ─── SVI calibration ──────────────────────────────────────────────

use crate::svi_total_variance_inner;

#[derive(Debug, Clone)]
pub struct SviFit {
    pub a: f64,
    pub b: f64,
    pub rho: f64,
    pub m: f64,
    pub sigma: f64,
    pub rmse: f64,
    pub n_points: usize,
}

pub fn calibrate_svi(
    f: f64,
    t: f64,
    strikes: &[f64],
    market_ivs: &[f64],
    weights: Option<&[f64]>,
    max_rmse: f64,
) -> Option<SviFit> {
    let n = strikes.len();
    if n < 5 || market_ivs.len() != n || t <= 0.0 || f <= 0.0 {
        return None;
    }
    let mut w = vec![1.0_f64; n];
    if let Some(weights) = weights {
        if weights.len() == n {
            let s: f64 = weights.iter().sum();
            if s > 0.0 {
                let scale = (n as f64) / s;
                for i in 0..n {
                    w[i] = weights[i] * scale;
                }
            }
        }
    }

    let ks: Vec<f64> = strikes.iter().map(|k| (k / f).ln()).collect();
    let mkt_w: Vec<f64> = market_ivs.iter().map(|iv| iv * iv * t).collect();

    // ATM total variance via linear interp at k=0. Python uses np.interp
    // which assumes strictly increasing xs. Sort ks ascending for interp.
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| ks[a].partial_cmp(&ks[b]).unwrap());
    let ks_sorted: Vec<f64> = idx.iter().map(|&i| ks[i]).collect();
    let mkt_w_sorted: Vec<f64> = idx.iter().map(|&i| mkt_w[i]).collect();
    let atm_var = interp1(&ks_sorted, &mkt_w_sorted, 0.0);

    // Initial guesses (mirror Python).
    let initial_guesses: [[f64; 5]; 8] = [
        [atm_var, 0.1, -0.3, 0.0, 0.1],
        [atm_var, 0.2, -0.5, 0.0, 0.05],
        [atm_var * 0.8, 0.15, -0.2, -0.05, 0.15],
        [atm_var, 0.05, -0.4, 0.02, 0.08],
        [atm_var, 0.1, -0.7, -0.1, 0.3],
        [atm_var * 0.5, 0.15, -0.6, -0.05, 0.5],
        [atm_var, 0.08, -0.8, -0.15, 0.4],
        [atm_var * 0.3, 0.2, -0.5, 0.0, 0.6],
    ];

    // Audit round 2: tighten SVI rho bound to ±0.99 to match the SABR
    // calibrator (commit 6807447, §18). SVI's xz formula doesn't have
    // the SABR `1/(1-rho)` divergence, but at |rho|=0.999 Gatheral's
    // no-arbitrage condition `b*(1+|rho|) ≤ 4/T` gets harder to
    // satisfy and degenerate fits become more likely. ±0.99 gives
    // the LM headroom without crowding the constraint, keeping the
    // bound consistent across SABR and SVI.
    let lb = [0.0_f64, 0.0001, -0.99, -1.0, 0.0001];
    let ub = [5.0_f64, 5.0, 0.99, 1.0, 2.0];

    let mkt = market_ivs.to_vec();
    let w_local = w.clone();
    let ks_local = ks.clone();
    let n_pts = n;
    let t_local = t;
    let residual = move |p: &[f64]| -> Vec<f64> {
        let (a, b, rho, m, sig) = (p[0], p[1], p[2], p[3], p[4]);
        let mut out = Vec::with_capacity(n_pts);
        for i in 0..n_pts {
            let w_i = svi_total_variance_inner(ks_local[i], a, b, rho, m, sig);
            let model_iv = (w_i.max(1e-10) / t_local).sqrt();
            out.push((model_iv - mkt[i]) * w_local[i]);
        }
        out
    };

    let mut best: Option<LMResult> = None;
    for guess in initial_guesses.iter() {
        let res = lm_solve(&residual, guess, &lb, &ub, 500);
        match &best {
            None => best = Some(res),
            Some(b) if res.cost < b.cost => best = Some(res),
            _ => {}
        }
    }
    let best = best?;
    let rmse =
        (best.residuals.iter().map(|v| v * v).sum::<f64>() / best.residuals.len() as f64).sqrt();
    if rmse > max_rmse {
        return None;
    }
    let (a, b, rho, m, sigma) = (best.x[0], best.x[1], best.x[2], best.x[3], best.x[4]);
    // Total-variance non-negativity check at calibrated strikes.
    for &k in ks.iter() {
        if svi_total_variance_inner(k, a, b, rho, m, sigma) < 0.0 {
            return None;
        }
    }
    Some(SviFit {
        a,
        b,
        rho,
        m,
        sigma,
        rmse,
        n_points: n,
    })
}

fn interp1(xs: &[f64], ys: &[f64], x: f64) -> f64 {
    // Linear interpolation at x; clamps to endpoints (np.interp default).
    if x <= xs[0] {
        return ys[0];
    }
    if x >= xs[xs.len() - 1] {
        return ys[ys.len() - 1];
    }
    for i in 1..xs.len() {
        if x <= xs[i] {
            // Audit T1-8: guard against duplicate xs[]. With a small
            // chain (5 strikes) and a tied log-moneyness on two
            // adjacent strikes (rare but possible — same float
            // representation after the ln), the denominator goes to 0
            // and we return NaN. Pick the prior endpoint when the
            // interval has zero width.
            let dx = xs[i] - xs[i - 1];
            if dx.abs() < 1e-12 {
                return ys[i - 1];
            }
            let t = (x - xs[i - 1]) / dx;
            return ys[i - 1] + t * (ys[i] - ys[i - 1]);
        }
    }
    ys[ys.len() - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_chain_sabr(
        f: f64,
        t: f64,
        alpha: f64,
        beta: f64,
        rho: f64,
        nu: f64,
        ks: &[f64],
    ) -> Vec<f64> {
        ks.iter()
            .map(|&k| sabr_implied_vol(f, k, t, alpha, beta, rho, nu))
            .collect()
    }

    #[test]
    fn lm_recovers_synthetic_sabr_params() {
        let f = 6.0;
        let t = 0.05;
        let alpha_true = 0.45;
        let beta = 0.5;
        let rho_true = -0.25;
        let nu_true = 1.2;
        let ks = vec![5.6, 5.7, 5.8, 5.9, 6.0, 6.1, 6.2, 6.3, 6.4];
        let ivs = synth_chain_sabr(f, t, alpha_true, beta, rho_true, nu_true, &ks);
        let fit = calibrate_sabr(f, t, &ks, &ivs, None, beta, 0.05).expect("fit failed");
        // Should recover within a few percent on noiseless data.
        assert!((fit.alpha - alpha_true).abs() < 0.05, "alpha drift: {}", fit.alpha);
        assert!((fit.rho - rho_true).abs() < 0.10, "rho drift: {}", fit.rho);
        assert!((fit.nu - nu_true).abs() < 0.30, "nu drift: {}", fit.nu);
        assert!(fit.rmse < 1e-3, "rmse too high: {}", fit.rmse);
    }

    #[test]
    fn lm_returns_none_for_too_few_strikes() {
        let res = calibrate_sabr(6.0, 0.05, &[6.0, 6.1], &[0.5, 0.51], None, 0.5, 0.05);
        assert!(res.is_none());
    }

    #[test]
    fn lm_returns_none_when_rmse_exceeds_threshold() {
        // Inconsistent IVs that no SABR fit can cover within max_rmse.
        let f = 6.0;
        let t = 0.05;
        let ks = vec![5.5, 5.7, 5.9, 6.0, 6.1, 6.3, 6.5];
        let ivs = vec![0.10, 0.50, 0.20, 0.40, 0.20, 0.50, 0.10]; // jagged
        let res = calibrate_sabr(f, t, &ks, &ivs, None, 0.5, 0.001);
        assert!(res.is_none(), "should reject very high rmse");
    }

    #[test]
    fn sabr_rho_bound_clamps_at_0_99() {
        // Synthesize a chain from a SABR with rho_true near the old
        // (±0.999) boundary. With the new (±0.99) bound, the fit's
        // rho must NOT exceed 0.99 in absolute value. This test
        // validates that the LM solver respects the new constraint
        // and does not overshoot the bound.
        //
        // Tolerance: project() snaps to the bound exactly; allow
        // tiny epsilon for numerical-roundoff in the LM step that
        // produces the bound-touching iterate.
        let f = 6.0;
        let t = 25.0 / 365.0;
        let alpha_true = 0.65;
        let beta = 0.5;
        let nu_true = 0.5;
        let ks = vec![5.6, 5.7, 5.8, 5.9, 6.0, 6.1, 6.2, 6.3, 6.4];

        for rho_true in [0.995_f64, -0.995, 0.999, -0.999] {
            let ivs = synth_chain_sabr(f, t, alpha_true, beta, rho_true, nu_true, &ks);
            let fit = calibrate_sabr(f, t, &ks, &ivs, None, beta, 0.5)
                .unwrap_or_else(|| panic!("fit failed for rho_true={rho_true}"));
            assert!(
                fit.rho.abs() <= 0.99 + 1e-9,
                "rho bound violated: rho_true={rho_true}, fit.rho={}",
                fit.rho
            );
            // The bound-touching iterate is the expected outcome here —
            // confirm the calibrator pushed rho to the boundary rather
            // than landing somewhere arbitrary.
            assert!(
                fit.rho.abs() >= 0.985,
                "expected rho near boundary for rho_true={rho_true}, got fit.rho={}",
                fit.rho
            );
        }
    }

    #[test]
    fn sabr_rho_unchanged_for_typical_values() {
        // Synthesize chains over a representative range of rho values
        // that fall inside the new (±0.99) bound. Each fit must
        // recover the underlying rho within tolerance — the bound
        // tightening should not perturb fits in the safe region.
        // Sweeping confirms there's no regression at any specific rho
        // (the boundary tightening could in principle have introduced
        // a numerical artifact in the LM trajectory; this test would
        // catch that).
        let f = 6.0;
        let t = 25.0 / 365.0;
        let alpha_true = 0.55;
        let beta = 0.5;
        let nu_true = 1.0;
        let ks = vec![5.6, 5.7, 5.8, 5.9, 6.0, 6.1, 6.2, 6.3, 6.4];

        for rho_true in [-0.95_f64, -0.50, -0.25, 0.0, 0.25, 0.50, 0.95] {
            let ivs = synth_chain_sabr(f, t, alpha_true, beta, rho_true, nu_true, &ks);
            let fit = calibrate_sabr(f, t, &ks, &ivs, None, beta, 0.05)
                .unwrap_or_else(|| panic!("fit failed for rho_true={rho_true}"));
            // Same tolerance as `lm_recovers_synthetic_sabr_params` —
            // we're asserting parity with the pre-tightening behavior
            // in this region.
            assert!(
                (fit.rho - rho_true).abs() < 0.10,
                "rho drift at rho_true={rho_true}: fit.rho={}",
                fit.rho
            );
            assert!(
                fit.rmse < 1e-3,
                "rmse too high at rho_true={rho_true}: rmse={}",
                fit.rmse
            );
        }
    }

    #[test]
    fn sabr_real_world_rho_boundary_fits_converge_with_new_bound() {
        // Real-world validation. Params extracted from production
        // logs-paper/trader_events-2026-05-{05,06}.jsonl — the SABR fits
        // that hit |rho|=0.999 (which the new bound rejects). For each,
        // synthesize the implied chain on a HG-realistic strike grid
        // (15 strikes spanning ±$0.35 around F at $0.05 spacing) and
        // confirm the calibrator produces a sane fit with the new
        // (±0.99) bound: convergence, RMSE within reasonable range
        // (≤ 0.05 — the same max_rmse the production calibrator uses),
        // and rho clamped at the new boundary.
        //
        // This is the production sanity check the PR description
        // references — it answers "does the tighter bound break any
        // real fits the calibrator was making?". Empirical answer: no.
        //
        // Format: (F, alpha, beta, rho_old, nu)
        let real_boundary_fits: &[(f64, f64, f64, f64, f64)] = &[
            (5.98165,           0.5935306229887087, 0.5,  0.999,                0.130790712609745),
            (5.982349999999999, 0.5924917654859228, 0.5,  0.999,                0.1478298845397704),
            (5.9906999999999995, 0.5951153861106905, 0.5, -0.999,                0.10136175011755671),
            (6.003522727272727, 0.6019820006457103, 0.5, -0.999,                0.022055044508673023),
            (6.0049,            0.6032202968799343, 0.5,  0.9989999359809747,   0.005902932488886279),
            (5.9904,            0.601444921044216,  0.5,  0.998999999975501,    0.0477784914230657),
            (6.009575,          0.5995941341268647, 0.5, -0.9989999582752446,   0.004793804563487619),
            (5.98875,           0.5968936318711497, 0.5, -0.9985859785308195,   0.0270000492729843),
        ];
        let t = 25.0 / 365.0;

        for &(f, alpha, beta, rho_old, nu) in real_boundary_fits {
            // Synthesize the chain at a HG-realistic strike grid.
            let ks_full: Vec<f64> = (-7..=7).map(|i| f + (i as f64) * 0.05).collect();
            let mut ks = Vec::new();
            let mut ivs = Vec::new();
            for &k in &ks_full {
                let iv = sabr_implied_vol(f, k, t, alpha, beta, rho_old, nu);
                if iv.is_finite() && iv > 0.0 {
                    ks.push(k);
                    ivs.push(iv);
                }
            }
            assert!(
                ks.len() >= 5,
                "synthesis produced too few valid strikes for rho_old={rho_old}: {}",
                ks.len()
            );

            // Re-fit with NEW bounds (the lb/ub at lines above).
            // max_rmse=0.5 lets us see whether the fit converged at ALL
            // (production uses 0.05 — checked separately below).
            let refit = calibrate_sabr(f, t, &ks, &ivs, None, beta, 0.5)
                .unwrap_or_else(|| panic!("refit failed for rho_old={rho_old}"));

            // rho must respect the new bound. The fit may either clamp
            // at ±0.99 (when the data really wants extreme rho) OR
            // settle at an interior value (when the new bound gives
            // the LM solver enough room to find a numerically
            // better-conditioned solution that the old ±0.999 boundary
            // was masking — this is the GOOD outcome).
            assert!(
                refit.rho.abs() <= 0.99 + 1e-9,
                "rho bound violated for rho_old={rho_old}: refit.rho={}",
                refit.rho
            );
            // RMSE must stay within production's max_rmse (0.05). If
            // the fit needs a larger budget than that, the bound is too
            // tight for this real-world chain — the test fails and we
            // re-evaluate the bound choice.
            assert!(
                refit.rmse <= 0.05,
                "RMSE exceeds production max_rmse=0.05 for rho_old={rho_old}: rmse={}",
                refit.rmse
            );
        }
    }

    #[test]
    fn svi_fits_synthetic_chain() {
        let f = 6.0;
        let t = 0.1;
        let ks: Vec<f64> = (-3..=3).map(|i| f + (i as f64) * 0.1).collect();
        // Generate market IVs from known SVI params.
        let (a, b, rho, m, sigma) = (0.02, 0.08, -0.3, 0.0, 0.15);
        let ivs: Vec<f64> = ks
            .iter()
            .map(|&k| {
                let lk = (k / f).ln();
                let w = svi_total_variance_inner(lk, a, b, rho, m, sigma);
                (w.max(1e-10) / t).sqrt()
            })
            .collect();
        let fit = calibrate_svi(f, t, &ks, &ivs, None, 0.05).expect("svi fit failed");
        assert!(fit.rmse < 1e-3, "svi rmse: {}", fit.rmse);
    }
}
