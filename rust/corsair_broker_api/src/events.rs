//! Event types streamed by the broker (fills, connection state).

use serde::{Deserialize, Serialize};

use crate::contract::InstrumentId;
use crate::orders::{OrderId, Side};

/// Trade execution. Emitted on the fills stream when a venue confirms
/// a fill (or partial fill) on one of our orders.
///
/// Maps to: ib_insync `Fill` (execution + commissionReport).
#[derive(Debug, Clone)]
pub struct Fill {
    /// Globally unique execution id from the venue. Adapters dedupe
    /// on this; the same exec_id never produces two Fill events.
    pub exec_id: String,
    pub order_id: OrderId,
    pub instrument_id: InstrumentId,
    pub side: Side,
    pub qty: u32,
    pub price: f64,
    /// Exchange-clock timestamp if the venue provides it; otherwise
    /// adapter-clock at receive time.
    pub timestamp_ns: u64,
    /// Commission for this fill in account currency. None if the
    /// commission report hasn't arrived yet (IBKR delivers it in a
    /// separate event ~ms later); fill_handler can wait or proceed
    /// with adapter's best estimate.
    pub commission: Option<f64>,
}

/// L2 market depth update. Emitted on the depth stream when IBKR
/// pushes a book operation. Consumers (e.g. broker daemon's depth
/// book aggregator) maintain per-leg L2 state from these.
///
/// Each update describes ONE level on ONE side: e.g. "delete level 2
/// on bid side". Adapters do not maintain the book themselves; they
/// just relay raw IBKR ops with the request's instrument_id attached.
#[derive(Debug, Clone)]
pub struct DepthUpdate {
    pub instrument_id: InstrumentId,
    /// Level (0=top of book).
    pub position: i32,
    /// 0=insert, 1=update, 2=delete.
    pub operation: i32,
    /// True for bid side, false for ask.
    pub is_bid: bool,
    pub price: f64,
    pub size: u64,
    pub timestamp_ns: u64,
}

/// Connection state. Emitted on the connection stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    /// Initial state, before connect() has been called.
    Disconnected,
    /// Connect attempt in flight.
    Connecting,
    /// Connected and ready (handshake completed, account synced).
    Connected,
    /// Lost connection unexpectedly. Adapter will attempt reconnect
    /// per its config.
    LostConnection,
    /// Reconnecting after a loss.
    Reconnecting,
    /// disconnect() called explicitly. No reconnect will occur.
    Closed,
}

impl ConnectionState {
    pub fn is_connected(self) -> bool {
        matches!(self, ConnectionState::Connected)
    }
}

/// Connection event. Includes the new state plus an optional reason
/// (e.g., "Error 320: read past end of socket stream").
#[derive(Debug, Clone)]
pub struct ConnectionEvent {
    pub state: ConnectionState,
    /// Reason for the state change, if available. Critical for
    /// diagnosing flaps.
    pub reason: Option<String>,
    pub timestamp_ns: u64,
}
