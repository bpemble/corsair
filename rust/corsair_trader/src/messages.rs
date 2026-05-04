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

/// Lightweight first-pass header. Hot path parses ONLY this, then
/// re-parses the body bytes directly into the typed message struct
/// for the matched variant. Avoids the `serde_json::Value`-tree round
/// trip the previous `GenericMsg` carried via `#[serde(flatten)] extra`.
///
/// rmp_serde silently ignores unknown msgpack-map keys when a
/// `#[derive(Deserialize)]` struct doesn't declare `deny_unknown_fields`,
/// so this header skips every other field cheaply (no Value allocations).
#[derive(Debug, Deserialize)]
pub struct MsgHeader {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub ts_ns: Option<u64>,
}

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
    #[serde(default)]
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
}

#[derive(Debug, Deserialize)]
pub struct HelloMsg {
    #[serde(default)]
    pub config: Option<HelloConfig>,
}

#[derive(Debug, Clone, Deserialize)]
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

/// Underlying-tick wire message. `ts_ns` is parsed but not read by
/// hot-path code (trader uses its own monotonic clock for timing);
/// kept on the struct so deserialization stays canonical.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct UnderlyingTickMsg {
    pub price: f64,
    #[serde(default)]
    pub ts_ns: Option<u64>,
}

/// Risk state from broker. Hot path reads only the gating-relevant
/// subset (effective_delta, margin_pct); the others are parsed for
/// telemetry / future use.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct RiskStateMsg {
    pub margin_usd: f64,
    pub margin_pct: f64,
    pub options_delta: f64,
    pub hedge_delta: i64,
    pub effective_delta: f64,
    pub theta: f64,
    pub vega: f64,
    pub gamma: f64,
    pub total_contracts: i64,
    pub n_positions: i64,
}

/// Order status update from broker. Trader uses this to clear
/// `OurOrder` entries on terminal states (Filled / Cancelled /
/// Rejected) so subsequent quote updates don't try to amend a dead
/// order. Broker emits `"order_status"` events with snake_case
/// `order_id`; the alias accepts the legacy `orderId` form too in
/// case the field name diverges between adapters.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct OrderAckMsg {
    #[serde(alias = "orderId", default)]
    pub order_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(alias = "lmtPrice", default)]
    pub lmt_price: Option<f64>,
    #[serde(alias = "orderRef", default)]
    pub order_ref: Option<String>,
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
#[derive(Debug, Clone, Serialize)]
pub struct PlaceOrder {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub ts_ns: u64,
    pub strike: f64,
    pub expiry: String,
    pub right: String,
    pub side: String,
    pub qty: i32,
    pub price: f64,
    #[serde(rename = "orderRef")]
    pub order_ref: String,
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
    pub ipc_p50_us: Option<u64>,
    pub ipc_p99_us: Option<u64>,
    pub ipc_n: usize,
    pub ttt_p50_us: Option<u64>,
    pub ttt_p99_us: Option<u64>,
    pub ttt_n: usize,
    pub n_options: usize,
    pub n_active_orders: usize,
    pub n_vol_expiries: usize,
    pub killed: Vec<String>,
    pub weekend_paused: bool,
}
