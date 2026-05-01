//! Decision flow — port of Python's `_decide_on_tick` in
//! src/trader/main.py. Single function, all gates inline for cache
//! friendliness.
//!
//! Returns a `Decision` enum the caller acts on.

use crate::messages::{TickMsg, VolParams};
use crate::pricing::{black76_price, sabr_implied_vol, svi_implied_vol};
use crate::state::{DecisionCounters, OurOrder, TraderState};

// Constants matching Python's src/trader/main.py.
pub const MAX_STRIKE_OFFSET_USD: f64 = 0.30;
pub const STALENESS_INTERVAL_SECS: f64 = 0.10;
pub const STALENESS_TICKS: i32 = 1;
pub const COOLDOWN_NS: u64 = 250_000_000; // 250ms
pub const DEAD_BAND_TICKS: i32 = 1;
pub const GTD_LIFETIME_S: f64 = 5.0;
pub const GTD_REFRESH_LEAD_S: f64 = 1.5;
pub const RISK_STATE_STALE_S: f64 = 5.0;
pub const MIN_BBO_SIZE: i32 = 1;
pub const MAX_FORWARD_DRIFT_TICKS: i32 = 200;
pub const ATM_TOL_USD: f64 = 0.025; // half-strike tolerance for OTM-only
pub const CANCEL_THRESHOLD_S: f64 = 1.0; // skip cancel-before-replace if GTD imminent

#[derive(Debug)]
pub enum Decision {
    /// No action; reason already counted in DecisionCounters.
    Skip,
    /// Send place_order at this price for this side. If `cancel_old_oid`
    /// is Some, send a cancel_order first.
    Place {
        side: Side,
        price: f64,
        cancel_old_oid: Option<i64>,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        }
    }
    /// Compact single-char encoding for HashMap keys ('B'/'S').
    /// Saves one heap allocation per key vs the String form.
    pub fn as_char(self) -> char {
        match self {
            Side::Buy => 'B',
            Side::Sell => 'S',
        }
    }
}

