//! Vol-surface fitter — periodic SABR refit + IPC publish.
//!
//! Per cycle (~60 s cadence):
//!   1. For each registered product:
//!      a. Walk options in market_data
//!      b. OTM-only filter: for each strike, take the OTM side's
//!         microprice (K ≥ F → call; K < F → put). Skips the ITM
//!         half of the chain, whose 80–100-tick bid-ask spreads on
//!         deep-ITM legs make IV inversion noisy and contaminate the
//!         LM fit (was pinning C-side ρ at +0.999 ~30% of the time
//!         under the prior per-side scheme).
//!      c. Group by expiry. ONE combined surface per (product,
//!         expiry) — PCP holds at the IV level, so OTM call and OTM
//!         put on opposite wings describe the same smile.
//!      d. For each expiry with ≥3 valid (strike, IV) pairs, run
//!         `corsair_pricing::calibrate::calibrate_sabr` with vega²
//!         weights (Lesniewski 2008).
//!      e. Cache the fit under BOTH (product, expiry, 'C') and
//!         (product, expiry, 'P') keys, and publish two identical
//!         "vol_surface" events (side="C", side="P"), so all
//!         downstream readers (build_chain_payload, ipc.rs theo
//!         enrichment, trader vol_surfaces map) keep their per-side
//!         lookup paths unchanged. On too-few-strikes or fit
//!         rejection emit a "vol_surface_failed" diagnostic instead.
//!
//! History: per-side fits were introduced 2026-05-05 to isolate
//! C/P microstructure asymmetries during one-sided flow. In practice
//! they instead amplified bid-ask noise on deep-ITM strikes (low
//! time-value, wide spreads) into divergent fit params — observed
//! 2026-05-06 with C-side ρ pinned at +0.999 / ν=0.6 on ~30% of
//! cycles while P-side fit ρ≈+0.25 / ν≈2.0 on identical underlying
//! data. The OTM-only single-fit scheme avoids the noisy data path
//! entirely; PCP gives us the missing wing for free.

use corsair_ipc::SHMServer;
use corsair_pricing::calibrate::calibrate_sabr;
use corsair_pricing::greeks::compute_greeks;
use corsair_pricing::implied_vol;
use serde::Serialize;
use std::sync::Arc;

use crate::runtime::Runtime;

#[derive(Debug, Serialize)]
struct VolSurfaceEvent<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    /// YYYYMMDD string
    expiry: String,
    /// "C" or "P". Both events carry identical params — the broker
    /// fits one combined surface per (product, expiry) and emits the
    /// pair so the trader's per-side `vol_surfaces` map populates
    /// without protocol changes.
    side: &'a str,
    /// Forward used for the fit (parity-implied if available, spot otherwise).
    forward: f64,
    /// Spot price (front-month underlying) observed AT FIT TIME. Used
    /// by the trader's Taylor reprice as the anchor for measuring spot
    /// drift since the fit: theo += delta × (current_spot − spot_at_fit).
    /// Distinct from `forward` — `forward` includes the static carry
    /// between front-month (HGK6) and the option's underlying month
    /// (HGM6) which is structurally ~$0.04–0.05; using `forward` as the
    /// Taylor anchor would conflate that carry with actual spot
    /// movement and shift theos by delta × carry every tick. With
    /// `spot_at_fit`, Taylor reflects only post-fit spot dynamics.
    spot_at_fit: f64,
    /// SABR params (carries `model` field for trader-side dispatch).
    params: SabrParams,
    /// Fit RMSE in IV space.
    rmse: f64,
    /// Number of (strike, IV) pairs the fit consumed.
    n_points: usize,
    /// Lowest strike used in the fit. Trader uses these as the
    /// calibrated-range bounds (audit item 6, spec §3.3): refuse to
    /// quote strikes outside [calibrated_min_k, calibrated_max_k]
    /// because SABR extrapolation past the fit's strike envelope is
    /// unreliable.
    calibrated_min_k: f64,
    calibrated_max_k: f64,
    /// ts_ns — unix epoch nanoseconds.
    ts_ns: u64,
}

