//! Order types — placement requests, status updates, lifecycle.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::contract::Contract;

/// Broker-assigned order identifier. Stable for the lifetime of the
/// order; not reused after the order goes terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OrderId(pub u64);

impl std::fmt::Display for OrderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ord#{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_char(self) -> char {
        match self {
            Side::Buy => 'B',
            Side::Sell => 'S',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum OrderType {
    /// Limit at `price`.
    Limit,
    /// Market — `price` ignored.
    Market,
    /// Stop limit (rarely used by us; included for completeness).
    StopLimit { stop_price: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce {
    /// Good-till-cancelled. Persists across sessions.
    Gtc,
    /// Day order. Cancelled at session close.
    Day,
    /// Good-till-date. Use the gtd_until_utc field on the request.
    Gtd,
    /// Immediate-or-cancel. Cancels remainder if not immediately filled.
    Ioc,
    /// Fill-or-kill. Cancels entire order if not immediately filled in full.
    Fok,
}

/// Request to place a new order.
#[derive(Debug, Clone)]
pub struct PlaceOrderReq {
    pub contract: Contract,
    pub side: Side,
    pub qty: u32,
    pub order_type: OrderType,
    /// Required for [`OrderType::Limit`] and [`OrderType::StopLimit`];
    /// ignored for [`OrderType::Market`].
    pub price: Option<f64>,
    pub tif: TimeInForce,
    /// Required when `tif == Gtd`; ignored otherwise.
    pub gtd_until_utc: Option<DateTime<Utc>>,
    /// Caller-side tracking key. Adapter passes this through to the
    /// gateway as a reference field (IBKR `orderRef`, FIX tag 11
    /// `ClOrdID`). Maps back to caller intent on fills/status.
    pub client_order_ref: String,
    /// FA / sub-account selector. Required by IBKR for FA accounts
    /// (see CLAUDE.md §1); adapters can default to None for
    /// single-account setups.
    pub account: Option<String>,
}

/// Modify an existing order. price + qty are the modifiable fields;
/// other attributes (TIF, side, contract) cannot be changed via modify.
#[derive(Debug, Clone)]
pub struct ModifyOrderReq {
    pub price: Option<f64>,
    pub qty: Option<u32>,
    /// Refresh GTD expiry. Required when [`TimeInForce::Gtd`] is used
    /// and the original is approaching expiry.
    pub gtd_until_utc: Option<DateTime<Utc>>,
}

/// Lifecycle states. Reflects IBKR's order status set and FIX's
/// tag 39 (OrdStatus) one-to-one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrderStatus {
    /// Sent to gateway, not yet acknowledged at the exchange.
    PendingSubmit,
    /// Live at the exchange.
    Submitted,
    /// Fully filled.
    Filled,
    /// Cancelled (operator, GTD expiry, IOC unfilled remainder, etc.).
    Cancelled,
    /// Rejected by the exchange / risk system / gateway.
    Rejected,
    /// Cancel sent, awaiting confirmation.
    PendingCancel,
    /// API-level state — we don't expect to see this much; included
    /// for protocol completeness.
    Inactive,
}

impl OrderStatus {
    /// Terminal states — no further updates expected for this order.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected
        )
    }
}

/// Push event delivered on the order-status stream.
#[derive(Debug, Clone)]
pub struct OrderStatusUpdate {
    pub order_id: OrderId,
    pub status: OrderStatus,
    pub filled_qty: u32,
    pub remaining_qty: u32,
    /// Volume-weighted average fill price across all fills so far.
    /// 0.0 when no fills have occurred.
    pub avg_fill_price: f64,
    /// Last fill price (if any).
    pub last_fill_price: Option<f64>,
    pub timestamp_ns: u64,
    /// Reject reason from the exchange / gateway. Populated on
    /// [`OrderStatus::Rejected`]; None otherwise.
    pub reject_reason: Option<String>,
}

/// Snapshot of a live order returned by `Broker::open_orders`.
#[derive(Debug, Clone)]
pub struct OpenOrder {
    pub order_id: OrderId,
    pub contract: Contract,
    pub side: Side,
    pub qty: u32,
    pub order_type: OrderType,
    pub price: Option<f64>,
    pub tif: TimeInForce,
    pub gtd_until_utc: Option<DateTime<Utc>>,
    pub client_order_ref: String,
    pub status: OrderStatus,
    pub filled_qty: u32,
    pub remaining_qty: u32,
}