/// Top-level decision entry point. Mirrors `_decide_on_tick` flow
/// in Python. Returns a Vec because each tick produces up to 2
/// decisions (BUY + SELL).
pub fn decide_on_tick(
    state: &mut TraderState,
    counters: &mut DecisionCounters,
    tick: &TickMsg,
    now_monotonic_ns: u64,
) -> Vec<Decision> {
    let mut out = Vec::with_capacity(2);
    let forward = state.underlying_price;
    if forward <= 0.0 {
        return out;
    }
    let strike = tick.strike;
    let expiry = &tick.expiry;
    let right = &tick.right;

    // Don't quote into a halt
    if !state.kills.is_empty() {
        return out;
    }
    if state.weekend_paused {
        return out;
    }

    // Compute risk-gate values once per tick.
    let (risk_buy, risk_sell, risk_all) = compute_risk_gates(state, now_monotonic_ns);

    // ATM-window restriction.
    if (strike - forward).abs() > MAX_STRIKE_OFFSET_USD {
        counters.skip_off_atm += 1;
        return out;
    }

    // OTM-only restriction (CLAUDE.md §12).
    let r_upper = right.chars().next().unwrap_or('C').to_ascii_uppercase();
    if r_upper == 'C' && strike < forward - ATM_TOL_USD {
        counters.skip_itm += 1;
        return out;
    }
    if r_upper == 'P' && strike > forward + ATM_TOL_USD {
        counters.skip_itm += 1;
        return out;
    }

    // All-blocking risk gate hoisted before per-side loop.
    if risk_all {
        counters.risk_block += 2;
        return out;
    }

    // Vol surface lookup: try (expiry, right_char) then either side.
    let r_char = right.chars().next().unwrap_or('C').to_ascii_uppercase();
    let vp_msg = state
        .vol_surfaces
        .get(&(expiry.clone(), r_char))
        .or_else(|| state.vol_surfaces.get(&(expiry.clone(), 'C')))
        .or_else(|| state.vol_surfaces.get(&(expiry.clone(), 'P')));
    let vp_msg = match vp_msg {
        Some(v) => v.clone(),
        None => {
            counters.skip_no_vol_surface += 1;
            return out;
        }
    };

    let fit_forward = vp_msg.forward;
    let tte = match time_to_expiry_years(expiry) {
        Some(t) if t > 0.0 => t,
        _ => return out,
    };

    // Forward-drift guard.
    let drift = (forward - fit_forward).abs();
    let max_drift = MAX_FORWARD_DRIFT_TICKS as f64 * state.tick_size;
    if drift > max_drift {
        counters.skip_forward_drift += 1;
        return out;
    }

    // Pre-compute theo once per tick (side-independent within a right).
    // BUT theo IS right-dependent (calls and puts have different prices
    // even at the same iv). Pass the option's right (already computed
    // above as r_char for the vol_surfaces key).
    let (iv, theo) = match compute_theo(fit_forward, strike, tte, r_char, &vp_msg.params) {
        Some(v) => v,
        None => {
            counters.skip_other += 1;
            return out;
        }
    };

    // Bid/ask + sizes for the dark-book guards.
    let bid = tick.bid.unwrap_or(0.0);
    let ask = tick.ask.unwrap_or(0.0);
    let bid_size = tick.bid_size.unwrap_or(0);
    let ask_size = tick.ask_size.unwrap_or(0);

    // Two-sided market check.
    if bid <= 0.0 || ask <= 0.0 {
        counters.skip_one_sided_or_dark += 2;
        return out;
    }
    // Min BBO size check.
    if bid_size < MIN_BBO_SIZE || ask_size < MIN_BBO_SIZE {
        counters.skip_thin_book += 2;
        return out;
    }

    let edge = state.min_edge_ticks as f64 * state.tick_size;

    for side in [Side::Buy, Side::Sell] {
        let mut target = match side {
            Side::Buy => theo - edge,
            Side::Sell => theo + edge,
        };
        if target <= 0.0 {
            counters.skip_target_nonpositive += 1;
            continue;
        }
        // Tick-jump (improve on incumbent BBO). When our naive
        // theo±edge target is at or behind the existing best, try
        // to jump 1 tick ahead — provided this still gives at least
        // `edge` (min_edge_ticks * tick_size) edge vs theo.
        //
        // Without the edge constraint, jumping into a tight market
        // compresses our edge to 1 tick or less. User report
        // 2026-05-01: "edge as low as 0.0005" (1 tick). The fix
        // preserves the configured min_edge_ticks invariant: never
        // place a quote with less than `edge` of edge to theo,
        // whether naive or jumped.
        match side {
            Side::Buy => {
                let jumped = bid + state.tick_size;
                // Only jump if naive is at-or-behind incumbent AND
                // jumping still keeps min_edge_ticks of edge.
                if target < jumped && (theo - jumped) >= edge {
                    target = jumped;
                }
            }
            Side::Sell => {
                let jumped = ask - state.tick_size;
                if target > jumped && (jumped - theo) >= edge {
                    target = jumped;
                }
            }
        }
        // Cross-protect: don't cross existing best on the opposite side.
        match side {
            Side::Buy => {
                if target >= ask {
                    counters.skip_would_cross_ask += 1;
                    continue;
                }
            }
            Side::Sell => {
                if target <= bid {
                    counters.skip_would_cross_bid += 1;
                    continue;
                }
            }
        }
        // Quantize to tick.
        let target_q =
            (target / state.tick_size).round() * state.tick_size;
        let target_q = (target_q * 10000.0).round() / 10000.0; // 4dp clean

        // Per-side risk gate.
        match side {
            Side::Buy if risk_buy => {
                counters.risk_block_buy += 1;
                continue;
            }
            Side::Sell if risk_sell => {
                counters.risk_block_sell += 1;
                continue;
            }
            _ => {}
        }

        // Compact key: strike-bits + expiry (one String) + right-char +
        // side-char. Saves 2 String allocations per key construction
        // vs the previous all-strings form.
        let key = (
            TraderState::strike_key(strike),
            expiry.clone(),
            r_char,
            side.as_char(),
        );

        // Dead-band + GTD-refresh check.
        let existing = state.our_orders.get(&key).cloned();
        if let Some(ref ex) = existing {
            let age_s = (now_monotonic_ns - ex.place_monotonic_ns) as f64 / 1e9;
            let in_band = (target_q - ex.price).abs()
                < DEAD_BAND_TICKS as f64 * state.tick_size;
            let needs_gtd_refresh = age_s > (GTD_LIFETIME_S - GTD_REFRESH_LEAD_S);
            if in_band && !needs_gtd_refresh {
                counters.skip_in_band += 1;
                continue;
            }
            // Cooldown floor.
            if (now_monotonic_ns - ex.place_monotonic_ns) < COOLDOWN_NS {
                counters.skip_cooldown += 1;
                continue;
            }
        }

        // Dark-book ON-PLACE re-check (latest tick state).
        // Trivially the SAME tick state since we're acting on this tick;
        // but kept for parity with Python where _decide_on_tick may
        // process a tick AFTER newer ticks have been queued. In the
        // single-thread Rust version they're identical, so skip the
        // duplicate check.

        // Cancel-before-replace: only if old order has substantial GTD left.
        let cancel_old_oid = match &existing {
            Some(ex) => {
                let age_s = (now_monotonic_ns - ex.place_monotonic_ns) as f64 / 1e9;
                let gtd_remaining = GTD_LIFETIME_S - age_s;
                if gtd_remaining > CANCEL_THRESHOLD_S {
                    if let Some(oid) = ex.order_id {
                        counters.replace_cancel += 1;
                        Some(oid)
                    } else {
                        None
                    }
                } else {
                    if ex.order_id.is_some() {
                        counters.replace_skip_cancel_near_gtd += 1;
                    }
                    None
                }
            }
            None => None,
        };

        out.push(Decision::Place {
            side,
            price: target_q,
            cancel_old_oid,
        });
        counters.place += 1;
    }
    out
}

