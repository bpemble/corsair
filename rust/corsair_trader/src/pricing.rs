//! Pure-Rust pricing primitives for the trader. Mirrors the math in
//! `corsair_pricing` (the PyO3 crate) but with no Python deps. Single
//! source of truth for the formulas is `src/sabr.py`.
//!
//! For the binary's hot path we want native Rust calls (no FFI cost).
//! The corsair_pricing crate stays as the Python extension.

use statrs::distribution::{ContinuousCDF, Normal};
use std::sync::OnceLock;

/// Standard normal `N(0, 1)` shared across calls. Bundle 6K
/// (2026-05-06): previously each Black-76 evaluation called
/// `Normal::new(0.0, 1.0).unwrap()` to construct a fresh instance —
/// not free under statrs (validates parameters; small struct copy).
/// One-time `OnceLock` init means the hot path reads a `&Normal`
/// directly.
fn standard_normal() -> &'static Normal {
    static N: OnceLock<Normal> = OnceLock::new();
    N.get_or_init(|| Normal::new(0.0, 1.0).expect("standard normal init"))
}

/// Convenience accessor: just price + delta from `black76_greeks`.
/// Used only by pricing.rs unit tests; production code calls
/// `black76_greeks` directly via `compute_theo`.
#[cfg(test)]
pub fn black76_price_and_delta(
    f: f64, k: f64, t: f64, sigma: f64, r: f64, right: char,
) -> (f64, f64) {
    let g = black76_greeks(f, k, t, sigma, r, right);
    (g.price, g.delta)
}

/// Test-only price-only convenience.
#[cfg(test)]
pub fn black76_price(f: f64, k: f64, t: f64, sigma: f64, r: f64, right: char) -> f64 {
    black76_greeks(f, k, t, sigma, r, right).price
}

/// Black-76 price + all four greeks in one pass. d1/d2 + CDFs are
/// computed once; theta and vega need additional terms beyond price+
/// delta. Per-contract values at multiplier=1.0 (callers scale to
/// dollar terms by multiplying portfolio multiplier).
///
/// Sign convention:
/// - delta: +ve for calls, -ve for puts (long position)
/// - theta: -ve for both calls and puts (long position decays)
/// - vega:  +ve for both (per 1% vol move; long position gains in vol)
///
/// Used by the per-strike greek cache (CLAUDE.md §25 — improving-only
/// gating reads cached theta/vega to compute post-fill portfolio
/// greek change on the hot path).
#[derive(Debug, Clone, Copy)]
pub struct BlackGreeks {
    pub price: f64,
    pub delta: f64,
    pub theta: f64,
    pub vega: f64,
}

pub fn black76_greeks(
    f: f64, k: f64, t: f64, sigma: f64, r: f64, right: char,
) -> BlackGreeks {
    let is_call = right == 'C' || right == 'c';
    if t <= 0.0 || sigma <= 0.0 || f <= 0.0 || k <= 0.0 {
        let price = if is_call { (f - k).max(0.0) } else { (k - f).max(0.0) };
        let delta = if is_call {
            if f > k { 1.0 } else if f < k { 0.0 } else { 0.5 }
        } else {
            if f > k { 0.0 } else if f < k { -1.0 } else { -0.5 }
        };
        return BlackGreeks { price, delta, theta: 0.0, vega: 0.0 };
    }
    let sqrt_t = t.sqrt();
    let d1 = ((f / k).ln() + 0.5 * sigma * sigma * t) / (sigma * sqrt_t);
    let d2 = d1 - sigma * sqrt_t;
    let n = standard_normal();
    let disc = if r == 0.0 { 1.0 } else { (-r * t).exp() };
    let n_d1 = n.cdf(d1);
    let pdf_d1 = (-0.5 * d1 * d1).exp() / (2.0_f64 * std::f64::consts::PI).sqrt();
    // Vega per 1% vol move (matches corsair_pricing::greeks convention):
    //   ∂V/∂σ × 0.01 = f * disc * φ(d1) * sqrt(t) / 100
    let vega = f * disc * pdf_d1 * sqrt_t / 100.0;
    // Theta in dollar/day (matches corsair_pricing::greeks convention):
    //   ∂V/∂T = -f * disc * φ(d1) * σ / (2*sqrt(t))  [no rate term when r=0]
    //   /365 for per-day. Both rights share the σ-decay term; the rate
    //   term diverges by sign for calls vs puts but vanishes when r=0.
    let common = -disc * f * pdf_d1 * sigma / (2.0 * sqrt_t);
    let theta = if is_call {
        (common - r * k * disc * n.cdf(d2)) / 365.0
    } else {
        (common + r * k * disc * n.cdf(-d2)) / 365.0
    };
    if is_call {
        let price = disc * (f * n_d1 - k * n.cdf(d2));
        let delta = disc * n_d1;
        BlackGreeks { price, delta, theta, vega }
    } else {
        let price = disc * (k * n.cdf(-d2) - f * (1.0 - n_d1));
        let delta = disc * (n_d1 - 1.0);
        BlackGreeks { price, delta, theta, vega }
    }
}

