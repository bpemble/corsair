//! Tick types — market data subscriptions and updates.
//!
//! The Broker delivers ticks for ALL active subscriptions on a single
//! broadcast channel. Consumers filter by `instrument_id`. For high-
//! volume subscriptions (full chain, many products) consider whether
//! the runtime should split this; for our ~60 strikes × 2 sides + 1
//! underlying the single channel is fine.

use serde::{Deserialize, Serialize};

use crate::contract::InstrumentId;

/// What the tick is reporting. Mirrors IBKR's tickType taxonomy
/// reduced to what we actually consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TickKind {
    /// Best bid price.
    Bid,
    /// Best ask price.
    Ask,
    /// Last trade price.
    Last,
    /// Best bid size (number of contracts at the bid).
    BidSize,
    /// Best ask size.
    AskSize,
    /// Cumulative session volume.
    Volume,
}

/// A single market data update. Adapters MUST emit at least Bid+Ask+
/// BidSize+AskSize for every subscribed instrument; Last+Volume are
/// best-effort.
#[derive(Debug, Clone)]
pub struct Tick {
    pub instrument_id: InstrumentId,
    pub kind: TickKind,
    /// Set for price ticks (Bid/Ask/Last). None for size ticks.
    pub price: Option<f64>,
    /// Set for size ticks (BidSize/AskSize/Volume). None for price ticks.
    pub size: Option<u64>,
    pub timestamp_ns: u64,
}

/// Request for a market data subscription. Adapters maintain
/// subscription state internally; the [`TickStreamHandle`] returned
/// is opaque to callers and used to unsubscribe.
#[derive(Debug, Clone)]
pub struct TickSubscription {
    pub instrument_id: InstrumentId,
    /// Whether to receive tick-by-tick (true) vs snapshot-style (false).
    /// IBKR's distinction; iLink/MDP3 always streams tick-by-tick.
    /// Adapters should treat this as a hint, not a hard requirement.
    pub tick_by_tick: bool,
    /// Optional subscription tag for the consumer's own tracking.
    /// Round-tripped on the handle but not interpreted by the adapter.
    pub consumer_tag: Option<String>,
    /// IBKR-specific: full contract descriptor for the reqMktData
    /// request. Required for futures/options where IBKR rejects
    /// con_id-only subscriptions with `Error validating request: Please
    /// enter exchange`. Stocks/SMART-routed instruments can leave None.
    /// Adapters that key purely on instrument_id (mock, FIX) ignore.
    pub contract: Option<crate::Contract>,
}

/// Opaque handle returned by `subscribe_ticks`. Consumers pass this
/// back to `unsubscribe_ticks` when they're done. Internally an
/// adapter-assigned u64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TickStreamHandle(pub u64);
