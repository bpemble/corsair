// Rust hot-path math for Corsair v2.
//
// Mirrors src/pricing.py (PricingEngine.black76_price + .implied_vol).
// The Python module remains the source of truth for behavior; this
// crate must produce numerically-equivalent output. tests/test_pricing_parity.py
// asserts equivalence to ~1e-9 (Black-76 price) / ~1e-5 (implied vol) across
// 1000s of random inputs.
//
// Brent's method is hand-rolled to match scipy.optimize.brentq's
// classical algorithm (combination of bisection + secant + inverse
// quadratic interpolation, with the standard mflag five-condition
// fallback to bisection).

// Public so other Rust crates in the workspace (e.g., corsair_position,
// corsair_constraint) can call the math directly without going through
// PyO3. Python wrappers in lib.rs expose a subset of this surface.
pub mod calibrate;
pub mod greeks;
pub mod span;

use pyo3::prelude::*;
use statrs::distribution::{ContinuousCDF, Normal};

#[inline]
fn norm_cdf(x: f64) -> f64 {
    // Normal::new(0,1) is cheap (just stores mean+stddev). Constructing
    // per-call avoids the lazy-static / once_cell dance.
    Normal::new(0.0, 1.0).unwrap().cdf(x)
}

#[pyfunction]
#[pyo3(signature = (f, k, t, sigma, r=0.0, right="C"))]
fn black76_price(f: f64, k: f64, t: f64, sigma: f64, r: f64, right: &str) -> f64 {
    let is_call = right.eq_ignore_ascii_case("C");
    let r_char = if is_call { 'C' } else { 'P' };
    black76_price_inner(f, k, t, sigma, r, r_char)
}

/// Inner Black-76 used by span/greeks modules — takes a char to skip
/// the str-parse on every call. Identical numerics to black76_price.
#[inline]
pub fn black76_price_inner(f: f64, k: f64, t: f64, sigma: f64, r: f64, right: char) -> f64 {
    let is_call = right == 'C' || right == 'c';
    if t <= 0.0 || sigma <= 0.0 || f <= 0.0 || k <= 0.0 {
        return if is_call {
            (f - k).max(0.0)
        } else {
            (k - f).max(0.0)
        };
    }
    let sqrt_t = t.sqrt();
    let d1 = ((f / k).ln() + 0.5 * sigma * sigma * t) / (sigma * sqrt_t);
    let d2 = d1 - sigma * sqrt_t;
    let df = (-r * t).exp();
    if is_call {
        df * (f * norm_cdf(d1) - k * norm_cdf(d2))
    } else {
        df * (k * norm_cdf(-d2) - f * norm_cdf(-d1))
    }
}

#[pyfunction]
#[pyo3(signature = (market_price, f, k, t, r=0.0, right="C"))]
pub fn implied_vol(market_price: f64, f: f64, k: f64, t: f64, r: f64, right: &str) -> Option<f64> {
    if t <= 0.0 || market_price <= 0.0 {
        return None;
    }
    let is_call = right.eq_ignore_ascii_case("C");
    let intrinsic = if is_call {
        (f - k).max(0.0)
    } else {
        (k - f).max(0.0)
    };
    let df = (-r * t).exp();
    if market_price < df * intrinsic - 1e-10 {
        return None;
    }
    let objective = |sigma: f64| black76_price(f, k, t, sigma, r, right) - market_price;
    brent(&objective, 0.01, 5.0, 1e-6, 100)
}

