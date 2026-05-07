//! `NativeBroker` — `corsair_broker_api::Broker` impl over `NativeClient`.
//!
//! Direct-wire IBKR adapter. Embedded by the `corsair_broker` daemon
//! as its production `Broker` implementation. The PyO3 + ib_insync
//! bridge it replaced (`corsair_broker_ibkr`) was retired during the
//! Phase 6.7 cutover and the crate has since been deleted.
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
use parking_lot::Mutex as PMutex;
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
    cancel_mkt_data, cancel_mkt_depth, cancel_order, req_account_updates,
    req_contract_details, req_executions, req_mkt_data, req_mkt_depth, req_open_orders,
    req_positions, ContractRequest,
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

    /// Pending place_order / modify_order acks, keyed by orderId.
    ///
    /// Place path: resolved on the first OpenOrder OR OrderStatus
    /// (Submitted/PendingSubmit/Filled/Cancelled/PendingCancel) for
    /// this orderId, OR an Error with req_id == orderId. Lets the
    /// caller detect IBKR rejects synchronously instead of returning
    /// a phantom OrderId (P0-4 + CLAUDE.md §14 hedge_qty trust).
    ///
    /// Amend path (`expected_amend_price = Some`): resolved ONLY by
    /// `OpenOrder` whose `lmt_price` matches the requested price.
    /// PreSubmitted/PendingSubmit comes back from the local IB
    /// Gateway before IBKR's server has actually processed the
    /// amend, so resolving on it under-reports amend RTT (~hundreds
    /// of µs vs the real ~tens of ms). Matching the price-out vs
    /// price-back means we resolve only after IBKR's server has
    /// updated its book to the new price.
    pending_place_acks: HashMap<i32, PendingAck>,

    /// v2 wire-timing — broker-internal precise timestamps per orderId.
    /// `place_order` populates `send_ns` immediately before client.send_raw;
    /// the dispatcher (route) populates `ack_ns` on OpenOrder/OrderStatus
    /// removal of pending_place_acks. handle_place reads + drains after
    /// place_order returns. Replaces the call-boundary "marker" timestamps
    /// that included ~30 µs of place_order setup overhead.
    pub wire_send_ns: HashMap<i32, u64>,
    pub wire_ack_ns: HashMap<i32, u64>,

    /// HONEST amend ack timestamp: stamped only when the dispatcher
    /// observes an OpenOrder whose `lmt_price` matches the amend's
    /// requested price. Captures the moment IBKR's server has
    /// applied the amend, not the IB Gateway's PreSubmitted echo
    /// (which lands in microseconds and gives a phantom-fast amend
    /// RTT). The functional ack path (the `tx` on PendingAck)
    /// resolves permissively on any accept-state signal so the
    /// trader never timeouts; this map is consumed independently
    /// via `drain_strict_amend_timing` for dashboard reporting.
    pub strict_ack_ns: HashMap<i32, u64>,

    /// Expected amend price keyed by orderId, kept separate from
    /// PendingAck so the dispatcher can stamp `strict_ack_ns` even
    /// after the permissive resolution has removed the PendingAck.
    /// Cleared by `drain_strict_amend_timing` after handle_modify
    /// reads the strict ack timestamp.
    pub expected_amend_prices: HashMap<i32, f64>,

    /// Cached place_order contract templates keyed by InstrumentId.
    /// Avoids re-encoding the 14 contract-fixed fields on every
    /// place_order. See `place_template::ContractTemplate`. Volume
    /// of distinct contracts is bounded (~80 entries for HG) so we
    /// don't bother evicting.
    place_templates: HashMap<InstrumentId, crate::place_template::ContractTemplate>,

    /// reqId → InstrumentId for tick routing.
    tick_routes: HashMap<i32, InstrumentId>,

    /// reqId → option Right for the leg (only populated for option
    /// subscriptions). IBKR sends tick_type 27 (call OI) AND 28 (put
    /// OI) to every option subscription regardless of whether the leg
    /// is a call or a put — for the OPPOSITE side, IBKR sends 0. Without
    /// this map we'd take the wrong side's 0 and overwrite the right
    /// side's value (last-write-wins on a single iid). We use this to
    /// drop the wrong-side updates at decode time. Same applies to
    /// per-side volume (29 / 30).
    tick_route_right: HashMap<i32, Right>,

    /// reqId → InstrumentId for L2 depth routing. Separate from
    /// tick_routes because reqMktDepth uses a different reqId space
    /// per IBKR (and we want fast disambiguation in route()).
    depth_routes: HashMap<i32, InstrumentId>,

    /// handle → reqId for unsubscribe.
    handle_to_req_id: HashMap<TickStreamHandle, i32>,
    /// handle → reqId for L2 depth unsubscribe.
    handle_to_depth_req_id: HashMap<TickStreamHandle, i32>,

    /// Tracks bootstrap-time seeding.
    seeding: SeedingProgress,

    /// Diagnostic histogram of incoming TickSize tick_type values.
    /// Used by `dump_tick_type_hist` (logged from a periodic task) to
    /// surface routing bugs — e.g. if call OI (tick_type 27) never
    /// appears in this map but put OI (28) does, our subscription or
    /// the gateway's contract permission is the problem, not our
    /// dispatch table.
    pub(crate) tick_type_hist: HashMap<i32, u64>,
}

struct PendingContractRequest {
    accumulated: Vec<ContractDetailsMsg>,
    sender: Option<oneshot::Sender<Result<Vec<Contract>, BrokerError>>>,
}

struct PendingExecutionsRequest {
    accumulated: Vec<Fill>,
    sender: Option<oneshot::Sender<Result<Vec<Fill>, BrokerError>>>,
}

/// Pending place/modify ack. The dispatcher resolves `tx`
/// permissively on any OpenOrder or accept-state OrderStatus for
/// the matching orderId so the trader's modify call never
/// timeouts on amends where IBKR emits something other than a
/// matching-price OpenOrder. The honest amend RTT (where
/// available) is captured separately in `BrokerState::strict_ack_ns`
/// — see the OpenOrder branch of `route()` and
/// `drain_strict_amend_ack_ns()`.
struct PendingAck {
    tx: oneshot::Sender<Result<(), BrokerError>>,
}

/// Floating-point tolerance for amend price match. CME tick sizes for
/// our products are ≥ 0.0005 USD, so an epsilon two orders of magnitude
/// smaller is safely below tick precision while absorbing any rounding
/// the wire format introduces.
const AMEND_PRICE_EPS: f64 = 1e-6;

/// Tracks whether we've seen the initial PositionEnd / OpenOrderEnd /
/// AccountDownloadEnd signals after connect. Lets callers gate
/// `positions()` etc. on initial seeding being complete.
#[derive(Debug, Default, Clone, Copy)]
pub struct SeedingProgress {
    pub positions_done: bool,
    pub open_orders_done: bool,
    pub account_done: bool,
}

