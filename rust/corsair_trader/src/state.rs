//! Trader state. Mirrors src/trader/main.py's TraderState dataclass.
//!
//! Lock-shard layout (Priority 1, 2026-05-04):
//!   - High-cardinality maps live in `dashmap::DashMap`. DashMap
//!     internally shards by key hash so the 10Hz staleness sweep and
//!     the 10s telemetry snapshot can iterate one shard while the hot
//!     loop inserts into another shard without contention.
//!   - Histograms (`ipc_ns`, `ttt_ns`) live behind their own
//!     `parking_lot::Mutex`. The telemetry sort is ~ms; isolating the
//!     histograms keeps that work off the hot path's lock budget.
//!     Samples stored in nanoseconds (switched 2026-05-06 from µs to
//!     resolve sub-µs A/B comparisons; integer-truncated µs lost
//!     5–20% of the dynamic range we needed to measure).
//!   - Scalars (config, risk_state, underlying_price, weekend_paused)
//!     live behind a small `parking_lot::Mutex`. Hot path snapshots
//!     them once per tick into a stack-local view; broker-event
//!     handlers and `risk_state` updates take the lock briefly to
//!     write.
//!
//! The previous design wrapped the entire `TraderState` in one
//! `std::sync::Mutex` and the hot path took it three times per tick
//! (~10µs each). Bg tasks contended on that single mutex, producing
//! 50µs–1ms p99 tail events when telemetry/staleness happened to
//! arrive while the hot loop held it. The shard layout removes the
//! single point of serialization.