/// Brent's method for univariate root-finding. Mirrors
/// `scipy.optimize.brentq(f, a, b, xtol=tol, maxiter=max_iter)`.
///
/// Returns None if the bracket [a, b] does not contain a root
/// (i.e., f(a) and f(b) have the same sign), matching scipy's
/// behavior of raising ValueError in the same case (which the
/// Python wrapper catches and converts to None).
fn brent<F: Fn(f64) -> f64>(f: &F, mut a: f64, mut b: f64, xtol: f64, max_iter: usize) -> Option<f64> {
    let mut fa = f(a);
    let mut fb = f(b);
    if fa * fb >= 0.0 {
        return None;
    }
    if fa.abs() < fb.abs() {
        std::mem::swap(&mut a, &mut b);
        std::mem::swap(&mut fa, &mut fb);
    }
    let mut c = a;
    let mut fc = fa;
    let mut d = a; // initial value unused — first iteration is mflag=true so the !mflag branch can't reach d
    let mut mflag = true;

    for _ in 0..max_iter {
        if (b - a).abs() < xtol {
            return Some(b);
        }
        if fb == 0.0 {
            return Some(b);
        }

        let s_iqi_or_secant = if fa != fc && fb != fc {
            // Inverse quadratic interpolation.
            a * fb * fc / ((fa - fb) * (fa - fc))
                + b * fa * fc / ((fb - fa) * (fb - fc))
                + c * fa * fb / ((fc - fa) * (fc - fb))
        } else {
            // Secant fallback when f-values would cause divide-by-zero in IQI.
            b - fb * (b - a) / (fb - fa)
        };

        // Five-condition fallback to bisection (Brent's classical safeguard).
        let lo = (3.0 * a + b) / 4.0;
        let hi = b;
        let between = if lo <= hi {
            s_iqi_or_secant >= lo && s_iqi_or_secant <= hi
        } else {
            s_iqi_or_secant >= hi && s_iqi_or_secant <= lo
        };
        let cond1 = !between;
        let cond2 = mflag && (s_iqi_or_secant - b).abs() >= (b - c).abs() / 2.0;
        let cond3 = !mflag && (s_iqi_or_secant - b).abs() >= (c - d).abs() / 2.0;
        let cond4 = mflag && (b - c).abs() < xtol;
        let cond5 = !mflag && (c - d).abs() < xtol;
        let s = if cond1 || cond2 || cond3 || cond4 || cond5 {
            mflag = true;
            (a + b) / 2.0
        } else {
            mflag = false;
            s_iqi_or_secant
        };

        let fs = f(s);
        d = c;
        c = b;
        fc = fb;
        if fa * fs < 0.0 {
            b = s;
            fb = fs;
        } else {
            a = s;
            fa = fs;
        }
        if fa.abs() < fb.abs() {
            std::mem::swap(&mut a, &mut b);
            std::mem::swap(&mut fa, &mut fb);
        }
    }
    Some(b)
}

// ─────────────────────────────────────────────────────────────────────
// Phase 6 (mm_service_split.md): SABR implied vol + the trader's
// decide_quote on the same crate. Mirrors src/sabr.py:sabr_implied_vol
// and src/trader/quote_decision.py:decide. Parity tests in
// tests/test_pricing_parity.py and tests/test_decision_parity.py.

/// Hagan 2002 SABR implied vol approximation.
///
/// Mirrors `src/sabr.py:sabr_implied_vol`. ATM is detected at
/// SVI raw total variance: w(k) = a + b * (rho * (k - m) + sqrt((k - m)^2 + sigma^2))
/// where k = log(K/F). Mirrors src/sabr.py:svi_total_variance for parity.
#[inline(always)]
pub(crate) fn svi_total_variance_inner(k: f64, a: f64, b: f64, rho: f64, m: f64, sigma: f64) -> f64 {
    let dk = k - m;
    a + b * (rho * dk + (dk * dk + sigma * sigma).sqrt())
}

/// SVI implied vol from log-moneyness — mirrors src/sabr.py:svi_implied_vol.
/// Returns 0.0 for invalid inputs and 0.001 for non-positive variance
/// (matches Python's safe-floor behavior). The 0.001 floor is what
/// keeps Black76 from blowing up on early-fit junk parameters.
#[pyfunction]
#[pyo3(signature = (f, k_strike, t, a, b, rho, m, sigma))]
fn svi_implied_vol(
    f: f64, k_strike: f64, t: f64,
    a: f64, b: f64, rho: f64, m: f64, sigma: f64,
) -> f64 {
    if t <= 0.0 || k_strike <= 0.0 || f <= 0.0 {
        return 0.0;
    }
    let k = (k_strike / f).ln();
    let w = svi_total_variance_inner(k, a, b, rho, m, sigma);
    if w <= 0.0 {
        return 0.001;
    }
    (w / t).sqrt()
}