#[derive(Clone)]
struct BrokerChannels {
    fills: broadcast::Sender<Fill>,
    status: broadcast::Sender<OrderStatusUpdate>,
    ticks: broadcast::Sender<Tick>,
    depth: broadcast::Sender<corsair_broker_api::events::DepthUpdate>,
    errors: broadcast::Sender<BrokerError>,
    connection: broadcast::Sender<ConnectionEvent>,
}

impl BrokerChannels {
    fn new() -> Self {
        Self {
            fills: broadcast::channel(FILL_CHANNEL_CAP).0,
            status: broadcast::channel(STATUS_CHANNEL_CAP).0,
            ticks: broadcast::channel(TICK_CHANNEL_CAP).0,
            depth: broadcast::channel(TICK_CHANNEL_CAP).0,
            errors: broadcast::channel(ERROR_CHANNEL_CAP).0,
            connection: broadcast::channel(CONNECTION_CHANNEL_CAP).0,
        }
    }
}

/// Fast-path tick publisher closure type. NativeBroker calls this in
/// the dispatcher's TickPrice/TickSize arms to publish ticks
/// directly to a downstream consumer (typically the SHM IPC server's
/// events ring), bypassing the broadcast channel + forward_ticks
/// pump. The closure owns its encoding (msgpack/serde/etc.) so we
/// don't tie the wire-client crate to a transport library.
///
/// The in-process broadcast channel still fires for other consumers
/// (market_data state, etc.) so this is purely an additional write.
pub type TickPublisher = Arc<dyn for<'a> Fn(&'a Tick) + Send + Sync + 'static>;

pub struct NativeBroker {
    cfg: NativeBrokerConfig,
    client: Arc<NativeClient>,
    /// Hot-path state. Sync mutex (parking_lot) — short critical
    /// sections only, never held across .await. Replacing tokio's
    /// async mutex eliminates the per-lock yield-point overhead.
    state: Arc<PMutex<BrokerState>>,
    channels: BrokerChannels,
    capabilities: BrokerCapabilities,
    next_req_id: Arc<AtomicI32>,
    next_handle: Arc<AtomicI32>,
    connected: Arc<AtomicBool>,
    rx_holder: Mutex<Option<mpsc::Receiver<(Vec<String>, u64)>>>,
    /// Optional fast-path tick publisher. When set, the dispatcher
    /// writes tick events directly to the underlying SHM ring AS WELL
    /// AS broadcasting on the in-process channel. corsair_broker
    /// daemon wires this on boot via `set_tick_publisher`.
    /// Sync mutex — set_tick_publisher is the only writer (called
    /// once at boot); dispatcher is the only reader (cheap clone of
    /// the Arc on every event). parking_lot's contention model is
    /// fine for this access pattern.
    tick_publisher: Arc<PMutex<Option<TickPublisher>>>,
}

impl NativeBroker {
    pub fn new(cfg: NativeBrokerConfig) -> Self {
        let (client, rx) = NativeClient::new(cfg.client.clone());
        Self {
            cfg,
            client: Arc::new(client),
            state: Arc::new(PMutex::new(BrokerState::default())),
            channels: BrokerChannels::new(),
            capabilities: BrokerCapabilities::ibkr_default(),
            next_req_id: Arc::new(AtomicI32::new(1000)),
            next_handle: Arc::new(AtomicI32::new(1)),
            connected: Arc::new(AtomicBool::new(false)),
            rx_holder: Mutex::new(Some(rx)),
            tick_publisher: Arc::new(PMutex::new(None)),
        }
    }

    /// Wire a fast-path tick publisher. The closure runs synchronously
    /// inside the dispatcher's TickPrice/TickSize handlers. It MUST
    /// be cheap (target: <5 µs). corsair_broker daemon installs this
    /// on boot to forward ticks directly to the SHM events ring.
    ///
    /// Once set, the in-process broadcast channel still fires for
    /// other consumers (market_data state) — fast-path is additive.
    pub async fn set_tick_publisher(&self, publisher: TickPublisher) {
        *self.tick_publisher.lock() = Some(publisher);
    }

    /// Snapshot the current bootstrap-seeding progress.
    pub async fn seeding_progress(&self) -> SeedingProgress {
        self.state.lock().seeding
    }

    /// Reset seeding flags before issuing a re-snapshot reqXxx pair.
    /// Without this, a second `wait_for_seeding` returns immediately
    /// based on the stale `done=true` from the prior bootstrap.
    pub async fn reset_seeding_flags(&self) {
        let mut s = self.state.lock();
        s.seeding = SeedingProgress::default();
    }

