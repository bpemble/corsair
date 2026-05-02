//! `NativeBroker` — `corsair_broker_api::Broker` impl over `NativeClient`.
//!
//! Phase 6.6 of the v3 migration. Replaces the PyO3 + ib_insync adapter
//! in `corsair_broker_ibkr` with a direct-wire IBKR adapter. Once this
//! is complete and validated, the existing PyO3 crate can be retired
//! and Phase 5B.7 cutover can finally happen.
//!
//! # Architecture
//!
//! ```text
//!   ┌──────────────┐        ┌──────────────────┐
//!   │  NativeClient│  rx ──→│  Dispatcher task │
//!   │  (TCP socket)│        │  routes to:      │
//!   └──────────────┘        │   • position     │
//!         ↑                 │     cache        │
//!     send_raw              │   • account      │
//!         │                 │     values cache │
//!   ┌──────────────┐        │   • open orders  │
//!   │  Broker trait│        │   • broadcast    │
//!   │  (NativeBroker)       │     channels     │
//!   └──────────────┘        │   • pending      │
//!                           │     waiters      │
//!                           └──────────────────┘
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::NaiveDate;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use corsair_broker_api::{
    capabilities::{BrokerCapabilities, BrokerKind as _BrokerKind},
    contract::{
        ChainQuery, Contract, ContractKind, Currency, Exchange, FutureQuery, InstrumentId,
        OptionQuery, Right,
    },
    error::BrokerError,
    events::{ConnectionEvent, ConnectionState, Fill},
    orders::{
        ModifyOrderReq, OpenOrder, OrderId, OrderStatus, OrderStatusUpdate, OrderType,
        PlaceOrderReq, Side, TimeInForce,
    },
    position::{AccountSnapshot, Position},
    tick::{Tick, TickKind, TickStreamHandle, TickSubscription},
    Broker, Result as BResult,
};

use crate::client::{NativeClient, NativeClientConfig};
use crate::error::NativeError;
use crate::requests::{
    cancel_mkt_data, cancel_order, place_order, req_account_updates, req_contract_details,
    req_executions, req_mkt_data, req_open_orders, req_positions, ContractRequest,
    ExecutionFilter, PlaceOrderParams,
};
use crate::types::{
    AccountValueMsg, ContractDetailsMsg, ErrorMsg, ExecutionMsg, InboundMsg, OpenOrderMsg,
    PositionMsg,
};

const _: _BrokerKind = _BrokerKind::Ibkr;

const FILL_CHANNEL_CAP: usize = 1024;
const STATUS_CHANNEL_CAP: usize = 4096;
const TICK_CHANNEL_CAP: usize = 16384;
const ERROR_CHANNEL_CAP: usize = 256;
const CONNECTION_CHANNEL_CAP: usize = 64;

#[derive(Debug, Clone)]
pub struct NativeBrokerConfig {
    pub client: NativeClientConfig,
    /// FA sub-account selector. Sent on every order's `account=` field.
    pub account: String,
}

#[derive(Default)]
struct BrokerState {
    /// Position cache, keyed by (account, conId).
    positions: HashMap<(String, i64), Position>,

    /// Latest open order snapshot, keyed by IBKR orderId.
    open_orders: HashMap<i32, OpenOrder>,

    /// Account values keyed by (key, currency, account).
    account_values: HashMap<(String, String, String), AccountValueMsg>,

    /// Pending qualify/chain responses, keyed by reqId.
    pending_contract_details: HashMap<i32, PendingContractRequest>,

    /// Pending recent_fills responses, keyed by reqId.
    pending_executions: HashMap<i32, PendingExecutionsRequest>,

    /// reqId → InstrumentId for tick routing.
    tick_routes: HashMap<i32, InstrumentId>,

    /// handle → reqId for unsubscribe.
    handle_to_req_id: HashMap<TickStreamHandle, i32>,
}

struct PendingContractRequest {
    accumulated: Vec<ContractDetailsMsg>,
    sender: Option<oneshot::Sender<Result<Vec<Contract>, BrokerError>>>,
}

struct PendingExecutionsRequest {
    accumulated: Vec<Fill>,
    sender: Option<oneshot::Sender<Result<Vec<Fill>, BrokerError>>>,
}

#[derive(Clone)]
struct BrokerChannels {
    fills: broadcast::Sender<Fill>,
    status: broadcast::Sender<OrderStatusUpdate>,
    ticks: broadcast::Sender<Tick>,
    errors: broadcast::Sender<BrokerError>,
    connection: broadcast::Sender<ConnectionEvent>,
}