use crate::messages::VolParams;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Slim option state cached per (strike, expiry, right). Drops the
/// expiry/right strings that the TickMsg carries (they're already in
/// the key tuple); avoids one heap allocation per tick. Also avoids
/// pulling the message-type string into the hot path.
///
/// `strike` was previously stored here for log fidelity but is fully
/// recoverable from `OptionKey.0` (the integer-quantized key inverse-
/// converts back to f64 via `key.0 as f64 / 10_000.0`). Likewise
/// `ts_ns`/`broker_recv_ns` were set but never read elsewhere — the
/// only consumer of broker_recv_ns reads it off the live `TickMsg`.
/// Both removed; the cache is now strictly the L1 BBO snapshot.
#[derive(Debug, Clone, Copy, Default)]
pub struct OptionState {
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub bid_size: Option<i32>,
    pub ask_size: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct VolSurfaceEntry {
    pub forward: f64,
    pub params: VolParams,
    /// Broker fit timestamp (CLOCK_REALTIME ns). Used to gate quoting
    /// when the surface is stale (HI-002).
    pub fit_ts_ns: u64,
    /// Calibrated-strike envelope from the broker's fit. Trader's
    /// is_strike_calibrated gate skips strikes outside this range;
    /// None when broker is older than the gate (back-compat).
    pub calibrated_min_k: Option<f64>,
    pub calibrated_max_k: Option<f64>,
    /// Spot price observed at fit time. Anchor for the Taylor reprice
    /// path: theo += delta × (current_spot − spot_at_fit). Distinct
    /// from `forward` so the static carry between front-month and the
    /// option's underlying month doesn't pollute the Taylor shift.
    pub spot_at_fit: f64,
}

/// Per-resting-order metadata; keyed by (strike, expiry, right, side).
#[derive(Debug, Clone)]
pub struct OurOrder {
    pub price: f64,
    pub place_monotonic_ns: u64,  // monotonic at last place (cooldown / GTD)
    pub order_id: Option<i64>,    // populated on place_ack
}

/// HashMap key types. Aliased so the verbose tuple shape lives in one
/// place; refactors that change the shape touch one line.
///
/// The strike component is a quantized integer (`strike_key()` returns
/// `(strike * 10_000).round() as i64`) — using `f64::to_bits()`
/// previously meant any rounding drift between producers (e.g. broker
/// emitting 6.025000000000001 vs trader cache showing 6.025) produced
/// distinct keys and silent cache misses. The integer form rounds to
/// the nearest 1/10_000 (well below the 0.0005 tick grid) so two
/// representations of the same strike collide deterministically.
pub type OptionKey = (i64, Arc<str>, char);
pub type VolSurfaceKey = (Arc<str>, char);
pub type TheoCacheKey = (i64, Arc<str>, char, u64);
pub type OurOrderKey = (i64, Arc<str>, char, char);

/// Histograms behind their own mutex. Hot path pushes one sample;
/// telemetry sorts a snapshot every 10s.
///
/// Caps default to the production values (2000 IPC, 500 TTT) but are
/// overridable via `CORSAIR_TRADER_HIST_IPC_CAP` /
/// `CORSAIR_TRADER_HIST_TTT_CAP` env vars at boot. The replay harness
/// (corsair_tick_replay → trader) bumps these to ~50k so a 3-minute
/// run captures every sample for KS/bootstrap comparison; production
/// stays at the smaller caps so the telemetry snapshot stays cheap.
pub struct Histograms {
    pub ipc_ns: VecDeque<u64>,
    pub ttt_ns: VecDeque<u64>,
    pub ipc_cap: usize,
    pub ttt_cap: usize,
}

impl Default for Histograms {
    fn default() -> Self {
        Self {
            ipc_ns: VecDeque::new(),
            ttt_ns: VecDeque::new(),
            ipc_cap: env_usize("CORSAIR_TRADER_HIST_IPC_CAP", 2000),
            ttt_cap: env_usize("CORSAIR_TRADER_HIST_TTT_CAP", 500),
        }
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

/// Small fields read on every tick. Snapshot into `ScalarSnapshot`
/// once per tick to avoid taking the lock repeatedly inside
/// `decide_on_tick`.
#[derive(Debug)]
pub struct ScalarState {
    pub underlying_price: f64,
    pub risk_effective_delta: Option<f64>,
    pub risk_margin_pct: Option<f64>,
    pub risk_hedge_delta: Option<i64>,
    /// Latest broker-published theta (sum across positions). Negative
    /// for short-vol books. Trader self-gates against `theta_kill`
    /// as defense-in-depth. 2026-05-05 incident: theta -$850 ran
    /// silently for 6h because trader had no theta check.
    pub risk_theta: Option<f64>,
    /// Latest broker-published vega.
    pub risk_vega: Option<f64>,
    pub risk_state_age_monotonic_ns: u64,
    pub min_edge_ticks: i32,
    pub tick_size: f64,
    pub delta_ceiling: f64,
    pub delta_kill: f64,
    /// Theta breach threshold (negative; 0 disables). From broker hello.
    pub theta_kill: f64,
    /// Vega breach threshold (positive; 0 disables). From broker hello.
    pub vega_kill: f64,
    pub margin_ceiling_pct: f64,
    pub weekend_paused: bool,
    /// Quote-lifetime config from broker hello. Defaults match
    /// CLAUDE.md §16 Layer 6 throttling chain — replaced by broker
    /// values when hello arrives.
    pub gtd_lifetime_s: f64,
    pub gtd_refresh_lead_s: f64,
    pub dead_band_ticks: i32,
    /// Spec §3.4 wide-spread skip multiplier; 0 disables.
    pub skip_if_spread_over_edge_mul: f64,
}

impl ScalarState {
    fn new() -> Self {
        Self {
            underlying_price: 0.0,
            risk_effective_delta: None,
            risk_margin_pct: None,
            risk_hedge_delta: None,
            risk_theta: None,
            risk_vega: None,
            risk_state_age_monotonic_ns: 0,
            min_edge_ticks: 2,
            tick_size: 0.0005,
            delta_ceiling: 3.0,
            delta_kill: 5.0,
            theta_kill: -500.0,
            vega_kill: 0.0,
            margin_ceiling_pct: 0.50,
            weekend_paused: false,
            // Defaults match the broker's QuotingSection defaults
            // (corsair_broker/src/config.rs::default_*). When the
            // `hello` IPC arrives, these are overwritten with the
            // YAML's runtime values; if hello is dropped (rare —
            // ring full at boot), we degrade to broker-aligned
            // values rather than the old Python-era constants.
            gtd_lifetime_s: 30.0,
            gtd_refresh_lead_s: 3.5,
            dead_band_ticks: 1,
            skip_if_spread_over_edge_mul: 4.0,
        }
    }

    /// Plain-old-data snapshot for the hot path. Reading scalars under
    /// a parking_lot guard takes ~5ns; copying the struct after release
    /// is cheap and lets `decide_on_tick` operate without holding the
    /// scalar lock through the full decision flow.
    pub fn snapshot(&self) -> ScalarSnapshot {
        ScalarSnapshot {
            underlying_price: self.underlying_price,
            risk_effective_delta: self.risk_effective_delta,
            risk_margin_pct: self.risk_margin_pct,
            risk_theta: self.risk_theta,
            risk_vega: self.risk_vega,
            risk_state_age_monotonic_ns: self.risk_state_age_monotonic_ns,
            min_edge_ticks: self.min_edge_ticks,
            tick_size: self.tick_size,
            delta_ceiling: self.delta_ceiling,
            delta_kill: self.delta_kill,
            theta_kill: self.theta_kill,
            vega_kill: self.vega_kill,
            margin_ceiling_pct: self.margin_ceiling_pct,
            weekend_paused: self.weekend_paused,
            gtd_lifetime_s: self.gtd_lifetime_s,
            gtd_refresh_lead_s: self.gtd_refresh_lead_s,
            dead_band_ticks: self.dead_band_ticks,
            skip_if_spread_over_edge_mul: self.skip_if_spread_over_edge_mul,
        }
    }
}

/// Stack-local copy of `ScalarState` for the duration of a single
/// `decide_on_tick` call.
#[derive(Debug, Clone, Copy)]
pub struct ScalarSnapshot {
    pub underlying_price: f64,
    pub risk_effective_delta: Option<f64>,
    pub risk_margin_pct: Option<f64>,
    pub risk_theta: Option<f64>,
    pub risk_vega: Option<f64>,
    pub risk_state_age_monotonic_ns: u64,
    pub min_edge_ticks: i32,
    pub tick_size: f64,
    pub delta_ceiling: f64,
    pub delta_kill: f64,
    pub theta_kill: f64,
    pub vega_kill: f64,
    pub margin_ceiling_pct: f64,
    pub weekend_paused: bool,
    pub gtd_lifetime_s: f64,
    pub gtd_refresh_lead_s: f64,
    pub dead_band_ticks: i32,
    pub skip_if_spread_over_edge_mul: f64,
}

/// Shared trader state. All fields use interior mutability so the hot
/// path and bg tasks can share `&SharedState` (no outer Mutex).
pub struct SharedState {
    /// Latest tick per (strike-bits, expiry-arc, right-char). Stores
    /// the slim OptionState (no redundant strings). Bounded by quoted
    /// strikes (≤60 in production).
    ///
    /// Expiry uses `Arc<str>` interned via `intern_expiry` — production
    /// has ~4 unique expiries, so all clones become Arc bumps (~5ns)
    /// instead of String heap allocations (~70ns each). The keys for
    /// `options`, `vol_surfaces`, `theo_cache`, `our_orders`, and
    /// `orderid_to_key` all share the same Arcs.
    pub options: DashMap<OptionKey, OptionState>,

    /// Vol surface params per (expiry, right-char). Bounded by ~4
    /// entries.
    ///
    /// Bundle 3E (2026-05-06): values are wrapped in `Arc<>` so
    /// `lookup_vol_surface` can clone the `Arc` (5 ns refcount bump)
    /// instead of cloning the full `VolSurfaceEntry` (a `String` for
    /// `model` plus 8 `Option<f64>` fields — measurable per-tick cost).
    pub vol_surfaces: DashMap<VolSurfaceKey, Arc<VolSurfaceEntry>>,

    /// Optimization #3 — SVI/SABR theo cache, keyed by
    /// (strike_bits, expiry, right_char, fit_ts_ns). Stores the pair
    /// (theo_at_fit, delta_at_fit), both pure functions of
    /// (fit_forward, strike, tte, params, right); within a single fit
    /// cycle the only changing input is tte, which moves less than 1
    /// tick over a 60s fit window. We invalidate the entry when
    /// fit_ts_ns changes (new SABR fit landed). Saves ~80µs per tick
    /// (SVI/SABR + Black76 + greeks) in the steady-state amend loop.
    /// Delta is cached alongside theo so the Taylor reprice path
    /// (theo + delta × (spot − fit_forward)) doesn't recompute greeks
    /// on the hot path.
    pub theo_cache: DashMap<TheoCacheKey, (f64, f64)>,

    /// Resting orders we've placed, keyed by
    /// (strike-bits, expiry, right-char, side-char).
    /// side-char: 'B' for BUY, 'S' for SELL.
    pub our_orders: DashMap<OurOrderKey, OurOrder>,

    /// orderId → key reverse map, for terminal-status cleanup.
    pub orderid_to_key: DashMap<i64, OurOrderKey>,

    /// Expiry interning table. Production sees ~4 unique expiries; the
    /// table is essentially immortal after warmup. Hot-path-only
    /// writer (process_event runs single-threaded on the main tokio
    /// worker), so no race on first-insert.
    pub expiry_intern: DashMap<String, Arc<str>>,

    /// Active kills by source. Broker emits `kill` / `resume` events
    /// to set/clear. The hot path uses `kills_count` (an atomic
    /// mirror of `kills.len()`) for the per-tick check — `DashMap::
    /// is_empty()` would touch every shard (~128 reads on a 32-thread
    /// box), measurable per-tick cost; the atomic is one Relaxed
    /// load.
    pub kills: DashMap<String, String>,
    /// Mirror of `kills.len()` maintained by the kill/resume handlers
    /// in `process_event`. Read from the hot path's risk gate.
    pub kills_count: AtomicUsize,

    /// Histogram samples (bounded ring). Separate mutex from scalars
    /// so telemetry's sort doesn't block hot-path scalar reads.
    pub histograms: Mutex<Histograms>,

    /// Small fields read on every tick. Snapshot to a stack-local
    /// `ScalarSnapshot` to avoid repeated lock acquires in the hot
    /// path.
    pub scalars: Mutex<ScalarState>,

    /// Tier A Exp 2 (2026-05-06): per-tick burst limiter.
    ///
    /// Caps concurrent outbound wire round-trips at `MAX_BURST_SENDS`
    /// (2) within a `BURST_WINDOW_NS` (50ms) sliding window. Empirical
    /// from `logs-paper/wire_timing-2026-05-06.jsonl` (n=32k):
    ///
    ///   inflight=0   p50= 42ms p99= 443ms
    ///   inflight=1   p50= 50ms p99= 655ms
    ///   inflight=2   p50=246ms p99= 920ms   ← 5× jump at p50
    ///   inflight=3+  p50=351-683ms p99=1.0-1.1s
    ///
    /// ~10% of orders today fire at inflight≥2 and pay the inflated
    /// RTT. Gating new sends at the 2nd-concurrent boundary defers
    /// those by ≤50ms (one window), letting them re-fire when the
    /// queue has drained.
    pub outbound_limiter: OutboundLimiter,

    /// Burst-cap window in ns. Default `BURST_WINDOW_NS_DEFAULT`
    /// (50 ms); `CORSAIR_TRADER_BURST_WINDOW_NS=0` disables the gate.
    /// Read once at boot in `main.rs` and stored here so the hot path
    /// avoids touching the env on every tick.
    pub burst_window_ns: u64,
}

/// Sliding-window burst limiter for outbound IPC commands.
/// 2 timestamp slots — if both have entries within the last
/// `BURST_WINDOW_NS`, new sends are gated. Slot eviction picks the
/// oldest timestamp.
#[derive(Default)]
pub struct OutboundLimiter {
    slot_a: AtomicU64,
    slot_b: AtomicU64,
}

impl OutboundLimiter {
    /// Try to consume a burst slot at `now_ns`. Returns true if the
    /// caller may send (not at cap), false if it should skip.
    /// `max_age_ns` is the sliding-window length.
    #[inline]
    pub fn try_consume(&self, now_ns: u64, max_age_ns: u64) -> bool {
        let a = self.slot_a.load(Ordering::Relaxed);
        let b = self.slot_b.load(Ordering::Relaxed);
        let a_recent = now_ns.saturating_sub(a) < max_age_ns;
        let b_recent = now_ns.saturating_sub(b) < max_age_ns;
        if a_recent && b_recent {
            return false;
        }
        // Overwrite the oldest slot. Under hot-path single-thread
        // access this is race-free; under cross-thread (staleness
        // loop calling concurrently) the worst case is one extra
        // send slipping through — acceptable.
        if a <= b {
            self.slot_a.store(now_ns, Ordering::Relaxed);
        } else {
            self.slot_b.store(now_ns, Ordering::Relaxed);
        }
        true
    }
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            options: DashMap::new(),
            vol_surfaces: DashMap::new(),
            theo_cache: DashMap::new(),
            our_orders: DashMap::new(),
            orderid_to_key: DashMap::new(),
            expiry_intern: DashMap::new(),
            kills: DashMap::new(),
            kills_count: AtomicUsize::new(0),
            histograms: Mutex::new(Histograms::default()),
            scalars: Mutex::new(ScalarState::new()),
            outbound_limiter: OutboundLimiter::default(),
            burst_window_ns: env_u64(
                "CORSAIR_TRADER_BURST_WINDOW_NS",
                crate::decision::BURST_WINDOW_NS_DEFAULT,
            ),
        }
    }

