//! Decision flow — port of Python's `_decide_on_tick` in
//! src/trader/main.py. Single function, all gates inline for cache
//! friendliness.
//!
//! Returns a `Decision` enum the caller acts on.
//!
//! Lock-shard refactor (Priority 1, 2026-05-04): the function now
//! takes `&SharedState` (no exclusive reference). Scalars are
//! snapshotted once at the top of the function so the decision
//! flow doesn't repeatedly acquire `state.scalars`. Counter
//! increments use atomic `fetch_add` — wait-free, no lock.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::messages::{TickMsg, VolParams};
use crate::pricing::{black76_price_and_delta, sabr_implied_vol, svi_implied_vol};
use crate::state::{DecisionCounters, ScalarSnapshot, SharedState};

// Constants matching Python's src/trader/main.py.
// 0.25 = ATM + 5 OTM strikes per side (spec §3.3, asym ATM+OTM).
// 0.30 admitted a 7th strike (offset 0.30) past the spec window.
/// Strike-window gate: skip if `|K - pricing_forward| > this`.
/// 0.30 (5 strikes × 0.05 + ½-strike of slack) so the in-scope set
/// stays stable as F drifts within an ATM bin. Tightening to 0.25
/// (2026-05-05 morning) caused 6.20 C to flip out of scope when
/// F=5.9295 even though ATM was 6.00 — wing strikes don't appear in
/// `our_orders` for whole rotations of the SABR fit, then snap back
/// when F crosses the bin boundary. CLAUDE.md §12 documents 0.30 as
/// the historical / intended value.
pub const MAX_STRIKE_OFFSET_USD: f64 = 0.30;
pub const STALENESS_TICKS: i32 = 1;
pub const COOLDOWN_NS: u64 = 250_000_000; // 250ms
/// Maximum time to wait for a place_ack before assuming it was lost
/// and proceeding with a fresh place at the same key. The original
/// HI-003 gate used COOLDOWN_NS (250ms) — too short for real ack
/// latency tails (place_rtt p99 ~1s on paper). With place_rtt p50
/// ~140ms, 2s gives ~14× headroom; well above any reasonable retry
/// while still bounded so we recover if place_ack genuinely drops.
/// 2026-05-04: pre-fix this was 250ms, which combined with the
/// camelCase place_ack bug let the trader pile orders at IBKR
/// (one new order every 250ms × 4 strikes × 2 sides = ~32/sec) and
/// every market cross filled the entire stack.
pub const UNACK_INFLIGHT_NS: u64 = 2_000_000_000; // 2s
pub const RISK_STATE_STALE_S: f64 = 5.0;
pub const MIN_BBO_SIZE: i32 = 1;
/// Skip quoting when |spot - fit_forward| exceeds this. Reverted to
/// 200 on 2026-05-05 evening after empirical observation showed the
/// HG K6→M6 calendar carry is structurally ~$0.04–0.05 (80–100 ticks),
/// so a 100-tick guard fired on ~24% of decisions during normal US
/// session. The morning's adverse-selection event was misdiagnosed
/// as drift; root causes were the missing kill IPC + tick cache
/// initialization bug + symmetric quoting absorbing one-sided flow.
/// All three are fixed; the drift guard's job is to catch genuinely
/// stale fits between 60s SABR cycles, which 200 ticks ($0.10) covers
/// while leaving the structural carry untouched.
pub const MAX_FORWARD_DRIFT_TICKS: i32 = 200;
pub const ATM_TOL_USD: f64 = 0.025; // half-strike tolerance for OTM-only

/// Tier A Exp 2 burst-cap window (sliding) — gate fires when ≥2 sends
/// have already happened within this many ns. 50 ms = 20/sec sustained
/// throughput, max 2-burst within window. Targeted at the empirical
/// inflight=2+ RTT cliff (5× p50 jump per wire_timing analysis).
///
/// At runtime the operator may override via env
/// `CORSAIR_TRADER_BURST_WINDOW_NS` (resolved at boot in `main.rs`,
/// stored as `SharedState::burst_window_ns`). Set to 0 to disable
/// the gate entirely — useful for live deployments where alpha-loss
/// from delayed quotes may outweigh the RTT-compression benefit
/// observed in paper.
pub const BURST_WINDOW_NS_DEFAULT: u64 = 50_000_000;

