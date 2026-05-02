//! `IbkrAdapter` — implements `corsair_broker_api::Broker` by sending
//! commands to the bridge thread.
//!
//! Each trait method:
//!   1. Constructs a `Command` variant carrying the input + a oneshot
//!      reply channel
//!   2. Sends the command into the bridge
//!   3. Awaits the oneshot reply
//!   4. Returns the result to the caller
//!
//! All gateway-specific quirks (FA-orderKey rewriting, multiple Trade
//! objects per orderId, account-required-on-every-order) are handled
//! inside the bridge thread's `dispatch` function — they don't leak
//! across this surface.

use async_trait::async_trait;
use corsair_broker_api::{
    AccountSnapshot, Broker, BrokerCapabilities, BrokerError, ChainQuery, ConnectionEvent,
    Contract, FutureQuery, ModifyOrderReq, OpenOrder, OptionQuery, OrderId, OrderStatusUpdate,
    PlaceOrderReq, Position, Result as BrokerResult, Tick, TickStreamHandle, TickSubscription,
};
use corsair_broker_api::events::Fill;
use tokio::sync::{broadcast, oneshot};

use crate::bridge::{Bridge, BridgeConfig, BridgeError};
use crate::commands::Command;
use crate::conversion::ibkr_caps;

/// IBKR Broker implementation. Owns the bridge thread and exposes the
/// trait surface to consumers.
///
/// Construction:
/// ```ignore
/// use corsair_broker_ibkr::{IbkrAdapter, BridgeConfig};
/// use corsair_broker_api::Broker;
///
/// let cfg = BridgeConfig {
///     gateway_host: "127.0.0.1".into(),
///     gateway_port: 4002,
///     client_id: 0,
///     account: "DUP000000".into(),
///     ..Default::default()
/// };
/// let mut adapter = IbkrAdapter::new(cfg).expect("spawn bridge");
/// adapter.connect().await.expect("connect");
/// ```
pub struct IbkrAdapter {
    bridge: Bridge,
    capabilities: BrokerCapabilities,
}

impl IbkrAdapter {
    /// Spawn the bridge thread and return an adapter wired to it.
    /// Does NOT connect — callers must invoke `Broker::connect`
    /// separately to establish the IBKR session.
    pub fn new(cfg: BridgeConfig) -> Result<Self, BridgeError> {
        let bridge = Bridge::spawn(cfg)?;
        Ok(Self {
            bridge,
            capabilities: ibkr_caps(),
        })
    }

    /// Helper: send a command and await the reply.
    async fn round_trip<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<BrokerResult<T>>) -> Command,
    ) -> BrokerResult<T> {
        let (tx, rx) = oneshot::channel();
        let cmd = build(tx);
        if self.bridge.send(cmd).is_err() {
            return Err(BrokerError::Internal(
                "bridge thread is gone (channel closed)".into(),
            ));
        }
        match rx.await {
            Ok(v) => v,
            Err(_) => Err(BrokerError::Internal(
                "bridge dropped reply oneshot before responding".into(),
            )),
        }
    }
}

#[async_trait]
impl Broker for IbkrAdapter {
    // ── Lifecycle ───────────────────────────────────────────────────

    async fn connect(&mut self) -> BrokerResult<()> {
        self.round_trip(|reply| Command::Connect { reply }).await
    }

    async fn disconnect(&mut self) -> BrokerResult<()> {
        self.round_trip(|reply| Command::Disconnect { reply }).await
    }

    fn is_connected(&self) -> bool {
        // We don't gate this on a round-trip to keep it cheap; the
        // bridge sets a local flag we can poll. Phase 2.4 wires this.
        // For now we conservatively report false until connect()
        // succeeds (and never updates this stub). Consumers that need
        // accuracy should subscribe to subscribe_connection().
        false
    }

    // ── Order entry ─────────────────────────────────────────────────

    async fn place_order(&self, req: PlaceOrderReq) -> BrokerResult<OrderId> {
        self.round_trip(|reply| Command::PlaceOrder { req, reply })
            .await
    }

    async fn cancel_order(&self, id: OrderId) -> BrokerResult<()> {
        self.round_trip(|reply| Command::CancelOrder { id, reply })
            .await
    }

    async fn modify_order(&self, id: OrderId, req: ModifyOrderReq) -> BrokerResult<()> {
        self.round_trip(|reply| Command::ModifyOrder { id, req, reply })
            .await
    }

    // ── State queries ──────────────────────────────────────────────

    async fn positions(&self) -> BrokerResult<Vec<Position>> {
        self.round_trip(|reply| Command::Positions { reply }).await
    }

