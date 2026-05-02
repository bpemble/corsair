//! Command + response types for the Tokio ↔ Python bridge.
//!
//! Each variant of [`Command`] corresponds to one Broker trait method.
//! The bridge thread receives a Command + a one-shot reply channel,
//! executes the IBKR call on the asyncio loop, and sends the result
//! back. The Tokio side awaits the one-shot to surface the result
//! to the caller.

use corsair_broker_api::{
    AccountSnapshot, ChainQuery, Contract, FutureQuery, ModifyOrderReq, OpenOrder, OptionQuery,
    OrderId, PlaceOrderReq, Position, Result as BrokerResult, TickStreamHandle, TickSubscription,
};
use corsair_broker_api::events::Fill;
use tokio::sync::oneshot;

/// Commands sent from the Tokio side to the Python bridge thread.
///
/// Each command carries the input payload + a one-shot Sender for the
/// result. The bridge thread executes the corresponding ib_insync call
/// (synchronously from its perspective) and sends the result back.
pub enum Command {
    Connect {
        reply: oneshot::Sender<BrokerResult<()>>,
    },
    Disconnect {
        reply: oneshot::Sender<BrokerResult<()>>,
    },
    PlaceOrder {
        req: PlaceOrderReq,
        reply: oneshot::Sender<BrokerResult<OrderId>>,
    },
    CancelOrder {
        id: OrderId,
        reply: oneshot::Sender<BrokerResult<()>>,
    },
    ModifyOrder {
        id: OrderId,
        req: ModifyOrderReq,
        reply: oneshot::Sender<BrokerResult<()>>,
    },
    Positions {
        reply: oneshot::Sender<BrokerResult<Vec<Position>>>,
    },
    AccountValues {
        reply: oneshot::Sender<BrokerResult<AccountSnapshot>>,
    },
    OpenOrders {
        reply: oneshot::Sender<BrokerResult<Vec<OpenOrder>>>,
    },
    RecentFills {
        since_ns: u64,
        reply: oneshot::Sender<BrokerResult<Vec<Fill>>>,
    },
    QualifyFuture {
        q: FutureQuery,
        reply: oneshot::Sender<BrokerResult<Contract>>,
    },
    QualifyOption {
        q: OptionQuery,
        reply: oneshot::Sender<BrokerResult<Contract>>,
    },
    ListChain {
        q: ChainQuery,
        reply: oneshot::Sender<BrokerResult<Vec<Contract>>>,
    },
    SubscribeTicks {
        sub: TickSubscription,
        reply: oneshot::Sender<BrokerResult<TickStreamHandle>>,
    },
    UnsubscribeTicks {
        h: TickStreamHandle,
        reply: oneshot::Sender<BrokerResult<()>>,
    },
    /// Sentinel command instructing the bridge thread to shut down its
    /// event loop and exit cleanly. Sent during Drop of the Bridge
    /// handle.
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::Connect { .. } => write!(f, "Connect"),
            Command::Disconnect { .. } => write!(f, "Disconnect"),
            Command::PlaceOrder { .. } => write!(f, "PlaceOrder"),
            Command::CancelOrder { id, .. } => write!(f, "CancelOrder({id})"),
            Command::ModifyOrder { id, .. } => write!(f, "ModifyOrder({id})"),
            Command::Positions { .. } => write!(f, "Positions"),
            Command::AccountValues { .. } => write!(f, "AccountValues"),
            Command::OpenOrders { .. } => write!(f, "OpenOrders"),
            Command::RecentFills { since_ns, .. } => write!(f, "RecentFills({since_ns})"),
            Command::QualifyFuture { .. } => write!(f, "QualifyFuture"),
            Command::QualifyOption { .. } => write!(f, "QualifyOption"),
            Command::ListChain { .. } => write!(f, "ListChain"),
            Command::SubscribeTicks { .. } => write!(f, "SubscribeTicks"),
            Command::UnsubscribeTicks { .. } => write!(f, "UnsubscribeTicks"),
            Command::Shutdown { .. } => write!(f, "Shutdown"),
        }
    }
}