    /// Convert a strike to the integer key we use in maps. Quantizes
    /// to 1/10_000 — the strike grid is 0.0005, so this resolution is
    /// 5× finer than any valid strike spacing. Using `to_bits()`
    /// previously caused silent cache misses whenever two producers
    /// generated bit-different f64s for the same nominal strike (e.g.
    /// 6.025 vs 6.025000000000001 from float-arithmetic round-trip).
    #[inline(always)]
    pub fn strike_key(strike: f64) -> i64 {
        (strike * 10_000.0).round() as i64
    }

    /// Return the interned `Arc<str>` for `expiry`; allocate on first
    /// sight, Arc-bump thereafter. Production tops out at ~4 unique
    /// expiries; the table never grows large. Hot-path-only caller
    /// (process_event on the main worker) — no cross-thread race.
    #[inline]
    pub fn intern_expiry(&self, expiry: &str) -> Arc<str> {
        if let Some(arc) = self.expiry_intern.get(expiry) {
            return Arc::clone(arc.value());
        }
        let arc: Arc<str> = Arc::from(expiry);
        self.expiry_intern.insert(expiry.to_string(), Arc::clone(&arc));
        arc
    }

    /// Snapshot the scalar block into a stack-local view. One lock
    /// acquire per tick.
    #[inline]
    pub fn scalar_snapshot(&self) -> ScalarSnapshot {
        self.scalars.lock().snapshot()
    }

