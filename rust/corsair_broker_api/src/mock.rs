//! In-memory MockBroker for unit testing.
//!
//! Records every call (place/cancel/modify) so tests can assert on the
//! exact sequence of broker-bound operations a consumer issued.
//! Drives push streams from test code via the `inject_*` methods so
//! tests can simulate fills / status updates / connection flaps
//! without external dependencies.
//!
//! Usage:
//!
//! ```ignore
//! use corsair_broker_api::mock::MockBroker;
//! use corsair_broker_api::*;
//!
//! let mut b = MockBroker::new();
//! b.connect().await.unwrap();
//!
//! // Test consumer code that uses `&dyn Broker`:
//! let id = b.place_order(some_req).await.unwrap();
//!
//! // Verify the consumer made the right call:
//! assert_eq!(b.calls().lock().unwrap().place_orders.len(), 1);
//!
//! // Drive a fill from the test:
//! b.inject_fill(some_fill);
//! ```

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::NaiveDate;
use tokio::sync::broadcast;

use crate::capabilities::{BrokerCapabilities, BrokerKind};
use crate::contract::{
    ChainQuery, Contract, ContractKind, FutureQuery, InstrumentId, OptionQuery,
};
use crate::error::BrokerError;
use crate::events::{ConnectionEvent, ConnectionState, Fill};
use crate::orders::{
    ModifyOrderReq, OpenOrder, OrderId, OrderStatusUpdate, PlaceOrderReq, TimeInForce,
};
use crate::position::{AccountSnapshot, Position};
use crate::tick::{Tick, TickStreamHandle, TickSubscription};
use crate::{Broker, Result, STREAM_CAPACITY};

/// Recorded calls, one Vec per method. Tests inspect this to verify
/// behavior.
#[derive(Debug, Default)]
pub struct CallLog {
    pub place_orders: Vec<PlaceOrderReq>,
    pub cancel_orders: Vec<OrderId>,
    pub modify_orders: Vec<(OrderId, ModifyOrderReq)>,
    pub subscribe_ticks: Vec<TickSubscription>,
    pub unsubscribe_ticks: Vec<TickStreamHandle>,
    pub connect_count: u32,
    pub disconnect_count: u32,
}

pub struct MockBroker {
    connected: Arc<Mutex<bool>>,
    next_order_id: Arc<Mutex<u64>>,
    next_handle: Arc<Mutex<u64>>,
    calls: Arc<Mutex<CallLog>>,

    fills_tx: broadcast::Sender<Fill>,
    status_tx: broadcast::Sender<OrderStatusUpdate>,
    ticks_tx: broadcast::Sender<Tick>,
    errors_tx: broadcast::Sender<BrokerError>,
    conn_tx: broadcast::Sender<ConnectionEvent>,

    capabilities: BrokerCapabilities,
    /// Test fixtures:
    fixed_positions: Arc<Mutex<Vec<Position>>>,
    fixed_account: Arc<Mutex<Option<AccountSnapshot>>>,
    fixed_open_orders: Arc<Mutex<Vec<OpenOrder>>>,
    fixed_fills: Arc<Mutex<Vec<Fill>>>,
}