impl BrokerChannels {
    fn new() -> Self {
        Self {
            fills: broadcast::channel(FILL_CHANNEL_CAP).0,
            status: broadcast::channel(STATUS_CHANNEL_CAP).0,
            ticks: broadcast::channel(TICK_CHANNEL_CAP).0,
            errors: broadcast::channel(ERROR_CHANNEL_CAP).0,
            connection: broadcast::channel(CONNECTION_CHANNEL_CAP).0,
        }
    }
}

pub struct NativeBroker {
    cfg: NativeBrokerConfig,
    client: Arc<NativeClient>,
    state: Arc<Mutex<BrokerState>>,
    channels: BrokerChannels,
    capabilities: BrokerCapabilities,
    next_req_id: Arc<AtomicI32>,
    next_handle: Arc<AtomicI32>,
    connected: Arc<AtomicBool>,
    rx_holder: Mutex<Option<mpsc::UnboundedReceiver<Vec<String>>>>,
}

impl NativeBroker {
    pub fn new(cfg: NativeBrokerConfig) -> Self {
        let (client, rx) = NativeClient::new(cfg.client.clone());
        Self {
            cfg,
            client: Arc::new(client),
            state: Arc::new(Mutex::new(BrokerState::default())),
            channels: BrokerChannels::new(),
            capabilities: BrokerCapabilities::ibkr_default(),
            next_req_id: Arc::new(AtomicI32::new(1000)),
            next_handle: Arc::new(AtomicI32::new(1)),
            connected: Arc::new(AtomicBool::new(false)),
            rx_holder: Mutex::new(Some(rx)),
        }
    }

    fn alloc_req_id(&self) -> i32 {
        self.next_req_id.fetch_add(1, Ordering::Relaxed)
    }

    fn alloc_handle(&self) -> TickStreamHandle {
        TickStreamHandle(self.next_handle.fetch_add(1, Ordering::Relaxed) as u64)
    }

    fn map_native_err(e: NativeError) -> BrokerError {
        let msg = e.to_string();
        match e {
            NativeError::Lost(_) => BrokerError::ConnectionLost(msg),
            NativeError::NotConnected => BrokerError::NotConnected(msg),
            NativeError::ServerVersionTooLow(_, _)
            | NativeError::Protocol(_)
            | NativeError::Malformed(_) => BrokerError::Protocol {
                code: None,
                message: msg,
            },
            NativeError::HandshakeTimeout | NativeError::Io(_) => {
                BrokerError::ConnectionLost(msg)
            }
        }
    }