/// SABR Hagan (2002) implied vol. Mirror of src/sabr.py:sabr_implied_vol.
///
/// Bundle 6K (2026-05-06): caches `one_minus_beta` and powers thereof
/// once per call, replacing repeated `(1.0 - beta)` and powf reuses.
/// `.powf` calls remain only where the exponent is a runtime float
/// (carry the actual `1.0 - beta` and `(1.0-beta)/2`); the `.powi`
/// uses sit on integer exponents and are already cheap.
pub fn sabr_implied_vol(
    f: f64, k: f64, t: f64,
    alpha: f64, beta: f64, rho: f64, nu: f64,
) -> f64 {
    if t <= 0.0 || alpha <= 0.0 || f <= 0.0 || k <= 0.0 {
        return if alpha > 0.0 { alpha } else { 0.01 };
    }
    const EPS: f64 = 1e-7;

    let one_minus_beta = 1.0 - beta;
    let one_minus_beta_sq = one_minus_beta * one_minus_beta;
    let one_minus_beta_4 = one_minus_beta_sq * one_minus_beta_sq;
    let rho_sq = rho * rho;
    let nu_sq = nu * nu;
    let alpha_sq = alpha * alpha;
    let p3 = (2.0 - 3.0 * rho_sq) * nu_sq / 24.0;

    // ATM case.
    if (f - k).abs() < EPS * f {
        let fmid = f;
        let fmid_beta = fmid.powf(one_minus_beta);
        let term1 = alpha / fmid_beta;
        let p1 = (one_minus_beta_sq / 24.0) * alpha_sq
            / fmid.powf(2.0 * one_minus_beta);
        let p2 = 0.25 * rho * beta * nu * alpha / fmid_beta;
        return term1 * (1.0 + (p1 + p2 + p3) * t);
    }

    let fk = f * k;
    let fk_beta = fk.powf(0.5 * one_minus_beta);
    let log_fk = (f / k).ln();
    let log_fk_sq = log_fk * log_fk;
    let log_fk_4 = log_fk_sq * log_fk_sq;

    let z = (nu / alpha) * fk_beta * log_fk;
    let xz = if z.abs() < EPS {
        1.0
    } else {
        let sqrt_term = (1.0 - 2.0 * rho * z + z * z).sqrt();
        z / ((sqrt_term + z - rho) / (1.0 - rho)).ln()
    };

    let denom1 = fk_beta
        * (1.0
            + one_minus_beta_sq / 24.0 * log_fk_sq
            + one_minus_beta_4 / 1920.0 * log_fk_4);

    let p1 = one_minus_beta_sq / 24.0 * alpha_sq / fk.powf(one_minus_beta);
    let p2 = 0.25 * rho * beta * nu * alpha / fk_beta;

    (alpha / denom1) * xz * (1.0 + (p1 + p2 + p3) * t)
}

/// SVI raw total variance. Mirrors svi_total_variance in src/sabr.py.
#[inline(always)]
fn svi_total_variance(k: f64, a: f64, b: f64, rho: f64, m: f64, sigma: f64) -> f64 {
    let dk = k - m;
    a + b * (rho * dk + (dk * dk + sigma * sigma).sqrt())
}

