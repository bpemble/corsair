//! Vol-surface fitter — periodic SABR refit + IPC publish.
//!
//! Mirrors what today's Python `src/sabr.py::MultiExpirySABR` does
//! at runtime, in a stripped-down Rust form sufficient for the
//! trader's needs.
//!
//! Per cycle (~60s cadence):
//!   1. For each registered product:
//!      a. Walk options in market_data
//!      b. For each option with bid+ask, compute mid, solve IV via
//!         corsair_pricing::implied_vol (Brent on Black-76)
//!      c. Group by expiry
//!      d. For each expiry with ≥3 valid (strike, IV) pairs, run
//!         corsair_pricing::calibrate::calibrate_sabr
//!      e. Publish a "vol_surface" event with the fit params
//!
//! Skip:
//!   - Per-side (C vs P) fitting — fits one combined surface per
//!     expiry (Python is per-side; Rust simpler version is OK for
//!     shadow validation).
//!   - SVI fitting — SABR only for now.
//!   - Quality-rejection RMSE thresholds — accepts any fit.
//!
//! Phase 6+ work: rebuild full Python parity (per-side fits, SVI,
//! quality gates, fast-recal triggers, async fit pool).

use corsair_ipc::SHMServer;
use corsair_pricing::calibrate::calibrate_sabr;
use corsair_pricing::implied_vol;
use serde::Serialize;
use std::sync::Arc;

use crate::runtime::Runtime;

#[derive(Debug, Serialize)]
struct VolSurfaceEvent<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    /// "sabr"
    model: &'a str,
    /// YYYYMMDD string
    expiry: String,
    /// Side: "C", "P", or "BOTH" (combined fit).
    side: &'a str,
    /// Forward used for the fit (parity-implied if available, spot otherwise).
    forward: f64,
    /// SABR params.
    params: SabrParams,
    /// Fit RMSE in IV space.
    rmse: f64,
    /// Number of (strike, IV) pairs the fit consumed.
    n_points: usize,
    /// ts_ns — unix epoch nanoseconds.
    ts_ns: u64,
}

#[derive(Debug, Serialize)]
struct SabrParams {
    alpha: f64,
    beta: f64,
    rho: f64,
    nu: f64,
}

#[derive(Debug, Serialize)]
struct VolSurfaceFailedEvent<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    expiry: String,
    reason: &'a str,
    n_strikes: usize,
    ts_ns: u64,
}

/// Spawn the periodic vol-surface fitter task. Cadence is 60s by
/// default; trader's behavior tracking depends on getting a fresh
/// surface within a few minutes.
pub fn spawn_vol_surface(runtime: Arc<Runtime>, server: Arc<SHMServer>) {
    tokio::spawn(periodic_vol_surface(runtime, server));
}

async fn periodic_vol_surface(runtime: Arc<Runtime>, server: Arc<SHMServer>) {
    let mut t = tokio::time::interval(std::time::Duration::from_secs(60));
    log::info!("ipc periodic_vol_surface: cadence 60s");
    // Skip the first immediate tick — let market_data accumulate
    // ticks before we try to fit.
    t.tick().await;
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    loop {
        t.tick().await;
        fit_and_publish(&runtime, &server);
    }
}