    fn spawn_dispatcher(&self, mut rx: mpsc::UnboundedReceiver<Vec<String>>) {
        let state = Arc::clone(&self.state);
        let channels = self.channels.clone();
        let connected = Arc::clone(&self.connected);
        tokio::spawn(async move {
            while let Some(fields) = rx.recv().await {
                let parsed = match crate::parse_inbound(&fields) {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("native broker: parse error: {e}");
                        continue;
                    }
                };
                Self::route(&state, &channels, parsed).await;
            }
            connected.store(false, Ordering::SeqCst);
            let _ = channels.connection.send(ConnectionEvent {
                state: ConnectionState::LostConnection,
                timestamp_ns: now_ns(),
                reason: Some("recv channel closed".into()),
            });
        });
    }

    async fn route(
        state: &Arc<Mutex<BrokerState>>,
        channels: &BrokerChannels,
        msg: InboundMsg,
    ) {
        match msg {
            InboundMsg::Position(p) => {
                if let Some(pos) = native_to_position(&p) {
                    let mut s = state.lock().await;
                    let key = (p.account.clone(), p.contract.con_id);
                    s.positions.insert(key, pos);
                }
            }
            InboundMsg::AccountValue(a) => {
                let mut s = state.lock().await;
                let key = (a.key.clone(), a.currency.clone(), a.account.clone());
                s.account_values.insert(key, a);
            }
            InboundMsg::OpenOrder(o) => {
                if let Some(open) = native_to_open_order(&o) {
                    let mut s = state.lock().await;
                    s.open_orders.insert(o.order_id, open);
                }
            }
            InboundMsg::OrderStatus(o) => {
                {
                    let mut s = state.lock().await;
                    if let Some(open) = s.open_orders.get_mut(&o.order_id) {
                        open.status = parse_status(&o.status);
                        open.filled_qty = o.filled as u32;
                    }
                }
                let _ = channels.status.send(OrderStatusUpdate {
                    order_id: OrderId(o.order_id as u64),
                    status: parse_status(&o.status),
                    filled_qty: o.filled as u32,
                    remaining_qty: o.remaining as u32,
                    avg_fill_price: o.avg_fill_price,
                    last_fill_price: if o.last_fill_price > 0.0 {
                        Some(o.last_fill_price)
                    } else {
                        None
                    },
                    timestamp_ns: now_ns(),
                    reject_reason: None,
                });
            }
            InboundMsg::Execution(e) => {
                if let Some(fill) = execution_to_fill(&e) {
                    // If req_id > 0, this is a response to reqExecutions.
                    // Otherwise, it's a real-time exec from execDetailsEvent.
                    if e.req_id > 0 {
                        let mut s = state.lock().await;
                        if let Some(p) = s.pending_executions.get_mut(&e.req_id) {
                            p.accumulated.push(fill);
                            return;
                        }
                    }
                    let _ = channels.fills.send(fill);
                }
            }
            InboundMsg::ExecutionEnd(req_id) => {
                let mut s = state.lock().await;
                if let Some(mut p) = s.pending_executions.remove(&req_id) {
                    if let Some(sender) = p.sender.take() {
                        let _ = sender.send(Ok(std::mem::take(&mut p.accumulated)));
                    }
                }
            }
            InboundMsg::ContractDetails(cd) => {
                let mut s = state.lock().await;
                if let Some(p) = s.pending_contract_details.get_mut(&cd.req_id) {
                    p.accumulated.push(cd);
                }
            }
            InboundMsg::ContractDetailsEnd(req_id) => {
                let mut s = state.lock().await;
                if let Some(mut p) = s.pending_contract_details.remove(&req_id) {
                    let contracts: Vec<Contract> =
                        p.accumulated.iter().filter_map(native_to_contract).collect();
                    if let Some(sender) = p.sender.take() {
                        let _ = sender.send(Ok(contracts));
                    }
                }
            }
            InboundMsg::TickPrice(t) => {
                let s = state.lock().await;
                if let Some(iid) = s.tick_routes.get(&t.req_id) {
                    let kind = match t.tick_type {
                        1 => Some(TickKind::Bid),
                        2 => Some(TickKind::Ask),
                        4 => Some(TickKind::Last),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        let _ = channels.ticks.send(Tick {
                            instrument_id: *iid,
                            kind,
                            price: Some(t.price),
                            size: None,
                            timestamp_ns: now_ns(),
                        });
                    }
                }
            }
            InboundMsg::TickSize(t) => {
                let s = state.lock().await;
                if let Some(iid) = s.tick_routes.get(&t.req_id) {
                    let kind = match t.tick_type {
                        0 => Some(TickKind::BidSize),
                        3 => Some(TickKind::AskSize),
                        8 => Some(TickKind::Volume),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        let _ = channels.ticks.send(Tick {
                            instrument_id: *iid,
                            kind,
                            price: None,
                            size: Some(t.size as u64),
                            timestamp_ns: now_ns(),
                        });
                    }
                }
            }
            InboundMsg::Error(e) => {
                let be = error_to_broker_error(&e);
                if e.req_id > 0 && is_contract_error(e.error_code) {
                    let mut s = state.lock().await;
                    if let Some(mut p) = s.pending_contract_details.remove(&e.req_id) {
                        if let Some(sender) = p.sender.take() {
                            let _ = sender.send(Err(BrokerError::ContractNotFound(
                                e.error_string.clone(),
                            )));
                        }
                    }
                }
                let _ = channels.errors.send(be);
            }
            _ => {}
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────

fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn parse_status(s: &str) -> OrderStatus {
    match s {
        "PendingSubmit" | "ApiPending" | "PreSubmitted" => OrderStatus::PendingSubmit,
        "Submitted" => OrderStatus::Submitted,
        "Filled" => OrderStatus::Filled,
        "Cancelled" | "ApiCancelled" => OrderStatus::Cancelled,
        "PendingCancel" => OrderStatus::PendingCancel,
        "Inactive" => OrderStatus::Inactive,
        _ => OrderStatus::Inactive,
    }
}

fn is_contract_error(code: i32) -> bool {
    matches!(code, 200 | 321 | 322)
}

fn error_to_broker_error(e: &ErrorMsg) -> BrokerError {
    match e.error_code {
        1100 | 1101 | 1102 | 1300 => BrokerError::ConnectionLost(e.error_string.clone()),
        100 => BrokerError::RateLimited(e.error_string.clone()),
        110 | 200 | 321 | 322 => BrokerError::ContractNotFound(e.error_string.clone()),
        _ => BrokerError::Protocol {
            code: Some(e.error_code),
            message: e.error_string.clone(),
        },
    }
}

fn native_to_contract(cd: &ContractDetailsMsg) -> Option<Contract> {
    let expiry = NaiveDate::parse_from_str(&cd.contract.last_trade_date, "%Y%m%d")
        .or_else(|_| NaiveDate::parse_from_str(&cd.contract.last_trade_date, "%Y%m"))
        .unwrap_or_else(|_| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
    Some(Contract {
        instrument_id: InstrumentId(cd.contract.con_id as u64),
        kind: parse_kind(&cd.contract.sec_type)?,
        symbol: cd.contract.symbol.clone(),
        local_symbol: cd.contract.local_symbol.clone(),
        expiry,
        strike: if cd.contract.strike > 0.0 {
            Some(cd.contract.strike)
        } else {
            None
        },
        right: parse_right(&cd.contract.right),
        multiplier: cd.contract.multiplier.parse().unwrap_or(0.0),
        exchange: parse_exchange(&cd.contract.exchange),
        currency: parse_currency(&cd.contract.currency),
    })
}

fn native_to_position(p: &PositionMsg) -> Option<Position> {
    Some(Position {
        contract: Contract {
            instrument_id: InstrumentId(p.contract.con_id as u64),
            kind: parse_kind(&p.contract.sec_type)?,
            symbol: p.contract.symbol.clone(),
            local_symbol: p.contract.local_symbol.clone(),
            expiry: NaiveDate::parse_from_str(&p.contract.last_trade_date, "%Y%m%d")
                .unwrap_or_else(|_| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
            strike: if p.contract.strike > 0.0 {
                Some(p.contract.strike)
            } else {
                None
            },
            right: parse_right(&p.contract.right),
            multiplier: p.contract.multiplier.parse().unwrap_or(0.0),
            exchange: parse_exchange(&p.contract.exchange),
            currency: parse_currency(&p.contract.currency),
        },
        quantity: p.position as i32,
        avg_cost: p.avg_cost,
        realized_pnl: 0.0,
        unrealized_pnl: 0.0,
    })
}

fn native_to_open_order(o: &OpenOrderMsg) -> Option<OpenOrder> {
    let kind = parse_kind(&o.contract.sec_type)?;
    let contract = Contract {
        instrument_id: InstrumentId(o.contract.con_id as u64),
        kind,
        symbol: o.contract.symbol.clone(),
        local_symbol: o.contract.local_symbol.clone(),
        expiry: NaiveDate::parse_from_str(&o.contract.last_trade_date, "%Y%m%d")
            .unwrap_or_else(|_| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
        strike: if o.contract.strike > 0.0 {
            Some(o.contract.strike)
        } else {
            None
        },
        right: parse_right(&o.contract.right),
        multiplier: o.contract.multiplier.parse().unwrap_or(0.0),
        exchange: parse_exchange(&o.contract.exchange),
        currency: parse_currency(&o.contract.currency),
    };
    Some(OpenOrder {
        order_id: OrderId(o.order_id as u64),
        contract,
        side: parse_side(&o.action)?,
        qty: o.total_quantity as u32,
        order_type: parse_order_type(&o.order_type, 0.0),
        price: if o.lmt_price.is_finite() && o.lmt_price > 0.0 {
            Some(o.lmt_price)
        } else {
            None
        },
        tif: parse_tif(&o.tif),
        gtd_until_utc: None,
        client_order_ref: o.order_ref.clone(),
        status: OrderStatus::PendingSubmit,
        filled_qty: 0,
        remaining_qty: o.remaining as u32,
    })
}

fn parse_side(s: &str) -> Option<Side> {
    match s {
        "BUY" => Some(Side::Buy),
        "SELL" => Some(Side::Sell),
        _ => None,
    }
}

fn parse_order_type(s: &str, aux: f64) -> OrderType {
    match s {
        "MKT" => OrderType::Market,
        "LMT" => OrderType::Limit,
        "STP LMT" => OrderType::StopLimit { stop_price: aux },
        _ => OrderType::Limit,
    }
}

fn parse_tif(s: &str) -> TimeInForce {
    match s {
        "GTC" => TimeInForce::Gtc,
        "DAY" => TimeInForce::Day,
        "GTD" => TimeInForce::Gtd,
        "IOC" => TimeInForce::Ioc,
        "FOK" => TimeInForce::Fok,
        _ => TimeInForce::Day,
    }
}

fn parse_right(s: &str) -> Option<Right> {
    match s {
        "C" | "CALL" => Some(Right::Call),
        "P" | "PUT" => Some(Right::Put),
        _ => None,
    }
}

fn parse_kind(s: &str) -> Option<ContractKind> {
    match s {
        "FUT" => Some(ContractKind::Future),
        "OPT" | "FOP" => Some(ContractKind::Option),
        _ => None,
    }
}

fn parse_exchange(s: &str) -> Exchange {
    match s {
        "COMEX" => Exchange::Comex,
        "NYMEX" => Exchange::Nymex,
        "CBOT" => Exchange::Cbot,
        "CME" => Exchange::Cme,
        _ => Exchange::Other(0),
    }
}

fn parse_currency(s: &str) -> Currency {
    match s {
        "USD" => Currency::Usd,
        "EUR" => Currency::Eur,
        "GBP" => Currency::Gbp,
        "JPY" => Currency::Jpy,
        _ => Currency::Usd,
    }
}

fn execution_to_fill(e: &ExecutionMsg) -> Option<Fill> {
    Some(Fill {
        instrument_id: InstrumentId(e.contract.con_id as u64),
        order_id: OrderId(e.order_id as u64),
        exec_id: e.exec_id.clone(),
        side: if e.side == "BOT" {
            Side::Buy
        } else if e.side == "SLD" {
            Side::Sell
        } else {
            return None;
        },
        qty: e.shares as u32,
        price: e.price,
        timestamp_ns: now_ns(),
        commission: None,
    })
}

fn exchange_to_str(e: Exchange) -> &'static str {
    match e {
        Exchange::Comex => "COMEX",
        Exchange::Nymex => "NYMEX",
        Exchange::Cbot => "CBOT",
        Exchange::Cme => "CME",
        Exchange::Other(_) => "",
    }
}

fn currency_to_str(c: Currency) -> &'static str {
    match c {
        Currency::Usd => "USD",
        Currency::Eur => "EUR",
        Currency::Gbp => "GBP",
        Currency::Jpy => "JPY",
    }
}

fn place_order_contract_request(c: &Contract) -> ContractRequest {
    let sec_type = match c.kind {
        ContractKind::Future => "FUT",
        ContractKind::Option => "FOP",
    };
    let right = c.right.map(|r| match r {
        Right::Call => "C",
        Right::Put => "P",
    });
    ContractRequest {
        con_id: c.instrument_id.0 as i64,
        symbol: c.symbol.clone(),
        sec_type: sec_type.into(),
        last_trade_date: c.expiry.format("%Y%m%d").to_string(),
        strike: c.strike.unwrap_or(0.0),
        right: right.unwrap_or("").into(),
        multiplier: format!("{}", c.multiplier as i64),
        exchange: exchange_to_str(c.exchange).into(),
        primary_exchange: String::new(),
        currency: currency_to_str(c.currency).into(),
        local_symbol: c.local_symbol.clone(),
        trading_class: String::new(),
    }
}

fn build_place_params(
    req: &PlaceOrderReq,
    fallback_account: &str,
) -> Result<PlaceOrderParams, BrokerError> {
    let action = match req.side {
        Side::Buy => "BUY".to_string(),
        Side::Sell => "SELL".to_string(),
    };
    let (order_type, lmt_price, aux_price) = match req.order_type {
        OrderType::Limit => {
            let price = req.price.ok_or_else(|| {
                BrokerError::InvalidRequest("limit order requires price".into())
            })?;
            ("LMT".to_string(), price, 0.0)
        }
        OrderType::Market => ("MKT".to_string(), 0.0, 0.0),
        OrderType::StopLimit { stop_price } => {
            let price = req.price.ok_or_else(|| {
                BrokerError::InvalidRequest("stop-limit requires limit price".into())
            })?;
            ("STP LMT".to_string(), price, stop_price)
        }
    };
    let tif = match req.tif {
        TimeInForce::Gtc => "GTC",
        TimeInForce::Day => "DAY",
        TimeInForce::Gtd => "GTD",
        TimeInForce::Ioc => "IOC",
        TimeInForce::Fok => "FOK",
    }
    .to_string();
    let good_till_date = match (req.tif, req.gtd_until_utc) {
        (TimeInForce::Gtd, Some(t)) => t.format("%Y%m%d %H:%M:%S UTC").to_string(),
        (TimeInForce::Gtd, None) => {
            return Err(BrokerError::InvalidRequest(
                "Gtd TIF requires gtd_until_utc".into(),
            ))
        }
        _ => String::new(),
    };
    let account = req
        .account
        .clone()
        .unwrap_or_else(|| fallback_account.to_string());
    Ok(PlaceOrderParams {
        action,
        total_quantity: req.qty as f64,
        order_type,
        lmt_price,
        aux_price,
        tif,
        good_till_date,
        account,
        order_ref: req.client_order_ref.clone(),
        outside_rth: false,
    })
}

fn empty_contract_request(symbol: &str, sec_type: &str) -> ContractRequest {
    ContractRequest {
        con_id: 0,
        symbol: symbol.into(),
        sec_type: sec_type.into(),
        last_trade_date: String::new(),
        strike: 0.0,
        right: String::new(),
        multiplier: String::new(),
        exchange: String::new(),
        primary_exchange: String::new(),
        currency: String::new(),
        local_symbol: String::new(),
        trading_class: String::new(),
    }
}

// ── Broker trait impl ─────────────────────────────────────────────────

#[async_trait]
impl Broker for NativeBroker {
    async fn connect(&mut self) -> BResult<()> {
        if self.connected.load(Ordering::SeqCst) {
            return Ok(());
        }

        self.client.connect().await.map_err(Self::map_native_err)?;
        self.client
            .wait_for_bootstrap(Duration::from_secs(10))
            .await
            .map_err(Self::map_native_err)?;

        let rx = self
            .rx_holder
            .lock()
            .await
            .take()
            .ok_or_else(|| BrokerError::Internal("rx already taken".into()))?;
        self.spawn_dispatcher(rx);

        for frame in [
            req_account_updates(true, &self.cfg.account),
            req_positions(),
            req_open_orders(),
        ] {
            self.client
                .send_raw(&frame)
                .await
                .map_err(Self::map_native_err)?;
        }

        self.connected.store(true, Ordering::SeqCst);
        let _ = self.channels.connection.send(ConnectionEvent {
            state: ConnectionState::Connected,
            timestamp_ns: now_ns(),
            reason: None,
        });

        Ok(())
    }

    async fn disconnect(&mut self) -> BResult<()> {
        self.connected.store(false, Ordering::SeqCst);
        self.client
            .disconnect()
            .await
            .map_err(Self::map_native_err)?;
        let _ = self.channels.connection.send(ConnectionEvent {
            state: ConnectionState::Closed,
            timestamp_ns: now_ns(),
            reason: Some("disconnect requested".into()),
        });
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    async fn place_order(&self, req: PlaceOrderReq) -> BResult<OrderId> {
        if req.qty == 0 {
            return Err(BrokerError::InvalidRequest("qty must be > 0".into()));
        }
        let order_id = self.client.alloc_order_id().await;
        if order_id <= 0 {
            return Err(BrokerError::Internal(
                "next_valid_id not seeded — call wait_for_bootstrap".into(),
            ));
        }
        let cr = place_order_contract_request(&req.contract);
        let params = build_place_params(&req, &self.cfg.account)?;
        let frame = place_order(order_id, &cr, &params);
        self.client
            .send_raw(&frame)
            .await
            .map_err(Self::map_native_err)?;
        Ok(OrderId(order_id as u64))
    }

    async fn cancel_order(&self, id: OrderId) -> BResult<()> {
        let frame = cancel_order(id.0 as i32);
        self.client
            .send_raw(&frame)
            .await
            .map_err(Self::map_native_err)
    }

    async fn modify_order(&self, id: OrderId, req: ModifyOrderReq) -> BResult<()> {
        // IBKR's modify is "place_order with the same orderId". We need
        // the original contract + immutable fields; pull them from the
        // open order cache.
        let existing = {
            let s = self.state.lock().await;
            s.open_orders.get(&(id.0 as i32)).cloned()
        };
        let existing = existing.ok_or_else(|| {
            BrokerError::Internal(format!("modify_order: orderId={} not in open cache", id.0))
        })?;

        let cr = place_order_contract_request(&existing.contract);
        let placeholder = PlaceOrderReq {
            contract: existing.contract.clone(),
            side: existing.side,
            qty: req.qty.unwrap_or(existing.qty),
            order_type: existing.order_type,
            price: req.price.or(existing.price),
            tif: existing.tif,
            gtd_until_utc: req.gtd_until_utc.or(existing.gtd_until_utc),
            client_order_ref: existing.client_order_ref.clone(),
            account: Some(self.cfg.account.clone()),
        };
        let params = build_place_params(&placeholder, &self.cfg.account)?;
        let frame = place_order(id.0 as i32, &cr, &params);
        self.client
            .send_raw(&frame)
            .await
            .map_err(Self::map_native_err)
    }

    async fn positions(&self) -> BResult<Vec<Position>> {
        let s = self.state.lock().await;
        Ok(s.positions.values().cloned().collect())
    }

    async fn account_values(&self) -> BResult<AccountSnapshot> {
        let s = self.state.lock().await;
        let mut snap = AccountSnapshot {
            net_liquidation: 0.0,
            maintenance_margin: 0.0,
            initial_margin: 0.0,
            buying_power: 0.0,
            realized_pnl_today: 0.0,
            timestamp_ns: now_ns(),
        };
        for ((key, _ccy, _acct), v) in s.account_values.iter() {
            let parsed: f64 = v.value.parse().unwrap_or(0.0);
            match key.as_str() {
                "NetLiquidation" => snap.net_liquidation = parsed,
                "MaintMarginReq" => snap.maintenance_margin = parsed,
                "InitMarginReq" => snap.initial_margin = parsed,
                "BuyingPower" => snap.buying_power = parsed,
                "RealizedPnL" => snap.realized_pnl_today = parsed,
                _ => {}
            }
        }
        Ok(snap)
    }

    async fn open_orders(&self) -> BResult<Vec<OpenOrder>> {
        let s = self.state.lock().await;
        Ok(s.open_orders.values().cloned().collect())
    }

    async fn recent_fills(&self, since_ns: u64) -> BResult<Vec<Fill>> {
        let req_id = self.alloc_req_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.state.lock().await;
            s.pending_executions.insert(
                req_id,
                PendingExecutionsRequest {
                    accumulated: Vec::new(),
                    sender: Some(tx),
                },
            );
        }
        let time = if since_ns == 0 {
            String::new()
        } else {
            // ExecutionFilter expects "yyyymmdd-hh:mm:ss" UTC.
            let secs = (since_ns / 1_000_000_000) as i64;
            chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
                .map(|dt| dt.format("%Y%m%d-%H:%M:%S").to_string())
                .unwrap_or_default()
        };
        let f = ExecutionFilter {
            client_id: 0,
            account: self.cfg.account.clone(),
            time,
            symbol: String::new(),
            sec_type: String::new(),
            exchange: String::new(),
            side: String::new(),
        };
        self.client
            .send_raw(&req_executions(req_id, &f))
            .await
            .map_err(Self::map_native_err)?;

        match tokio::time::timeout(Duration::from_secs(15), rx).await {
            Ok(Ok(Ok(fills))) => Ok(fills),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(BrokerError::Internal("recent_fills channel dropped".into())),
            Err(_) => {
                let mut s = self.state.lock().await;
                s.pending_executions.remove(&req_id);
                Err(BrokerError::Internal("recent_fills timeout".into()))
            }
        }
    }

    async fn qualify_future(&self, q: FutureQuery) -> BResult<Contract> {
        let req_id = self.alloc_req_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.state.lock().await;
            s.pending_contract_details.insert(
                req_id,
                PendingContractRequest {
                    accumulated: Vec::new(),
                    sender: Some(tx),
                },
            );
        }
        let mut cr = empty_contract_request(&q.symbol, "FUT");
        cr.last_trade_date = q.expiry.format("%Y%m%d").to_string();
        cr.exchange = exchange_to_str(q.exchange).into();
        cr.currency = currency_to_str(q.currency).into();

        self.client
            .send_raw(&req_contract_details(req_id, &cr))
            .await
            .map_err(Self::map_native_err)?;

        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(Ok(mut contracts))) if !contracts.is_empty() => {
                Ok(contracts.swap_remove(0))
            }
            Ok(Ok(Ok(_))) => Err(BrokerError::ContractNotFound(format!(
                "no match for {} {}",
                q.symbol, q.expiry
            ))),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => {
                Err(BrokerError::Internal("qualify_future channel dropped".into()))
            }
            Err(_) => {
                let mut s = self.state.lock().await;
                s.pending_contract_details.remove(&req_id);
                Err(BrokerError::Internal("qualify_future timeout".into()))
            }
        }
    }

    async fn qualify_option(&self, q: OptionQuery) -> BResult<Contract> {
        let req_id = self.alloc_req_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.state.lock().await;
            s.pending_contract_details.insert(
                req_id,
                PendingContractRequest {
                    accumulated: Vec::new(),
                    sender: Some(tx),
                },
            );
        }
        let mut cr = empty_contract_request(&q.symbol, "FOP");
        cr.last_trade_date = q.expiry.format("%Y%m%d").to_string();
        cr.strike = q.strike;
        cr.right = match q.right {
            Right::Call => "C".into(),
            Right::Put => "P".into(),
        };
        cr.multiplier = format!("{}", q.multiplier as i64);
        cr.exchange = exchange_to_str(q.exchange).into();
        cr.currency = currency_to_str(q.currency).into();

        self.client
            .send_raw(&req_contract_details(req_id, &cr))
            .await
            .map_err(Self::map_native_err)?;

        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(Ok(mut contracts))) if !contracts.is_empty() => {
                Ok(contracts.swap_remove(0))
            }
            Ok(Ok(Ok(_))) => Err(BrokerError::ContractNotFound(format!(
                "no match for {} {} {} {:?}",
                q.symbol, q.expiry, q.strike, q.right
            ))),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => {
                Err(BrokerError::Internal("qualify_option channel dropped".into()))
            }
            Err(_) => {
                let mut s = self.state.lock().await;
                s.pending_contract_details.remove(&req_id);
                Err(BrokerError::Internal("qualify_option timeout".into()))
            }
        }
    }

    async fn list_chain(&self, q: ChainQuery) -> BResult<Vec<Contract>> {
        let req_id = self.alloc_req_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.state.lock().await;
            s.pending_contract_details.insert(
                req_id,
                PendingContractRequest {
                    accumulated: Vec::new(),
                    sender: Some(tx),
                },
            );
        }
        let sec_type = match q.kind {
            Some(ContractKind::Future) => "FUT",
            Some(ContractKind::Option) => "FOP",
            _ => "FUT",
        };
        let mut cr = empty_contract_request(&q.symbol, sec_type);
        cr.exchange = exchange_to_str(q.exchange).into();
        cr.currency = currency_to_str(q.currency).into();

        self.client
            .send_raw(&req_contract_details(req_id, &cr))
            .await
            .map_err(Self::map_native_err)?;

        match tokio::time::timeout(Duration::from_secs(15), rx).await {
            Ok(Ok(Ok(mut contracts))) => {
                if let Some(min_expiry) = q.min_expiry {
                    contracts.retain(|c| c.expiry >= min_expiry);
                }
                Ok(contracts)
            }
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(BrokerError::Internal("list_chain channel dropped".into())),
            Err(_) => {
                let mut s = self.state.lock().await;
                s.pending_contract_details.remove(&req_id);
                Err(BrokerError::Internal("list_chain timeout".into()))
            }
        }
    }

    async fn subscribe_ticks(&self, sub: TickSubscription) -> BResult<TickStreamHandle> {
        let req_id = self.alloc_req_id();
        let handle = self.alloc_handle();
        {
            let mut s = self.state.lock().await;
            s.tick_routes.insert(req_id, sub.instrument_id);
            s.handle_to_req_id.insert(handle, req_id);
        }
        // Phase 6.6: subscribe by conId only. The native client encodes
        // the contract via reqMktData which expects the full contract
        // descriptor — for round-trip subscription you must qualify first
        // so the InstrumentId is a real conId.
        let mut cr = empty_contract_request("", "");
        cr.con_id = sub.instrument_id.0 as i64;
        let frame = req_mkt_data(req_id, &cr, "", false, false);
        self.client
            .send_raw(&frame)
            .await
            .map_err(Self::map_native_err)?;
        Ok(handle)
    }

    async fn unsubscribe_ticks(&self, h: TickStreamHandle) -> BResult<()> {
        let req_id = {
            let mut s = self.state.lock().await;
            let req_id = s.handle_to_req_id.remove(&h);
            if let Some(rid) = req_id {
                s.tick_routes.remove(&rid);
            }
            req_id
        };
        if let Some(rid) = req_id {
            let frame = cancel_mkt_data(rid);
            self.client
                .send_raw(&frame)
                .await
                .map_err(Self::map_native_err)?;
        }
        Ok(())
    }

    fn subscribe_fills(&self) -> broadcast::Receiver<Fill> {
        self.channels.fills.subscribe()
    }

    fn subscribe_order_status(&self) -> broadcast::Receiver<OrderStatusUpdate> {
        self.channels.status.subscribe()
    }

    fn subscribe_ticks_stream(&self) -> broadcast::Receiver<Tick> {
        self.channels.ticks.subscribe()
    }

    fn subscribe_errors(&self) -> broadcast::Receiver<BrokerError> {
        self.channels.errors.subscribe()
    }

    fn subscribe_connection(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.channels.connection.subscribe()
    }

    fn capabilities(&self) -> &BrokerCapabilities {
        &self.capabilities
    }
}
