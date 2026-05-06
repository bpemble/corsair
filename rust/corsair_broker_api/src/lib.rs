//! Corsair v3 Broker API — exchange-gateway abstraction.
//!
//! This crate defines the [`Broker`] trait and supporting value types that
//! every exchange-gateway implementation must satisfy. The runtime
//! (`corsair_broker` daemon, `corsair_oms`, `corsair_position`,
//! `corsair_risk`, `corsair_hedge`) talks to `dyn Broker` — never to a
//! specific gateway's types — so the IBKR → FCM/iLink swap is a contained
//! adapter rewrite.
//!
//! # Design principles
//!
//! 1. **Hide gateway quirks.** IBKR's FA-orderKey rewriting, multiple
//!    Trade objects per orderId, account-field-required-on-every-order,
//!    Error 320 socket reads, the `_canonical_trade` walk — none of these
//!    leak across this trait. Consumers see clean order ids, fills, and
//!    status updates.
//!
//! 2. **Push streams, pull queries.** State that the runtime needs to
//!    react to (fills, status updates, ticks, errors) is delivered as a
//!    push stream. State the runtime queries on demand (positions,
//!    account values, open orders) is a single async call.
//!
//! 3. **Broker-assigned ids are u64.** No string ids in the hot path.
//!    Adapters translate their internal types (IBKR conId, IBKR orderId,
//!    iLink security_id) to/from these.
//!
//! 4. **Capabilities are explicit.** Adapters declare what they support
//!    via [`BrokerCapabilities`] so consumers don't have to env-check or
//!    feature-flag based on the wire protocol.
//!
//! # Phase status
//!
//! Phase 1 (this crate): trait surface lock. No production consumers yet.
//! See `docs/architecture/rust_runtime_v3.md` for full migration plan.

pub mod capabilities;
pub mod contract;
pub mod error;
pub mod events;
pub mod mock;
pub mod orders;
pub mod position;
pub mod tick;

pub use capabilities::{BrokerCapabilities, BrokerKind};
pub use contract::{
    ChainQuery, Contract, ContractKind, Currency, Exchange, FutureQuery, InstrumentId,
    OptionQuery, Right,
};
pub use error::BrokerError;
pub use events::{ConnectionEvent, ConnectionState};
pub use orders::{
    ModifyOrderReq, OpenOrder, OrderId, OrderStatus, OrderStatusUpdate, OrderType,
    PlaceOrderReq, Side, TimeInForce,
};
pub use position::{AccountSnapshot, Position};
pub use tick::{Tick, TickKind, TickStreamHandle, TickSubscription};

use async_trait::async_trait;
use tokio::sync::broadcast;

/// Result alias used throughout the trait.
pub type Result<T> = std::result::Result<T, BrokerError>;

/// Channel capacity for push streams (fills/status/ticks/errors).
/// 4096 frames keeps a few seconds of typical broker burst (~500/sec
/// peak observed during cut-over) without blocking the adapter; lagged
/// consumers drop frames and log a warning.
pub const STREAM_CAPACITY: usize = 4096;

