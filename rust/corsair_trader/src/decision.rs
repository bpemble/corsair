//! Decision flow — port of Python's `_decide_on_tick` in
//! src/trader/main.py. Single function, all gates inline for cache
//! friendliness.
//!
//! Returns a `Decision` enum the caller acts on.

use crate::messages::{TickMsg, VolParams};
use crate::pricing::{black76_price, sabr_implied_vol, svi_implied_vol};
use crate::state::{DecisionCounters, TraderState};

// Constants matching Python's src/trader/main.py.
pub const MAX_STRIKE_OFFSET_USD: f64 = 0.30;
// STALENESS_INTERVAL_SECS used to gate the staleness loop cadence;
// the loop now uses tokio::sleep(100ms) directly so the constant is
// retired. Kept commented for reference if we re-introduce config.
// pub const STALENESS_INTERVAL_SECS: f64 = 0.10;
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
    /// Today decide_on_tick early-returns by incrementing counters
    /// and falling through; this variant exists for API symmetry
    /// (callers can match on the full enum) but isn't constructed.
    #[allow(dead_code)]
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

    // HI-002: vol-surface staleness gate. Broker refits every 60s;
    // if we haven't seen a refresh in 120s the SVI extrapolation
    // anchor is stale (forward has likely drifted, market regime
    // could have shifted). Fail-safe.
    if vp_msg.fit_ts_ns > 0 {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        if now_ns.saturating_sub(vp_msg.fit_ts_ns) > 120_000_000_000 {
            counters.skip_vol_surface_stale += 1;
            return out;
        }
    }

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
    let (_iv, theo) = match compute_theo(fit_forward, strike, tte, r_char, &vp_msg.params) {
        Some(v) => v,
        None => {
            counters.skip_other += 1;
            return out;
        }
    };

    // Bid/ask + sizes for the dark-book guards.
    let raw_bid = tick.bid.unwrap_or(0.0);
    let raw_ask = tick.ask.unwrap_or(0.0);
    let bid_size = tick.bid_size.unwrap_or(0);
    let ask_size = tick.ask_size.unwrap_or(0);

    // Look up our resting orders (used by both the L2-aware path and
    // the depth-1 self-fill fallback below).
    let buy_key = (TraderState::strike_key(strike), expiry.clone(), r_char, 'B');
    let sell_key = (TraderState::strike_key(strike), expiry.clone(), r_char, 'S');
    let our_bid = state.our_orders.get(&buy_key).map(|o| o.price);
    let our_ask = state.our_orders.get(&sell_key).map(|o| o.price);

    // Compute "external" bid/ask — the next-best price after our own
    // resting orders. With L2 (broker rotates ~5 active depth subs
    // around ATM): if our_bid matches the top-of-book, fall to
    // level 1; otherwise level 0 IS external. Without L2 (other
    // strikes): use depth-1 self-fill approximation against raw_bid.
    let bid = if let Some(d0) = tick.depth_bid_0 {
        // L2 path.
        if our_bid.map(|p| (p - d0).abs() < 1e-9).unwrap_or(false) {
            // We're the top-of-book — external incumbent is level 1.
            tick.depth_bid_1.unwrap_or(0.0)
        } else {
            d0
        }
    } else {
        // L1 fallback.
        if our_bid.map(|p| (p - raw_bid).abs() < 1e-9).unwrap_or(false) {
            0.0
        } else {
            raw_bid
        }
    };
    let ask = if let Some(d0) = tick.depth_ask_0 {
        if our_ask.map(|p| (p - d0).abs() < 1e-9).unwrap_or(false) {
            tick.depth_ask_1.unwrap_or(0.0)
        } else {
            d0
        }
    } else if our_ask.map(|p| (p - raw_ask).abs() < 1e-9).unwrap_or(false) {
        0.0
    } else {
        raw_ask
    };

    // Two-sided market check (uses raw — even if WE are the BBO, we
    // still need real two-sided market on each side to enter).
    if raw_bid <= 0.0 || raw_ask <= 0.0 {
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
                // Only jump if there's an EXTERNAL incumbent bid
                // (filtered bid > 0 means external liquidity exists).
                // When `bid == 0` the self-fill filter zeroed it out
                // because we're the BBO ourselves — no need to jump
                // 1-tick "ahead" of our own resting order.
                if bid > 0.0 {
                    let jumped = bid + state.tick_size;
                    if target < jumped && (theo - jumped) >= edge {
                        target = jumped;
                    }
                }
            }
            Side::Sell => {
                if ask > 0.0 {
                    let jumped = ask - state.tick_size;
                    if target > jumped && (jumped - theo) >= edge {
                        target = jumped;
                    }
                }
            }
        }
        // Cross-protect: don't cross existing best on the opposite
        // side. Use the FILTERED bid/ask — if we are the contra-side
        // BBO (e.g., filtered ask=0 because our_ask matches), there's
        // no external incumbent on that side and we shouldn't gate.
        match side {
            Side::Buy => {
                if ask > 0.0 && target >= ask {
                    counters.skip_would_cross_ask += 1;
                    continue;
                }
            }
            Side::Sell => {
                if bid > 0.0 && target <= bid {
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
            // HI-003: no place while previous order is still unack'd
            // (order_id is None until place_ack arrives). Without this
            // gate, GTD-refresh path can fire a second place at the same
            // key while the first is in-flight, leaving the first to
            // expire via GTD-5s and bloating IBKR's order book.
            // Tolerance: only enforce this BEFORE the cooldown floor
            // expires; if cooldown has elapsed (>250 ms), the place_ack
            // path failed silently and we must replace.
            if ex.order_id.is_none()
                && (now_monotonic_ns - ex.place_monotonic_ns) < COOLDOWN_NS
            {
                counters.skip_unack_inflight += 1;
                continue;
            }
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
    // The `-1.0` is an intentional grace buffer (MED-004): a fill can
    // push effective_delta over the kill threshold between the broker's
    // 1Hz risk_state publish and the trader's next decision. Without
    // the buffer, that fill would slip through one extra place at the
    // ceiling. With it, we pre-emptively block placements when within
    // 1 contract-delta of the kill, giving risk_state propagation a
    // round trip to catch up. delta_kill ceiling is documented
    // unbuffered (e.g. 5.0); the trader self-blocks at 4.0.
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
