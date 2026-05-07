//! Message types matching Python's broker_ipc / protocol.py wire
//! format. All fields are `serde_json::Value` or typed where useful;
//! we use msgpack-named-fields with serde, so field names must match
//! Python dict keys exactly.
//!
//! Inbound (broker → trader): tick, underlying_tick, vol_surface,
//! risk_state, order_ack, place_ack, fill, kill, resume, hello,
//! weekend_pause, snapshot.
//!
//! Outbound (trader → broker): welcome, place_order, cancel_order,
//! telemetry.

use serde::{Deserialize, Serialize};

// MsgHeader was retired in Bundle 2C (2026-05-06): replaced by
// `msgpack_decode::decode_header`, a hand-rolled walker that returns
// borrowed bytes for the type field — no `String` allocation per
// inbound event.

/// Typed kill / resume — replaces ad-hoc `extra.get("source")` lookups.
#[derive(Debug, Deserialize)]
pub struct KillMsg {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WeekendPauseMsg {
    // No `#[serde(default)]` — a missing `paused` field must surface
    // as a parse error so the caller drops the message rather than
    // silently un-pausing (default bool = false). A weekend_pause
    // event with an absent `paused` field is structurally malformed;
    // safer to drop + count than guess.
    pub paused: bool,
}

/// Hello config (broker → trader on connect). Subset of fields the
/// trader actually reads; broker may emit more (silently ignored).
#[derive(Debug, Deserialize)]
pub struct HelloConfig {
    #[serde(default)]
    pub min_edge_ticks: Option<i64>,
    #[serde(default)]
    pub tick_size: Option<f64>,
    #[serde(default)]
    pub delta_ceiling: Option<f64>,
    #[serde(default)]
    pub delta_kill: Option<f64>,
    #[serde(default)]
    pub margin_ceiling_pct: Option<f64>,
    /// Quote lifetime in seconds (broker GTD). Trader uses
    /// `gtd_lifetime_s - gtd_refresh_lead_s` as the refresh threshold.
    #[serde(default)]
    pub gtd_lifetime_s: Option<f64>,
    /// Lead seconds before GTD expiry to refresh.
    #[serde(default)]
    pub gtd_refresh_lead_s: Option<f64>,
    /// Dead-band: skip if |new_price - rest_price| < N × tick_size.
    #[serde(default)]
    pub dead_band_ticks: Option<i64>,
    /// Spec §3.4 wide-market skip: skip if half_spread > N × min_edge.
    /// 0 disables.
    #[serde(default)]
    pub skip_if_spread_over_edge_mul: Option<f64>,
    /// Theta-breach threshold (negative; 0 disables). Trader self-gates
    /// at this value as defense-in-depth alongside the kill IPC event
    /// from the broker. 2026-05-05 incident: kill IPC was missing AND
    /// trader had no theta gate — adverse fills compounded for 6h.
    #[serde(default)]
    pub theta_kill: Option<f64>,
    /// Vega-breach threshold (positive; 0 disables; currently 0 per
    /// CLAUDE.md §13 Alabaster characterization).
    #[serde(default)]
    pub vega_kill: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct HelloMsg {
    #[serde(default)]
    pub config: Option<HelloConfig>,
}

// Serialize derive added 2026-05-05 so the msgpack_decode round-trip
// tests can encode TickMsg via rmp_serde::to_vec_named — the broker
// is the only producer of these on the wire, so the impl is unused
// in production but harmless.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TickMsg {
    pub strike: f64,
    pub expiry: String,
    pub right: String,
    #[serde(default)]
    pub bid: Option<f64>,
    #[serde(default)]
    pub ask: Option<f64>,
    #[serde(default)]
    pub bid_size: Option<i32>,
    #[serde(default)]
    pub ask_size: Option<i32>,
    #[serde(default)]
    pub ts_ns: Option<u64>,
    /// v2 wire-timing: broker's local clock when it published this tick
    /// frame. Trader caches the latest per-key value and echoes it back
    /// on PlaceOrder.triggering_tick_broker_recv_ns. Optional for
    /// forward-compat with pre-v2 broker builds.
    #[serde(default)]
    pub broker_recv_ns: Option<u64>,
    /// L2 depth: top-2 bid prices (level 0 = best, level 1 = next).
    /// Set when broker has an active reqMktDepth subscription for
    /// this leg (rotator covers ATM-nearest 5 strikes). None
    /// otherwise — trader falls back to depth-1 self-fill approx.
    #[serde(default)]
    pub depth_bid_0: Option<f64>,
    #[serde(default)]
    pub depth_bid_1: Option<f64>,
    #[serde(default)]
    pub depth_ask_0: Option<f64>,
    #[serde(default)]
    pub depth_ask_1: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VolSurfaceMsg {
    pub expiry: String,
    pub side: String,
    pub forward: f64,
    pub params: VolParams,
    /// Broker fit timestamp (CLOCK_REALTIME ns). Used by the
    /// trader's HI-002 staleness gate — if no refresh in 120s,
    /// skip quoting (SVI extrapolation isn't safe with stale anchor).
    #[serde(default)]
    pub ts_ns: Option<u64>,
    /// Calibrated-strike range used by the SABR fit. Trader's
    /// is_strike_calibrated gate refuses to quote strikes outside
    /// [calibrated_min_k, calibrated_max_k] — SABR extrapolation
    /// past the fit's strike envelope is unreliable. Optional for
    /// back-compat with older broker builds; when None the gate
    /// is skipped.
    #[serde(default)]
    pub calibrated_min_k: Option<f64>,
    #[serde(default)]
    pub calibrated_max_k: Option<f64>,
    /// Spot price (front-month underlying) observed by the broker AT
    /// FIT TIME. Anchor for the trader's Taylor reprice — Taylor uses
    /// (current_spot − spot_at_fit), NOT (current_spot − forward), so
    /// the static carry between front-month and the option's
    /// underlying month doesn't get treated as drift. Optional for
    /// back-compat with older broker builds; when None the trader
    /// falls back to its own current spot at message-arrival time
    /// (close enough — the gap is a few ms of IPC latency).
    #[serde(default)]
    pub spot_at_fit: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VolParams {
    pub model: String,
    // SVI fields
    #[serde(default)]
    pub a: Option<f64>,
    #[serde(default)]
    pub b: Option<f64>,
    #[serde(default)]
    pub rho: Option<f64>,
    #[serde(default)]
    pub m: Option<f64>,
    #[serde(default)]
    pub sigma: Option<f64>,
    // SABR fields
    #[serde(default)]
    pub alpha: Option<f64>,
    #[serde(default)]
    pub beta: Option<f64>,
    #[serde(default)]
    pub nu: Option<f64>,
}

/// Underlying-tick wire message. Only `price` is consumed; the wire
/// format also carries `ts_ns` but the trader uses its own monotonic
/// clock for timing so we don't bother decoding it (rmp_serde silently
/// skips unknown fields).
#[derive(Debug, Clone, Deserialize)]
pub struct UnderlyingTickMsg {
    pub price: f64,
}

/// Risk state from broker. Hot path reads only the gating-relevant
/// subset (effective_delta, margin_pct, theta, vega) plus hedge_delta
/// for telemetry. Other broker-emitted fields (margin_usd,
/// options_delta, gamma, total_contracts, n_positions) are present on
/// the wire but ignored — rmp_serde silently skips unknown fields, so
/// the struct only declares what we read.
#[derive(Debug, Clone, Deserialize)]
pub struct RiskStateMsg {
    pub margin_pct: f64,
    pub hedge_delta: i64,
    pub effective_delta: f64,
    pub theta: f64,
    pub vega: f64,
}

/// Order status update from broker. Trader uses this to clear
/// `OurOrder` entries on terminal states (Filled / Cancelled /
/// Rejected) so subsequent quote updates don't try to amend a dead
/// order. Broker emits `"order_status"` events with snake_case
/// `order_id`; the alias accepts the legacy `orderId` form too in
/// case the field name diverges between adapters.
///
/// Other broker-emitted fields (side, lmtPrice, orderRef) are not
/// consumed by the trader and are silently skipped by rmp_serde.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderAckMsg {
    #[serde(alias = "orderId", default)]
    pub order_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlaceAckMsg {
    #[serde(rename = "orderId")]
    pub order_id: i64,
    pub strike: f64,
    pub expiry: String,
    pub right: String,
    pub side: String,
    pub price: f64,
}

/// Outbound: place_order command sent to broker.
///
/// Hot-path build: `expiry` and `right` are borrowed from the trader's
/// current `TickMsg`/`expiry_arc` for the lifetime of the encode call,
/// avoiding two `String` allocations per Place. `side` is `&'static str`
/// (from `Side::as_str`). `order_ref` stays a `&'static str` literal at
/// construction since the trader uses one fixed value.
#[derive(Debug, Clone, Serialize)]
pub struct PlaceOrder<'a> {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub ts_ns: u64,
    pub strike: f64,
    pub expiry: &'a str,
    pub right: &'a str,
    pub side: &'static str,
    pub qty: i32,
    pub price: f64,
    #[serde(rename = "orderRef")]
    pub order_ref: &'static str,
    /// v2 wire-timing: TickMsg.broker_recv_ns of the tick that drove
    /// this decision (latest tick the trader had cached for this key
    /// at decide_quote time). Skipped on the wire when None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triggering_tick_broker_recv_ns: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CancelOrder {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub ts_ns: u64,
    #[serde(rename = "orderId")]
    pub order_id: i64,
}

/// Outbound: modify_order command. Replaces cancel-before-replace at
/// keys with a known live order_id — single round trip instead of two.
/// Broker (`ipc.rs::handle_modify`) sends a placeOrder with same id +
/// new price (IBKR's amend protocol). Wire-timing path mirrors
/// place_order so modify_rtt_us shows up in the latency block.
#[derive(Debug, Clone, Serialize)]
pub struct ModifyOrder {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub ts_ns: u64,
    pub order_id: i64,
    pub price: f64,
    /// GTD refresh in seconds. The broker converts to absolute UTC.
    /// Send on every modify so the order doesn't expire mid-update.
    pub gtd_seconds: u32,
    /// v2 wire-timing: the trigger tick that drove this amend, used
    /// by the broker's wire_timing JSONL emitter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triggering_tick_broker_recv_ns: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Welcome {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub ts_ns: u64,
    pub trader_version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Telemetry {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub ts_ns: u64,
    pub events: serde_json::Value,
    pub decisions: serde_json::Value,
    pub ipc_p50_ns: Option<u64>,
    pub ipc_p99_ns: Option<u64>,
    pub ipc_n: usize,
    pub ttt_p50_ns: Option<u64>,
    pub ttt_p99_ns: Option<u64>,
    pub ttt_n: usize,
    pub n_options: usize,
    pub n_active_orders: usize,
    pub n_vol_expiries: usize,
    pub killed: Vec<String>,
    pub weekend_paused: bool,
    /// Latest hedge delta from the broker's 1Hz risk_state publish.
    /// LOW-004: surfaced on telemetry alongside the IPC/TTT histograms
    /// for operational observability (cascade post-mortems use this
    /// to reconstruct hedge state at the 10s tick that bracketed an
    /// incident). None until the first risk_state event arrives.
    pub risk_hedge_delta: Option<i64>,
    /// Cumulative `frames_dropped` counter on the trader's commands
    /// SHM ring (writer side). Each unit is one outbound frame
    /// (place_order / modify_order / cancel_order / telemetry) that
    /// `write_frame` rejected because the ring was full. Read directly
    /// from `commands_ring.frames_dropped` at telemetry build time —
    /// monotonic across the trader process lifetime.
    pub commands_frames_dropped: u64,
}