#[derive(Debug)]
pub enum Decision {
    /// Send place_order at this price for this side. If `cancel_old_oid`
    /// is Some, send a cancel_order first. Used for fresh placements +
    /// the rare fallback case (no live order_id at this key).
    Place {
        side: Side,
        price: f64,
        cancel_old_oid: Option<i64>,
    },
    /// Send a single modify_order at the existing order_id. Replaces
    /// cancel-before-replace when we have a known live order_id —
    /// one round trip instead of two, no transient empty quote window.
    Modify {
        side: Side,
        order_id: i64,
        price: f64,
    },
    /// Cancel ALL of our resting orders. Fired when the trader self-
    /// blocks on risk (risk_all = effective_delta near kill threshold)
    /// and we still have live quotes at IBKR — without this, the
    /// resting stack continues to absorb fills until the kill fires
    /// or the orders GTD-expire. With it, we yank the stack the
    /// moment risk says "stop trading", well before the kill.
    /// 2026-05-04 cascade post-mortem: 12 fills accumulated in 13s
    /// because risk_all only blocked NEW placements while the existing
    /// 22 orders sat at theo±tick taking adverse hits.
    CancelAll {
        order_ids: Vec<i64>,
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
///
/// `expiry_arc` is the interned `Arc<str>` for `tick.expiry`; the
/// caller owns interning so all downstream HashMap keys share one Arc
/// and clones are bumps, not allocations.
pub fn decide_on_tick(
    state: &SharedState,
    counters: &DecisionCounters,
    tick: &TickMsg,
    expiry_arc: &Arc<str>,
    r_char: char,
    now_monotonic_ns: u64,
    now_wall_ns: u64,
    snap: &ScalarSnapshot,
) -> Vec<Decision> {
    let mut out = Vec::with_capacity(2);

    // Bundle 2D (2026-05-06): scalar snapshot is now passed in — taken
    // once by on_tick and shared with this function. Bundle 2G (same):
    // r_char is also passed in (computed in process_event from
    // tick.right) so this function doesn't redo `chars().next()`.

    // Spot is the front-month underlying price (HGK6 tick stream).
    // Distinct from the option's pricing forward (HGM6 parity-F or
    // similar carry-anchored value), which arrives via vol_surface.
    let spot = snap.underlying_price;
    if spot <= 0.0 {
        return out;
    }
    let strike = tick.strike;

    // Don't quote into a halt. Atomic mirror of `kills.len()` —
    // DashMap::is_empty() would touch every shard, measurable cost
    // per tick; this is one Relaxed load.
    if state.kills_count.load(Ordering::Relaxed) > 0 {
        return out;
    }
    if snap.weekend_paused {
        return out;
    }

    // Vol surface lookup hoisted ABOVE the strike-window gates so
    // gating anchors on the option's pricing forward (vp_msg.forward,
    // typically parity-F of the option's underlying contract) rather
    // than spot. With HG carry the two differ ~$0.05 (≈100 ticks);
    // gating on spot rejected strikes that ARE inside the option's
    // pricing window. Per quant advisor 2026-05-05 spec Rec 3.
    let vp_msg = state.lookup_vol_surface(expiry_arc, r_char);
    let vp_msg = match vp_msg {
        Some(v) => v,
        None => {
            counters.skip_no_vol_surface.fetch_add(1, Ordering::Relaxed);
            return out;
        }
    };
    let pricing_forward = vp_msg.forward;

    // Compute risk-gate values once per tick.
    let (risk_buy, risk_sell, risk_all) = compute_risk_gates(&snap, now_monotonic_ns);

    // ATM-window restriction — anchored on PRICING forward (Rec 3).
    if (strike - pricing_forward).abs() > MAX_STRIKE_OFFSET_USD {
        counters.skip_off_atm.fetch_add(1, Ordering::Relaxed);
        return out;
    }

    // OTM-only restriction (CLAUDE.md §12) — anchored on PRICING forward.
    if r_char == 'C' && strike < pricing_forward - ATM_TOL_USD {
        counters.skip_itm.fetch_add(1, Ordering::Relaxed);
        return out;
    }
    if r_char == 'P' && strike > pricing_forward + ATM_TOL_USD {
        counters.skip_itm.fetch_add(1, Ordering::Relaxed);
        return out;
    }

    // All-blocking risk gate hoisted before per-side loop.
    if risk_all {
        counters.risk_block.fetch_add(2, Ordering::Relaxed);
        // Proactive cancel: when self-blocked on risk AND we still
        // have live quotes resting, fire cancel_all immediately. The
        // alternative (passively waiting for the per-fill kill) lets
        // the resting stack absorb adverse fills until the next fill
        // crosses the kill — too slow in fast markets. Bounded fire:
        // emit once when our_orders is non-empty; subsequent ticks
        // see empty our_orders and skip.
        let order_ids: Vec<i64> = state
            .our_orders
            .iter()
            .filter_map(|e| e.value().order_id)
            .collect();
        if !order_ids.is_empty() {
            out.push(Decision::CancelAll { order_ids });
        }
        return out;
    }

    let tte = match time_to_expiry_years(expiry_arc) {
        Some(t) if t > 0.0 => t,
        _ => return out,
    };

    // HI-002: vol-surface staleness gate. Broker refits every 60s;
    // if we haven't seen a refresh in 120s the SVI extrapolation
    // anchor is stale (forward has likely drifted, market regime
    // could have shifted). Fail-safe.
    //
    // `vp_msg.fit_ts_ns` is broker CLOCK_REALTIME ns, so we compare
    // against `now_wall_ns` (passed in by the caller, sampled once at
    // process_event entry). The previous version sampled SystemTime
    // here on every tick — a vDSO syscall in the hot path. Reusing
    // the caller's wall-ns reading saves ~80-150 ns per tick.
    if vp_msg.fit_ts_ns > 0
        && now_wall_ns.saturating_sub(vp_msg.fit_ts_ns) > 120_000_000_000
    {
        counters
            .skip_vol_surface_stale
            .fetch_add(1, Ordering::Relaxed);
        return out;
    }

    // is_strike_calibrated (spec §3.3, audit item 6). Refuse to quote
    // strikes outside the SABR fit's calibrated envelope — SABR
    // extrapolation past the strike range used for the fit is
    // unreliable. Half a tick of slack on each side covers float
    // round-trip drift from broker → wire → trader. When either
    // bound is None (older broker) the gate is skipped.
    if let (Some(min_k), Some(max_k)) = (vp_msg.calibrated_min_k, vp_msg.calibrated_max_k) {
        let slack = snap.tick_size * 0.5;
        if strike < min_k - slack || strike > max_k + slack {
            counters
                .skip_uncalibrated_strike
                .fetch_add(1, Ordering::Relaxed);
            return out;
        }
    }

    // Forward-drift guard. Compares spot (front-month underlying)
    // against pricing_forward (option's parity-implied F). Detects
    // when the parity estimator has stopped tracking spot — the
    // broker's vol_surface fitter falls back to spot at fit-time
    // when this drift exceeds 50 ticks, so seeing it here means
    // either spot has moved between fits or the broker accepted a
    // borderline fit.
    //
    // DO NOT "fix" this to use vp_msg.spot_at_fit thinking it
    // aligns with §19. The §19 fix is about the Taylor anchor at
    // line 341 below. THIS gate intentionally compares against the
    // fit-time forward (= pricing_forward = vp_msg.forward), because
    // its job is precisely to detect when current spot has drifted
    // far from the fit's view of the world — the carry between
    // front-month spot and the option's pricing forward is part of
    // what this gate is sized to absorb (200 ticks ≈ $0.10 covers
    // the HG K6→M6 carry of ~100 ticks plus ~100 ticks of post-fit
    // drift). Substituting spot_at_fit removes the carry component
    // from the comparison and changes the gate's semantics in a
    // direction-asymmetric way that can mask stale-fit conditions.
    // See audits/sections-16-19-audit.md §1.4.
    let drift = (spot - pricing_forward).abs();
    let max_drift = MAX_FORWARD_DRIFT_TICKS as f64 * snap.tick_size;
    if drift > max_drift {
        counters.skip_forward_drift.fetch_add(1, Ordering::Relaxed);
        return out;
    }

    // Quantize strike once and reuse across all key tuples below
    // (theo_key, buy_key, sell_key, per-side place key) — saves
    // 3 redundant int-mul+round+cast calls (~6ns total).
    let sk = SharedState::strike_key(strike);

    // Theo cache (optimization #3). Within a single fit window
    // (60s), theo is a pure function of (forward, strike, tte,
    // params, right). The only mover is tte and it shifts <1µs/sec
    // — well below tick precision. So we cache by (key, fit_ts_ns)
    // and invalidate only when a new fit lands. Saves ~80µs per
    // tick on the SVI implied_vol + Black76 chain in the amend loop.
    let theo_key = (
        sk,
        Arc::clone(expiry_arc),
        r_char,
        vp_msg.fit_ts_ns,
    );
    // Cached value is (theo_at_fit, delta_at_fit) — both pure functions
    // of (fit_forward, strike, tte, params, right). Caching delta lets
    // the Taylor reprice below run without recomputing greeks per tick.
    let (theo_at_fit, delta_at_fit) = match state.theo_cache.get(&theo_key).map(|r| *r.value()) {
        Some(td) => td,
        None => {
            let (_iv, t, d) =
                match compute_theo(pricing_forward, strike, tte, r_char, &vp_msg.params) {
                    Some(v) => v,
                    None => {
                        counters.skip_other.fetch_add(1, Ordering::Relaxed);
                        return out;
                    }
                };
            // Bound cache size: prune all entries on fit change. The
            // hashmap key includes fit_ts_ns so old entries are dead;
            // sweep them when the fit advances by retaining only
            // current-fit keys. Cheap O(N≤44) per fit cycle.
            if state.theo_cache.len() > 200 {
                let current_fit = vp_msg.fit_ts_ns;
                state
                    .theo_cache
                    .retain(|(_, _, _, ts), _| *ts == current_fit);
            }
            state.theo_cache.insert(theo_key, (t, d));
            (t, d)
        }
    };

    // First-order Taylor reprice for spot drift since the SABR fit.
    // Anchor is `spot_at_fit` (front-month spot the broker saw when
    // fitting), NOT `pricing_forward` (the option's underlying-month
    // parity-F). Using `forward` would conflate the static front-vs-
    // deferred carry (~$0.04–0.05 in HG) with actual drift, shifting
    // every theo by delta × carry — that bug killed all SELL quotes
    // via cross-protect on 2026-05-06 between 12:35 and 12:45 UTC.
    //
    //   theo ≈ theo_at_fit + delta_at_fit × (spot − spot_at_fit)
    //
    // The SVI/SABR surface anchor stays at fit-time forward (don't
    // pass current spot to SVI — see §16). Taylor adds back the
    // first-order directional shift so quotes don't sit fit-frozen
    // for up to 60s between fits. Without this, fast moves drive
    // adverse fills (overnight 2026-05-06 pickoff cluster, §14).
    //
    // Bounded below at 1¢ (mirrors Python). Gamma curvature dominates
    // beyond ~50 ticks of drift; the MAX_FORWARD_DRIFT_TICKS gate
    // upstream catches that regime so this stays a safe first-order.
    let theo = (theo_at_fit + delta_at_fit * (spot - vp_msg.spot_at_fit)).max(0.01);

    // Bid/ask + sizes for the dark-book guards.
    let raw_bid = tick.bid.unwrap_or(0.0);
    let raw_ask = tick.ask.unwrap_or(0.0);
    let bid_size = tick.bid_size.unwrap_or(0);
    let ask_size = tick.ask_size.unwrap_or(0);

    // Look up our resting orders (used by both the L2-aware path and
    // the depth-1 self-fill fallback below).
    let buy_key = (sk, Arc::clone(expiry_arc), r_char, 'B');
    let sell_key = (sk, Arc::clone(expiry_arc), r_char, 'S');
    let our_bid = state.our_orders.get(&buy_key).map(|r| r.value().price);
    let our_ask = state.our_orders.get(&sell_key).map(|r| r.value().price);

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
        counters
            .skip_one_sided_or_dark
            .fetch_add(2, Ordering::Relaxed);
        return out;
    }
    // Min BBO size check.
    if bid_size < MIN_BBO_SIZE || ask_size < MIN_BBO_SIZE {
        counters.skip_thin_book.fetch_add(2, Ordering::Relaxed);
        return out;
    }

    let edge = snap.min_edge_ticks as f64 * snap.tick_size;

    // Spec §3.4: wide-market skip. If the half-spread is more than
    // `skip_if_spread_over_edge_mul × min_edge`, both sides are too
    // wide to quote into safely (theo±edge would land far inside the
    // BBO and we'd be picked off by the next tightening). 0 disables.
    if snap.skip_if_spread_over_edge_mul > 0.0 {
        let half_spread = (raw_ask - raw_bid) * 0.5;
        if half_spread > snap.skip_if_spread_over_edge_mul * edge {
            counters.skip_wide_spread.fetch_add(2, Ordering::Relaxed);
            return out;
        }
    }

    for side in [Side::Buy, Side::Sell] {
        let mut target = match side {
            Side::Buy => theo - edge,
            Side::Sell => theo + edge,
        };
        if target <= 0.0 {
            counters
                .skip_target_nonpositive
                .fetch_add(1, Ordering::Relaxed);
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
                    let jumped = bid + snap.tick_size;
                    if target < jumped && (theo - jumped) >= edge {
                        target = jumped;
                    }
                }
            }
            Side::Sell => {
                if ask > 0.0 {
                    let jumped = ask - snap.tick_size;
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
                    counters
                        .skip_would_cross_ask
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            Side::Sell => {
                if bid > 0.0 && target <= bid {
                    counters
                        .skip_would_cross_bid
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
        }
        // Cap relative to market consensus mid (2026-05-05 evening).
        // The stale-theo failure mode: when SABR is fit on slightly
        // biased input (one-sided strikes) AND spot drifts between
        // refits, theo can sit several ticks above market mid for
        // tens of seconds. Without this cap, bid = theo - edge ends
        // up AT or ABOVE market ask, and sellers happily hit our bid
        // 11 times in 5 seconds (5.90 P @ 0.0975 incident, 14:22:55–
        // 14:23:00 UTC). The cap forces every quote to sit at least
        // `edge` away from market mid, regardless of how biased theo
        // is. Trusts the market consensus when our model disagrees.
        let mkt_mid = (raw_bid + raw_ask) * 0.5;
        target = match side {
            Side::Buy => target.min(mkt_mid - edge),
            Side::Sell => target.max(mkt_mid + edge),
        };
        if target <= 0.0 {
            counters
                .skip_target_nonpositive
                .fetch_add(1, Ordering::Relaxed);
            continue;
        }
        // Quantize to tick.
        let target_q = (target / snap.tick_size).round() * snap.tick_size;
        let target_q = (target_q * 10000.0).round() / 10000.0; // 4dp clean

        // Per-side risk gate.
        match side {
            Side::Buy if risk_buy => {
                counters.risk_block_buy.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Side::Sell if risk_sell => {
                counters.risk_block_sell.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            _ => {}
        }

        // Compact key: strike-bits + expiry-arc + right-char +
        // side-char. Arc bump per clone, no String alloc.
        let key = (sk, Arc::clone(expiry_arc), r_char, side.as_char());

        // Dead-band + GTD-refresh check.
        let existing = state.our_orders.get(&key).map(|r| r.value().clone());
        if let Some(ref ex) = existing {
            // HI-003: no place while previous order is still unack'd
            // (order_id is None until place_ack arrives). Without this
            // gate, every quote update past the cooldown floor would
            // place a SECOND order at the same key while the first is
            // still resting at IBKR — we'd accumulate hundreds of
            // resting orders within seconds because we don't know the
            // first one's orderId to cancel it.
            //
            // 2026-05-04 cascade root cause: this gate timeout was set
            // to COOLDOWN_NS (250ms), well below typical place_ack
            // latency. Combined with a camelCase field-name bug where
            // place_ack never reached the trader, every key piled
            // ~5 fresh orders/sec at IBKR. Fix: extend the gate
            // window to UNACK_INFLIGHT_NS (2s); past that, place_ack
            // is presumed lost and we proceed (phantom orphan at
            // IBKR, but bounded by GTD-5s).
            if ex.order_id.is_none()
                && now_monotonic_ns.saturating_sub(ex.place_monotonic_ns) < UNACK_INFLIGHT_NS
            {
                counters.skip_unack_inflight.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let age_s = now_monotonic_ns.saturating_sub(ex.place_monotonic_ns) as f64 / 1e9;
            let in_band =
                (target_q - ex.price).abs() < snap.dead_band_ticks as f64 * snap.tick_size;
            let needs_gtd_refresh =
                age_s > (snap.gtd_lifetime_s - snap.gtd_refresh_lead_s);
            if in_band && !needs_gtd_refresh {
                counters.skip_in_band.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // Cooldown floor.
            if now_monotonic_ns.saturating_sub(ex.place_monotonic_ns) < COOLDOWN_NS {
                counters.skip_cooldown.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }

        // Dark-book ON-PLACE re-check (latest tick state).
        // Trivially the SAME tick state since we're acting on this tick;
        // but kept for parity with Python where _decide_on_tick may
        // process a tick AFTER newer ticks have been queued. In the
        // single-thread Rust version they're identical, so skip the
        // duplicate check.

        // Tier A Exp 2 (2026-05-06): outbound burst-cap gate. Caps
        // concurrent wire round-trips at 2 within a sliding window
        // (default 50 ms, env-overridable). Modify and Place both
        // consume slots — empirical inflight=2 inflates RTT 5×
        // regardless of kind. Cancels (CancelAll path) bypass the
        // cap because they're rare and time-critical (yanking quotes
        // during risk events). Window of 0 disables the gate.
        let burst_window = state.burst_window_ns;
        if burst_window > 0
            && !state
                .outbound_limiter
                .try_consume(now_monotonic_ns, burst_window)
        {
            counters.skip_burst_cap.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // Re-quote path:
        //   - If we have a live order_id at this key, AMEND (single
        //     modify_order). Cuts wire RTT in half vs cancel+place and
        //     leaves no empty-quote window where opposite-side traders
        //     can pick the lone surviving leg.
        //   - If no order_id (place_ack hasn't arrived yet AND cooldown
        //     elapsed → place ack failed silently), fall back to a
        //     fresh Place with no cancel.
        //   - If existing order is near-GTD-expiry, prefer modify too —
        //     a modify refreshes GTD via gtd_until_utc on the same id.
        if let Some(ref ex) = existing {
            if let Some(oid) = ex.order_id {
                counters.modify.fetch_add(1, Ordering::Relaxed);
                out.push(Decision::Modify {
                    side,
                    order_id: oid,
                    price: target_q,
                });
                continue;
            }
        }
        out.push(Decision::Place {
            side,
            price: target_q,
            cancel_old_oid: None,
        });
        counters.place.fetch_add(1, Ordering::Relaxed);
    }
    out
}

/// Risk gates — return (buy_blocked, sell_blocked, all_blocked).
/// Mirrors the Python check in _decide_on_tick. Reads the
/// stack-local `ScalarSnapshot` so the caller pays for one mutex
/// acquire (snapshot) per tick instead of one per gate read.
pub fn compute_risk_gates(snap: &ScalarSnapshot, now_monotonic_ns: u64) -> (bool, bool, bool) {
    let eff = snap.risk_effective_delta;
    let age_s = if snap.risk_state_age_monotonic_ns > 0 {
        (now_monotonic_ns.saturating_sub(snap.risk_state_age_monotonic_ns)) as f64 / 1e9
    } else {
        f64::INFINITY
    };
    // Fail-closed if either gating-relevant field has not yet arrived
    // OR the snapshot is stale. Margin-None gets the same treatment as
    // delta-None: without a margin reading we can't enforce
    // margin_ceiling_pct, so we must not place. (Prior version only
    // fail-closed on delta-None — a broker that briefly published
    // effective_delta without margin_pct could open a placement window.)
    if eff.is_none() || snap.risk_margin_pct.is_none() || age_s > RISK_STATE_STALE_S {
        return (false, false, true); // risk_all
    }
    let eff = eff.unwrap();
    let mut buy = false;
    let mut sell = false;
    let mut all = false;
    if eff + 1.0 >= snap.delta_ceiling {
        buy = true;
    }
    if eff - 1.0 <= -snap.delta_ceiling {
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
    if eff.abs() >= snap.delta_kill - 1.0 {
        all = true;
    }
    if let Some(margin_pct) = snap.risk_margin_pct {
        if margin_pct >= snap.margin_ceiling_pct {
            all = true;
        }
    }
    // Theta self-gate (2026-05-05 incident fix): the broker also fires
    // a theta kill at the same threshold and now publishes a kill IPC
    // event, but this local check is defense-in-depth against IPC
    // drops or kill-event ring-full. theta_kill is negative (e.g.
    // -500) and risk_theta accumulates negative for short-vol books;
    // breach is `theta < theta_kill`. Zero disables.
    if snap.theta_kill < 0.0 {
        if let Some(theta) = snap.risk_theta {
            if theta < snap.theta_kill {
                all = true;
            }
        }
    }
    // Vega self-gate. vega_kill is positive and the magnitude of the
    // worst-case vega exposure trips it. CLAUDE.md §13: 0 disables
    // (current operational state per Alabaster characterization).
    if snap.vega_kill > 0.0 {
        if let Some(vega) = snap.risk_vega {
            if vega.abs() >= snap.vega_kill {
                all = true;
            }
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
) -> Option<(f64, f64, f64)> {
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
    let (theo, delta) = black76_price_and_delta(forward, strike, tte, iv, 0.0, right);
    if theo <= 0.0 {
        return None;
    }
    Some((iv, theo, delta))
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
    use crate::state::{DecisionCounters, SharedState, VolSurfaceEntry};
    use std::sync::atomic::Ordering;

    /// Test helper: derives `r_char` from `tick.right` and snapshots
    /// scalars, then forwards to `decide_on_tick`. Bundle 2D/2G shifted
    /// those derivations to the caller (`on_tick`); production sites
    /// pass them in directly. Tests use this shim to stay terse.
    fn run_decide(
        state: &SharedState,
        counters: &DecisionCounters,
        tick: &TickMsg,
        expiry_arc: &Arc<str>,
        now_mono: u64,
        now_wall: u64,
    ) -> Vec<Decision> {
        let r_char = tick
            .right
            .chars()
            .next()
            .unwrap_or('C')
            .to_ascii_uppercase();
        let snap = state.scalar_snapshot();
        decide_on_tick(state, counters, tick, expiry_arc, r_char, now_mono, now_wall, &snap)
    }

    fn fresh_state(forward: f64) -> SharedState {
        let s = SharedState::new();
        {
            let mut sc = s.scalars.lock();
            sc.underlying_price = forward;
            // Pretend risk_state has arrived (otherwise gate fail-closes).
            sc.risk_effective_delta = Some(0.0);
            sc.risk_margin_pct = Some(0.0);
            sc.risk_state_age_monotonic_ns = 1;
        }
        s
    }

    fn install_svi_surface(state: &SharedState, expiry: &str, side: char) {
        let expiry_arc = state.intern_expiry(expiry);
        // fit_ts_ns must be within 120s of "now" or the HI-002
        // staleness gate fires and the dark-book / thin-book tests
        // never reach their target check. Use SystemTime::now() so
        // the fixture stays fresh whenever the test runs.
        let fit_ts_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        state.vol_surfaces.insert(
            (expiry_arc, side),
            Arc::new(VolSurfaceEntry {
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
                fit_ts_ns,
                // None disables the calibrated-range gate so existing
                // tests (which already assert on other gates) don't
                // hit the new gate first.
                calibrated_min_k: None,
                calibrated_max_k: None,
                // Test fixture: spot_at_fit = forward (no Taylor shift)
                // so existing tests continue to assert against fit-frozen
                // theo, isolating each gate. The Taylor-specific test
                // exercises a non-zero (spot − spot_at_fit) explicitly.
                spot_at_fit: 6.0,
            }),
        );
    }

    fn make_tick(strike: f64, expiry: &str, right: &str, bid: f64, ask: f64) -> TickMsg {
        TickMsg {
            strike,
            expiry: expiry.to_string(),
            right: right.to_string(),
            bid: Some(bid),
            ask: Some(ask),
            bid_size: Some(10),
            ask_size: Some(10),
            ts_ns: Some(1),
            broker_recv_ns: None,
            depth_bid_0: None,
            depth_bid_1: None,
            depth_ask_0: None,
            depth_ask_1: None,
        }
    }

    #[test]
    fn no_vol_surface_skips() {
        let state = fresh_state(6.0);
        let counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert!(counters.skip_no_vol_surface.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn off_atm_skips() {
        let state = fresh_state(6.0);
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(7.0, "20260526", "C", 0.01, 0.02);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_off_atm.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn itm_call_skips() {
        let state = fresh_state(6.0);
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(5.85, "20260526", "C", 0.20, 0.22);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_itm.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn itm_put_skips() {
        let state = fresh_state(6.0);
        install_svi_surface(&state, "20260526", 'P');
        let counters = DecisionCounters::default();
        let tick = make_tick(6.15, "20260526", "P", 0.20, 0.22);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_itm.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn dark_book_skips_both_sides() {
        let state = fresh_state(6.0);
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let mut tick = make_tick(6.0, "20260526", "C", 0.0, 0.10);
        tick.bid = Some(0.0);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_one_sided_or_dark.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn thin_book_skips() {
        let state = fresh_state(6.0);
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let mut tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        tick.bid_size = Some(0);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_thin_book.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn uncalibrated_strike_skips() {
        // Calibrated range [5.95, 6.05]; quote at 6.10 → outside range,
        // gate fires.
        let state = fresh_state(6.0);
        install_svi_surface(&state, "20260526", 'C');
        // Mutate the surface entry to set calibrated bounds.
        let expiry_arc = state.intern_expiry("20260526");
        if let Some(mut entry) = state.vol_surfaces.get_mut(&(expiry_arc, 'C')) {
            // Bundle 3E: VolSurfaceEntry now lives behind Arc;
            // Arc::make_mut clones when shared (here refcount=1, so
            // it gives back the inner without cost).
            let inner = Arc::make_mut(&mut *entry);
            inner.calibrated_min_k = Some(5.95);
            inner.calibrated_max_k = Some(6.05);
        }
        let counters = DecisionCounters::default();
        // 6.10 is past calibrated_max_k=6.05 (more than half a tick),
        // but still inside ATM-window (6.10 - 6.0 = 0.10 < 0.30).
        let tick = make_tick(6.10, "20260526", "C", 0.10, 0.105);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_uncalibrated_strike.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn wide_spread_skips_both_sides() {
        // half_spread = 0.05; edge = min_edge_ticks(2) × tick_size(0.0005) = 0.001.
        // Default mul=4 → threshold 0.004 < 0.05 → fires.
        let state = fresh_state(6.0);
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.20);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert_eq!(counters.skip_wide_spread.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn wide_spread_disabled_when_mul_zero() {
        // Setting skip_if_spread_over_edge_mul = 0 disables the gate;
        // the same tick that fires above should pass through.
        let state = fresh_state(6.0);
        state.scalars.lock().skip_if_spread_over_edge_mul = 0.0;
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.20);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert_eq!(counters.skip_wide_spread.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn risk_state_unknown_blocks_all() {
        let state = SharedState::new();
        {
            let mut sc = state.scalars.lock();
            sc.underlying_price = 6.0;
            // risk_effective_delta is None → fail-closed.
        }
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert_eq!(counters.risk_block.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn delta_kill_blocks_all() {
        let state = fresh_state(6.0);
        {
            let mut sc = state.scalars.lock();
            sc.delta_kill = 5.0;
            sc.risk_effective_delta = Some(5.0);
        }
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert_eq!(counters.risk_block.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn theta_kill_blocks_all() {
        // 2026-05-05 incident regression test: theta of -850 with
        // theta_kill at -500 must block placements, not fall through.
        let state = fresh_state(6.0);
        {
            let mut sc = state.scalars.lock();
            sc.theta_kill = -500.0;
            sc.risk_theta = Some(-850.0);
        }
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert_eq!(counters.risk_block.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn theta_kill_disabled_when_zero() {
        // CLAUDE.md §13 pattern: 0 disables. A theta of -1000 must NOT
        // block when theta_kill is 0.
        let state = fresh_state(6.0);
        {
            let mut sc = state.scalars.lock();
            sc.theta_kill = 0.0; // disabled
            sc.risk_theta = Some(-1000.0);
        }
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        // Should NOT block on theta. (May skip for other reasons, e.g.
        // wide spread, but risk_block from theta should be 0.)
        let _ = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        // The risk_block counter increments by 2 only when ALL is set.
        // With only theta_kill=0 and no other gate trips, ALL won't fire
        // from the theta path.
        assert_eq!(counters.risk_block.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn vega_kill_blocks_all() {
        let state = fresh_state(6.0);
        {
            let mut sc = state.scalars.lock();
            sc.vega_kill = 500.0;
            sc.risk_vega = Some(-700.0); // |vega| > kill
        }
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
        assert_eq!(counters.risk_block.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn weekend_pause_blocks() {
        let state = fresh_state(6.0);
        state.scalars.lock().weekend_paused = true;
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
    }

    #[test]
    fn kill_state_blocks() {
        let state = fresh_state(6.0);
        state
            .kills
            .insert("daily_halt".to_string(), "test".to_string());
        // Mirror the kills_count bookkeeping that process_event does
        // on a real `kill` message — the hot path reads kills_count
        // (atomic) rather than kills.is_empty() (which would touch
        // every DashMap shard) so tests must keep them in sync.
        state.kills_count.fetch_add(1, Ordering::Relaxed);
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(6.0, "20260526", "C", 0.10, 0.12);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let decisions = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
        assert!(decisions.is_empty());
    }

    #[test]
    fn taylor_cache_stores_theo_and_delta_pair() {
        // Verifies the cache layout change (f64 → (f64, f64)). The
        // Taylor reprice path in decide_on_tick reads the stored
        // delta_at_fit; without the pair shape, the formula has no
        // delta to apply. Pricing-level correctness of the Taylor
        // formula is covered by `black76_taylor_first_order_check`
        // in pricing.rs.
        //
        // Test runs decide_on_tick with a tick that's likely to be
        // skipped by downstream gates (wide spread) — but the theo
        // cache populates BEFORE those gates fire, so the cache
        // entry is observable regardless of whether a Place result.
        let state = fresh_state(6.00);
        install_svi_surface(&state, "20260526", 'C');
        let counters = DecisionCounters::default();
        let tick = make_tick(6.05, "20260526", "C", 0.05, 0.30);
        let expiry_arc = state.intern_expiry(&tick.expiry);
        let now_wall_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        let _ = run_decide(&state, &counters, &tick, &expiry_arc, 1_000_000_000, now_wall_ns);

        // Cache should hold one entry — the (theo_at_fit, delta_at_fit) pair.
        let cache_entries: Vec<_> = state
            .theo_cache
            .iter()
            .map(|e| *e.value())
            .collect();
        assert_eq!(cache_entries.len(), 1, "expected exactly one cache entry");
        let (theo_at_fit, delta_at_fit) = cache_entries[0];
        assert!(theo_at_fit > 0.0, "theo_at_fit must be positive: {}", theo_at_fit);
        // Slightly-OTM call (K=6.05 vs F=6.0) → delta in (0, 0.5).
        assert!(
            delta_at_fit > 0.0 && delta_at_fit < 0.5,
            "delta_at_fit out of expected range for slightly-OTM call: {}",
            delta_at_fit
        );
    }
}
