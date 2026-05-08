// Black-76 futures-options Greeks — mirrors src/greeks.py exactly.
//
// Returns per-contract delta + dollar-denominated gamma, theta, vega.
// Theta is expressed per-calendar-day (matching the /365 in Python).
//
// Hot path: called on every fill (position book update) and every risk
// cycle (5min). Python costs ~80–100 μs per call due to scipy.stats.norm
// overhead; Rust is sub-microsecond.

use statrs::distribution::{ContinuousCDF, Normal};
use std::sync::OnceLock;

/// Cached standard normal. `Normal::new(0.0, 1.0)` allocates + validates
/// on every call; this OnceLock cache mirrors `lib.rs::STD_NORMAL` and
/// keeps the hot path allocation-free. Audit round 2.
static STD_NORMAL: OnceLock<Normal> = OnceLock::new();

#[inline]
fn norm_cdf(x: f64) -> f64 {
    STD_NORMAL
        .get_or_init(|| Normal::new(0.0, 1.0).unwrap())
        .cdf(x)
}

#[inline]
fn norm_pdf(x: f64) -> f64 {
    // Standard normal pdf: 1/√(2π) · e^(-x²/2)
    let inv_sqrt_2pi = 0.398_942_280_401_432_7_f64;
    inv_sqrt_2pi * (-0.5 * x * x).exp()
}

#[derive(Debug, Clone, Copy)]
pub struct GreeksOut {
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
    pub iv: f64,
}

pub fn compute_greeks(
    f: f64,
    k: f64,
    t: f64,
    sigma: f64,
    r: f64,
    right: char,
    multiplier: f64,
) -> GreeksOut {
    let is_call = right == 'C' || right == 'c';

    if t < 1e-10 || sigma <= 0.0 || f <= 0.0 || k <= 0.0 {
        let mut intrinsic_delta = if f > k {
            1.0
        } else if f < k {
            0.0
        } else {
            0.5
        };
        if !is_call {
            intrinsic_delta -= 1.0;
        }
        return GreeksOut {
            delta: intrinsic_delta,
            gamma: 0.0,
            theta: 0.0,
            vega: 0.0,
            iv: sigma,
        };
    }

    let discount = (-r * t).exp();
    let sqrt_t = t.sqrt();
    let d1 = ((f / k).ln() + 0.5 * sigma * sigma * t) / (sigma * sqrt_t);
    let d2 = d1 - sigma * sqrt_t;

    let n_d1 = norm_pdf(d1);
    let big_n_d1 = norm_cdf(d1);
    let big_n_d2 = norm_cdf(d2);

    // Delta
    let delta = if is_call {
        discount * big_n_d1
    } else {
        discount * (big_n_d1 - 1.0)
    };

    // Gamma (dollar)
    let gamma = discount * n_d1 / (f * sigma * sqrt_t) * multiplier;

    // Theta (dollar/day)
    let common = -discount * f * n_d1 * sigma / (2.0 * sqrt_t);
    let theta = if is_call {
        (common - r * k * discount * big_n_d2) / 365.0
    } else {
        (common + r * k * discount * norm_cdf(-d2)) / 365.0
    };
    let theta_dollar = theta * multiplier;

    // Vega (per 1% vol move, dollar)
    let vega = f * discount * n_d1 * sqrt_t / 100.0;
    let vega_dollar = vega * multiplier;

    if !delta.is_finite() || !gamma.is_finite() || !theta_dollar.is_finite()
        || !vega_dollar.is_finite()
    {
        let mut intrinsic_delta = if f > k {
            1.0
        } else if f < k {
            0.0
        } else {
            0.5
        };
        if !is_call {
            intrinsic_delta -= 1.0;
        }
        return GreeksOut {
            delta: intrinsic_delta,
            gamma: 0.0,
            theta: 0.0,
            vega: 0.0,
            iv: sigma,
        };
    }

    GreeksOut {
        delta,
        gamma,
        theta: theta_dollar,
        vega: vega_dollar,
        iv: sigma,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atm_call_delta_about_half_discount() {
        // F=K, T=0.05, sigma=0.5: d1 ≈ 0.0559 → N(d1) ≈ 0.522 (positive
        // half-life skew); discount=1 (r=0). So delta is just above 0.5.
        let g = compute_greeks(6.0, 6.0, 0.05, 0.5, 0.0, 'C', 25_000.0);
        assert!(g.delta > 0.5 && g.delta < 0.6, "delta={}", g.delta);
    }

    #[test]
    fn atm_put_delta_about_negative_half() {
        let g = compute_greeks(6.0, 6.0, 0.05, 0.5, 0.0, 'P', 25_000.0);
        assert!(g.delta < -0.4 && g.delta > -0.5, "delta={}", g.delta);
    }

    #[test]
    fn deep_otm_call_delta_near_zero() {
        let g = compute_greeks(6.0, 9.0, 0.05, 0.3, 0.0, 'C', 25_000.0);
        assert!(g.delta < 0.01, "deep OTM call delta={}", g.delta);
    }

    #[test]
    fn deep_itm_call_delta_near_discount() {
        let g = compute_greeks(6.0, 4.0, 0.05, 0.3, 0.0, 'C', 25_000.0);
        // discount=1 (r=0), so delta should approach 1.0
        assert!(g.delta > 0.95, "deep ITM call delta={}", g.delta);
    }

    #[test]
    fn vega_positive_for_call_and_put() {
        let gc = compute_greeks(6.0, 6.0, 0.05, 0.4, 0.0, 'C', 25_000.0);
        let gp = compute_greeks(6.0, 6.0, 0.05, 0.4, 0.0, 'P', 25_000.0);
        assert!(gc.vega > 0.0 && gp.vega > 0.0);
        assert!((gc.vega - gp.vega).abs() < 1e-6); // put-call vega parity
    }

    #[test]
    fn invalid_inputs_return_intrinsic_delta() {
        let g = compute_greeks(6.0, 6.0, 0.0, 0.5, 0.0, 'C', 25_000.0);
        assert_eq!(g.delta, 0.5); // F == K → 0.5 intrinsic delta
        assert_eq!(g.gamma, 0.0);
        assert_eq!(g.vega, 0.0);
    }
}