    /// Wait until positions, open orders, and account values have all
    /// streamed their "End" signals — meaning the initial snapshot is
    /// complete. Returns Ok even if the timeout elapses (caller
    /// inspects `seeding_progress()` to decide what to do).
    ///
    /// Critical for clean cutover: don't seed PortfolioState until
    /// positions_done, otherwise a partial snapshot can mask short
    /// inventory.
    pub async fn wait_for_seeding(&self, timeout: Duration) -> SeedingProgress {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let p = self.state.lock().seeding;
            if p.positions_done && p.open_orders_done && p.account_done {
                return p;
            }
            if tokio::time::Instant::now() >= deadline {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
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

    fn spawn_dispatcher(&self, mut rx: mpsc::Receiver<(Vec<String>, u64)>) {
        let state = Arc::clone(&self.state);
        let channels = self.channels.clone();
        let connected = Arc::clone(&self.connected);
        let tick_publisher_arc = Arc::clone(&self.tick_publisher);
        tokio::spawn(async move {
            while let Some((fields, recv_ns)) = rx.recv().await {
                // v2 wire-timing — recv_ns now comes from the recv
                // task's SCM_TIMESTAMPNS cmsg (kernel RX ingress on
                // the gateway socket). Falls back to user-space
                // now_ns() if SO_TIMESTAMPNS wasn't enabled.
                let parsed = match crate::parse_inbound(&fields) {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("native broker: parse error: {e}");
                        continue;
                    }
                };
                // Snapshot the tick publisher Arc (cheap clone).
                // None until `set_tick_publisher` is called by the
                // corsair_broker daemon during boot.
                let tp = tick_publisher_arc.lock().clone();
                Self::route(&state, &channels, parsed, tp.as_deref(), recv_ns);
            }
            connected.store(false, Ordering::SeqCst);
            let _ = channels.connection.send(ConnectionEvent {
                state: ConnectionState::LostConnection,
                timestamp_ns: now_ns(),
                reason: Some("recv channel closed".into()),
            });
        });
    }

    /// Route a single parsed inbound message. Sync (parking_lot
    /// state, no awaits inside). The dispatcher task awaits the
    /// recv channel; route itself is straight-line.
    ///
    /// `recv_ns` is the broker-edge wall-clock at the moment the
    /// frame was pulled from the recv channel — the closest broker-
    /// internal proxy for "this tick arrived on the wire" (within
    /// ~5 µs of TCP recv). Used to stamp Tick.timestamp_ns for
    /// downstream consumers; the v2 wire_timing JSONL pulls it from
    /// there via TickEvent.broker_recv_ns.
    fn route(
        state: &Arc<PMutex<BrokerState>>,
        channels: &BrokerChannels,
        msg: InboundMsg,
        tick_publisher: Option<&(dyn Fn(&Tick) + Send + Sync)>,
        recv_ns: u64,
    ) {
        match msg {
            InboundMsg::Position(p) => {
                let key = (p.account.clone(), p.contract.con_id);
                if p.position == 0.0 {
                    // Closed position — IBKR sends qty=0 to indicate
                    // close. Don't leave a stale zero entry in the
                    // cache (would inflate positions().len() and
                    // confuse downstream seeding).
                    let mut s = state.lock();
                    s.positions.remove(&key);
                } else if let Some(pos) = native_to_position(&p) {
                    let mut s = state.lock();
                    s.positions.insert(key, pos);
                }
            }
            InboundMsg::PositionEnd => {
                let mut s = state.lock();
                s.seeding.positions_done = true;
            }
            InboundMsg::OpenOrderEnd => {
                let mut s = state.lock();
                s.seeding.open_orders_done = true;
            }
            InboundMsg::AccountDownloadEnd(_) => {
                let mut s = state.lock();
                s.seeding.account_done = true;
            }
            InboundMsg::AccountValue(a) => {
                let mut s = state.lock();
                let key = (a.key.clone(), a.currency.clone(), a.account.clone());
                s.account_values.insert(key, a);
            }
            InboundMsg::OpenOrder(o) => {
                let order_id = o.order_id;
                let lmt_price = o.lmt_price;
                let parsed = native_to_open_order(&o);
                let ack = {
                    let mut s = state.lock();
                    if let Some(open) = parsed {
                        s.open_orders.insert(order_id, open);
                    }
                    let now = now_ns();
                    // Always resolve permissively (place + amend) so
                    // the trader's modify call never timeouts on
                    // amends where IBKR doesn't emit a matching-price
                    // OpenOrder. The "honest" amend ack is observed
                    // separately below.
                    s.wire_ack_ns.entry(order_id).or_insert(now);
                    // Honest amend RTT instrumentation: if this
                    // OpenOrder has a matching expected price, stamp
                    // strict_ack_ns. Lives outside PendingAck (in
                    // `expected_amend_prices`) so we can still stamp
                    // even after the permissive resolver has removed
                    // the PendingAck on an earlier non-matching
                    // OpenOrder. Drained by handle_modify post-ack
                    // to compute the dashboard's amend metric
                    // independent of the resolution path.
                    if let Some(&expected) = s.expected_amend_prices.get(&order_id) {
                        if (lmt_price - expected).abs() < AMEND_PRICE_EPS {
                            s.strict_ack_ns.entry(order_id).or_insert(now);
                        }
                    }
                    s.pending_place_acks.remove(&order_id).map(|p| p.tx)
                };
                if let Some(tx) = ack {
                    // Resolve to Ok regardless of native_to_open_order
                    // success — IBKR has accepted the order at the
                    // wire level. Native_to_open_order can fail on
                    // unfamiliar sec_types but the order is real.
                    let _ = tx.send(Ok(()));
                }
            }
            InboundMsg::OrderStatus(o) => {
                let parsed_status = parse_status(&o.status);
                // ACK resolution: IBKR commonly emits OrderStatus
                // (Submitted / PreSubmitted) before OpenOrder, so the
                // place_order waiter must resolve here too. Resolving
                // ONLY on terminal-or-active states (not Inactive)
                // avoids resolving on a stale rebroadcast for an
                // already-rejected order. ApiPending/PendingSubmit
                // we treat as not-yet-acked.
                let ack = if matches!(
                    parsed_status,
                    OrderStatus::Submitted
                        | OrderStatus::PendingSubmit  // PreSubmitted = IBKR
                            // gateway accepted the message — used for
                            // PLACE acks only. Amend acks ignore
                            // OrderStatus and gate on OpenOrder with
                            // matching lmt_price (see OpenOrder branch
                            // + PendingAck doc). PreSubmitted from
                            // local Gateway lands in microseconds
                            // before IBKR's server has actually applied
                            // the amend, which under-reports amend RTT.
                        | OrderStatus::Filled
                        | OrderStatus::Cancelled
                        | OrderStatus::PendingCancel
                ) {
                    let mut s = state.lock();
                    if let Some(open) = s.open_orders.get_mut(&o.order_id) {
                        open.status = parsed_status;
                        open.filled_qty = o.filled as u32;
                    }
                    // Purge of terminal entries was tried 2026-05-04
                    // and reverted: when IBKR Cancels an order (GTD
                    // expiry, peer cross), the trader hasn't yet
                    // observed the cancel via order_status — its
                    // next amend attempt hits "orderId not in open
                    // cache" because we just purged it. Keep terminal
                    // entries; build_chain_payload + cancel_all_resting
                    // already filter on status so the bloat is
                    // cosmetic. Cleanup belongs in a periodic GC.
                    //
                    // Permissive resolver: any accept-state signal
                    // resolves the waiter (place AND amend). Honest
                    // amend RTT is observed separately via OpenOrder
                    // matching-price in the OpenOrder branch above
                    // and surfaced through `strict_ack_ns`.
                    s.wire_ack_ns.entry(o.order_id).or_insert_with(now_ns);
                    s.pending_place_acks.remove(&o.order_id).map(|p| p.tx)
                } else {
                    let mut s = state.lock();
                    if let Some(open) = s.open_orders.get_mut(&o.order_id) {
                        open.status = parsed_status;
                        open.filled_qty = o.filled as u32;
                    }
                    None
                };
                if let Some(tx) = ack {
                    let _ = tx.send(Ok(()));
                }
                // Lock dropped before broadcast::send so a slow
                // consumer cannot stall the dispatcher and back up the
                // recv channel (P0-5).
                let _ = channels.status.send(OrderStatusUpdate {
                    order_id: OrderId(o.order_id as u64),
                    status: parsed_status,
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
                    let routed_to_pending = if e.req_id > 0 {
                        let mut s = state.lock();
                        if let Some(p) = s.pending_executions.get_mut(&e.req_id) {
                            p.accumulated.push(fill.clone());
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !routed_to_pending {
                        // Lock released before broadcast (P0-5).
                        let _ = channels.fills.send(fill);
                    }
                }
            }
            InboundMsg::ExecutionEnd(req_id) => {
                let mut s = state.lock();
                if let Some(mut p) = s.pending_executions.remove(&req_id) {
                    if let Some(sender) = p.sender.take() {
                        let _ = sender.send(Ok(std::mem::take(&mut p.accumulated)));
                    }
                }
            }
            InboundMsg::ContractDetails(cd) => {
                let mut s = state.lock();
                if let Some(p) = s.pending_contract_details.get_mut(&cd.req_id) {
                    p.accumulated.push(cd);
                }
            }
            InboundMsg::ContractDetailsEnd(req_id) => {
                let mut s = state.lock();
                if let Some(mut p) = s.pending_contract_details.remove(&req_id) {
                    let contracts: Vec<Contract> =
                        p.accumulated.iter().filter_map(native_to_contract).collect();
                    if let Some(sender) = p.sender.take() {
                        let _ = sender.send(Ok(contracts));
                    }
                }
            }
            InboundMsg::TickPrice(t) => {
                let kind = match t.tick_type {
                    1 => Some(TickKind::Bid),
                    2 => Some(TickKind::Ask),
                    4 => Some(TickKind::Last),
                    // Delayed-data tick types — IBKR sends these instead
                    // of live (1/2/4) when account entitlements lack
                    // real-time market data. Paper accounts typically
                    // have delayed-only for some products.
                    66 => Some(TickKind::Bid),
                    67 => Some(TickKind::Ask),
                    68 => Some(TickKind::Last),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let iid_opt = state.lock().tick_routes.get(&t.req_id).copied();
                    if let Some(iid) = iid_opt {
                        let tick = Tick {
                            instrument_id: iid,
                            kind,
                            price: Some(t.price),
                            size: None,
                            // v2 wire-timing — channel-recv time, not now().
                            timestamp_ns: recv_ns,
                        };
                        // Fast path: write directly to SHM via the
                        // tick publisher closure. Bypasses the
                        // broadcast → forward_ticks pump (~20 µs/tick).
                        if let Some(publish) = tick_publisher {
                            publish(&tick);
                        }
                        // Broadcast for in-process consumers
                        // (market_data state, etc.).
                        let _ = channels.ticks.send(tick);
                    }
                }
            }
            InboundMsg::TickSize(t) => {
                // Diagnostic: count distinct tick_types observed.
                {
                    let mut s = state.lock();
                    *s.tick_type_hist.entry(t.tick_type).or_insert(0) += 1;
                }
                // Drop wrong-side OI / per-side-volume ticks. IBKR sends
                // BOTH 27 (call OI) AND 28 (put OI) to every option
                // subscription regardless of leg right — for the
                // opposite side it sends 0. Without filtering, the 0
                // overwrites the real value. Same for 29 (call vol) /
                // 30 (put vol). Tick 8 (overall option vol) is right-
                // agnostic and always kept.
                if matches!(t.tick_type, 27 | 28 | 29 | 30) {
                    let leg_right = state.lock().tick_route_right.get(&t.req_id).copied();
                    if let Some(r) = leg_right {
                        let want = match (t.tick_type, r) {
                            (27, Right::Call) => true,
                            (28, Right::Put) => true,
                            (29, Right::Call) => true,
                            (30, Right::Put) => true,
                            _ => false, // wrong-side; drop
                        };
                        if !want {
                            return;
                        }
                    }
                }
                let kind = match t.tick_type {
                    0 => Some(TickKind::BidSize),
                    3 => Some(TickKind::AskSize),
                    // Generic tick "100" / "101" deliver:
                    //   8  → Volume (overall option session volume —
                    //         IBKR uses this for options too, not just
                    //         futures; routing to OptionVolume so it
                    //         lands in the per-leg counter)
                    //   27 → OptionCallOpenInterest
                    //   28 → OptionPutOpenInterest
                    //   29 → OptionCallVolume
                    //   30 → OptionPutVolume
                    // The call/put split is informational — our
                    // subscription is per-leg so we already know the
                    // right from the contract; same TickKind for both.
                    8 | 29 | 30 => Some(TickKind::OptionVolume),
                    27 | 28 => Some(TickKind::OptionOpenInterest),
                    // Delayed-data size tick types.
                    69 => Some(TickKind::BidSize),
                    70 => Some(TickKind::AskSize),
                    71 => Some(TickKind::OptionVolume),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let iid_opt = state.lock().tick_routes.get(&t.req_id).copied();
                    if let Some(iid) = iid_opt {
                        let tick = Tick {
                            instrument_id: iid,
                            kind,
                            price: None,
                            size: Some(t.size as u64),
                            // v2 wire-timing — channel-recv time, not now().
                            timestamp_ns: recv_ns,
                        };
                        if let Some(publish) = tick_publisher {
                            publish(&tick);
                        }
                        let _ = channels.ticks.send(tick);
                    }
                }
            }
            InboundMsg::MarketDepth(d) => {
                let iid_opt = state.lock().depth_routes.get(&d.req_id).copied();
                if let Some(iid) = iid_opt {
                    let upd = corsair_broker_api::events::DepthUpdate {
                        instrument_id: iid,
                        position: d.position,
                        operation: d.operation,
                        // IBKR convention: side 0=ask, 1=bid.
                        is_bid: d.side == 1,
                        price: d.price,
                        size: d.size as u64,
                        timestamp_ns: recv_ns,
                    };
                    let _ = channels.depth.send(upd);
                }
            }
            InboundMsg::Error(e) => {
                let be = error_to_broker_error(&e);
                // Resolve pending contract / place_order waiters under
                // the lock, then drop and broadcast (P0-5). Order-side
                // error codes (200-range, 201, 202, 436) carry the
                // orderId in req_id; on those we fail the place ack so
                // place_order returns an error instead of a phantom OK.
                let mut to_fail_contract = None;
                let mut to_fail_place = None;
                {
                    let mut s = state.lock();
                    if e.req_id > 0 {
                        if is_contract_error(e.error_code) {
                            to_fail_contract = s.pending_contract_details.remove(&e.req_id);
                        }
                        if is_order_error(e.error_code) {
                            to_fail_place = s.pending_place_acks.remove(&e.req_id);
                        }
                    }
                }
                if let Some(mut p) = to_fail_contract {
                    if let Some(sender) = p.sender.take() {
                        let _ = sender.send(Err(BrokerError::ContractNotFound(
                            e.error_string.clone(),
                        )));
                    }
                }
                if let Some(p) = to_fail_place {
                    let _ = p.tx.send(Err(BrokerError::OrderRejected {
                        order_id: Some(e.req_id as u64),
                        reason: e.error_string.clone(),
                    }));
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
        // Audit T4-1: Rejected was previously bucketed into Inactive.
        // FA accounts return "Rejected" / "ApiRejected" on pre-trade
        // risk failures; the consumer (pump_status) already has a
        // Rejected arm at tasks.rs:189, so route them through.
        "Rejected" | "ApiRejected" => OrderStatus::Rejected,
        "Inactive" => OrderStatus::Inactive,
        _ => OrderStatus::Inactive,
    }
}

fn is_contract_error(code: i32) -> bool {
    matches!(code, 200 | 321 | 322)
}

/// IBKR error codes that indicate an order was REJECTED at submission.
/// Excludes informational warnings (399 "order modified to fit risk",
/// 461 "displaying liquid message") which leave the order live with
/// adjusted parameters; treating those as rejects causes the trader
/// to re-submit duplicates.
///
/// Code 202 ("order cancelled") is also excluded because it's the
/// normal response to OUR cancel, not a rejection of a fresh place.
fn is_order_error(code: i32) -> bool {
    matches!(
        code,
        201    // order rejected by IBKR (insufficient margin, etc.)
        | 203  // security symbol not found / not subscribed
        | 434  // qty=0
        | 436  // FA: must specify allocation
        | 10148 // OrderId not found
        | 10197 // No market for this order
    )
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
    // IBKR's `lastTradeDateOrContractMonth` is sent as e.g.
    // "20260527 13:00:00 US/Eastern" — full datetime+tz. Strip the
    // tz/time suffix before parsing. Plain YYYYMMDD also lands here
    // (some products) — split-on-space leaves it untouched.
    let date_str = cd.contract.last_trade_date
        .split_whitespace()
        .next()
        .unwrap_or("");
    let expiry = NaiveDate::parse_from_str(date_str, "%Y%m%d")
        .or_else(|_| NaiveDate::parse_from_str(date_str, "%Y%m"))
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
        trading_class: cd.contract.trading_class.clone(),
    })
}

fn native_to_position(p: &PositionMsg) -> Option<Position> {
    // Same datetime+tz suffix possibility as native_to_contract.
    let date_str = p.contract.last_trade_date
        .split_whitespace()
        .next()
        .unwrap_or("");
    Some(Position {
        contract: Contract {
            instrument_id: InstrumentId(p.contract.con_id as u64),
            kind: parse_kind(&p.contract.sec_type)?,
            symbol: p.contract.symbol.clone(),
            local_symbol: p.contract.local_symbol.clone(),
            expiry: NaiveDate::parse_from_str(date_str, "%Y%m%d")
                .or_else(|_| NaiveDate::parse_from_str(date_str, "%Y%m"))
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
            trading_class: p.contract.trading_class.clone(),
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
        trading_class: o.contract.trading_class.clone(),
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
        // Audit T1-1: tradingClass required for FUT under FA accounts.
        // Native_to_contract populates it from IBKR's contractDetails;
        // fall back to symbol if for some reason it's empty (matches
        // CME convention: HG futures tradingClass = "HG").
        trading_class: if c.trading_class.is_empty() {
            c.symbol.clone()
        } else {
            c.trading_class.clone()
        },
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
        // IBKR-documented format with explicit timezone string. The
        // example in the API docs is `yyyymmdd hh:mm:ss xx/xxxx` with
        // names like "US/Eastern". Empirically the gateway accepts
        // "UTC" too. Dash form (`yyyymmdd-hh:mm:ss`) is documented as
        // implicit-UTC but at server v178 we see the gateway rejecting
        // it with error 391 — sticking to the explicit-tz form.
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

/// Build a ContractRequest from the broker_api Contract for use in
/// reqMktData/reqContractDetails. The opposite of native_to_contract.
fn contract_to_request(c: &corsair_broker_api::Contract) -> ContractRequest {
    let sec_type = match c.kind {
        ContractKind::Future => "FUT",
        ContractKind::Option => "FOP",
    };
    ContractRequest {
        con_id: c.instrument_id.0 as i64,
        symbol: c.symbol.clone(),
        sec_type: sec_type.into(),
        last_trade_date: c.expiry.format("%Y%m%d").to_string(),
        strike: c.strike.unwrap_or(0.0),
        right: c.right.map(|r| match r {
            corsair_broker_api::Right::Call => "C",
            corsair_broker_api::Right::Put => "P",
        }).unwrap_or("").into(),
        multiplier: if c.multiplier > 0.0 {
            format!("{}", c.multiplier as u64)
        } else {
            String::new()
        },
        exchange: exchange_to_str(c.exchange).into(),
        primary_exchange: String::new(),
        currency: currency_to_str(c.currency).into(),
        local_symbol: c.local_symbol.clone(),
        trading_class: c.trading_class.clone(),
    }
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
    async fn connect(&self) -> BResult<()> {
        if self.connected.load(Ordering::SeqCst) {
            return Ok(());
        }

        self.client.connect().await.map_err(Self::map_native_err)?;
        self.client
            .wait_for_bootstrap(Duration::from_secs(10))
            .await
            .map_err(Self::map_native_err)?;

        // Spawn the dispatcher BEFORE issuing any user reqs so that
        // OpenOrderEnd / PositionEnd / AccountDownloadEnd terminators
        // have a consumer the moment they arrive on the recv channel.
        // (The mpsc is bounded at 16K — high enough to absorb the
        // pre-dispatcher startApi burst, but we still don't want to
        // rely on that capacity for correctness.) The yield_now()
        // below gives the just-spawned task a chance to actually
        // start polling before we generate downstream traffic — not
        // strictly required (tokio drains the channel either way) but
        // tightens the steady-state where the dispatcher is in the
        // ready queue when the first reply lands.
        let rx = self
            .rx_holder
            .lock()
            .await
            .take()
            .ok_or_else(|| BrokerError::Internal("rx already taken".into()))?;
        self.spawn_dispatcher(rx);
        tokio::task::yield_now().await;

        // CLAUDE.md §1: when clientId=0 (FA master client mode),
        // call reqAutoOpenOrders(True) so IBKR forwards OpenOrder /
        // OrderStatus / execDetails for orders placed by ANY client
        // on the connection. Without this, place_order goes out and
        // IBKR processes it but never sends back the OpenOrder /
        // OrderStatus, causing every place to time out at 2s with
        // `place_order ack timeout`. Sent AFTER wait_for_bootstrap
        // so we don't race the gateway's startup pipeline (per §5
        // historical note: pre-bootstrap reqs cause silent disconnects).
        let mut bootstrap_frames: Vec<Vec<u8>> = vec![
            req_account_updates(true, &self.cfg.account),
            req_positions(),
            req_open_orders(),
        ];
        if self.cfg.client.client_id == 0 {
            bootstrap_frames.push(crate::requests::req_auto_open_orders(true));
        }
        for frame in bootstrap_frames {
            self.client
                .send_raw(&frame)
                .await
                .map_err(Self::map_native_err)?;
        }
        if self.cfg.client.client_id == 0 {
            log::warn!(
                "broker: reqAutoOpenOrders(true) sent (clientId=0 master mode)"
            );
        }

        self.connected.store(true, Ordering::SeqCst);
        let _ = self.channels.connection.send(ConnectionEvent {
            state: ConnectionState::Connected,
            timestamp_ns: now_ns(),
            reason: None,
        });

        Ok(())
    }

    async fn disconnect(&self) -> BResult<()> {
        self.connected.store(false, Ordering::SeqCst);
        // Drain pending waiters so callers don't block on their
        // tokio::time::timeout (P1-8).
        {
            let mut s = self.state.lock();
            for (_, mut p) in s.pending_contract_details.drain() {
                if let Some(sender) = p.sender.take() {
                    let _ = sender.send(Err(BrokerError::ConnectionLost(
                        "disconnect during pending qualify".into(),
                    )));
                }
            }
            for (_, mut p) in s.pending_executions.drain() {
                if let Some(sender) = p.sender.take() {
                    let _ = sender.send(Err(BrokerError::ConnectionLost(
                        "disconnect during pending executions".into(),
                    )));
                }
            }
            for (_, p) in s.pending_place_acks.drain() {
                let _ = p.tx.send(Err(BrokerError::ConnectionLost(
                    "disconnect during pending place_order ack".into(),
                )));
            }
        }
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

        // Install a pending place_order waiter BEFORE sending the
        // wire frame so we don't race the dispatcher.
        let (tx, rx) = oneshot::channel::<Result<(), BrokerError>>();

        let params = build_place_params(&req, &self.cfg.account)?;

        // Coalesce the three short critical sections (insert ack,
        // entry-or-build template, stamp send_ns) into ONE lock.
        // Each individual lock pre-coalesce was sub-microsecond
        // under parking_lot, but on a contended dispatcher they
        // were three round-trips per place_order (~1-2 µs of
        // saved jitter on 10K places/session). The encode cost
        // for a fresh template lives inside the lock — it's
        // ~5 µs once per instrument and amortizes to nothing.
        // No `.await` inside — parking_lot::Mutex is not
        // .await-safe and we never need to be.
        let frame = {
            let mut s = self.state.lock();
            s.pending_place_acks.insert(order_id, PendingAck { tx });
            let template = s
                .place_templates
                .entry(req.contract.instrument_id)
                .or_insert_with(|| {
                    let cr = place_order_contract_request(&req.contract);
                    crate::place_template::ContractTemplate::from_contract(&cr)
                });
            // Build the wire frame against a *borrow* of the cached
            // template — no clone, no extra allocation beyond the
            // returned Vec<u8>.
            let frame = crate::place_template::place_order_fast(order_id, template, &params);
            // v2 wire-timing — stamp send_ns immediately, inside
            // the same critical section. Pairs with ack_ns set in
            // route() on OpenOrder/OrderStatus.
            s.wire_send_ns.insert(order_id, now_ns());
            frame
        };
        if let Err(e) = self.client.send_raw(&frame).await {
            // Send failed; clean up waiter.
            let mut s = self.state.lock();
            s.pending_place_acks.remove(&order_id);
            s.wire_send_ns.remove(&order_id);
            return Err(Self::map_native_err(e));
        }

        // Wait briefly for OpenOrder ack OR Error rejection. IBKR is
        // typically <100ms for the first response; place_rtt p99 ~1s
        // under healthy conditions per `wire_timing` analysis. 500ms
        // covers steady-state with margin while capping the worst-case
        // stall when the recently_terminated cache misses (orderId
        // terminated within the last few µs but pump_status hasn't
        // run yet). Reduced from 2s 2026-05-07 in conjunction with
        // the broker-side terminal cache — see
        // `corsair_broker::runtime::Runtime::recently_terminated`
        // and `docs/HANDOFF_LATENCY_LEDGER.md` §3.2.
        match tokio::time::timeout(Duration::from_millis(500), rx).await {
            Ok(Ok(Ok(()))) => Ok(OrderId(order_id as u64)),
            Ok(Ok(Err(broker_err))) => Err(broker_err),
            Ok(Err(_dropped)) => Err(BrokerError::Internal(
                "place_order ack channel dropped".into(),
            )),
            Err(_elapsed) => {
                // Time out — drop waiter; the order may still be live
                // at IBKR. Use Protocol (retriable per
                // BrokerError::is_retriable) so the trader can back
                // off and retry rather than treating this as a fatal
                // Internal error. Caller should also reconcile via
                // open_orders() to discover the orphaned order.
                //
                // Audit T1-3: also evict wire_send_ns + wire_ack_ns to
                // prevent slow leak of timing entries on chronic
                // timeout (each leaked entry = ~24B; over months of
                // FUT-ack issues this would build up).
                //
                // Audit (2026-05-05): place_order doesn't seed
                // expected_amend_prices / strict_ack_ns (those are
                // amend-side maps), but we drain them defensively in
                // case a stale entry from a prior modify on the same
                // orderId was orphaned by an earlier failure path.
                let mut s = self.state.lock();
                s.pending_place_acks.remove(&order_id);
                s.wire_send_ns.remove(&order_id);
                s.wire_ack_ns.remove(&order_id);
                s.expected_amend_prices.remove(&order_id);
                s.strict_ack_ns.remove(&order_id);
                drop(s);
                Err(BrokerError::Protocol {
                    code: None,
                    message: format!("place_order ack timeout (orderId={order_id})"),
                })
            }
        }
    }

    async fn cancel_order(&self, id: OrderId) -> BResult<()> {
        let frame = cancel_order(id.0 as i32);
        self.client
            .send_raw(&frame)
            .await
            .map_err(Self::map_native_err)
    }

    /// v2 wire-timing — drain the (send_ns, ack_ns) pair captured
    /// inside place_order + the dispatcher. Returns None if either
    /// is missing (uncommon — would mean place_order never reached
    /// send_raw, or ack hasn't arrived yet). Removes the entries
    /// from state so the maps don't grow unbounded.
    fn drain_wire_timing(&self, order_id: u64) -> Option<(u64, u64)> {
        let mut s = self.state.lock();
        let send = s.wire_send_ns.remove(&(order_id as i32))?;
        let ack = s.wire_ack_ns.remove(&(order_id as i32))?;
        Some((send, ack))
    }

    /// Honest amend RTT timestamp. Returns the wall-clock ns at
    /// which the dispatcher observed an OpenOrder whose lmt_price
    /// matched the modify request — i.e. when IBKR's server has
    /// applied the price change. Returns None if no matching
    /// OpenOrder was observed for this orderId (same-price refresh,
    /// IBKR edge cases, or the modify hadn't completed yet).
    /// Clears both `strict_ack_ns` and `expected_amend_prices`
    /// for this orderId so the maps don't grow unbounded.
    fn drain_strict_amend_ack_ns(&self, order_id: u64) -> Option<u64> {
        let mut s = self.state.lock();
        s.expected_amend_prices.remove(&(order_id as i32));
        s.strict_ack_ns.remove(&(order_id as i32))
    }

    // Fast-path modify: uses the pre-encoded ContractTemplate cache
    // built by place_order. IBKR's modify protocol is "placeOrder
    // with same orderId", so the contract section of the wire frame
    // is identical to the original place. Switching to template
    // memcpy here saves ~50µs per amend (was ~60µs encode time;
    // template path is ~10µs).
    async fn modify_order(&self, id: OrderId, req: ModifyOrderReq) -> BResult<()> {
        let existing = {
            let s = self.state.lock();
            s.open_orders.get(&(id.0 as i32)).cloned()
        };
        let existing = existing.ok_or_else(|| {
            BrokerError::Internal(format!("modify_order: orderId={} not in open cache", id.0))
        })?;

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

        // Fast frame build via cached contract template. First amend
        // for an instrument may pay the encode cost; subsequent ones
        // memcpy. Same template as the original place_order so amends
        // hit the cache immediately.
        let frame = {
            let mut s = self.state.lock();
            let template = s.place_templates
                .entry(existing.contract.instrument_id)
                .or_insert_with(|| {
                    let cr = place_order_contract_request(&existing.contract);
                    crate::place_template::ContractTemplate::from_contract(&cr)
                });
            crate::place_template::place_order_fast(id.0 as i32, template, &params)
        };
        // Wire trace for amend verification: confirms we're sending a
        // placeOrder with the SAME orderId as the existing order, only
        // price/gtd changed. IBKR's protocol treats this as amend.
        log::info!(
            "AMEND wire: orderId={} side={:?} qty={} old_price={:?} new_price={:?} gtd={:?} frame_len={}",
            id.0,
            existing.side,
            placeholder.qty,
            existing.price,
            placeholder.price,
            placeholder.gtd_until_utc.map(|d| d.timestamp()),
            frame.len(),
        );

        // Permissive resolver path (functional). The dispatcher
        // resolves on any accept-state OrderStatus or OpenOrder for
        // this orderId. amend_us via this path is phantom-fast for
        // price-change amends because PreSubmitted is the IB
        // Gateway's local echo, not a server-confirmed price update.
        //
        // To recover an honest server-side amend RTT, the dispatcher
        // ALSO observes OpenOrder-with-matching-price separately and
        // records its arrival timestamp in `strict_ack_ns`. That
        // observation does NOT affect the channel resolution (the
        // permissive path remains the functional ack path). After
        // modify_order returns, callers can drain `strict_ack_ns`
        // alongside `wire_send_ns`/`wire_ack_ns` to compute the
        // honest amend RTT for the dashboard.
        //
        // `expected_amend_price` is set so the dispatcher knows what
        // to compare incoming OpenOrder.lmt_price against. The
        // resolver itself ignores the field (returns true for both
        // None and Some) — gating logic is moved into the per-signal
        // observation in the dispatcher.
        let expected_amend_price = placeholder.price.unwrap_or(0.0);
        let (tx, rx) = oneshot::channel::<Result<(), BrokerError>>();
        {
            let mut s = self.state.lock();
            s.pending_place_acks.insert(id.0 as i32, PendingAck { tx });
            s.wire_send_ns.insert(id.0 as i32, now_ns());
            // Seed expected_amend_prices so the dispatcher can
            // continue stamping strict_ack_ns even after the
            // permissive resolver removes the PendingAck. Cleared
            // by drain_strict_amend_timing.
            s.expected_amend_prices.insert(id.0 as i32, expected_amend_price);
        }
        if let Err(e) = self.client.send_raw(&frame).await {
            let mut s = self.state.lock();
            s.pending_place_acks.remove(&(id.0 as i32));
            s.wire_send_ns.remove(&(id.0 as i32));
            return Err(Self::map_native_err(e));
        }

        // 500ms wait — same shape as place_order. The dispatcher
        // resolves on any accept-state signal. The honest amend RTT
        // (if any) is observed separately by the dispatcher via
        // `strict_ack_ns` and exposed through `drain_strict_amend_us`.
        // Reduced from 2s 2026-05-07 alongside the broker-side
        // recently_terminated cache; see place_order above for the
        // rationale.
        match tokio::time::timeout(Duration::from_millis(500), rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(broker_err))) => Err(broker_err),
            Ok(Err(_dropped)) => Err(BrokerError::Internal(
                "modify_order ack channel dropped".into(),
            )),
            Err(_elapsed) => {
                // Drain ALL maps the modify path seeds.
                // expected_amend_prices and strict_ack_ns were leaking
                // pre-fix; the ConstraintChecker-style amend-RTT
                // dashboard reads them via drain_strict_amend_ack_ns,
                // but on timeout the caller doesn't reach that drain.
                let mut s = self.state.lock();
                s.pending_place_acks.remove(&(id.0 as i32));
                s.wire_send_ns.remove(&(id.0 as i32));
                s.wire_ack_ns.remove(&(id.0 as i32));
                s.expected_amend_prices.remove(&(id.0 as i32));
                s.strict_ack_ns.remove(&(id.0 as i32));
                Err(BrokerError::Protocol {
                    code: None,
                    message: format!("modify_order ack timeout (orderId={})", id.0),
                })
            }
        }
    }

    async fn positions(&self) -> BResult<Vec<Position>> {
        let s = self.state.lock();
        Ok(s.positions.values().cloned().collect())
    }

    fn diagnostic_take_tick_type_hist(&self) -> Vec<(i32, u64)> {
        let mut s = self.state.lock();
        let mut out: Vec<(i32, u64)> = s.tick_type_hist.drain().collect();
        out.sort_by_key(|(k, _)| *k);
        out
    }

    async fn account_values(&self) -> BResult<AccountSnapshot> {
        let s = self.state.lock();
        let mut snap = AccountSnapshot {
            net_liquidation: 0.0,
            maintenance_margin: 0.0,
            initial_margin: 0.0,
            buying_power: 0.0,
            realized_pnl_today: 0.0,
            timestamp_ns: now_ns(),
        };
        // Filter by configured account: on FA logins, IBKR also
        // streams aggregate/master rollups (empty-string account or the
        // DFP master). Iterating those into our snapshot polluted
        // maintenance_margin with the FA-rolled-up value, which sat
        // above our 50% ceiling indefinitely with zero positions
        // locally and stuck the trader in risk_block.
        for ((key, _ccy, acct), v) in s.account_values.iter() {
            if acct != &self.cfg.account {
                continue;
            }
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
        let s = self.state.lock();
        Ok(s.open_orders.values().cloned().collect())
    }

    async fn recent_fills(&self, since_ns: u64) -> BResult<Vec<Fill>> {
        let req_id = self.alloc_req_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.state.lock();
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
                let mut s = self.state.lock();
                s.pending_executions.remove(&req_id);
                Err(BrokerError::Internal("recent_fills timeout".into()))
            }
        }
    }

    async fn qualify_future(&self, q: FutureQuery) -> BResult<Contract> {
        let req_id = self.alloc_req_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.state.lock();
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
                let mut s = self.state.lock();
                s.pending_contract_details.remove(&req_id);
                Err(BrokerError::Internal("qualify_future timeout".into()))
            }
        }
    }

    async fn qualify_option(&self, q: OptionQuery) -> BResult<Contract> {
        let req_id = self.alloc_req_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.state.lock();
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
        // Round strike to 4 decimals to scrub IEEE-754 accumulation
        // (e.g. 0.05 + 0.05 + ... = 5.8500000000000005). IBKR's contract
        // DB matches on exact decimal strikes; ryu serializes the noisy
        // float verbatim → "No security definition has been found".
        cr.strike = (q.strike * 10_000.0).round() / 10_000.0;
        cr.right = match q.right {
            Right::Call => "C".into(),
            Right::Put => "P".into(),
        };
        // Per IBKR FOP convention, omit multiplier when tradingClass is
        // specified — server infers from class. Including both can
        // trigger "No security definition" if our value (e.g. "25000")
        // disagrees with their canonical store.
        cr.exchange = exchange_to_str(q.exchange).into();
        cr.currency = currency_to_str(q.currency).into();
        cr.trading_class = q.symbol.clone();

        self.client
            .send_raw(&req_contract_details(req_id, &cr))
            .await
            .map_err(Self::map_native_err)?;

        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(Ok(mut contracts))) if !contracts.is_empty() => {
                let mut c = contracts.swap_remove(0);
                // IBKR's contractDetails reply for FOP doesn't always
                // include multiplier in the field position our decoder
                // reads (depends on serverVersion). Fall back to the
                // query's multiplier (which the caller pulled from
                // product config) so place_order has a non-zero value
                // — sending "0" here causes IBKR to reject with
                // cryptic errors like "VOL volatility required".
                if c.multiplier <= 0.0 {
                    c.multiplier = q.multiplier;
                }
                Ok(c)
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
                let mut s = self.state.lock();
                s.pending_contract_details.remove(&req_id);
                Err(BrokerError::Internal("qualify_option timeout".into()))
            }
        }
    }

    async fn list_chain(&self, q: ChainQuery) -> BResult<Vec<Contract>> {
        let req_id = self.alloc_req_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.state.lock();
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

        // 60s timeout (was 15s). IBKR's contract details endpoint is
        // slower than the live feed, especially during weekend / pre-
        // open windows when the contract DB hasn't warmed up. 15s was
        // too short — repeatedly timed out on Sun reopen 2026-05-03.
        // 60s gives more headroom; on success the call returns within
        // 1-5s, so this is purely a tail-latency budget.
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(Ok(mut contracts))) => {
                if let Some(min_expiry) = q.min_expiry {
                    contracts.retain(|c| c.expiry >= min_expiry);
                }
                Ok(contracts)
            }
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(BrokerError::Internal("list_chain channel dropped".into())),
            Err(_) => {
                let mut s = self.state.lock();
                s.pending_contract_details.remove(&req_id);
                Err(BrokerError::Internal("list_chain timeout".into()))
            }
        }
    }