/// |F-K| < 1e-7 * F (matches the Python `eps = 1e-7` branch).
#[pyfunction]
pub fn sabr_implied_vol(
    f: f64, k: f64, t: f64,
    alpha: f64, beta: f64, rho: f64, nu: f64,
) -> f64 {
    if t <= 0.0 || alpha <= 0.0 || f <= 0.0 || k <= 0.0 {
        return if alpha > 0.0 { alpha } else { 0.01 };
    }
    let eps = 1e-7;

    // ATM
    if (f - k).abs() < eps * f {
        let fmid = f;
        let fmid_beta = fmid.powf(1.0 - beta);
        let term1 = alpha / fmid_beta;
        let p1 = ((1.0 - beta).powi(2) / 24.0) * alpha * alpha
            / fmid.powf(2.0 - 2.0 * beta);
        let p2 = 0.25 * rho * beta * nu * alpha / fmid_beta;
        let p3 = (2.0 - 3.0 * rho * rho) * nu * nu / 24.0;
        return term1 * (1.0 + (p1 + p2 + p3) * t);
    }

    // General case
    let fk = f * k;
    let fk_beta = fk.powf((1.0 - beta) / 2.0);
    let log_fk = (f / k).ln();

    let z = (nu / alpha) * fk_beta * log_fk;
    let xz = if z.abs() < eps {
        1.0
    } else {
        let sqrt_term = (1.0 - 2.0 * rho * z + z * z).sqrt();
        z / ((sqrt_term + z - rho) / (1.0 - rho)).ln()
    };

    let one_minus_beta = 1.0 - beta;
    let denom1 = fk_beta
        * (1.0
            + one_minus_beta.powi(2) / 24.0 * log_fk.powi(2)
            + one_minus_beta.powi(4) / 1920.0 * log_fk.powi(4));
    let p1 = one_minus_beta.powi(2) / 24.0 * alpha * alpha
        / fk.powf(one_minus_beta);
    let p2 = 0.25 * rho * beta * nu * alpha / fk_beta;
    let p3 = (2.0 - 3.0 * rho * rho) * nu * nu / 24.0;

    (alpha / denom1) * xz * (1.0 + (p1 + p2 + p3) * t)
}

/// Trader's quote decision (Option B per mm_service_split). Returns a
/// dict mirroring src/trader/quote_decision.py:decide.
///
/// Args mirror the Python signature; vol_params is a Python dict with
/// either SABR keys (alpha, beta, rho, nu) or SVI keys (a, b, rho, m, sigma).
/// Only SABR is wired today — SVI returns a "skip" with reason
/// "model_not_in_rust" and the caller (Python) handles SVI in pure
/// Python until the SVI port lands.
#[pyfunction]
#[pyo3(signature = (forward, strike, expiry, right, side, vol_params,
                    market_bid, market_ask, min_edge_ticks, tick_size, tte))]
