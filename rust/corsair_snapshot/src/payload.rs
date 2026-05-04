//! Snapshot JSON payload — what the dashboard reads.

use chrono::NaiveDate;
use corsair_broker_api::Right;
use serde::{Deserialize, Serialize};

/// Top-level snapshot. Stable wire format; CHANGES to this struct
/// must coordinate with the Streamlit dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub timestamp_ns: u64,
    pub portfolio: PortfolioSnapshot,
    pub kill: KillSnapshot,
    pub hedges: Vec<HedgeSnapshot>,
    /// Account-level values from broker (NLV, margin, BP, etc.).
    pub account: AccountSnapshot,
    /// Underlying prices per product.
    pub underlying: std::collections::HashMap<String, f64>,

    /// Per-expiry option chain data, keyed by YYYYMMDD string.
    /// Dashboard reads this to render the chain table.
    #[serde(default)]
    pub chains: std::collections::HashMap<String, ChainExpirySnapshot>,
    /// ATM strike used for current quoting (front-expiry-relative).
    /// Dashboard highlights this row.
    #[serde(default)]
    pub atm_strike: f64,
    /// All expiries currently subscribed/quoted, sorted ascending.
    /// Dashboard's expiry dropdown.
    #[serde(default)]
    pub expiries: Vec<String>,
    /// Default expiry shown when dashboard first loads.
    #[serde(default)]
    pub front_month_expiry: Option<String>,
    /// Latency stats: rolling p50/p99 for TTT (tick→place_order ack)
    /// and place_rtt (broker→IBKR→broker). Surfaced to dashboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<LatencySnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LatencySnapshot {
    /// Tick→trader_decide→place_order_ack (microseconds).
    /// Captures the entire client-side hot path.
    pub ttt_us: LatencyStats,
    /// Broker→IBKR→broker round-trip on place_order (microseconds).
    pub place_rtt_us: LatencyStats,
    /// Broker→IBKR→broker round-trip on modify (microseconds). Now
    /// populated post-amend conversion: trader emits modify_order
    /// instead of cancel+place at any key with a known live order_id,
    /// so this is the relevant steady-state quote-update RTT.
    pub amend_us: LatencyStats,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LatencyStats {
    pub n: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p99: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainExpirySnapshot {
    pub strikes: std::collections::HashMap<String, StrikeBlockSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrikeBlockSnapshot {
    pub call: Option<SideBlockSnapshot>,
    pub put: Option<SideBlockSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideBlockSnapshot {
    /// Market bid (IBKR best bid).
    pub market_bid: f64,
    /// Market ask (IBKR best ask).
    pub market_ask: f64,
    /// Bid size at best bid.
    pub bid_size: u64,
    /// Ask size at best ask.
    pub ask_size: u64,
    /// Last trade price; 0 when no trades.
    pub last: f64,
    /// Position quantity (signed; long > 0, short < 0). 0 when none.
    pub pos: i32,
    /// Our resting bid price (Some(p) if we have a working BUY order
    /// for this leg). Read by the dashboard chain table to highlight
    /// rows where we're quoting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub our_bid: Option<f64>,
    /// Our resting ask price (Some(p) if we have a working SELL order
    /// for this leg).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub our_ask: Option<f64>,
    /// True when our_bid corresponds to an order that IBKR reports as
    /// Submitted/PreSubmitted (live at the exchange). False when the
    /// order is in flight (PendingSubmit) or status is unknown.
    /// Dashboard uses this to color the cell green (live+leading)
    /// vs amber (pending).
    #[serde(default)]
    pub bid_live: bool,
    /// True when our_ask is live at IBKR.
    #[serde(default)]
    pub ask_live: bool,
    /// L1 raw bid (IBKR best bid). Same as market_bid in our pipeline
    /// (we don't run a self-fill filter); kept as a separate field
    /// because the dashboard's at-BBO comparison reads `raw_bid` to
    /// decide "quoting" (green) vs "behind" (red).
    #[serde(default)]
    pub raw_bid: f64,
    /// L1 raw ask (IBKR best ask).
    #[serde(default)]
    pub raw_ask: f64,
    /// SVI/SABR theo price for this leg, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theo: Option<f64>,
    /// Option open interest. Pushed by IBKR generic tick "101".
    #[serde(default)]
    pub open_interest: u64,
    /// Option session volume. Pushed by IBKR generic tick "100".
    #[serde(default)]
    pub volume: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    pub net_delta: f64,
    pub net_theta: f64,
    pub net_vega: f64,
    pub net_gamma: f64,
    /// Options-only delta (excludes hedge). MED-002.
    #[serde(default)]
    pub options_delta: f64,
    /// Hedge contracts (signed). MED-002.
    #[serde(default)]
    pub hedge_delta: i64,
    /// options_delta + hedge_delta — what gates fire against per
    /// CLAUDE.md §14 (effective-delta gating). MED-002.
    #[serde(default)]
    pub effective_delta: f64,
    pub long_count: u32,
    pub short_count: u32,
    pub gross_positions: u32,
    pub fills_today: u32,
    pub realized_pnl_persisted: f64,
    pub mtm_pnl: f64,
    pub daily_pnl: f64,
    pub spread_capture_today: f64,
    pub session_open_nlv: Option<f64>,
    pub positions: Vec<PositionSnapshot>,
    /// Per-product breakdown.
    pub per_product: std::collections::HashMap<String, PortfolioPerProduct>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioPerProduct {
    pub net_delta: f64,
    pub net_theta: f64,
    pub net_vega: f64,
    pub long_count: u32,
    pub short_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionSnapshot {
    pub product: String,
    pub strike: f64,
    pub expiry: NaiveDate,
    pub right: Right,
    pub quantity: i32,
    pub avg_fill_price: f64,
    pub current_price: f64,
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
    pub multiplier: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSnapshot {
    pub killed: bool,
    pub source: Option<String>,
    pub kill_type: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HedgeSnapshot {
    pub product: String,
    pub hedge_qty: i32,
    pub avg_entry_f: f64,
    pub realized_pnl_usd: f64,
    pub mtm_usd: f64,
    pub mode: String,
    /// Resolved hedge futures contract expiry as YYYYMMDD (e.g.
    /// "20260626" for HGM6). Empty if not resolved yet.
    #[serde(default)]
    pub expiry: String,
    /// Current forward price (underlying mark) used for MTM. 0 when
    /// no underlying tick has arrived. Dashboard reads as "Mark".
    #[serde(default)]
    pub forward: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub net_liquidation: f64,
    pub maintenance_margin: f64,
    pub initial_margin: f64,
    pub buying_power: f64,
    pub realized_pnl_today: f64,
    /// IBKR scale factor (synthetic SPAN ÷ IBKR MaintMargin).
    /// 1.0 means no scaling applied yet.
    pub ibkr_scale: f64,
}

pub const SCHEMA_VERSION: u32 = 1;
