//! Trader state. Mirrors src/trader/main.py's TraderState dataclass.
//! Lives entirely in one struct so the hot-path can pass &mut self.

use crate::messages::VolParams;
use ahash::AHashMap;
use std::collections::VecDeque;

/// Slim option state cached per (strike, expiry, right). Drops the
/// expiry/right strings that the TickMsg carries (they're already in
/// the key tuple); avoids one heap allocation per tick. Also avoids
/// pulling the message-type string into the hot path.
///
/// `strike` and `ts_ns` are read by `staleness_check` (in main.rs)
/// and the JSONL writers; the warning about them being "never read"
/// is a false positive when the struct is matched generically.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default)]
pub struct OptionState {
    pub strike: f64,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub bid_size: Option<i32>,
    pub ask_size: Option<i32>,
    pub ts_ns: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct VolSurfaceEntry {
    pub forward: f64,
    pub params: VolParams,
}

/// Per-resting-order metadata; keyed by (strike, expiry, right, side).
/// `send_ns` is for log fidelity only; the hot path uses
/// `place_monotonic_ns` for cooldown / GTD tracking.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct OurOrder {
    pub price: f64,
    pub send_ns: u64,             // wall-clock at send (for logs)
    pub place_monotonic_ns: u64,  // monotonic at last place (cooldown / GTD)
    pub order_id: Option<i64>,    // populated on place_ack
}

pub struct TraderState {
    /// Latest tick per (strike-bits, expiry, right-char). Stores the
    /// slim OptionState (no redundant strings). Bounded by quoted
    /// strikes (≤60 in production).
    pub options: AHashMap<(u64, String, char), OptionState>,

    /// Vol surface params per (expiry, right-char). Bounded by ~4 entries.
    pub vol_surfaces: AHashMap<(String, char), VolSurfaceEntry>,

    /// Underlying spot. Updated from underlying_tick.
    pub underlying_price: f64,

    /// Resting orders we've placed, keyed by
    /// (strike-bits, expiry, right-char, side-char).
    /// side-char: 'B' for BUY, 'S' for SELL.
    pub our_orders: AHashMap<(u64, String, char, char), OurOrder>,

    /// orderId → key reverse map, for terminal-status cleanup.
    pub orderid_to_key: AHashMap<i64, (u64, String, char, char)>,

    /// Trader-side risk state from broker (1Hz publish).
    pub risk_effective_delta: Option<f64>,
    pub risk_margin_pct: Option<f64>,
    pub risk_state_age_monotonic_ns: u64,

    /// Configured limits (from broker hello).
    pub min_edge_ticks: i32,
    pub tick_size: f64,
    pub delta_ceiling: f64,
    pub delta_kill: f64,
    pub margin_ceiling_pct: f64,

    /// IPC + TTT histogram samples (bounded ring).
    pub ipc_us: VecDeque<u64>,
    pub ttt_us: VecDeque<u64>,

    /// Kills / weekend pause.
    pub kills: AHashMap<String, String>,
    pub weekend_paused: bool,
}

impl TraderState {
    pub fn new() -> Self {
        Self {
            options: AHashMap::new(),
            vol_surfaces: AHashMap::new(),
            underlying_price: 0.0,
            our_orders: AHashMap::new(),
            orderid_to_key: AHashMap::new(),
            risk_effective_delta: None,
            risk_margin_pct: None,
            risk_state_age_monotonic_ns: 0,
            // Defaults; broker hello overrides.
            min_edge_ticks: 2,
            tick_size: 0.0005,
            delta_ceiling: 3.0,
            delta_kill: 5.0,
            margin_ceiling_pct: 0.50,
            ipc_us: VecDeque::with_capacity(2000),
            ttt_us: VecDeque::with_capacity(500),
            kills: AHashMap::new(),
            weekend_paused: false,
        }
    }

    /// Convert a strike to the bit-pattern key we use in maps.
    /// (HashMap on f64 is annoying; the strike grid is fixed 0.05 increments
    /// so the bit pattern is stable.)
    #[inline(always)]
    pub fn strike_key(strike: f64) -> u64 {
        strike.to_bits()
    }
}

/// Counters for the telemetry payload. Equivalent to Python's
/// decisions_made Counter dict.
#[derive(Default)]
pub struct DecisionCounters {
    pub place: u64,
    pub skip_no_vol_surface: u64,
    pub skip_off_atm: u64,
    pub skip_itm: u64,
    pub skip_thin_book: u64,
    pub skip_one_sided_or_dark: u64,
    pub skip_forward_drift: u64,
    pub skip_in_band: u64,
    pub skip_cooldown: u64,
    pub skip_dark_at_place: u64,
    pub skip_target_nonpositive: u64,
    pub skip_would_cross_ask: u64,
    pub skip_would_cross_bid: u64,
    pub skip_other: u64,
    pub risk_block: u64,
    pub risk_block_buy: u64,
    pub risk_block_sell: u64,
    pub staleness_cancel: u64,
    pub staleness_cancel_dark: u64,
    pub replace_cancel: u64,
    pub replace_skip_cancel_near_gtd: u64,
}

impl DecisionCounters {
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        // Only emit non-zero counters to keep telemetry payload small,
        // matching Python's Counter() emit style.
        let pairs: &[(&str, u64)] = &[
            ("place", self.place),
            ("skip_no_vol_surface", self.skip_no_vol_surface),
            ("skip_off_atm", self.skip_off_atm),
            ("skip_itm", self.skip_itm),
            ("skip_thin_book", self.skip_thin_book),
            ("skip_one_sided_or_dark", self.skip_one_sided_or_dark),
            ("skip_forward_drift", self.skip_forward_drift),
            ("skip_in_band", self.skip_in_band),
            ("skip_cooldown", self.skip_cooldown),
            ("skip_dark_at_place", self.skip_dark_at_place),
            ("skip_target_nonpositive", self.skip_target_nonpositive),
            ("skip_would_cross_ask", self.skip_would_cross_ask),
            ("skip_would_cross_bid", self.skip_would_cross_bid),
            ("skip_other", self.skip_other),
            ("risk_block", self.risk_block),
            ("risk_block_buy", self.risk_block_buy),
            ("risk_block_sell", self.risk_block_sell),
            ("staleness_cancel", self.staleness_cancel),
            ("staleness_cancel_dark", self.staleness_cancel_dark),
            ("replace_cancel", self.replace_cancel),
            ("replace_skip_cancel_near_gtd", self.replace_skip_cancel_near_gtd),
        ];
        for (k, v) in pairs {
            if *v > 0 {
                m.insert((*k).to_string(), serde_json::json!(v));
            }
        }
        serde_json::Value::Object(m)
    }
}