fn decide_quote(
    py: Python<'_>,
    forward: f64,
    strike: f64,
    expiry: &str,
    right: &str,
    side: &str,
    vol_params: &Bound<'_, PyAny>,
    market_bid: Option<f64>,
    market_ask: Option<f64>,
    min_edge_ticks: i32,
    tick_size: f64,
    tte: f64,
) -> PyResult<PyObject> {
    let dict = pyo3::types::PyDict::new_bound(py);
    dict.set_item("side", side)?;
    dict.set_item("strike", strike)?;
    dict.set_item("expiry", expiry)?;
    dict.set_item("right", right)?;
    dict.set_item("price", py.None())?;
    dict.set_item("theo", py.None())?;
    dict.set_item("iv", py.None())?;

    if vol_params.is_none() {
        dict.set_item("action", "skip")?;
        dict.set_item("reason", "no_vol_surface")?;
        return Ok(dict.into());
    }

    // Extract model field; SABR + SVI now both handled in Rust.
    let model: String = vol_params
        .get_item("model")
        .ok()
        .and_then(|v| v.extract().ok())
        .unwrap_or_default();
    let iv = if model == "sabr" {
        let alpha: f64 = vol_params.get_item("alpha")?.extract()?;
        let beta: f64 = vol_params.get_item("beta")?.extract()?;
        let p_rho: f64 = vol_params.get_item("rho")?.extract()?;
        let nu: f64 = vol_params.get_item("nu")?.extract()?;
        sabr_implied_vol(forward, strike, tte, alpha, beta, p_rho, nu)
    } else if model == "svi" {
        // SVI: a + b * (rho * (k - m) + sqrt((k-m)^2 + sigma^2)),
        // where k = log(K/F). Caller MUST pass fit-time forward (not
        // current spot) — see src/trader/quote_decision.py docstring.
        let a: f64 = vol_params.get_item("a")?.extract()?;
        let b: f64 = vol_params.get_item("b")?.extract()?;
        let p_rho: f64 = vol_params.get_item("rho")?.extract()?;
        let m: f64 = vol_params.get_item("m")?.extract()?;
        let sigma: f64 = vol_params.get_item("sigma")?.extract()?;
        svi_implied_vol(forward, strike, tte, a, b, p_rho, m, sigma)
    } else {
        // Unknown model — punt back to Python.
        dict.set_item("action", "skip")?;
        dict.set_item("reason", "model_not_in_rust")?;
        return Ok(dict.into());
    };

    if iv <= 0.0 || iv.is_nan() {
        dict.set_item("action", "skip")?;
        dict.set_item("reason", "iv_invalid")?;
        return Ok(dict.into());
    }
    let theo = black76_price(forward, strike, tte, iv, 0.0, right);
    if theo <= 0.0 {
        dict.set_item("action", "skip")?;
        dict.set_item("reason", "theo_unavailable")?;
        return Ok(dict.into());
    }
    dict.set_item("iv", iv)?;
    dict.set_item("theo", theo)?;

    // CRITICAL: require a TWO-SIDED market before quoting.
    //
    // Option markets at thin strikes (deep ITM/OTM) often have only
    // one side displayed. If we BUY at theo-edge into a one-sided
    // ask-only market, our limit can become the SOLE bid in the
    // book — when an MM later dumps inventory, our order fills at
    // an unfavorable price even though the snapshot we acted on
    // looked sane. 21 trader-driven fills on 2026-05-01 (01:26 +
    // 01:43) had this exact shape: BUY HXEM6 deep ITM puts at theo
    // prices, then market thinned, fills landed at bid=0/ask=0
    // states with edge -$300 to -$850 each.
    //
    // Stricter rule: BOTH bid AND ask must be live before placing
    // either side. This eliminates one-sided liquidity traps at
    // the cost of missing some quoting opportunities at the wings.
    // Acceptable trade-off — the wings weren't giving us positive
    // edge anyway.
    let bid_live = market_bid.map(|b| b > 0.0).unwrap_or(false);
    let ask_live = market_ask.map(|a| a > 0.0).unwrap_or(false);
    if !bid_live || !ask_live {
        dict.set_item("action", "skip")?;
        dict.set_item("reason", "one_sided_or_dark")?;
        return Ok(dict.into());
    }

    let edge = (min_edge_ticks as f64) * tick_size;

    let result = match side {
        "BUY" => {
            let target = theo - edge;
            if target <= 0.0 {
                ("skip", "target_nonpositive", None::<f64>)
            } else if let Some(ask) = market_ask {
                if ask > 0.0 && target >= ask {
                    ("skip", "would_cross_ask", Some(target))
                } else {
                    let q = (target / tick_size).round() * tick_size;
                    ("place", "edge_below_theo", Some(q))
                }
            } else {
                let q = (target / tick_size).round() * tick_size;
                ("place", "edge_below_theo", Some(q))
            }
        }
        "SELL" => {
            let target = theo + edge;
            if let Some(bid) = market_bid {
                if bid > 0.0 && target <= bid {
                    ("skip", "would_cross_bid", Some(target))
                } else {
                    let q = (target / tick_size).round() * tick_size;
                    ("place", "edge_above_theo", Some(q))
                }
            } else {
                let q = (target / tick_size).round() * tick_size;
                ("place", "edge_above_theo", Some(q))
            }
        }
        _ => ("skip", "unknown_side", None::<f64>),
    };
    dict.set_item("action", result.0)?;
    dict.set_item("reason", result.1)?;
    if let Some(p) = result.2 {
        dict.set_item("price", p)?;
    }
    Ok(dict.into())
}