fn fit_and_publish(runtime: &Arc<Runtime>, server: &Arc<SHMServer>) {
    // Snapshot under lock then release before doing CPU work.
    let products: Vec<String> = runtime
        .portfolio
        .lock()
        .unwrap()
        .registry()
        .products();

    for product in products {
        let (forward, expiry_groups) = match snapshot_chain(runtime, &product) {
            Some(v) => v,
            None => continue,
        };
        for (expiry_str, strikes_with_iv) in expiry_groups {
            if strikes_with_iv.len() < 3 {
                let _ = server.publish(
                    &rmp_serde::to_vec_named(&VolSurfaceFailedEvent {
                        ty: "vol_surface_failed",
                        expiry: expiry_str,
                        reason: "too_few_strikes",
                        n_strikes: strikes_with_iv.len(),
                        ts_ns: now_ns(),
                    })
                    .unwrap_or_default(),
                );
                continue;
            }
            let tte = match strikes_with_iv.first() {
                Some((_, _, tte_y)) if *tte_y > 0.0 => *tte_y,
                _ => continue,
            };
            let ks: Vec<f64> = strikes_with_iv.iter().map(|(k, _, _)| *k).collect();
            let ivs: Vec<f64> = strikes_with_iv.iter().map(|(_, iv, _)| *iv).collect();
            // beta=0.5 + max_rmse=0.05 mirrors the operational config
            // (config/hg_v1_4_paper.yaml).
            let fit = calibrate_sabr(forward, tte, &ks, &ivs, None, 0.5, 0.05);
            match fit {
                Some(f) => {
                    let ev = VolSurfaceEvent {
                        ty: "vol_surface",
                        model: "sabr",
                        expiry: expiry_str.clone(),
                        side: "BOTH",
                        forward,
                        params: SabrParams {
                            alpha: f.alpha,
                            beta: f.beta,
                            rho: f.rho,
                            nu: f.nu,
                        },
                        rmse: f.rmse,
                        n_points: f.n_points,
                        ts_ns: now_ns(),
                    };
                    if let Ok(body) = rmp_serde::to_vec_named(&ev) {
                        if !server.publish(&body) {
                            log::warn!("vol_surface ring full — dropped");
                        } else {
                            log::info!(
                                "vol_surface {} {} → α={:.3} ρ={:+.2} ν={:.2} rmse={:.4} (n={})",
                                product, expiry_str, f.alpha, f.rho, f.nu, f.rmse, f.n_points
                            );
                        }
                    }
                }
                None => {
                    let _ = server.publish(
                        &rmp_serde::to_vec_named(&VolSurfaceFailedEvent {
                            ty: "vol_surface_failed",
                            expiry: expiry_str,
                            reason: "fit_rejected",
                            n_strikes: strikes_with_iv.len(),
                            ts_ns: now_ns(),
                        })
                        .unwrap_or_default(),
                    );
                }
            }
        }
    }
}

/// For a product, build a per-expiry list of (strike, iv, tte) from
/// the current market_data. Returns None when underlying isn't
/// populated yet.
fn snapshot_chain(
    runtime: &Arc<Runtime>,
    product: &str,
) -> Option<(f64, std::collections::HashMap<String, Vec<(f64, f64, f64)>>)> {
    let md = runtime.market_data.lock().unwrap();
    let forward = md.underlying_price(product)?;
    if forward <= 0.0 {
        return None;
    }
    let now = chrono::Utc::now();
    let mut by_expiry: std::collections::HashMap<String, Vec<(f64, f64, f64)>> =
        std::collections::HashMap::new();
    for opt in md.options_for_product(product) {
        let mid = opt.mid()?;
        if mid <= 0.0 {
            continue;
        }
        // TTE in years (CME 365.25-day convention).
        let expiry_dt = chrono::NaiveDateTime::new(
            opt.expiry,
            chrono::NaiveTime::from_hms_opt(20, 0, 0).unwrap(),
        );
        let secs = (chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(expiry_dt, chrono::Utc)
            - now)
            .num_seconds() as f64;
        if secs <= 0.0 {
            continue;
        }
        let tte = secs / (365.25 * 24.0 * 3600.0);
        let right_str = match opt.right {
            corsair_broker_api::Right::Call => "C",
            corsair_broker_api::Right::Put => "P",
        };
        let iv = match implied_vol(mid, forward, opt.strike, tte, 0.0, right_str) {
            Some(v) if v > 0.001 && v < 5.0 => v,
            _ => continue,
        };
        let expiry_str = opt.expiry.format("%Y%m%d").to_string();
        by_expiry
            .entry(expiry_str)
            .or_default()
            .push((opt.strike, iv, tte));
    }
    if by_expiry.is_empty() {
        return None;
    }
    Some((forward, by_expiry))
}

fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