    /// Look up a vol surface for `(expiry, right)`, falling back to
    /// `(expiry, 'C')` then `(expiry, 'P')`. Older brokers fit a
    /// per-side surface; newer brokers emit `side="BOTH"` which the
    /// dispatcher splits into both 'C' and 'P' entries. The fallback
    /// chain handles back-compat with surfaces that only have one side
    /// populated. Returns the cloned `VolSurfaceEntry` (or None) so
    /// the DashMap read guard drops before the caller does any work.
    #[inline]
    pub fn lookup_vol_surface(
        &self,
        expiry: &Arc<str>,
        right: char,
    ) -> Option<Arc<VolSurfaceEntry>> {
        self.vol_surfaces
            .get(&(Arc::clone(expiry), right))
            .map(|r| Arc::clone(r.value()))
            .or_else(|| {
                self.vol_surfaces
                    .get(&(Arc::clone(expiry), 'C'))
                    .map(|r| Arc::clone(r.value()))
            })
            .or_else(|| {
                self.vol_surfaces
                    .get(&(Arc::clone(expiry), 'P'))
                    .map(|r| Arc::clone(r.value()))
            })
    }
}

/// Counters for the telemetry payload. Equivalent to Python's
/// decisions_made Counter dict. All fields are AtomicU64 so the hot
/// path's counter bumps are wait-free (no mutex acquire).
pub struct DecisionCounters {
    pub place: AtomicU64,
    pub skip_no_vol_surface: AtomicU64,
    /// HI-002: vol surface fit_ts_ns is >120s old; broker likely
    /// disconnected or fitter stalled.
    pub skip_vol_surface_stale: AtomicU64,
    /// Audit item 6 / spec §3.3: strike outside SABR fit's calibrated
    /// range (`[calibrated_min_k, calibrated_max_k]` shipped via
    /// vol_surface IPC).
    pub skip_uncalibrated_strike: AtomicU64,
    /// HI-003: previous place at this key is still unack'd
    /// (order_id is None) and within the cooldown floor. Skip rather
    /// than fire a duplicate that would orphan the first.
    pub skip_unack_inflight: AtomicU64,
    /// MED-005: inbound IPC frames whose msgpack body failed to
    /// deserialize as the expected typed event. Bumps when broker
    /// schema drifts vs trader schema (e.g. a new required field on
    /// either side that the other doesn't know about). Surfaces in
    /// 10s telemetry alongside other counters; non-zero rate means
    /// recompile/redeploy needed.
    pub dropped_parse_errors: AtomicU64,
    pub skip_off_atm: AtomicU64,
    pub skip_itm: AtomicU64,
    pub skip_thin_book: AtomicU64,
    pub skip_one_sided_or_dark: AtomicU64,
    /// Spec §3.4: half_spread > skip_if_spread_over_edge_mul × min_edge.
    pub skip_wide_spread: AtomicU64,
    pub skip_forward_drift: AtomicU64,
    pub skip_in_band: AtomicU64,
    pub skip_cooldown: AtomicU64,
    pub skip_target_nonpositive: AtomicU64,
    pub skip_would_cross_ask: AtomicU64,
    pub skip_would_cross_bid: AtomicU64,
    pub skip_other: AtomicU64,
    pub risk_block: AtomicU64,
    pub risk_block_buy: AtomicU64,
    pub risk_block_sell: AtomicU64,
    pub staleness_cancel: AtomicU64,
    pub staleness_cancel_dark: AtomicU64,
    /// 2026-05-05 amend bias: drift-stale orders now refresh price via
    /// `modify_order` instead of `cancel`+next-tick-place. Each modify
    /// saves ~145 ms RTT vs cancel+place (per wire_timing analysis:
    /// modify p50=41ms, place p50=186ms). Dark-book staleness still
    /// uses cancel since we don't want to be in a dark market at any
    /// price. Increment counts drift-only refreshes.
    pub staleness_modify: AtomicU64,
    /// Quote update fired as a single modify_order (amend) instead of
    /// cancel + place. The `cancel + place` path was retired in favor
    /// of single-round-trip modify when a known live order_id is
    /// present at the key (the previous `replace_cancel` counter was
    /// never incremented and has been removed).
    pub modify: AtomicU64,
    /// Hot-path / staleness commands_ring write_frame returned false
    /// (ring full → frame silently dropped before this counter shipped).
    /// Each increment is one outbound place/modify/cancel frame that
    /// never reached the broker. JSONL `decisions` log and our_orders
    /// state are still populated unconditionally (Step 2), so the gap
    /// between this counter and broker-side wire records is the
    /// invisible-drop magnitude we're trying to surface.
    pub place_dropped: AtomicU64,
    /// Tier A Exp 2 (2026-05-06): outbound burst-cap gate fired —
    /// ≥2 sends within the last 50ms, this side skipped to avoid
    /// piling onto the IBKR OMS queue. Caller will re-evaluate on
    /// the next tick (typically <20ms later).
    pub skip_burst_cap: AtomicU64,
}

impl Default for DecisionCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl DecisionCounters {
    pub fn new() -> Self {
        Self {
            place: AtomicU64::new(0),
            skip_no_vol_surface: AtomicU64::new(0),
            skip_vol_surface_stale: AtomicU64::new(0),
            skip_uncalibrated_strike: AtomicU64::new(0),
            skip_unack_inflight: AtomicU64::new(0),
            dropped_parse_errors: AtomicU64::new(0),
            skip_off_atm: AtomicU64::new(0),
            skip_itm: AtomicU64::new(0),
            skip_thin_book: AtomicU64::new(0),
            skip_one_sided_or_dark: AtomicU64::new(0),
            skip_wide_spread: AtomicU64::new(0),
            skip_forward_drift: AtomicU64::new(0),
            skip_in_band: AtomicU64::new(0),
            skip_cooldown: AtomicU64::new(0),
            skip_target_nonpositive: AtomicU64::new(0),
            skip_would_cross_ask: AtomicU64::new(0),
            skip_would_cross_bid: AtomicU64::new(0),
            skip_other: AtomicU64::new(0),
            risk_block: AtomicU64::new(0),
            risk_block_buy: AtomicU64::new(0),
            risk_block_sell: AtomicU64::new(0),
            staleness_cancel: AtomicU64::new(0),
            staleness_cancel_dark: AtomicU64::new(0),
            staleness_modify: AtomicU64::new(0),
            modify: AtomicU64::new(0),
            place_dropped: AtomicU64::new(0),
            skip_burst_cap: AtomicU64::new(0),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        // Only emit non-zero counters to keep telemetry payload small,
        // matching Python's Counter() emit style.
        let pairs: &[(&str, &AtomicU64)] = &[
            ("place", &self.place),
            ("skip_no_vol_surface", &self.skip_no_vol_surface),
            // Audit T4-3: previously omitted from telemetry.
            ("skip_vol_surface_stale", &self.skip_vol_surface_stale),
            ("skip_uncalibrated_strike", &self.skip_uncalibrated_strike),
            ("skip_unack_inflight", &self.skip_unack_inflight),
            ("dropped_parse_errors", &self.dropped_parse_errors),
            ("skip_off_atm", &self.skip_off_atm),
            ("skip_itm", &self.skip_itm),
            ("skip_thin_book", &self.skip_thin_book),
            ("skip_one_sided_or_dark", &self.skip_one_sided_or_dark),
            ("skip_wide_spread", &self.skip_wide_spread),
            ("skip_forward_drift", &self.skip_forward_drift),
            ("skip_in_band", &self.skip_in_band),
            ("skip_cooldown", &self.skip_cooldown),
            ("skip_target_nonpositive", &self.skip_target_nonpositive),
            ("skip_would_cross_ask", &self.skip_would_cross_ask),
            ("skip_would_cross_bid", &self.skip_would_cross_bid),
            ("skip_other", &self.skip_other),
            ("risk_block", &self.risk_block),
            ("risk_block_buy", &self.risk_block_buy),
            ("risk_block_sell", &self.risk_block_sell),
            ("staleness_cancel", &self.staleness_cancel),
            ("staleness_cancel_dark", &self.staleness_cancel_dark),
            ("staleness_modify", &self.staleness_modify),
            ("modify", &self.modify),
            ("place_dropped", &self.place_dropped),
            ("skip_burst_cap", &self.skip_burst_cap),
        ];
        for (k, v) in pairs {
            let n = v.load(Ordering::Relaxed);
            if n > 0 {
                m.insert((*k).to_string(), serde_json::json!(n));
            }
        }
        serde_json::Value::Object(m)
    }
}