/// Calibrate SABR (alpha, rho, nu) given fixed beta. Mirrors
/// src/sabr.py:calibrate_sabr.
///
/// Returns a dict with keys (alpha, beta, rho, nu, rmse, n_points)
/// on success, None when n < 3, T/F invalid, all initial guesses
/// diverge, or the best RMSE exceeds max_rmse.
#[pyfunction]
#[pyo3(signature = (forward, tte, strikes, market_ivs,
                    beta=0.5, max_rmse=0.03, weights=None))]
fn calibrate_sabr(
    py: Python<'_>,
    forward: f64,
    tte: f64,
    strikes: Vec<f64>,
    market_ivs: Vec<f64>,
    beta: f64,
    max_rmse: f64,
    weights: Option<Vec<f64>>,
) -> PyResult<Option<PyObject>> {
    let w_ref = weights.as_deref();
    let fit = calibrate::calibrate_sabr(
        forward, tte, &strikes, &market_ivs, w_ref, beta, max_rmse,
    );
    match fit {
        None => Ok(None),
        Some(p) => {
            let dict = pyo3::types::PyDict::new_bound(py);
            dict.set_item("alpha", p.alpha)?;
            dict.set_item("beta", p.beta)?;
            dict.set_item("rho", p.rho)?;
            dict.set_item("nu", p.nu)?;
            dict.set_item("rmse", p.rmse)?;
            dict.set_item("n_points", p.n_points)?;
            Ok(Some(dict.into()))
        }
    }
}

/// Calibrate SVI (a, b, rho, m, sigma). Mirrors src/sabr.py:calibrate_svi.
#[pyfunction]
#[pyo3(signature = (forward, tte, strikes, market_ivs,
                    max_rmse=0.03, weights=None))]
fn calibrate_svi(
    py: Python<'_>,
    forward: f64,
    tte: f64,
    strikes: Vec<f64>,
    market_ivs: Vec<f64>,
    max_rmse: f64,
    weights: Option<Vec<f64>>,
) -> PyResult<Option<PyObject>> {
    let w_ref = weights.as_deref();
    let fit = calibrate::calibrate_svi(forward, tte, &strikes, &market_ivs, w_ref, max_rmse);
    match fit {
        None => Ok(None),
        Some(p) => {
            let dict = pyo3::types::PyDict::new_bound(py);
            dict.set_item("a", p.a)?;
            dict.set_item("b", p.b)?;
            dict.set_item("rho", p.rho)?;
            dict.set_item("m", p.m)?;
            dict.set_item("sigma", p.sigma)?;
            dict.set_item("rmse", p.rmse)?;
            dict.set_item("n_points", p.n_points)?;
            Ok(Some(dict.into()))
        }
    }
}

/// Black-76 Greeks for one option contract. Mirrors
/// src/greeks.py:GreeksCalculator.calculate.
///
/// Returns a dict with keys (delta, gamma, theta, vega, iv).
/// gamma/theta/vega are dollar-denominated via the multiplier;
/// theta is per-calendar-day (matches the Python /365 convention).
#[pyfunction]
#[pyo3(signature = (f, k, t, sigma, r=0.0, right="C", multiplier=50.0))]
fn compute_greeks(
    py: Python<'_>,
    f: f64, k: f64, t: f64, sigma: f64,
    r: f64, right: &str, multiplier: f64,
) -> PyResult<PyObject> {
    let r_char = right.chars().next().unwrap_or('C').to_ascii_uppercase();
    let g = greeks::compute_greeks(f, k, t, sigma, r, r_char, multiplier);
    let dict = pyo3::types::PyDict::new_bound(py);
    dict.set_item("delta", g.delta)?;
    dict.set_item("gamma", g.gamma)?;
    dict.set_item("theta", g.theta)?;
    dict.set_item("vega", g.vega)?;
    dict.set_item("iv", g.iv)?;
    Ok(dict.into())
}