    async fn subscribe_ticks(&self, sub: TickSubscription) -> BResult<TickStreamHandle> {
        let req_id = self.alloc_req_id();
        let handle = self.alloc_handle();
        // Insert routing state BEFORE the send so any inbound tick
        // arriving on this req_id is dispatchable. On send failure we
        // unwind the state below — without that cleanup, the maps
        // accumulate orphan entries on every TCP write error.
        // Audit round 2.
        {
            let mut s = self.state.lock();
            s.tick_routes.insert(req_id, sub.instrument_id);
            // Record option leg's Right so we can drop wrong-side OI /
            // volume ticks at decode time. Only options carry a Right;
            // futures subs leave this empty and the decode skip is a
            // no-op for them.
            if let Some(c) = &sub.contract {
                if let Some(r) = c.right {
                    s.tick_route_right.insert(req_id, r);
                }
            }
            s.handle_to_req_id.insert(handle, req_id);
        }
        // Build the full contract descriptor. IBKR rejects reqMktData
        // for futures/options if exchange is missing (`Error validating
        // request: Please enter exchange`). Caller passes the Contract
        // we resolved via list_chain so we have the canonical exchange.
        let mut cr = match &sub.contract {
            Some(c) => contract_to_request(c),
            None => empty_contract_request("", ""),
        };
        // con_id always comes from the subscription's instrument_id —
        // authoritative even when the caller passed a stale Contract.
        cr.con_id = sub.instrument_id.0 as i64;
        // Generic tick types we want IBKR to push:
        //   100 = Option Volume (call/put split)
        //   101 = Option Open Interest (call/put split)
        // Without this list, reqMktData only returns BBO + last —
        // dashboard shows blank OI/volume columns.
        let frame = req_mkt_data(req_id, &cr, "100,101", false, false);
        if let Err(e) = self.client.send_raw(&frame).await {
            let mut s = self.state.lock();
            s.tick_routes.remove(&req_id);
            s.tick_route_right.remove(&req_id);
            s.handle_to_req_id.remove(&handle);
            return Err(Self::map_native_err(e));
        }
        Ok(handle)
    }

