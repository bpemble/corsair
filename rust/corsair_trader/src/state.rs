//! Trader state. Mirrors src/trader/main.py's TraderState dataclass.
//! Lives entirely in one struct so the hot-path can pass &mut self.

use crate::messages::VolParams;
use ahash::AHashMap;
use std::collections::VecDeque;
use std::sync::Arc;

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
    /// v2 wire-timing: broker_recv_ns from the latest TickMsg. Echoed
    /// back on PlaceOrder so the broker can compute tick→ack latency.
    pub broker_recv_ns: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct VolSurfaceEntry {
    pub forward: f64,
    pub params: VolParams,
    /// Broker fit timestamp (CLOCK_REALTIME ns). Used to gate quoting
    /// when the surface is stale (HI-002).
    pub fit_ts_ns: u64,
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
    /// Latest tick per (strike-bits, expiry-arc, right-char). Stores
    /// the slim OptionState (no redundant strings). Bounded by quoted
    /// strikes (≤60 in production).
    ///
    /// Expiry uses `Arc<str>` interned via `intern_expiry` — production
    /// has ~4 unique expiries, so all clones become Arc bumps (~5ns)
    /// instead of String heap allocations (~70ns each). The keys for
    /// `options`, `vol_surfaces`, `theo_cache`, `our_orders`, and
    /// `orderid_to_key` all share the same Arcs.
    pub options: AHashMap<(u64, Arc<str>, char), OptionState>,

    /// Vol surface params per (expiry, right-char). Bounded by ~4 entries.
    pub vol_surfaces: AHashMap<(Arc<str>, char), VolSurfaceEntry>,

    /// Optimization #3 — SVI/SABR theo cache, keyed by
    /// (strike_bits, expiry, right_char, fit_ts_ns). theo is a pure
    /// function of (forward, strike, tte, params, right); within a
    /// single fit cycle the only changing input is tte, which moves
    /// less than 1 tick over a 60s fit window. We invalidate the
    /// entry when fit_ts_ns changes (new SABR fit landed). Saves
    /// ~80µs per tick (SVI/SABR + Black76) in the steady-state amend
    /// loop where the same key sees many ticks per second.
    pub theo_cache: AHashMap<(u64, Arc<str>, char, u64), f64>,

    /// Underlying spot. Updated from underlying_tick.
    pub underlying_price: f64,

    /// Resting orders we've placed, keyed by
    /// (strike-bits, expiry, right-char, side-char).
    /// side-char: 'B' for BUY, 'S' for SELL.
    pub our_orders: AHashMap<(u64, Arc<str>, char, char), OurOrder>,

    /// orderId → key reverse map, for terminal-status cleanup.
    pub orderid_to_key: AHashMap<i64, (u64, Arc<str>, char, char)>,

    /// Expiry interning table. Production sees ~4 unique expiries; the
    /// table is essentially immortal after warmup. Key is `String` so
    /// lookups can use `&str` via `Borrow<str>` without allocating a
    /// new String per query.
    pub expiry_intern: AHashMap<String, Arc<str>>,

    /// Trader-side risk state from broker (1Hz publish).
    pub risk_effective_delta: Option<f64>,
    pub risk_margin_pct: Option<f64>,
    /// LOW-004: cached hedge delta from the broker's 1Hz risk_state
    /// publish. Telemetry surfaces the latest value for observability
    /// alongside risk_effective_delta. None until the first
    /// risk_state event arrives.
    pub risk_hedge_delta: Option<i64>,
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
            theo_cache: AHashMap::new(),
            underlying_price: 0.0,
            our_orders: AHashMap::new(),
            orderid_to_key: AHashMap::new(),
            expiry_intern: AHashMap::new(),
            risk_effective_delta: None,
            risk_margin_pct: None,
            risk_hedge_delta: None,
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

    /// Return the interned `Arc<str>` for `expiry`; allocate on first
    /// sight, Arc-bump thereafter. Production tops out at ~4 unique
    /// expiries; the table never grows large.
    #[inline]
    pub fn intern_expiry(&mut self, expiry: &str) -> Arc<str> {
        if let Some(arc) = self.expiry_intern.get(expiry) {
            return Arc::clone(arc);
        }
        let arc: Arc<str> = Arc::from(expiry);
        self.expiry_intern.insert(expiry.to_string(), Arc::clone(&arc));
        arc
    }
}

/// Counters for the telemetry payload. Equivalent to Python's
/// decisions_made Counter dict.
#[derive(Default)]
pub struct DecisionCounters {
    pub place: u64,
    pub skip_no_vol_surface: u64,
    /// HI-002: vol surface fit_ts_ns is >120s old; broker likely
    /// disconnected or fitter stalled.
    pub skip_vol_surface_stale: u64,
    /// HI-003: previous place at this key is still unack'd
    /// (order_id is None) and within the cooldown floor. Skip rather
    /// than fire a duplicate that would orphan the first.
    pub skip_unack_inflight: u64,
    /// MED-005: inbound IPC frames whose msgpack body failed to
    /// deserialize as the expected typed event. Bumps when broker
    /// schema drifts vs trader schema (e.g. a new required field on
    /// either side that the other doesn't know about). Surfaces in
    /// 10s telemetry alongside other counters; non-zero rate means
    /// recompile/redeploy needed.
    pub dropped_parse_errors: u64,
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
    /// Quote update fired as a single modify_order (amend) instead of
    /// cancel + place. Replaces most replace_cancel events: a known
    /// live order_id at the key + price-update path goes through
    /// modify in one round trip.
    pub modify: u64,
    /// Modify rejected by IBKR (price too far, contract issue, etc).
    /// Trader falls back to cancel + place. Currently increments
    /// from the broker side via the modify_order Result; the trader
    /// logs the broker-side reject but doesn't yet observe it on
    /// the IPC. Reserved for the post-`order_status_modify` path.
    pub modify_reject: u64,
}

impl DecisionCounters {
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        // Only emit non-zero counters to keep telemetry payload small,
        // matching Python's Counter() emit style.
        let pairs: &[(&str, u64)] = &[
            ("place", self.place),
            ("skip_no_vol_surface", self.skip_no_vol_surface),
            // Audit T4-3: previously omitted from telemetry.
            ("skip_vol_surface_stale", self.skip_vol_surface_stale),
            ("skip_unack_inflight", self.skip_unack_inflight),
            ("dropped_parse_errors", self.dropped_parse_errors),
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
            ("modify", self.modify),
            ("modify_reject", self.modify_reject),
        ];
        for (k, v) in pairs {
            if *v > 0 {
                m.insert((*k).to_string(), serde_json::json!(v));
            }
        }
        serde_json::Value::Object(m)
    }
}