/// 16-element risk array (loss $ per LONG contract per scenario).
/// Mirrors src/synthetic_span.py:position_risk_array. Returned as a
/// Python list; convert to numpy on the call site if you need it.
#[pyfunction]
#[pyo3(signature = (f, k, t, iv, right,
                    up_scan_pct, down_scan_pct, vol_scan_pct,
                    extreme_mult, extreme_cover, multiplier))]
#[allow(clippy::too_many_arguments)]
fn span_position_risk_array(
    py: Python<'_>,
    f: f64, k: f64, t: f64, iv: f64, right: &str,
    up_scan_pct: f64, down_scan_pct: f64, vol_scan_pct: f64,
    extreme_mult: f64, extreme_cover: f64, multiplier: f64,
) -> PyResult<PyObject> {
    let cfg = span::SpanConfig {
        up_scan_pct, down_scan_pct, vol_scan_pct,
        extreme_mult, extreme_cover,
        short_option_minimum: 0.0, // unused for single-position scan
        multiplier,
    };
    let r_char = right.chars().next().unwrap_or('C').to_ascii_uppercase();
    let arr = span::position_risk_array(f, k, t, iv, r_char, &cfg);
    let list = pyo3::types::PyList::new_bound(py, arr.iter());
    Ok(list.into())
}

/// Portfolio SPAN margin. positions = list of (strike, right, T,
/// iv, signed_qty) tuples. Mirrors src/synthetic_span.py:portfolio_margin.
#[pyfunction]
#[pyo3(signature = (f, positions,
                    up_scan_pct, down_scan_pct, vol_scan_pct,
                    extreme_mult, extreme_cover,
                    short_option_minimum, multiplier))]
#[allow(clippy::too_many_arguments)]
fn span_portfolio_margin(
    py: Python<'_>,
    f: f64,
    positions: Vec<(f64, String, f64, f64, i64)>,
    up_scan_pct: f64, down_scan_pct: f64, vol_scan_pct: f64,
    extreme_mult: f64, extreme_cover: f64,
    short_option_minimum: f64, multiplier: f64,
) -> PyResult<PyObject> {
    let cfg = span::SpanConfig {
        up_scan_pct, down_scan_pct, vol_scan_pct,
        extreme_mult, extreme_cover,
        short_option_minimum, multiplier,
    };
    let positions_inner: Vec<(f64, char, f64, f64, i64)> = positions
        .into_iter()
        .map(|(k, right, t, iv, q)| {
            let c = right.chars().next().unwrap_or('C').to_ascii_uppercase();
            (k, c, t, iv, q)
        })
        .collect();
    let m = span::portfolio_margin(f, &positions_inner, &cfg);
    let dict = pyo3::types::PyDict::new_bound(py);
    dict.set_item("scan_risk", m.scan_risk)?;
    dict.set_item("net_option_value", m.net_option_value)?;
    dict.set_item("long_premium", m.long_premium)?;
    dict.set_item("short_minimum", m.short_minimum)?;
    dict.set_item("total_margin", m.total_margin)?;
    dict.set_item("worst_scenario", m.worst_scenario)?;
    Ok(dict.into())
}

#[pymodule]
fn corsair_pricing(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(black76_price, m)?)?;
    m.add_function(wrap_pyfunction!(implied_vol, m)?)?;
    m.add_function(wrap_pyfunction!(sabr_implied_vol, m)?)?;
    m.add_function(wrap_pyfunction!(svi_implied_vol, m)?)?;
    m.add_function(wrap_pyfunction!(decide_quote, m)?)?;
    m.add_function(wrap_pyfunction!(calibrate_sabr, m)?)?;
    m.add_function(wrap_pyfunction!(calibrate_svi, m)?)?;
    m.add_function(wrap_pyfunction!(compute_greeks, m)?)?;
    m.add_function(wrap_pyfunction!(span_position_risk_array, m)?)?;
    m.add_function(wrap_pyfunction!(span_portfolio_margin, m)?)?;
    Ok(())
}