/// Risk gates — return (buy_blocked, sell_blocked, all_blocked).
/// Mirrors the Python check in _decide_on_tick.
pub fn compute_risk_gates(state: &TraderState, now_monotonic_ns: u64) -> (bool, bool, bool) {
    let eff = state.risk_effective_delta;
    let age_s = if state.risk_state_age_monotonic_ns > 0 {
        (now_monotonic_ns - state.risk_state_age_monotonic_ns) as f64 / 1e9
    } else {
        f64::INFINITY
    };
    if eff.is_none() || age_s > RISK_STATE_STALE_S {
        return (false, false, true); // risk_all
    }
    let eff = eff.unwrap();
    let mut buy = false;
    let mut sell = false;
    let mut all = false;
    if eff + 1.0 >= state.delta_ceiling {
        buy = true;
    }
    if eff - 1.0 <= -state.delta_ceiling {
        sell = true;
    }
    if eff.abs() >= state.delta_kill - 1.0 {
        all = true;
    }
    if let Some(margin_pct) = state.risk_margin_pct {
        if margin_pct >= state.margin_ceiling_pct {
            all = true;
        }
    }
    (buy, sell, all)
}

/// Compute theo via SVI (or future SABR). Returns (iv, theo) or None.
/// CRITICAL: theo MUST use the option's actual right ('C' or 'P') —
/// call price ≠ put price. Bug 2026-05-01: passing 'C' for both
/// produced wildly wrong put theos (call price for OTM puts is
/// MUCH less than put price), making us SELL puts BELOW the bid
/// and BUY puts ABOVE the ask.
pub fn compute_theo(
    forward: f64,
    strike: f64,
    tte: f64,
    right: char,
    params: &VolParams,
) -> Option<(f64, f64)> {
    if forward <= 0.0 || strike <= 0.0 || tte <= 0.0 {
        return None;
    }
    let iv = match params.model.as_str() {
        "svi" => svi_implied_vol(
            forward,
            strike,
            tte,
            params.a?,
            params.b?,
            params.rho?,
            params.m?,
            params.sigma?,
        ),
        "sabr" => sabr_implied_vol(
            forward,
            strike,
            tte,
            params.alpha?,
            params.beta?,
            params.rho?,
            params.nu?,
        ),
        _ => return None,
    };
    if iv <= 0.0 || iv.is_nan() {
        return None;
    }
    let theo = black76_price(forward, strike, tte, iv, 0.0, right);
    if theo <= 0.0 {
        return None;
    }
    Some((iv, theo))
}

/// Convert a YYYYMMDD expiry string to time-to-expiry in years.
/// 16:00 CT = 21:00 UTC settlement. Caches the parsed expiry
/// datetime per-thread (see tte_cache module) to skip repeated
/// chrono parsing on every tick.
pub use crate::tte_cache::time_to_expiry_years;

#[cfg(test)]
mod tests {
    //! Decision-flow tests. Mirror inputs from
    //! `tests/test_decide_quote.py` so behavioral parity is
    //! verifiable across Rust and Python.

    use super::*;
    use crate::messages::{TickMsg, VolParams};
    use crate::state::{DecisionCounters, TraderState, VolSurfaceEntry};

    fn fresh_state(forward: f64) -> TraderState {
        let mut s = TraderState::new();
        s.underlying_price = forward;
        // Pretend risk_state has arrived (otherwise gate fail-closes).
        s.risk_effective_delta = Some(0.0);
        s.risk_margin_pct = Some(0.0);
        s.risk_state_age_monotonic_ns = 1;
        s
    }

    fn install_svi_surface(state: &mut TraderState, expiry: &str, side: char) {
        state.vol_surfaces.insert(
            (expiry.to_string(), side),
            VolSurfaceEntry {
                forward: 6.0,
                params: VolParams {
                    model: "svi".to_string(),
                    a: Some(0.005),
                    b: Some(0.05),
                    rho: Some(-0.3),
                    m: Some(0.0),
                    sigma: Some(0.1),
                    alpha: None,
                    beta: None,
                    nu: None,
                },
            },
        );
    }