    async fn account_values(&self) -> BrokerResult<AccountSnapshot> {
        self.round_trip(|reply| Command::AccountValues { reply }).await
    }

    async fn open_orders(&self) -> BrokerResult<Vec<OpenOrder>> {
        self.round_trip(|reply| Command::OpenOrders { reply }).await
    }

    async fn recent_fills(&self, since_ns: u64) -> BrokerResult<Vec<Fill>> {
        self.round_trip(|reply| Command::RecentFills { since_ns, reply })
            .await
    }

    // ── Contract resolution ────────────────────────────────────────

    async fn qualify_future(&self, q: FutureQuery) -> BrokerResult<Contract> {
        self.round_trip(|reply| Command::QualifyFuture { q, reply })
            .await
    }

    async fn qualify_option(&self, q: OptionQuery) -> BrokerResult<Contract> {
        self.round_trip(|reply| Command::QualifyOption { q, reply })
            .await
    }

    async fn list_chain(&self, q: ChainQuery) -> BrokerResult<Vec<Contract>> {
        self.round_trip(|reply| Command::ListChain { q, reply }).await
    }

    // ── Market data ────────────────────────────────────────────────

    async fn subscribe_ticks(&self, sub: TickSubscription) -> BrokerResult<TickStreamHandle> {
        self.round_trip(|reply| Command::SubscribeTicks { sub, reply })
            .await
    }

    async fn unsubscribe_ticks(&self, h: TickStreamHandle) -> BrokerResult<()> {
        self.round_trip(|reply| Command::UnsubscribeTicks { h, reply })
            .await
    }

    // ── Push streams ───────────────────────────────────────────────

    fn subscribe_fills(&self) -> broadcast::Receiver<Fill> {
        self.bridge.streams().fills.subscribe()
    }

    fn subscribe_order_status(&self) -> broadcast::Receiver<OrderStatusUpdate> {
        self.bridge.streams().status.subscribe()
    }

    fn subscribe_ticks_stream(&self) -> broadcast::Receiver<Tick> {
        self.bridge.streams().ticks.subscribe()
    }

    fn subscribe_errors(&self) -> broadcast::Receiver<BrokerError> {
        self.bridge.streams().errors.subscribe()
    }

    fn subscribe_connection(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.bridge.streams().connection.subscribe()
    }

    // ── Capabilities ───────────────────────────────────────────────

    fn capabilities(&self) -> &BrokerCapabilities {
        &self.capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bridge thread starts + accepts commands. Round-trip surfaces
    /// either:
    ///   - "not yet implemented" stub from dispatch (when ib_insync
    ///     IS installed and PyState::new succeeded), OR
    ///   - "bridge thread is gone" (when ib_insync ISN'T installed
    ///     and the bridge thread exited during PyState::new before
    ///     the command arrived)
    /// Both paths validate the architecture: command serialization,
    /// channel send, oneshot reply.
    #[tokio::test(flavor = "multi_thread")]
    async fn bridge_round_trips_command() {
        let adapter = IbkrAdapter::new(BridgeConfig::default()).unwrap();
        let result = adapter.positions().await;
        assert!(result.is_err(), "expected error during pre-Phase-2.4 stubs");
        match result {
            Err(BrokerError::Internal(msg)) => {
                let acceptable = msg.contains("not yet implemented")
                    || msg.contains("bridge thread is gone")
                    || msg.contains("dropped reply oneshot");
                assert!(acceptable, "unexpected Internal message: {msg}");
            }
            other => panic!("expected Internal error, got {:?}", other),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn streams_subscribe_returns_a_receiver() {
        let adapter = IbkrAdapter::new(BridgeConfig::default()).unwrap();
        let _fills_rx = adapter.subscribe_fills();
        let _status_rx = adapter.subscribe_order_status();
        let _ticks_rx = adapter.subscribe_ticks_stream();
        let _errors_rx = adapter.subscribe_errors();
        let _conn_rx = adapter.subscribe_connection();
        // Dropping all receivers is fine; the broadcast Sender stays
        // alive on the bridge.
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn capabilities_advertise_ibkr() {
        let adapter = IbkrAdapter::new(BridgeConfig::default()).unwrap();
        let caps = adapter.capabilities();
        assert_eq!(caps.kind, corsair_broker_api::BrokerKind::Ibkr);
        assert!(caps.requires_account_per_order);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dyn_broker_compatible() {
        let adapter: Box<dyn Broker> =
            Box::new(IbkrAdapter::new(BridgeConfig::default()).unwrap());
        // Trait-object dispatch — exercised by the call:
        let _ = adapter.is_connected();
    }
}
