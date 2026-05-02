//! Decoded value types — what we surface to the runtime after
//! parsing inbound IBKR messages.
//!
//! These are intentionally less-structured than
//! `corsair_broker_api`'s types: they preserve every field IBKR
//! sends so callers can extract what they need. The
//! `corsair_broker_ibkr_native` adapter layer translates these to
//! `corsair_broker_api` value types.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContractDecoded {
    pub con_id: i64,
    pub symbol: String,
    pub sec_type: String,
    pub last_trade_date: String,
    pub strike: f64,
    pub right: String,
    pub multiplier: String,
    pub exchange: String,
    pub primary_exchange: String,
    pub currency: String,
    pub local_symbol: String,
    pub trading_class: String,
}

#[derive(Debug, Clone)]
pub struct PositionMsg {
    pub account: String,
    pub contract: ContractDecoded,
    pub position: f64,
    pub avg_cost: f64,
}

#[derive(Debug, Clone)]
pub struct AccountValueMsg {
    pub key: String,
    pub value: String,
    pub currency: String,
    pub account: String,
}

#[derive(Debug, Clone)]
pub struct OpenOrderMsg {
    pub order_id: i32,
    pub contract: ContractDecoded,
    pub action: String,
    pub total_quantity: f64,
    pub order_type: String,
    pub lmt_price: f64,
    pub tif: String,
    pub account: String,
    pub order_ref: String,
    pub status: String,
    pub filled: f64,
    pub remaining: f64,
}

#[derive(Debug, Clone)]
pub struct OrderStatusMsg {
    pub order_id: i32,
    pub status: String,
    pub filled: f64,
    pub remaining: f64,
    pub avg_fill_price: f64,
    pub perm_id: i32,
    pub parent_id: i32,
    pub last_fill_price: f64,
    pub client_id: i32,
    pub why_held: String,
    pub mkt_cap_price: f64,
}

#[derive(Debug, Clone)]
pub struct ExecutionMsg {
    pub req_id: i32,
    pub order_id: i32,
    pub contract: ContractDecoded,
    pub exec_id: String,
    pub time: String,
    pub account: String,
    pub exchange: String,
    pub side: String,
    pub shares: f64,
    pub price: f64,
    pub perm_id: i32,
    pub client_id: i32,
    pub liquidation: i32,
    pub cum_qty: f64,
    pub avg_price: f64,
    pub order_ref: String,
}

#[derive(Debug, Clone)]
pub struct CommissionReportMsg {
    pub exec_id: String,
    pub commission: f64,
    pub currency: String,
    pub realized_pnl: f64,
}

#[derive(Debug, Clone)]
pub struct ContractDetailsMsg {
    pub req_id: i32,
    pub contract: ContractDecoded,
}

#[derive(Debug, Clone)]
pub struct TickPriceMsg {
    pub req_id: i32,
    /// 1=bid, 2=ask, 4=last, 6=high, 7=low, 9=close, 14=open
    pub tick_type: i32,
    pub price: f64,
    /// Attribute mask: bit 0 canAutoExecute, bit 1 pastLimit, bit 2 preOpen.
    pub attribs: i32,
}

#[derive(Debug, Clone)]
pub struct TickSizeMsg {
    pub req_id: i32,
    /// 0=bidSize, 3=askSize, 5=lastSize, 8=volume
    pub tick_type: i32,
    pub size: f64,
}

#[derive(Debug, Clone)]
pub struct TickOptionComputationMsg {
    pub req_id: i32,
    pub tick_type: i32,
    pub tick_attrib: i32,
    pub implied_vol: f64,
    pub delta: f64,
    pub opt_price: f64,
    pub pv_dividend: f64,
    pub gamma: f64,
    pub vega: f64,
    pub theta: f64,
    pub und_price: f64,
}

#[derive(Debug, Clone)]
pub struct ErrorMsg {
    pub req_id: i32,
    pub error_code: i32,
    pub error_string: String,
    pub advanced_order_reject_json: String,
}

/// Discriminated union of every parsed message. The native client's
/// dispatch task forwards these onto an mpsc channel that the
/// runtime drains.
#[derive(Debug, Clone)]
pub enum InboundMsg {
    Position(PositionMsg),
    PositionEnd,
    AccountValue(AccountValueMsg),
    AccountUpdateTime(String),
    AccountDownloadEnd(String), // account
    OpenOrder(OpenOrderMsg),
    OpenOrderEnd,
    OrderStatus(OrderStatusMsg),
    Execution(ExecutionMsg),
    ExecutionEnd(i32), // req_id
    CommissionReport(CommissionReportMsg),
    ContractDetails(ContractDetailsMsg),
    ContractDetailsEnd(i32), // req_id
    TickPrice(TickPriceMsg),
    TickSize(TickSizeMsg),
    TickOptionComputation(TickOptionComputationMsg),
    Error(ErrorMsg),
    NextValidId(i32),
    ManagedAccounts(Vec<String>),
    /// Anything we don't have a typed parser for yet — keeps wire
    /// surface complete during incremental Phase 6.5 work.
    Unparsed { type_id: i32, fields: Vec<String> },
}