    fn make_tick(strike: f64, expiry: &str, right: &str,
                 bid: f64, ask: f64) -> TickMsg {
        TickMsg {
            strike,
            expiry: expiry.to_string(),
            right: right.to_string(),
            bid: Some(bid),
            ask: Some(ask),
            bid_size: Some(10),
            ask_size: Some(10),
            ts_ns: Some(1),
        }
    }

    #[test]
    fn no_vol_surface_skips() {
        let mut state = fresh_state(6.0);
        let mut counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let decisions = decide_on_tick(&mut state, &mut counters, &tick, 1_000_000_000);
        assert!(decisions.is_empty());
        assert!(counters.skip_no_vol_surface > 0);
    }

    #[test]
    fn off_atm_skips() {
        let mut state = fresh_state(6.0);
        install_svi_surface(&mut state, "20260526", 'C');
        let mut counters = DecisionCounters::default();
        let tick = make_tick(7.0, "20260526", "C", 0.01, 0.02);
        let decisions = decide_on_tick(&mut state, &mut counters, &tick, 1_000_000_000);
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_off_atm, 1);
    }

    #[test]
    fn itm_call_skips() {
        let mut state = fresh_state(6.0);
        install_svi_surface(&mut state, "20260526", 'C');
        let mut counters = DecisionCounters::default();
        let tick = make_tick(5.85, "20260526", "C", 0.20, 0.22);
        let decisions = decide_on_tick(&mut state, &mut counters, &tick, 1_000_000_000);
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_itm, 1);
    }

    #[test]
    fn itm_put_skips() {
        let mut state = fresh_state(6.0);
        install_svi_surface(&mut state, "20260526", 'P');
        let mut counters = DecisionCounters::default();
        let tick = make_tick(6.15, "20260526", "P", 0.20, 0.22);
        let decisions = decide_on_tick(&mut state, &mut counters, &tick, 1_000_000_000);
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_itm, 1);
    }

    #[test]
    fn dark_book_skips_both_sides() {
        let mut state = fresh_state(6.0);
        install_svi_surface(&mut state, "20260526", 'C');
        let mut counters = DecisionCounters::default();
        let mut tick = make_tick(6.0, "20260526", "C", 0.0, 0.10);
        tick.bid = Some(0.0);
        let decisions = decide_on_tick(&mut state, &mut counters, &tick, 1_000_000_000);
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_one_sided_or_dark, 2);
    }

    #[test]
    fn thin_book_skips() {
        let mut state = fresh_state(6.0);
        install_svi_surface(&mut state, "20260526", 'C');
        let mut counters = DecisionCounters::default();
        let mut tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        tick.bid_size = Some(0);
        let decisions = decide_on_tick(&mut state, &mut counters, &tick, 1_000_000_000);
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_thin_book, 2);
    }

    #[test]
    fn risk_state_unknown_blocks_all() {
        let mut state = TraderState::new();
        state.underlying_price = 6.0;
        // risk_effective_delta is None → fail-closed.
        install_svi_surface(&mut state, "20260526", 'C');
        let mut counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let decisions = decide_on_tick(&mut state, &mut counters, &tick, 1_000_000_000);
        assert!(decisions.is_empty());
        assert_eq!(counters.risk_block, 2);
    }

    #[test]
    fn delta_kill_blocks_all() {
        let mut state = fresh_state(6.0);
        state.delta_kill = 5.0;
        state.risk_effective_delta = Some(5.0);
        install_svi_surface(&mut state, "20260526", 'C');
        let mut counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let decisions = decide_on_tick(&mut state, &mut counters, &tick, 1_000_000_000);
        assert!(decisions.is_empty());
        assert_eq!(counters.risk_block, 2);
    }

    #[test]
    fn weekend_pause_blocks() {
        let mut state = fresh_state(6.0);
        state.weekend_paused = true;
        install_svi_surface(&mut state, "20260526", 'C');
        let mut counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let decisions = decide_on_tick(&mut state, &mut counters, &tick, 1_000_000_000);
        assert!(decisions.is_empty());
    }

    #[test]
    fn kill_state_blocks() {
        let mut state = fresh_state(6.0);
        state.kills.insert("daily_halt".to_string(), "test".to_string());
        install_svi_surface(&mut state, "20260526", 'C');
        let mut counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let decisions = decide_on_tick(&mut state, &mut counters, &tick, 1_000_000_000);
        assert!(decisions.is_empty());
    }
}