/// The Broker trait — every exchange-gateway implementation provides this
/// surface. Consumers (OMS, position book, risk, hedge, market_data) call
/// methods on `dyn Broker` and subscribe to its push streams.
///
/// # Implementations
///
/// - `corsair_broker_ibkr_native` — native Rust IBKR API V100+ wire
///   client. The PyO3+`ib_insync` bridge that previously implemented
///   this trait was retired in the Phase 6.7 cutover (2026-05-02) and
///   the bridge crate was deleted in Phase 6.11.
/// - `corsair_broker_ilink` (future) — CME iLink (FIX 4.2) + MDP3 +
///   FCM drop-copy.
#[async_trait]
pub trait Broker: Send + Sync + 'static {
    // ── Lifecycle ───────────────────────────────────────────────────

    /// Connect to the gateway. Idempotent; if already connected, returns
    /// `Ok(())` without reconnecting.
    ///
    /// `&self` rather than `&mut self` so the broker can live behind
    /// `Arc<dyn Broker>` (interior mutability for connection state).
    /// Audit T1-2: previously `&mut self` forced an `AsyncMutex<Box<…>>`
    /// wrapper that serialized every place_order across its IBKR ack.
    ///
    /// Maps to: `connection.py:connect_ib` (the lean-bypass path).
    async fn connect(&self) -> Result<()>;

    /// Disconnect cleanly. After this returns, push streams have closed
    /// and `is_connected()` returns false.
    ///
    /// Maps to: `connection.py:disconnect_ib`, `IB.disconnect()`.
    async fn disconnect(&self) -> Result<()>;

    /// Current connection state (cheap; cached locally by the adapter).
    ///
    /// Maps to: `IB.isConnected()`.
    fn is_connected(&self) -> bool;

    // ── Order entry ─────────────────────────────────────────────────

    /// Place a new order. Returns the broker-assigned [`OrderId`] on
    /// successful submission to the gateway. The order is NOT
    /// guaranteed to be live at the exchange yet — wait for the
    /// corresponding [`OrderStatusUpdate`] on
    /// [`subscribe_order_status`](Self::subscribe_order_status).
    ///
    /// Maps to: `quote_engine.py:_try_place_order`,
    /// `hedge_manager.py:_place_or_log` (execute branch).
    async fn place_order(&self, req: PlaceOrderReq) -> Result<OrderId>;

    /// v2 wire-timing — drain the per-orderId precise broker
    /// timestamps captured inside `place_order` (just before TCP send)
    /// and the dispatcher (on OpenOrder/OrderStatus ack). Returns
    /// `(send_ns, ack_ns)` if both were captured. Adapters that don't
    /// instrument this return None (default impl); NativeBroker
    /// overrides. Removes ~30 µs of place_order setup overhead from
    /// the wire_timing histogram vs the call-boundary "marker" times.
    fn drain_wire_timing(&self, _order_id: u64) -> Option<(u64, u64)> {
        None
    }

    /// Honest amend RTT timestamp. Returns wall-clock ns when the
    /// broker observed IBKR confirming the new price (i.e. an
    /// OpenOrder whose lmt_price matches what we asked for).
    /// Distinct from the resolution path which is permissive (it
    /// accepts the IB Gateway's PreSubmitted echo so the trader
    /// never timeouts on amends). Default impl returns None for
    /// adapters that don't instrument; NativeBroker overrides.
    fn drain_strict_amend_ack_ns(&self, _order_id: u64) -> Option<u64> {
        None
    }

    /// Cancel an existing order. Returns Ok(()) on successful submission
    /// of the cancel; the order may still fill before the cancel is
    /// processed.
    ///
    /// Maps to: `quote_engine.py:_try_cancel_order`, `IB.cancelOrder()`.
    async fn cancel_order(&self, id: OrderId) -> Result<()>;

    /// Modify an existing order (price / qty). Many gateways implement
    /// this as cancel-replace; returning the same OrderId vs a new one
    /// is implementation-defined and surfaced in the OrderStatusUpdate.
    ///
    /// Maps to: `quote_engine.py:_send_or_update` (modify path).
    async fn modify_order(&self, id: OrderId, req: ModifyOrderReq) -> Result<()>;

    // ── State queries (pull) ────────────────────────────────────────

    /// Current positions held in the account. Reconciled at boot per
    /// CLAUDE.md §10.
    ///
    /// Maps to: `IB.positions()`.
    async fn positions(&self) -> Result<Vec<Position>>;

    /// Account-level state (margin, P&L, buying power). Cached by
    /// adapter; freshness depends on gateway's account update cadence
    /// (~5 min for IBKR).
    ///
    /// Maps to: `IB.accountValues()` reduction in
    /// `constraint_checker.IBKRMarginChecker.update_cached_margin`.
    async fn account_values(&self) -> Result<AccountSnapshot>;

    /// Diagnostic: snapshot of (tick_type, count) for incoming TickSize
    /// messages since the last call (drain-and-reset semantics). Default
    /// returns empty for non-IBKR adapters. Used by `corsair_broker::
    /// tasks::periodic_tick_type_hist` to surface routing or
    /// permissions issues — e.g. if call OI (27) never appears but put
    /// OI (28) does, the data feed itself is the problem.
    fn diagnostic_take_tick_type_hist(&self) -> Vec<(i32, u64)> {
        Vec::new()
    }

    /// Live orders at the gateway. Reconciliation source; the OMS keeps
    /// its own canonical view from order_status_stream events.
    ///
    /// Maps to: `IB.openTrades()` reduction.
    async fn open_orders(&self) -> Result<Vec<OpenOrder>>;

    /// Recent fills since `since_ns` (epoch nanoseconds). Used by the
    /// fill handler's `replay_missed_executions` path: gateways do not
    /// replay execDetailsEvents on reconnect, so after a disconnect the
    /// runtime must explicitly query for fills landed during the gap
    /// and replay them through the dedupe-by-exec_id pipeline.
    ///
    /// Maps to: `fill_handler.py:replay_missed_executions` calling
    /// `IB.reqExecutionsAsync(ExecutionFilter())`. CLAUDE.md §10
    /// names this as part of the position-reconciliation hard
    /// prerequisite for live deployment.
    async fn recent_fills(&self, since_ns: u64) -> Result<Vec<events::Fill>>;

    // ── Contract resolution ─────────────────────────────────────────

    /// Resolve a futures contract by symbol + expiry. Returns a
    /// fully-qualified [`Contract`] with broker-assigned
    /// [`InstrumentId`].
    ///
    /// Maps to: `IB.qualifyContractsAsync(Future(...))`.
    async fn qualify_future(&self, q: FutureQuery) -> Result<Contract>;

    /// Resolve an options contract.
    ///
    /// Maps to: `IB.qualifyContractsAsync(Option(...))`.
    async fn qualify_option(&self, q: OptionQuery) -> Result<Contract>;

    /// List the chain of contracts for a symbol satisfying the query
    /// (e.g., all HG futures expiries for hedge contract resolution).
    ///
    /// Maps to: `IB.reqContractDetailsAsync()`.
    async fn list_chain(&self, q: ChainQuery) -> Result<Vec<Contract>>;

    // ── Market data subscriptions ──────────────────────────────────

    /// Subscribe to ticks for an instrument. Returns a handle the caller
    /// uses to identify their subscription on the tick stream and to
    /// later [`unsubscribe_ticks`](Self::unsubscribe_ticks).
    ///
    /// Maps to: `IB.reqMktData()` (in market_data.py + hedge_manager.py).
    async fn subscribe_ticks(&self, sub: TickSubscription) -> Result<TickStreamHandle>;

    /// Unsubscribe from a tick subscription. After this returns, no
    /// more Tick events for this handle will arrive on the tick stream.
    ///
    /// Maps to: `IB.cancelMktData()`.
    async fn unsubscribe_ticks(&self, h: TickStreamHandle) -> Result<()>;

    /// Subscribe to L2 market depth for a contract. IBKR caps
    /// concurrent depth requests at ~3-5 per account; the broker
    /// daemon rotates active subscriptions across the quoted strike
    /// set. `num_rows` is how many price levels per side IBKR
    /// returns (typical: 5).
    ///
    /// Depth updates land on `subscribe_depth_stream()`. Returns a
    /// handle for unsubscribing.
    async fn subscribe_market_depth(
        &self,
        sub: tick::TickSubscription,
        num_rows: i32,
    ) -> Result<TickStreamHandle> {
        let _ = (sub, num_rows);
        Err(BrokerError::Internal(
            "subscribe_market_depth not implemented".into(),
        ))
    }

    async fn unsubscribe_market_depth(&self, h: TickStreamHandle) -> Result<()> {
        let _ = h;
        Err(BrokerError::Internal(
            "unsubscribe_market_depth not implemented".into(),
        ))
    }

    /// Stream of all depth updates. Single channel; consumers filter
    /// by `instrument_id`.
    fn subscribe_depth_stream(&self) -> broadcast::Receiver<events::DepthUpdate> {
        // Default: empty channel that closes immediately. Real
        // implementations override.
        let (tx, rx) = broadcast::channel(1);
        drop(tx);
        rx
    }

    // ── Push streams (broker → runtime) ────────────────────────────

    /// Subscribe to fill events. Multiple consumers can subscribe; each
    /// gets every fill. Lagged consumers will drop frames and log a
    /// warning (broadcast::Receiver semantics).
    ///
    /// Maps to: `IB.execDetailsEvent` subscription in
    /// fill_handler.py + hedge_manager.py.
    fn subscribe_fills(&self) -> broadcast::Receiver<events::Fill>;

    /// Subscribe to order status updates (PendingSubmit → Submitted →
    /// Filled / Cancelled / Rejected).
    ///
    /// Maps to: `IB.orderStatusEvent` subscription in quote_engine.py.
    fn subscribe_order_status(&self) -> broadcast::Receiver<OrderStatusUpdate>;

    /// Subscribe to ticks. Single channel per Broker; ticks for ALL
    /// active subscriptions land here. Filter by
    /// [`Tick::instrument_id`].
    ///
    /// Wire equivalent: a stream of IBKR `tickPrice` / `tickSize`
    /// (and friends) consolidated into the [`Tick`] envelope.
    fn subscribe_ticks_stream(&self) -> broadcast::Receiver<Tick>;

    /// Subscribe to broker errors (Error 320, 10197, blackouts, etc.).
    ///
    /// Maps to: `IB.errorEvent` subscription in
    /// quote_engine.py:_on_ib_error.
    fn subscribe_errors(&self) -> broadcast::Receiver<BrokerError>;

    /// Subscribe to connection events (connect / disconnect / reconnect).
    /// Used by the watchdog and by risk to clear disconnect-source
    /// kills on reconnect.
    ///
    /// Maps to: `IB.connectedEvent` + `IB.disconnectedEvent` in
    /// watchdog.py.
    fn subscribe_connection(&self) -> broadcast::Receiver<ConnectionEvent>;

    // ── Capabilities ───────────────────────────────────────────────

    /// Adapter-specific capabilities. Consumers query this once at
    /// startup to configure their behavior (FA-account field, drop-copy
    /// vs in-band fills, supported TIFs, etc.).
    fn capabilities(&self) -> &BrokerCapabilities;

    // ── Lifecycle hooks (optional — default no-op) ─────────────────

    /// Wait until the adapter's initial state snapshot has finished
    /// streaming (positions, open orders, account values). Adapters
    /// that bootstrap synchronously inside `connect()` can leave this
    /// as the default no-op. Adapters that stream asynchronously
    /// after connect (NativeBroker's reqPositions / reqOpenOrders /
    /// reqAccountUpdates) MUST override to gate the runtime on
    /// PositionEnd / OpenOrderEnd / AccountDownloadEnd.
    ///
    /// CLAUDE.md §10 names live-deployment position reconciliation as
    /// a hard prerequisite — this method is the trait-level surface
    /// for that.
    ///
    /// Returns `Ok(())` even on timeout — the caller decides whether
    /// to proceed with a partial snapshot.
    async fn wait_for_initial_snapshot(
        &self,
        _timeout: std::time::Duration,
    ) -> Result<()> {
        Ok(())
    }

    /// Install a fast-path tick publisher closure. NativeBroker calls
    /// this in its tick dispatcher to forward ticks directly to a
    /// downstream consumer (typically the SHM IPC server's events
    /// ring), bypassing the broadcast channel + forward task pump.
    ///
    /// Default impl: no-op — broadcast channel remains the primary
    /// tick path. Adapters that want the fast-path optimization
    /// override this. Returns true if the publisher was actually
    /// installed; caller falls back to the broadcast pump on false.
    async fn set_tick_publisher(
        &self,
        _publisher: std::sync::Arc<dyn for<'a> Fn(&'a Tick) + Send + Sync + 'static>,
    ) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Fill;
    use crate::mock::MockBroker;
    use chrono::NaiveDate;

    fn sample_future_query() -> FutureQuery {
        FutureQuery {
            symbol: "HG".into(),
            expiry: NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
            exchange: Exchange::Comex,
            currency: Currency::Usd,
        }
    }

    fn sample_option_query() -> OptionQuery {
        OptionQuery {
            symbol: "HXE".into(),
            expiry: NaiveDate::from_ymd_opt(2026, 5, 26).unwrap(),
            strike: 6.05,
            right: Right::Call,
            exchange: Exchange::Comex,
            currency: Currency::Usd,
            multiplier: 25_000.0,
        }
    }

    fn sample_place(contract: Contract) -> PlaceOrderReq {
        PlaceOrderReq {
            contract,
            side: Side::Buy,
            qty: 1,
            order_type: OrderType::Limit,
            price: Some(0.025),
            tif: TimeInForce::Gtd,
            gtd_until_utc: Some(chrono::Utc::now() + chrono::Duration::seconds(30)),
            client_order_ref: "test_ref_1".into(),
            account: Some("DUP000000".into()),
        }
    }

    #[tokio::test]
    async fn lifecycle_connect_disconnect_roundtrip() {
        let mut b = MockBroker::new();
        assert!(!b.is_connected());
        b.connect().await.unwrap();
        assert!(b.is_connected());
        b.disconnect().await.unwrap();
        assert!(!b.is_connected());
    }

    #[tokio::test]
    async fn place_order_returns_unique_ids() {
        let mut b = MockBroker::new();
        b.connect().await.unwrap();
        let c = b.qualify_future(sample_future_query()).await.unwrap();
        let id1 = b.place_order(sample_place(c.clone())).await.unwrap();
        let id2 = b.place_order(sample_place(c)).await.unwrap();
        assert_ne!(id1.0, id2.0);
        let log = b.calls();
        let g = log.lock().unwrap();
        assert_eq!(g.place_orders.len(), 2);
    }

    #[tokio::test]
    async fn place_when_disconnected_errors() {
        let b = MockBroker::new();
        let c = Contract {
            instrument_id: InstrumentId(1),
            kind: ContractKind::Future,
            symbol: "HG".into(),
            local_symbol: "HGM6".into(),
            expiry: NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
            strike: None,
            right: None,
            multiplier: 25_000.0,
            exchange: Exchange::Comex,
            currency: Currency::Usd,
            trading_class: "HG".into(),
        };
        let err = b.place_order(sample_place(c)).await.unwrap_err();
        assert!(err.is_connection_error());
    }

    #[tokio::test]
    async fn cancel_logged_in_call_log() {
        let mut b = MockBroker::new();
        b.connect().await.unwrap();
        b.cancel_order(OrderId(42)).await.unwrap();
        let log = b.calls();
        let g = log.lock().unwrap();
        assert_eq!(g.cancel_orders, vec![OrderId(42)]);
    }

    #[tokio::test]
    async fn modify_logged_in_call_log() {
        let mut b = MockBroker::new();
        b.connect().await.unwrap();
        b.modify_order(
            OrderId(7),
            ModifyOrderReq {
                price: Some(0.030),
                qty: None,
                gtd_until_utc: None,
            },
        )
        .await
        .unwrap();
        let log = b.calls();
        let g = log.lock().unwrap();
        assert_eq!(g.modify_orders.len(), 1);
        assert_eq!(g.modify_orders[0].0, OrderId(7));
    }

    #[tokio::test]
    async fn fills_stream_delivers_to_multiple_consumers() {
        let mut b = MockBroker::new();
        b.connect().await.unwrap();
        let mut rx1 = b.subscribe_fills();
        let mut rx2 = b.subscribe_fills();

        let fill = Fill {
            exec_id: "exec-1".into(),
            order_id: OrderId(1),
            instrument_id: InstrumentId(100),
            side: Side::Buy,
            qty: 1,
            price: 0.025,
            timestamp_ns: 0,
            commission: Some(2.5),
        };
        b.inject_fill(fill.clone());

        let r1 = rx1.recv().await.unwrap();
        let r2 = rx2.recv().await.unwrap();
        assert_eq!(r1.exec_id, "exec-1");
        assert_eq!(r2.exec_id, "exec-1");
    }

    #[tokio::test]
    async fn order_status_stream_reports_terminal_states() {
        let mut b = MockBroker::new();
        b.connect().await.unwrap();
        let mut rx = b.subscribe_order_status();
        b.inject_status(OrderStatusUpdate {
            order_id: OrderId(1),
            status: OrderStatus::Filled,
            filled_qty: 1,
            remaining_qty: 0,
            avg_fill_price: 0.025,
            last_fill_price: Some(0.025),
            timestamp_ns: 0,
            reject_reason: None,
        });
        let s = rx.recv().await.unwrap();
        assert!(s.status.is_terminal());
        assert_eq!(s.filled_qty, 1);
    }

    #[tokio::test]
    async fn connection_stream_reports_state_transitions() {
        let mut b = MockBroker::new();
        let mut rx = b.subscribe_connection();
        b.connect().await.unwrap();
        let e = rx.recv().await.unwrap();
        assert_eq!(e.state, ConnectionState::Connected);
        assert!(e.state.is_connected());
        b.disconnect().await.unwrap();
        let e = rx.recv().await.unwrap();
        assert_eq!(e.state, ConnectionState::Closed);
    }

    #[tokio::test]
    async fn errors_stream_categorizes_correctly() {
        let mut b = MockBroker::new();
        b.connect().await.unwrap();
        let mut rx = b.subscribe_errors();
        b.inject_error(BrokerError::ConnectionLost("test".into()));
        let e = rx.recv().await.unwrap();
        assert!(e.is_connection_error());
        assert!(e.is_retriable());

        b.inject_error(BrokerError::Account("no funds".into()));
        let e = rx.recv().await.unwrap();
        assert!(!e.is_connection_error());
        assert!(!e.is_retriable());
    }

    #[tokio::test]
    async fn qualify_future_returns_unique_instrument_ids() {
        let b = MockBroker::new();
        let c1 = b.qualify_future(sample_future_query()).await.unwrap();
        let c2 = b.qualify_future(sample_future_query()).await.unwrap();
        assert_ne!(c1.instrument_id, c2.instrument_id);
        assert_eq!(c1.kind, ContractKind::Future);
        assert_eq!(c1.right, None);
        assert_eq!(c1.strike, None);
    }

    #[tokio::test]
    async fn qualify_option_includes_strike_and_right() {
        let b = MockBroker::new();
        let c = b.qualify_option(sample_option_query()).await.unwrap();
        assert_eq!(c.kind, ContractKind::Option);
        assert_eq!(c.right, Some(Right::Call));
        assert_eq!(c.strike, Some(6.05));
    }

    #[tokio::test]
    async fn list_chain_returns_min_expiry_filtered_set() {
        let b = MockBroker::new();
        let q = ChainQuery {
            symbol: "HG".into(),
            exchange: Exchange::Comex,
            currency: Currency::Usd,
            kind: Some(ContractKind::Future),
            min_expiry: Some(NaiveDate::from_ymd_opt(2026, 6, 1).unwrap()),
        };
        let chain = b.list_chain(q).await.unwrap();
        assert!(!chain.is_empty());
        for c in &chain {
            assert!(c.expiry >= NaiveDate::from_ymd_opt(2026, 6, 1).unwrap());
        }
    }

    #[tokio::test]
    async fn capabilities_advertise_kind() {
        let b = MockBroker::new();
        assert_eq!(b.capabilities().kind, BrokerKind::Mock);
        assert!(b.capabilities().supported_tifs.contains(&TimeInForce::Gtd));
    }

    #[tokio::test]
    async fn order_status_terminal_helper() {
        assert!(OrderStatus::Filled.is_terminal());
        assert!(OrderStatus::Cancelled.is_terminal());
        assert!(OrderStatus::Rejected.is_terminal());
        assert!(!OrderStatus::Submitted.is_terminal());
        assert!(!OrderStatus::PendingSubmit.is_terminal());
    }

    #[tokio::test]
    async fn dyn_broker_object_safety() {
        // Ensure the trait is dyn-compatible: the runtime will hold
        // a `Box<dyn Broker>` so this is load-bearing.
        let mut b: Box<dyn Broker> = Box::new(MockBroker::new());
        b.connect().await.unwrap();
        assert!(b.is_connected());
    }

    #[tokio::test]
    async fn ibkr_default_capabilities_match_claude_md() {
        let caps = BrokerCapabilities::ibkr_default();
        assert!(caps.requires_account_per_order);
        assert!(caps.provides_maintenance_margin);
        assert_eq!(caps.kind, BrokerKind::Ibkr);
    }

    #[tokio::test]
    async fn recent_fills_filters_by_since_ns() {
        let b = MockBroker::new();
        let f1 = Fill {
            exec_id: "old-1".into(),
            order_id: OrderId(1),
            instrument_id: InstrumentId(100),
            side: Side::Buy,
            qty: 1,
            price: 0.025,
            timestamp_ns: 1_000,
            commission: None,
        };
        let f2 = Fill {
            exec_id: "new-1".into(),
            order_id: OrderId(2),
            instrument_id: InstrumentId(100),
            side: Side::Sell,
            qty: 1,
            price: 0.030,
            timestamp_ns: 5_000,
            commission: None,
        };
        b.set_recent_fills(vec![f1, f2]);
        // since_ns = 2_000 → only f2 (5_000) returns.
        let got = b.recent_fills(2_000).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].exec_id, "new-1");
    }

    #[tokio::test]
    async fn ilink_default_capabilities_differ_from_ibkr() {
        let caps = BrokerCapabilities::ilink_default();
        assert!(!caps.requires_account_per_order);
        assert!(caps.fills_on_separate_channel);
        assert!(caps.typical_rtt_ms.0 < 10);
    }
}