/// SVI implied vol from log-moneyness. Mirrors svi_implied_vol in
/// src/sabr.py (and the recently-ported corsair_pricing version).
///
/// IMPORTANT: caller must pass the FIT-TIME forward, not current spot.
/// SVI's `m` is anchored on log(K/F_fit). See trader/quote_decision.py
/// docstring for the 2026-05-01 incident that motivated this rule.
pub fn svi_implied_vol(
    f: f64, k_strike: f64, t: f64,
    a: f64, b: f64, rho: f64, m: f64, sigma: f64,
) -> f64 {
    if t <= 0.0 || k_strike <= 0.0 || f <= 0.0 {
        return 0.0;
    }
    let k = (k_strike / f).ln();
    let w = svi_total_variance(k, a, b, rho, m, sigma);
    if w <= 0.0 {
        return 0.001;
    }
    (w / t).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black76_intrinsic_at_zero_vol() {
        // T=0 collapses to intrinsic.
        assert_eq!(black76_price(100.0, 90.0, 0.0, 0.2, 0.0, 'C'), 10.0);
        assert_eq!(black76_price(100.0, 110.0, 0.0, 0.2, 0.0, 'P'), 10.0);
    }

    #[test]
    fn black76_delta_atm_call() {
        // ATM, normal vol+T → call delta should be near 0.5
        // (with disc=1 since r=0).
        let (_p, d) = black76_price_and_delta(100.0, 100.0, 0.25, 0.20, 0.0, 'C');
        assert!((d - 0.5).abs() < 0.05, "atm call delta {}", d);
    }

    #[test]
    fn black76_delta_atm_put() {
        // ATM put delta near -0.5.
        let (_p, d) = black76_price_and_delta(100.0, 100.0, 0.25, 0.20, 0.0, 'P');
        assert!((d - (-0.5)).abs() < 0.05, "atm put delta {}", d);
    }

    #[test]
    fn black76_taylor_first_order_check() {
        // Sanity: theo(F + dF) ≈ theo(F) + delta(F) × dF for small dF.
        let f0 = 6.0;
        let k = 6.05;
        let t = 25.0 / 365.0;
        let sigma = 0.30;
        let (theo0, delta0) = black76_price_and_delta(f0, k, t, sigma, 0.0, 'C');
        let df = 0.005; // 10 ticks on HG
        let (theo1, _) = black76_price_and_delta(f0 + df, k, t, sigma, 0.0, 'C');
        let theo_taylor = theo0 + delta0 * df;
        // First-order error should be tiny (gamma × dF² / 2 ≪ 1bp on cheap option).
        assert!(
            (theo1 - theo_taylor).abs() < 1e-4,
            "Taylor mismatch: actual={:.6}, taylor={:.6}",
            theo1, theo_taylor
        );
    }

    #[test]
    fn black76_price_unchanged_after_refactor() {
        // Regression: confirm price-only path still matches the
        // combined path bit-for-bit (same d1/cdf chain).
        let cases = [
            (100.0, 90.0, 0.5, 0.20, 0.0, 'C'),
            (100.0, 110.0, 0.5, 0.20, 0.0, 'P'),
            (6.0, 5.95, 0.07, 0.30, 0.0, 'P'),
            (6.0, 6.10, 0.07, 0.30, 0.0, 'C'),
        ];
        for (f, k, t, sigma, r, right) in cases {
            let p_only = black76_price(f, k, t, sigma, r, right);
            let (p_pair, _d) = black76_price_and_delta(f, k, t, sigma, r, right);
            assert_eq!(p_only, p_pair, "mismatch f={} k={} t={} right={}", f, k, t, right);
        }
    }

    #[test]
    fn svi_intel_check() {
        // Reproduces the 2026-05-01 fit-forward test:
        // F_fit=6.021, K=5.6, T=0.07
        // svi_total_variance with these params should give ~0.00484
        let f = 6.021;
        let k = 5.6;
        let t = 25.5 / 365.0;
        let iv = svi_implied_vol(
            f, k, t,
            0.0019008499098876505,
            0.03656021179212421,
            -0.7899231280970652,
            -0.08124400811300346,
            0.07679654333384238,
        );
        // Should be ~0.253
        assert!((iv - 0.253).abs() < 0.005, "iv={}", iv);
    }
}