impl MockBroker {
    pub fn new() -> Self {
        let (fills_tx, _) = broadcast::channel(STREAM_CAPACITY);
        let (status_tx, _) = broadcast::channel(STREAM_CAPACITY);
        let (ticks_tx, _) = broadcast::channel(STREAM_CAPACITY);
        let (errors_tx, _) = broadcast::channel(STREAM_CAPACITY);
        let (conn_tx, _) = broadcast::channel(STREAM_CAPACITY);

        let caps = BrokerCapabilities {
            kind: BrokerKind::Mock,
            requires_account_per_order: false,
            fills_on_separate_channel: false,
            supported_tifs: vec![
                TimeInForce::Gtc,
                TimeInForce::Day,
                TimeInForce::Gtd,
                TimeInForce::Ioc,
                TimeInForce::Fok,
            ],
            typical_rtt_ms: (1, 5),
            provides_maintenance_margin: true,
        };

        Self {
            connected: Arc::new(Mutex::new(false)),
            next_order_id: Arc::new(Mutex::new(1)),
            next_handle: Arc::new(Mutex::new(1)),
            calls: Arc::new(Mutex::new(CallLog::default())),
            fills_tx,
            status_tx,
            ticks_tx,
            errors_tx,
            conn_tx,
            capabilities: caps,
            fixed_positions: Arc::new(Mutex::new(Vec::new())),
            fixed_account: Arc::new(Mutex::new(None)),
            fixed_open_orders: Arc::new(Mutex::new(Vec::new())),
            fixed_fills: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Access the call log for assertions.
    pub fn calls(&self) -> Arc<Mutex<CallLog>> {
        Arc::clone(&self.calls)
    }

    /// Inject a fill onto the fills stream from test code.
    pub fn inject_fill(&self, f: Fill) {
        let _ = self.fills_tx.send(f);
    }

    /// Inject an order status update.
    pub fn inject_status(&self, s: OrderStatusUpdate) {
        let _ = self.status_tx.send(s);
    }

    /// Inject a tick.
    pub fn inject_tick(&self, t: Tick) {
        let _ = self.ticks_tx.send(t);
    }

    /// Inject a broker error event.
    pub fn inject_error(&self, e: BrokerError) {
        let _ = self.errors_tx.send(e);
    }

    /// Inject a connection state change.
    pub fn inject_connection(&self, e: ConnectionEvent) {
        let _ = self.conn_tx.send(e);
    }

    /// Set the positions returned by `positions()`.
    pub fn set_positions(&self, positions: Vec<Position>) {
        *self.fixed_positions.lock().unwrap() = positions;
    }

    /// Set the account snapshot returned by `account_values()`.
    pub fn set_account(&self, account: AccountSnapshot) {
        *self.fixed_account.lock().unwrap() = Some(account);
    }

    /// Set open orders returned by `open_orders()`.
    pub fn set_open_orders(&self, orders: Vec<OpenOrder>) {
        *self.fixed_open_orders.lock().unwrap() = orders;
    }

    /// Set the historical fills returned by `recent_fills()`. Used to
    /// test reconnect-replay paths.
    pub fn set_recent_fills(&self, fills: Vec<Fill>) {
        *self.fixed_fills.lock().unwrap() = fills;
    }
}

impl Default for MockBroker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Broker for MockBroker {
    async fn connect(&mut self) -> Result<()> {
        *self.connected.lock().unwrap() = true;
        self.calls.lock().unwrap().connect_count += 1;
        let _ = self.conn_tx.send(ConnectionEvent {
            state: ConnectionState::Connected,
            reason: Some("mock connect".into()),
            timestamp_ns: 0,
        });
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        *self.connected.lock().unwrap() = false;
        self.calls.lock().unwrap().disconnect_count += 1;
        let _ = self.conn_tx.send(ConnectionEvent {
            state: ConnectionState::Closed,
            reason: Some("mock disconnect".into()),
            timestamp_ns: 0,
        });
        Ok(())
    }

    fn is_connected(&self) -> bool {
        *self.connected.lock().unwrap()
    }

    async fn place_order(&self, req: PlaceOrderReq) -> Result<OrderId> {
        if !*self.connected.lock().unwrap() {
            return Err(BrokerError::NotConnected("mock not connected".into()));
        }
        let mut next = self.next_order_id.lock().unwrap();
        let id = OrderId(*next);
        *next += 1;
        self.calls.lock().unwrap().place_orders.push(req);
        Ok(id)
    }

    async fn cancel_order(&self, id: OrderId) -> Result<()> {
        if !*self.connected.lock().unwrap() {
            return Err(BrokerError::NotConnected("mock not connected".into()));
        }
        self.calls.lock().unwrap().cancel_orders.push(id);
        Ok(())
    }

    async fn modify_order(&self, id: OrderId, req: ModifyOrderReq) -> Result<()> {
        if !*self.connected.lock().unwrap() {
            return Err(BrokerError::NotConnected("mock not connected".into()));
        }
        self.calls.lock().unwrap().modify_orders.push((id, req));
        Ok(())
    }

    async fn positions(&self) -> Result<Vec<Position>> {
        Ok(self.fixed_positions.lock().unwrap().clone())
    }

    async fn account_values(&self) -> Result<AccountSnapshot> {
        self.fixed_account.lock().unwrap().clone().ok_or_else(|| {
            BrokerError::Internal("mock: no account snapshot configured".into())
        })
    }

    async fn open_orders(&self) -> Result<Vec<OpenOrder>> {
        Ok(self.fixed_open_orders.lock().unwrap().clone())
    }

    async fn recent_fills(&self, since_ns: u64) -> Result<Vec<crate::events::Fill>> {
        // Mock: return any pre-loaded fills with timestamp >= since_ns.
        let fills = self.fixed_fills.lock().unwrap().clone();
        Ok(fills.into_iter().filter(|f| f.timestamp_ns >= since_ns).collect())
    }

    async fn qualify_future(&self, q: FutureQuery) -> Result<Contract> {
        let mut next = self.next_handle.lock().unwrap();
        let id = InstrumentId(*next);
        *next += 1;
        Ok(Contract {
            instrument_id: id,
            kind: ContractKind::Future,
            symbol: q.symbol.clone(),
            local_symbol: format!("{}{}", q.symbol, q.expiry.format("%y%m")),
            expiry: q.expiry,
            strike: None,
            right: None,
            multiplier: 25_000.0,
            exchange: q.exchange,
            currency: q.currency,
            trading_class: q.symbol.clone(),
        })
    }

    async fn qualify_option(&self, q: OptionQuery) -> Result<Contract> {
        let mut next = self.next_handle.lock().unwrap();
        let id = InstrumentId(*next);
        *next += 1;
        Ok(Contract {
            instrument_id: id,
            kind: ContractKind::Option,
            symbol: q.symbol.clone(),
            local_symbol: format!(
                "{}-{}-{:.2}{}",
                q.symbol,
                q.expiry,
                q.strike,
                q.right.as_char()
            ),
            expiry: q.expiry,
            strike: Some(q.strike),
            right: Some(q.right),
            multiplier: q.multiplier,
            exchange: q.exchange,
            currency: q.currency,
            trading_class: q.symbol.clone(),
        })
    }

    async fn list_chain(&self, q: ChainQuery) -> Result<Vec<Contract>> {
        // Synthetic chain: 3 expiries one month apart starting from min_expiry
        // (or today if not set). All futures, for hedge contract resolution
        // testing.
        let start = q.min_expiry.unwrap_or(NaiveDate::from_ymd_opt(2026, 5, 1).unwrap());
        let mut out = Vec::new();
        for i in 0..3 {
            let mut expiry = start;
            for _ in 0..i {
                expiry = expiry
                    .checked_add_months(chrono::Months::new(1))
                    .unwrap_or(expiry);
            }
            let mut next = self.next_handle.lock().unwrap();
            let id = InstrumentId(*next);
            *next += 1;
            out.push(Contract {
                instrument_id: id,
                kind: ContractKind::Future,
                symbol: q.symbol.clone(),
                local_symbol: format!("{}{}", q.symbol, expiry.format("%y%m")),
                expiry,
                strike: None,
                right: None,
                multiplier: 25_000.0,
                exchange: q.exchange,
                currency: q.currency,
                trading_class: q.symbol.clone(),
            });
        }
        Ok(out)
    }

    async fn subscribe_ticks(&self, sub: TickSubscription) -> Result<TickStreamHandle> {
        let mut next = self.next_handle.lock().unwrap();
        let h = TickStreamHandle(*next);
        *next += 1;
        self.calls.lock().unwrap().subscribe_ticks.push(sub);
        Ok(h)
    }

    async fn unsubscribe_ticks(&self, h: TickStreamHandle) -> Result<()> {
        self.calls.lock().unwrap().unsubscribe_ticks.push(h);
        Ok(())
    }

    fn subscribe_fills(&self) -> broadcast::Receiver<Fill> {
        self.fills_tx.subscribe()
    }

    fn subscribe_order_status(&self) -> broadcast::Receiver<OrderStatusUpdate> {
        self.status_tx.subscribe()
    }

    fn subscribe_ticks_stream(&self) -> broadcast::Receiver<Tick> {
        self.ticks_tx.subscribe()
    }

    fn subscribe_errors(&self) -> broadcast::Receiver<BrokerError> {
        self.errors_tx.subscribe()
    }

    fn subscribe_connection(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.conn_tx.subscribe()
    }

    fn capabilities(&self) -> &BrokerCapabilities {
        &self.capabilities
    }
}

// Suppress the "unused" warning on `id` in cancel_order — we record but
// don't otherwise use it.
#[allow(dead_code)]
fn _ignore_unused() {}