    async fn unsubscribe_ticks(&self, h: TickStreamHandle) -> BResult<()> {
        let req_id = {
            let mut s = self.state.lock();
            let req_id = s.handle_to_req_id.remove(&h);
            if let Some(rid) = req_id {
                s.tick_routes.remove(&rid);
                // Audit round 2: drop the option-Right entry too.
                // Previously leaked, accumulating across long-running
                // sessions that rotate strike subscriptions.
                s.tick_route_right.remove(&rid);
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

    async fn subscribe_market_depth(
        &self,
        sub: TickSubscription,
        num_rows: i32,
    ) -> BResult<TickStreamHandle> {
        let req_id = self.alloc_req_id();
        let handle = self.alloc_handle();
        {
            let mut s = self.state.lock();
            s.depth_routes.insert(req_id, sub.instrument_id);
            s.handle_to_depth_req_id.insert(handle, req_id);
        }
        let mut cr = match &sub.contract {
            Some(c) => contract_to_request(c),
            None => empty_contract_request("", ""),
        };
        cr.con_id = sub.instrument_id.0 as i64;
        let frame = req_mkt_depth(req_id, &cr, num_rows);
        if let Err(e) = self.client.send_raw(&frame).await {
            let mut s = self.state.lock();
            s.depth_routes.remove(&req_id);
            s.handle_to_depth_req_id.remove(&handle);
            return Err(Self::map_native_err(e));
        }
        Ok(handle)
    }

    async fn unsubscribe_market_depth(&self, h: TickStreamHandle) -> BResult<()> {
        let req_id = {
            let mut s = self.state.lock();
            let req_id = s.handle_to_depth_req_id.remove(&h);
            if let Some(rid) = req_id {
                s.depth_routes.remove(&rid);
            }
            req_id
        };
        if let Some(rid) = req_id {
            let frame = cancel_mkt_depth(rid);
            self.client
                .send_raw(&frame)
                .await
                .map_err(Self::map_native_err)?;
        }
        Ok(())
    }

    fn subscribe_depth_stream(
        &self,
    ) -> broadcast::Receiver<corsair_broker_api::events::DepthUpdate> {
        self.channels.depth.subscribe()
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

    async fn wait_for_initial_snapshot(&self, timeout: Duration) -> BResult<()> {
        let progress = self.wait_for_seeding(timeout).await;
        log::info!(
            "NativeBroker initial snapshot: positions={} open_orders={} account={}",
            progress.positions_done,
            progress.open_orders_done,
            progress.account_done,
        );
        Ok(())
    }

    async fn set_tick_publisher(
        &self,
        publisher: Arc<dyn for<'a> Fn(&'a Tick) + Send + Sync + 'static>,
    ) -> bool {
        *self.tick_publisher.lock() = Some(publisher);
        log::warn!("NativeBroker: tick fast-path publisher installed");
        true
    }
}
