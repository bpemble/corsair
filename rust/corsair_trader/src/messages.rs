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

/// All inbound events deserialize via this catch-all then dispatch
/// on the `type` field. Rmp-serde handles map-style msgpack messages.
#[derive(Debug, Deserialize)]
pub struct GenericMsg {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub ts_ns: Option<u64>,
    // All other fields are kept as a Value tree for flexible access.
    #[serde(flatten)]
    pub extra: serde_json::Value,
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct VolSurfaceMsg {
    pub expiry: String,
    pub side: String,
    pub forward: f64,
    pub params: VolParams,
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

/// place_ack message from broker. The trader uses order_id to map
/// place commands back to keys (see state.rs::OurOrder); the other
/// fields are parsed for completeness/log fidelity.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct OrderAckMsg {
    #[serde(rename = "orderId", default)]
    pub order_id: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(rename = "lmtPrice", default)]
    pub lmt_price: Option<f64>,
    #[serde(rename = "orderRef", default)]
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
}

#[derive(Debug, Clone, Serialize)]
pub struct CancelOrder {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub ts_ns: u64,
    #[serde(rename = "orderId")]
    pub order_id: i64,
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