#[derive(Debug, Serialize)]
struct SabrParams {
    /// "sabr" — required by `corsair_trader::messages::VolParams::model`
    /// (no default on that field, so deserialization fails without it).
    model: &'static str,
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
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    log::info!("ipc periodic_vol_surface: cadence 60s");
    // Skip the first immediate tick — let market_data accumulate
    // ticks before we try to fit.
    t.tick().await;
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    loop {
        t.tick().await;
        // SABR LM solve runs ~5–50 ms per expiry. With 2-3 expiries
        // per cycle that's enough sync compute to stall a worker —
        // defer to spawn_blocking so the hot tokio workers (cmd_pump,
        // tick fastpath) keep ticking.
        let runtime = runtime.clone();
        let server = server.clone();
        let _ = tokio::task::spawn_blocking(move || {
            fit_and_publish(&runtime, &server);
        })
        .await;
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
        let (forward, spot_at_fit, expiry_groups) = match snapshot_chain(runtime, &product) {
            Some(v) => v,
            None => continue,
        };
        // One combined fit per (product, expiry) on OTM-only data.
        // Cached + emitted under both side keys for back-compat with
        // per-side downstream readers (see file-level doc).
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

            // Vega² weights for residuals (Lesniewski 2008). ATM
            // strikes (high vega) carry most of the dollar pricing
            // sensitivity, so wing IV jitter shouldn't dominate the
            // loss. Black-76 vega is right-independent, so 'C' is a
            // placeholder — the same weight comes out for either side.
            let weights: Vec<f64> = ks
                .iter()
                .zip(ivs.iter())
                .map(|(&k, &iv)| {
                    let g = compute_greeks(forward, k, tte, iv, 0.0, 'C', 1.0);
                    let v = g.vega;
                    if v.is_finite() && v > 0.0 { v * v } else { 1e-9 }
                })
                .collect();

            // Min/max strike for the calibrated-range gate. Strikes
            // are sorted in build_chain_payload, but be defensive.
            let (min_k, max_k) = ks.iter().fold(
                (f64::INFINITY, f64::NEG_INFINITY),
                |(lo, hi), &k| (lo.min(k), hi.max(k)),
            );
            // beta=0.5 + max_rmse=0.05 mirrors the operational config
            // (now in runtime_v3.yaml; the legacy hg_v1_4_paper.yaml
            // was deleted 2026-05-05).
            let fit = calibrate_sabr(forward, tte, &ks, &ivs, Some(&weights), 0.5, 0.05);
            match fit {
                Some(f) => {
                    let entry = crate::runtime::VolSurfaceCacheEntry {
                        forward,
                        tte,
                        alpha: f.alpha,
                        beta: f.beta,
                        rho: f.rho,
                        nu: f.nu,
                    };
                    // Copy-on-write: clone the previous Arc's contents,
                    // insert under BOTH side keys, publish a new Arc.
                    // Readers holding the old Arc keep using their
                    // snapshot (no torn reads).
                    {
                        let mut guard = runtime.vol_surface_cache.lock().unwrap();
                        let mut next: std::collections::HashMap<_, _> = (**guard).clone();
                        next.insert(
                            (product.clone(), expiry_str.clone(), 'C'),
                            entry.clone(),
                        );
                        next.insert(
                            (product.clone(), expiry_str.clone(), 'P'),
                            entry,
                        );
                        *guard = std::sync::Arc::new(next);
                    }
                    // Emit one event per side with identical params.
                    // Trader keys vol_surfaces by (expiry, side); we
                    // populate both so its existing per-side lookup
                    // (and back-fallback) works without changes.
                    for side_label in ["C", "P"] {
                        let ev = VolSurfaceEvent {
                            ty: "vol_surface",
                            expiry: expiry_str.clone(),
                            side: side_label,
                            forward,
                            spot_at_fit,
                            params: SabrParams {
                                model: "sabr",
                                alpha: f.alpha,
                                beta: f.beta,
                                rho: f.rho,
                                nu: f.nu,
                            },
                            rmse: f.rmse,
                            n_points: f.n_points,
                            calibrated_min_k: min_k,
                            calibrated_max_k: max_k,
                            ts_ns: now_ns(),
                        };
                        if let Ok(body) = rmp_serde::to_vec_named(&ev) {
                            if !server.publish(&body) {
                                log::warn!("vol_surface ring full — dropped");
                            }
                        }
                    }
                    log::info!(
                        "vol_surface {} {} → α={:.3} ρ={:+.2} ν={:.2} rmse={:.4} (n={}, OTM-combined)",
                        product, expiry_str, f.alpha, f.rho, f.nu, f.rmse, f.n_points
                    );
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
/// the current market_data, OTM-only. Returns None when underlying
/// isn't populated yet.
///
/// OTM-only filter (2026-05-06): for each strike, take the call mid
/// when K ≥ F and the put mid when K < F. Skips deep-ITM data
/// (large bid-ask spread relative to time-value → unstable IV
/// inversion that previously contaminated per-side LM fits — see the
/// file-level doc).
///
/// Microprice for IV inversion: uses size-weighted price
/// `(bid×ask_size + ask×bid_size) / (bid_size+ask_size)` instead of
/// raw mid. Reflects queue imbalance — when one side has heavy
/// resting size, the price the market actually clears at is closer
/// to the lighter side.
fn snapshot_chain(
    runtime: &Arc<Runtime>,
    product: &str,
) -> Option<(f64, f64, std::collections::HashMap<String, Vec<(f64, f64, f64)>>)> {
    let md = runtime.market_data.lock().unwrap();
    let und_spot = md.underlying_price(product)?;
    if und_spot <= 0.0 {
        return None;
    }
    // Derive forward from the option chain via put-call parity: for
    // each strike where both C_mid and P_mid are valid,
    //   F ≈ K + (C_mid - P_mid)
    // The mean across strikes near the underlying spot gives a robust
    // estimate of the market-implied forward. This is critical when
    // the underlying we subscribed to (HGK6 front-month) differs from
    // the option's actual underlying (HGM6 next-month). Without this,
    // theo is systematically biased — calls too cheap, puts too rich
    // — by the contango/backwardation between contract months.
    //
    // Falls back to spot when fewer than 2 valid strikes are usable
    // (boot before chain populates, or extreme one-sided market).
    let forward = {
        // Build C_mid/P_mid map per (expiry, strike). Same strike
        // appears once per side; collect both then pair.
        use std::collections::HashMap as HM;
        let mut sides: HM<(chrono::NaiveDate, i64), (Option<f64>, Option<f64>)> = HM::new();
        for opt in md.options_for_product(product) {
            let Some(mid) = opt.mid() else { continue };
            if mid <= 0.0 {
                continue;
            }
            let key = (opt.expiry, (opt.strike * 10_000.0).round() as i64);
            let entry = sides.entry(key).or_insert((None, None));
            match opt.right {
                corsair_broker_api::Right::Call => entry.0 = Some(mid),
                corsair_broker_api::Right::Put => entry.1 = Some(mid),
            }
        }
        // Use only strikes within 5 nickels (±$0.25) of spot —
        // ATM/near-ATM where both sides are most liquid + parity
        // is least noisy.
        let mut samples: Vec<f64> = Vec::new();
        for ((_exp, k_i), (cm, pm)) in &sides {
            let k = (*k_i as f64) / 10_000.0;
            if (k - und_spot).abs() > 0.25 {
                continue;
            }
            if let (Some(c), Some(p)) = (cm, pm) {
                samples.push(k + (c - p));
            }
        }
        if samples.len() >= 2 {
            let parity_f = samples.iter().sum::<f64>() / (samples.len() as f64);
            // Sanity-check parity F against spot. There are TWO
            // structurally-different drift sources we must NOT
            // conflate:
            //   1. CARRY (legitimate, structural): options on HGM6
            //      price against HGM6, but we tick HGK6 (front-month)
            //      as our spot reference. The K6→M6 calendar spread
            //      is normally $0.04–0.05 in HG. Parity F correctly
            //      reflects that carry.
            //   2. BIAS from one-sided market mids (the failure
            //      mode that actually motivated this guard, observed
            //      2026-05-05 morning at $0.044 drift).
            //
            // We can't tell them apart from drift magnitude alone.
            // Original threshold of $0.025 sat right inside the normal
            // carry range, triggered constantly during US session,
            // forced spot fallback, fit RMSE blew out, fits got
            // rejected, no vol_surface events emitted.
            //
            // Threshold raised to $0.20 (400 ticks): wide enough to
            // absorb normal carry + most one-sided-mid bias episodes,
            // narrow enough to still catch parity computations that
            // are genuinely broken. The trader's MAX_FORWARD_DRIFT_TICKS
            // (100) catches the smaller bias on the hot path; this
            // guard catches catastrophic parity failures only.
            const PARITY_VS_SPOT_LIMIT_USD: f64 = 0.20;
            if (parity_f - und_spot).abs() > PARITY_VS_SPOT_LIMIT_USD {
                log::warn!(
                    "vol_surface[{}]: parity F={:.4} drifted {:+.4} from spot {:.4} \
                     (>$0.20 limit) — falling back to spot",
                    product,
                    parity_f,
                    parity_f - und_spot,
                    und_spot
                );
                und_spot
            } else {
                parity_f
            }
        } else {
            // Fall back to spot. Theo will be biased by carry but at
            // least the chain has *some* fit, and the bias decays
            // toward expiry as carry → 0.
            und_spot
        }
    };
    let now = chrono::Utc::now();
    let mut by_expiry: std::collections::HashMap<String, Vec<(f64, f64, f64)>> =
        std::collections::HashMap::new();
    for opt in md.options_for_product(product) {
        // OTM-only filter: K ≥ F → take the call's microprice; K < F
        // → take the put's. Each strike contributes exactly one IV
        // (its OTM-side inversion), so deep-ITM legs with low time
        // value and wide spreads never feed the LM.
        let take_this = match opt.right {
            corsair_broker_api::Right::Call => opt.strike >= forward,
            corsair_broker_api::Right::Put => opt.strike < forward,
        };
        if !take_this {
            continue;
        }
        // Microprice for IV inversion (Rec 2 expanded, 2026-05-05).
        // Skip dark / one-sided / inverted books — also strikes with
        // zero size on either side, since the displayed mid is stale
        // (no recent two-sided quote means the price isn't anchored
        // to live liquidity). Stale strikes were pulling fits high
        // during fast spot moves, contributing to the 5.90 P /
        // 6.05 C accumulation incident on 2026-05-05.
        if opt.bid <= 0.0 || opt.ask <= 0.0 || opt.bid >= opt.ask {
            continue;
        }
        if opt.bid_size == 0 || opt.ask_size == 0 {
            continue;
        }
        let total = (opt.bid_size + opt.ask_size) as f64;
        let price =
            (opt.bid * (opt.ask_size as f64) + opt.ask * (opt.bid_size as f64)) / total;
        if price <= 0.0 {
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
        let iv = match implied_vol(price, forward, opt.strike, tte, 0.0, right_str) {
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
    Some((forward, und_spot, by_expiry))
}

use crate::time::now_ns;
