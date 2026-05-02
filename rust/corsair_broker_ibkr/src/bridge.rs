//! Python event-loop thread bridge.
//!
//! Spawns a single OS thread that owns:
//!   - A Python asyncio event loop
//!   - An `ib_insync.IB` instance
//!   - Event callbacks pumping into Rust broadcast channels
//!
//! The thread executes a tight loop:
//!   1. Drain any pending events from ib_insync callbacks → broadcast
//!   2. Try to receive a command (short timeout)
//!   3. If a command arrived, dispatch it (acquires GIL, calls ib_insync,
//!      converts result, sends back via oneshot)
//!   4. Step the asyncio loop one cycle so awaitable callbacks fire
//!   5. Repeat
//!
//! When [`Bridge`] is dropped, a Shutdown command is sent and the
//! thread joins cleanly. Python state is dropped under the GIL.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use corsair_broker_api::events::{ConnectionEvent, Fill};
use corsair_broker_api::{BrokerError, OrderStatusUpdate, Tick};
use thiserror::Error;
use tokio::sync::broadcast;

use crate::commands::Command;

/// Configuration passed to the bridge thread at construction time.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub gateway_host: String,
    pub gateway_port: u16,
    pub client_id: i32,
    pub account: String,
    /// How long the bridge thread blocks on the command channel before
    /// pumping the asyncio loop. Trade-off: shorter = more responsive
    /// to ib_insync callbacks, more CPU; longer = lower CPU. 1 ms is
    /// the default — comfortably below our latency budgets.
    pub poll_interval_ms: u64,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            gateway_host: "127.0.0.1".into(),
            gateway_port: 4002,
            client_id: 0,
            account: String::new(),
            poll_interval_ms: 1,
        }
    }
}

/// Errors that can occur in the bridge layer (above the BrokerError
/// returned to consumers).
#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("bridge thread panicked")]
    ThreadPanic,
    #[error("python init failed: {0}")]
    PythonInit(String),
    #[error("channel closed unexpectedly")]
    ChannelClosed,
}

/// Stream channels exposed by the bridge.
pub struct BridgeStreams {
    pub fills: broadcast::Sender<Fill>,
    pub status: broadcast::Sender<OrderStatusUpdate>,
    pub ticks: broadcast::Sender<Tick>,
    pub errors: broadcast::Sender<BrokerError>,
    pub connection: broadcast::Sender<ConnectionEvent>,
}

impl BridgeStreams {
    fn new(capacity: usize) -> Self {
        let (fills, _) = broadcast::channel(capacity);
        let (status, _) = broadcast::channel(capacity);
        let (ticks, _) = broadcast::channel(capacity);
        let (errors, _) = broadcast::channel(capacity);
        let (connection, _) = broadcast::channel(capacity);
        Self {
            fills,
            status,
            ticks,
            errors,
            connection,
        }
    }
}

/// Handle to the running bridge thread. Cloning is allowed (cheap;
/// internally just clones the std::sync::mpsc::Sender). Dropping the
/// LAST clone signals the bridge thread to shut down.
pub struct Bridge {
    cmd_tx: mpsc::Sender<Command>,
    streams: std::sync::Arc<BridgeStreams>,
    join_handle: std::sync::Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
}

impl Bridge {
    /// Spawn the bridge thread. Returns once the thread has booted but
    /// BEFORE the first connect() call (the Tokio side calls
    /// `Broker::connect` separately).
    ///
    /// This is non-async because thread spawning is sync and the bridge
    /// itself doesn't depend on any tokio runtime context.
    pub fn spawn(cfg: BridgeConfig) -> Result<Self, BridgeError> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let streams = std::sync::Arc::new(BridgeStreams::new(
            corsair_broker_api::STREAM_CAPACITY,
        ));
        let streams_for_thread = streams.clone();

        let join_handle = thread::Builder::new()
            .name("corsair_broker_ibkr_bridge".into())
            .spawn(move || {
                bridge_thread_main(cfg, cmd_rx, streams_for_thread);
            })
            .map_err(|e| BridgeError::PythonInit(format!("spawn failed: {e}")))?;

        Ok(Self {
            cmd_tx,
            streams,
            join_handle: std::sync::Arc::new(std::sync::Mutex::new(Some(join_handle))),
        })
    }

    /// Send a command to the bridge thread. Non-blocking; the response
    /// arrives on the oneshot channel embedded in the command.
    pub fn send(&self, cmd: Command) -> Result<(), BridgeError> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| BridgeError::ChannelClosed)
    }

    pub fn streams(&self) -> &BridgeStreams {
        &self.streams
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // Send shutdown sentinel and wait for the thread to exit.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.cmd_tx.send(Command::Shutdown { reply: tx });
        // Best-effort: wait up to 2s for the thread to acknowledge.
        // The oneshot is async but we're in Drop (sync); we can spin
        // briefly or just close the join_handle.
        if let Ok(mut guard) = self.join_handle.lock() {
            if let Some(h) = guard.take() {
                // Drop blocks; if we're inside a Tokio runtime this
                // would deadlock. Use spawn_blocking equivalent: just
                // detach and let the thread exit on its own. The
                // Shutdown command is sufficient.
                let _ = h.join();
            }
        }
        let _ = rx;
    }
}

// ─── Bridge thread main loop ───────────────────────────────────────

fn bridge_thread_main(
    cfg: BridgeConfig,
    cmd_rx: mpsc::Receiver<Command>,
    streams: std::sync::Arc<BridgeStreams>,
) {
    log::info!(
        "corsair_broker_ibkr bridge: starting (host={}:{}, client_id={})",
        cfg.gateway_host, cfg.gateway_port, cfg.client_id
    );

    // The Python state lives entirely on this thread. We initialize
    // here (idempotent) and use Python::with_gil for every call.
    pyo3::prepare_freethreaded_python();

    // Construct the inner ib_insync state holder. This is opaque from
    // Rust; methods on it are called via Python::with_gil + getattr.
    let py_state = match python::PyState::new(&cfg) {
        Ok(s) => s,
        Err(e) => {
            log::error!("bridge: PyState init failed: {e}");
            return;
        }
    };

    let poll = Duration::from_millis(cfg.poll_interval_ms);

    loop {
        // 1) Try to receive a command.
        match cmd_rx.recv_timeout(poll) {
            Ok(Command::Shutdown { reply }) => {
                log::info!("bridge: shutdown command received");
                let _ = reply.send(());
                break;
            }
            Ok(cmd) => {
                python::dispatch(&py_state, cmd, &streams);
            }
            Err(RecvTimeoutError::Timeout) => {
                // No command — pump asyncio + drain events.
                python::pump(&py_state, &streams);
            }
            Err(RecvTimeoutError::Disconnected) => {
                log::info!("bridge: cmd channel closed; exiting");
                break;
            }
        }
    }

    // Clean up Python state under the GIL.
    drop(py_state);
    log::info!("corsair_broker_ibkr bridge: stopped");
}

// ─── Python state holder + dispatch ────────────────────────────────
//
// Encapsulated in a submodule so `bridge.rs` stays focused on the
// thread-loop architecture.

mod python {
    use super::*;
    use crate::commands::Command;
    use crate::conversion;
    use corsair_broker_api::{
        AccountSnapshot, Contract, ContractKind, FutureQuery, ModifyOrderReq, OpenOrder,
        OptionQuery, OrderId, OrderType, PlaceOrderReq, Position, Side, TickStreamHandle,
        TickSubscription, TimeInForce,
    };
    use corsair_broker_api::events::Fill;
    use pyo3::prelude::*;
    use pyo3::types::{PyAny, PyDict, PyList, PyTuple};
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Python-side state owned by the bridge thread. PyObject is Send
    /// under GIL acquisition rules; we only touch these fields under
    /// `Python::with_gil` from the bridge thread.
    pub struct PyState {
        pub ib: PyObject,
        pub asyncio_loop: PyObject,
        /// Python list owning queued events. ib_insync callbacks
        /// append to this; `pump` drains it and forwards to broadcast
        /// channels.
        pub event_queue: PyObject,
        /// The compiled event-registrar module (kept alive for ticker
        /// callback registration on subscribe_ticks).
        pub registrar_mod: PyObject,
        /// Strong reference to the registered callbacks so Python
        /// doesn't garbage-collect them while ib_insync is still
        /// holding weak references via `event +=` registration.
        pub _ib_callbacks: PyObject,
        /// Cache of qualified ib_insync Contract objects, keyed by
        /// our InstrumentId (== IBKR conId). Populated by the
        /// qualify_* commands; consumed by place_order so we don't
        /// reconstruct the Contract field-by-field on every order.
        pub contract_cache: RefCell<HashMap<u64, PyObject>>,
        /// Cache of live Trade objects keyed by OrderId — used to
        /// resolve the Trade for cancel_order / modify_order.
        pub trade_cache: RefCell<HashMap<u64, PyObject>>,
        /// Cache of Ticker objects keyed by TickStreamHandle for
        /// later unsubscribe.
        pub ticker_cache: RefCell<HashMap<u64, PyObject>>,
        /// Strong references to per-ticker update callbacks so Python
        /// doesn't garbage-collect them.
        pub ticker_callbacks: RefCell<HashMap<u64, PyObject>>,
        /// Monotonic counter for issuing TickStreamHandles.
        pub next_handle: RefCell<u64>,
        /// Account selector (FA accounts require this on every order).
        pub account: String,
        /// Connection target — captured for `connect` to use.
        pub gateway_host: String,
        pub gateway_port: u16,
        pub client_id: i32,
    }

    /// Python source for the event-callback registrar. Defines six
    /// closures that append (tag, payload) tuples to the shared event
    /// queue. Returning the list keeps a Python reference alive — if
    /// we let it drop, ib_insync's event listeners get GCed.
    const EVENT_REGISTRAR_SRC: &str = r#"
def register(ib, queue):
    def on_fill(trade, fill):
        queue.append(('fill', fill))
    def on_status(trade):
        queue.append(('status', trade))
    def on_error(reqId, errorCode, errorString, contract):
        queue.append(('error', (reqId, errorCode, errorString, contract)))
    def on_connected():
        queue.append(('connect', None))
    def on_disconnected():
        queue.append(('disconnect', None))
    ib.execDetailsEvent += on_fill
    ib.orderStatusEvent += on_status
    ib.errorEvent += on_error
    ib.connectedEvent += on_connected
    ib.disconnectedEvent += on_disconnected
    # Return references so they don't get garbage-collected.
    return [on_fill, on_status, on_error, on_connected, on_disconnected]

def attach_ticker(ticker, queue):
    def on_update(t):
        queue.append(('tick', t))
    ticker.updateEvent += on_update
    return on_update
"#;

    impl PyState {
        pub fn new(cfg: &BridgeConfig) -> Result<Self, BridgeError> {
            Python::with_gil(|py| -> Result<Self, BridgeError> {
                let asyncio = py.import_bound("asyncio").map_err(|e| {
                    BridgeError::PythonInit(format!("import asyncio: {e}"))
                })?;
                let ib_insync = py.import_bound("ib_insync").map_err(|e| {
                    BridgeError::PythonInit(format!("import ib_insync: {e}"))
                })?;

                // Create a fresh asyncio loop on this thread and install
                // it as the thread's current loop. asyncio's
                // get_event_loop() only works on threads that already
                // have a loop (the main thread by default); for our
                // dedicated bridge thread we create one explicitly.
                //
                // We DON'T call ib_insync.util.startLoop() here. That
                // function applies nest_asyncio to allow nested
                // run_until_complete from inside async code (e.g. from
                // a Jupyter cell). For our bridge — where Rust drives
                // run_until_complete from a sync context — nest_asyncio
                // creates a loop-binding mismatch (Future "attached to
                // different loop" errors). The loop we create here is
                // the only loop ib_insync ever sees; ib_insync's
                // internal futures bind to it correctly.
                let new_loop = asyncio
                    .call_method0("new_event_loop")
                    .map_err(|e| {
                        BridgeError::PythonInit(format!("new_event_loop: {e}"))
                    })?;
                asyncio
                    .call_method1("set_event_loop", (&new_loop,))
                    .map_err(|e| {
                        BridgeError::PythonInit(format!("set_event_loop: {e}"))
                    })?;

                let asyncio_loop: PyObject = new_loop.into();

                let ib_class = ib_insync
                    .getattr("IB")
                    .map_err(|e| BridgeError::PythonInit(format!("getattr IB: {e}")))?;
                let ib: PyObject = ib_class
                    .call0()
                    .map_err(|e| BridgeError::PythonInit(format!("IB(): {e}")))?
                    .into();

                let event_queue: PyObject = PyList::empty_bound(py).into();

                // Compile the event registrar module and attach
                // callbacks to the IB instance.
                let registrar_mod = pyo3::types::PyModule::from_code_bound(
                    py,
                    EVENT_REGISTRAR_SRC,
                    "corsair_event_registrar.py",
                    "corsair_event_registrar",
                )
                .map_err(|e| {
                    BridgeError::PythonInit(format!("compile registrar: {e}"))
                })?;
                let register = registrar_mod.getattr("register").map_err(|e| {
                    BridgeError::PythonInit(format!("getattr register: {e}"))
                })?;
                let callbacks = register
                    .call1((ib.bind(py), event_queue.bind(py)))
                    .map_err(|e| {
                        BridgeError::PythonInit(format!("register(...): {e}"))
                    })?;

                Ok(PyState {
                    ib,
                    asyncio_loop,
                    event_queue,
                    registrar_mod: registrar_mod.into(),
                    _ib_callbacks: callbacks.into(),
                    contract_cache: RefCell::new(HashMap::new()),
                    trade_cache: RefCell::new(HashMap::new()),
                    ticker_cache: RefCell::new(HashMap::new()),
                    ticker_callbacks: RefCell::new(HashMap::new()),
                    next_handle: RefCell::new(1),
                    account: cfg.account.clone(),
                    gateway_host: cfg.gateway_host.clone(),
                    gateway_port: cfg.gateway_port,
                    client_id: cfg.client_id,
                })
            })
        }
    }

    /// Map a PyErr to BrokerError::Protocol for surface across the bridge.
    fn pyerr_to_broker(err: PyErr, context: &str) -> BrokerError {
        BrokerError::Protocol {
            code: None,
            message: format!("{context}: {err}"),
        }
    }

    /// Run an async ib_insync coroutine to completion on the bridge's
    /// event loop. Returns the awaited value.
    fn run_until_complete<'py>(
        loop_obj: &Bound<'py, PyAny>,
        coro: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        loop_obj.call_method1("run_until_complete", (coro,))
    }

    // ─── Connect / Disconnect ─────────────────────────────────────

    fn cmd_connect(state: &PyState) -> Result<(), BrokerError> {
        Python::with_gil(|py| -> Result<(), BrokerError> {
            let ib = state.ib.bind(py);

            // Use ib_insync's SYNC connect() rather than connectAsync.
            // Sync version internally manages its own loop via
            // ib.run() — works correctly when called from a non-async
            // context like ours (Rust → with_gil → ib.connect).
            //
            // Lean bypass: ib.connect with readonly=False (default)
            // does the four lean reqs; the v1.4 full bootstrap
            // (positions + openOrders + accountUpdates) is what we
            // want and ib_insync's connect handles automatically.
            let kwargs = PyDict::new_bound(py);
            kwargs
                .set_item("host", &state.gateway_host)
                .map_err(|e| pyerr_to_broker(e, "set host"))?;
            kwargs
                .set_item("port", state.gateway_port)
                .map_err(|e| pyerr_to_broker(e, "set port"))?;
            kwargs
                .set_item("clientId", state.client_id)
                .map_err(|e| pyerr_to_broker(e, "set clientId"))?;
            if !state.account.is_empty() {
                kwargs
                    .set_item("account", &state.account)
                    .map_err(|e| pyerr_to_broker(e, "set account"))?;
            }
            // Connection timeout (seconds) — IBKR docs recommend ≥30s.
            kwargs
                .set_item("timeout", 30)
                .map_err(|e| pyerr_to_broker(e, "set timeout"))?;
            ib.call_method("connect", (), Some(&kwargs))
                .map_err(|e| pyerr_to_broker(e, "connect"))?;

            // FA accounts require reqAutoOpenOrders for clientId=0
            // (CLAUDE.md §1). Sync version is fine.
            if state.client_id == 0 {
                let _ = ib.call_method1("reqAutoOpenOrders", (true,));
            }

            log::warn!(
                "corsair_broker_ibkr: connected to {}:{} as clientId={} account={}",
                state.gateway_host,
                state.gateway_port,
                state.client_id,
                if state.account.is_empty() { "(default)" } else { &state.account }
            );
            Ok(())
        })
    }

    fn cmd_disconnect(state: &PyState) -> Result<(), BrokerError> {
        Python::with_gil(|py| -> Result<(), BrokerError> {
            let ib = state.ib.bind(py);
            ib.call_method0("disconnect")
                .map_err(|e| pyerr_to_broker(e, "disconnect"))?;
            Ok(())
        })
    }

    // ─── Order placement ──────────────────────────────────────────

    fn build_ib_order<'py>(
        py: Python<'py>,
        state: &PyState,
        req: &PlaceOrderReq,
    ) -> Result<Bound<'py, PyAny>, BrokerError> {
        let ib_insync = py
            .import_bound("ib_insync")
            .map_err(|e| pyerr_to_broker(e, "import ib_insync"))?;

        let action = conversion::side_str(req.side);
        let qty = req.qty as f64;
        let kwargs = PyDict::new_bound(py);
        kwargs
            .set_item("tif", conversion::tif_str(req.tif))
            .map_err(|e| pyerr_to_broker(e, "set tif"))?;
        // Account: FA accounts require this. Use the request's account
        // override if provided, else the bridge's configured account.
        let account_override = req.account.as_deref().unwrap_or("");
        let account = if !account_override.is_empty() {
            account_override.to_string()
        } else {
            state.account.clone()
        };
        if !account.is_empty() {
            kwargs
                .set_item("account", account)
                .map_err(|e| pyerr_to_broker(e, "set account"))?;
        }
        if !req.client_order_ref.is_empty() {
            kwargs
                .set_item("orderRef", &req.client_order_ref)
                .map_err(|e| pyerr_to_broker(e, "set orderRef"))?;
        }
        if let (TimeInForce::Gtd, Some(dt)) = (req.tif, req.gtd_until_utc) {
            kwargs
                .set_item("goodTillDate", conversion::format_gtd(dt))
                .map_err(|e| pyerr_to_broker(e, "set goodTillDate"))?;
        }

        // Construct the order class.
        let order = match req.order_type {
            OrderType::Limit => {
                let price = req.price.ok_or_else(|| {
                    BrokerError::InvalidRequest(
                        "OrderType::Limit requires price".into(),
                    )
                })?;
                let cls = ib_insync
                    .getattr("LimitOrder")
                    .map_err(|e| pyerr_to_broker(e, "getattr LimitOrder"))?;
                cls.call((action, qty, price), Some(&kwargs))
                    .map_err(|e| pyerr_to_broker(e, "LimitOrder()"))?
            }
            OrderType::Market => {
                let cls = ib_insync
                    .getattr("MarketOrder")
                    .map_err(|e| pyerr_to_broker(e, "getattr MarketOrder"))?;
                cls.call((action, qty), Some(&kwargs))
                    .map_err(|e| pyerr_to_broker(e, "MarketOrder()"))?
            }
            OrderType::StopLimit { stop_price } => {
                let price = req.price.ok_or_else(|| {
                    BrokerError::InvalidRequest(
                        "OrderType::StopLimit requires price".into(),
                    )
                })?;
                let cls = ib_insync
                    .getattr("StopLimitOrder")
                    .map_err(|e| pyerr_to_broker(e, "getattr StopLimitOrder"))?;
                cls.call((action, qty, price, stop_price), Some(&kwargs))
                    .map_err(|e| pyerr_to_broker(e, "StopLimitOrder()"))?
            }
        };
        Ok(order)
    }

    /// Construct (or retrieve from cache) the ib_insync Contract for
    /// a given Contract value type. Caches by instrument_id.
    fn ib_contract_for<'py>(
        py: Python<'py>,
        state: &PyState,
        c: &Contract,
    ) -> Result<Bound<'py, PyAny>, BrokerError> {
        // Cache hit
        if let Some(cached) = state.contract_cache.borrow().get(&c.instrument_id.0) {
            return Ok(cached.bind(py).clone());
        }

        let ib_insync = py
            .import_bound("ib_insync")
            .map_err(|e| pyerr_to_broker(e, "import ib_insync"))?;
        let kwargs = PyDict::new_bound(py);
        kwargs
            .set_item("conId", c.instrument_id.0 as i64)
            .map_err(|e| pyerr_to_broker(e, "set conId"))?;
        kwargs
            .set_item("symbol", &c.symbol)
            .map_err(|e| pyerr_to_broker(e, "set symbol"))?;
        kwargs
            .set_item("localSymbol", &c.local_symbol)
            .map_err(|e| pyerr_to_broker(e, "set localSymbol"))?;
        kwargs
            .set_item(
                "lastTradeDateOrContractMonth",
                conversion::format_yyyymmdd(c.expiry),
            )
            .map_err(|e| pyerr_to_broker(e, "set expiry"))?;
        kwargs
            .set_item("multiplier", format!("{}", c.multiplier as i64))
            .map_err(|e| pyerr_to_broker(e, "set multiplier"))?;
        kwargs
            .set_item("exchange", conversion::format_exchange(c.exchange))
            .map_err(|e| pyerr_to_broker(e, "set exchange"))?;
        kwargs
            .set_item("currency", conversion::format_currency(c.currency))
            .map_err(|e| pyerr_to_broker(e, "set currency"))?;

        let constructed = match c.kind {
            ContractKind::Future => {
                let cls = ib_insync
                    .getattr("Future")
                    .map_err(|e| pyerr_to_broker(e, "getattr Future"))?;
                cls.call((), Some(&kwargs))
                    .map_err(|e| pyerr_to_broker(e, "Future()"))?
            }
            ContractKind::Option => {
                if let Some(strike) = c.strike {
                    kwargs
                        .set_item("strike", strike)
                        .map_err(|e| pyerr_to_broker(e, "set strike"))?;
                }
                if let Some(right) = c.right {
                    kwargs
                        .set_item("right", right.as_char().to_string())
                        .map_err(|e| pyerr_to_broker(e, "set right"))?;
                }
                let cls = ib_insync
                    .getattr("Option")
                    .map_err(|e| pyerr_to_broker(e, "getattr Option"))?;
                cls.call((), Some(&kwargs))
                    .map_err(|e| pyerr_to_broker(e, "Option()"))?
            }
        };

        // Cache for later orders.
        state
            .contract_cache
            .borrow_mut()
            .insert(c.instrument_id.0, constructed.clone().into());
        Ok(constructed)
    }

    fn cmd_place_order(
        state: &PyState,
        req: PlaceOrderReq,
    ) -> Result<OrderId, BrokerError> {
        Python::with_gil(|py| -> Result<OrderId, BrokerError> {
            let ib = state.ib.bind(py);
            let contract = ib_contract_for(py, state, &req.contract)?;
            let order = build_ib_order(py, state, &req)?;
            let trade = ib
                .call_method1("placeOrder", (contract, order))
                .map_err(|e| pyerr_to_broker(e, "placeOrder"))?;
            // Trade.order.orderId
            let order_obj = trade
                .getattr("order")
                .map_err(|e| pyerr_to_broker(e, "getattr trade.order"))?;
            let order_id_i64: i64 = order_obj
                .getattr("orderId")
                .map_err(|e| pyerr_to_broker(e, "getattr orderId"))?
                .extract()
                .map_err(|e| pyerr_to_broker(e, "extract orderId"))?;
            let oid = order_id_i64 as u64;
            // Cache the Trade for cancel/modify.
            state
                .trade_cache
                .borrow_mut()
                .insert(oid, trade.clone().into());
            Ok(OrderId(oid))
        })
    }

    fn cmd_cancel_order(state: &PyState, id: OrderId) -> Result<(), BrokerError> {
        Python::with_gil(|py| -> Result<(), BrokerError> {
            let ib = state.ib.bind(py);
            let cache = state.trade_cache.borrow();
            let trade = cache.get(&id.0).ok_or_else(|| {
                BrokerError::InvalidRequest(format!(
                    "no cached Trade for {id}; cancel after place_order required"
                ))
            })?;
            let order_obj = trade
                .bind(py)
                .getattr("order")
                .map_err(|e| pyerr_to_broker(e, "getattr trade.order"))?;
            ib.call_method1("cancelOrder", (order_obj,))
                .map_err(|e| pyerr_to_broker(e, "cancelOrder"))?;
            Ok(())
        })
    }

    fn cmd_modify_order(
        state: &PyState,
        id: OrderId,
        req: ModifyOrderReq,
    ) -> Result<(), BrokerError> {
        Python::with_gil(|py| -> Result<(), BrokerError> {
            let ib = state.ib.bind(py);
            let cache = state.trade_cache.borrow();
            let trade = cache.get(&id.0).ok_or_else(|| {
                BrokerError::InvalidRequest(format!(
                    "no cached Trade for {id}; modify after place_order required"
                ))
            })?;
            let trade_bound = trade.bind(py);
            let order_obj = trade_bound
                .getattr("order")
                .map_err(|e| pyerr_to_broker(e, "getattr trade.order"))?;
            let contract = trade_bound
                .getattr("contract")
                .map_err(|e| pyerr_to_broker(e, "getattr trade.contract"))?;
            // Modify: update order fields and re-place. ib_insync's
            // pattern for amend is to mutate the order and call
            // placeOrder again with the same orderId.
            if let Some(p) = req.price {
                let _ = order_obj.setattr("lmtPrice", p);
            }
            if let Some(q) = req.qty {
                let _ = order_obj.setattr("totalQuantity", q as f64);
            }
            if let Some(dt) = req.gtd_until_utc {
                let _ = order_obj.setattr("goodTillDate", conversion::format_gtd(dt));
            }
            ib.call_method1("placeOrder", (contract, order_obj))
                .map_err(|e| pyerr_to_broker(e, "placeOrder (modify)"))?;
            Ok(())
        })
    }

    // ─── State queries ────────────────────────────────────────────

    fn cmd_positions(state: &PyState) -> Result<Vec<Position>, BrokerError> {
        Python::with_gil(|py| -> Result<Vec<Position>, BrokerError> {
            let ib = state.ib.bind(py);
            let positions_list = ib
                .call_method0("positions")
                .map_err(|e| pyerr_to_broker(e, "positions"))?;
            let mut out = Vec::new();
            let iter = positions_list
                .iter()
                .map_err(|e| pyerr_to_broker(e, "iter positions"))?;
            for p in iter {
                let pos = p.map_err(|e| pyerr_to_broker(e, "iter step positions"))?;
                match conversion::position_from_py(&pos) {
                    Ok(parsed) => out.push(parsed),
                    Err(e) => log::warn!("skipping unparseable position: {e:?}"),
                }
            }
            Ok(out)
        })
    }

    fn cmd_account_values(state: &PyState) -> Result<AccountSnapshot, BrokerError> {
        Python::with_gil(|py| -> Result<AccountSnapshot, BrokerError> {
            let ib = state.ib.bind(py);
            let rows = ib
                .call_method0("accountValues")
                .map_err(|e| pyerr_to_broker(e, "accountValues"))?;
            conversion::account_snapshot_from_rows(&rows)
        })
    }

    fn cmd_open_orders(state: &PyState) -> Result<Vec<OpenOrder>, BrokerError> {
        Python::with_gil(|py| -> Result<Vec<OpenOrder>, BrokerError> {
            let ib = state.ib.bind(py);
            let trades = ib
                .call_method0("openTrades")
                .map_err(|e| pyerr_to_broker(e, "openTrades"))?;
            let mut out = Vec::new();
            let iter = trades
                .iter()
                .map_err(|e| pyerr_to_broker(e, "iter openTrades"))?;
            for t in iter {
                let trade = t.map_err(|e| pyerr_to_broker(e, "iter step trade"))?;
                let contract_obj = match trade.getattr("contract") {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let contract = match conversion::contract_from_py(&contract_obj) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let order_obj = match trade.getattr("order") {
                    Ok(o) => o,
                    Err(_) => continue,
                };
                let oid: i64 = order_obj
                    .getattr("orderId")
                    .ok()
                    .and_then(|v| v.extract().ok())
                    .unwrap_or(0);
                let action: String = order_obj
                    .getattr("action")
                    .ok()
                    .and_then(|v| v.extract().ok())
                    .unwrap_or_default();
                let side = match action.as_str() {
                    "BUY" => Side::Buy,
                    _ => Side::Sell,
                };
                let qty: f64 = order_obj
                    .getattr("totalQuantity")
                    .ok()
                    .and_then(|v| v.extract().ok())
                    .unwrap_or(0.0);
                let price: Option<f64> = order_obj
                    .getattr("lmtPrice")
                    .ok()
                    .and_then(|v| v.extract().ok());
                let tif_str: String = order_obj
                    .getattr("tif")
                    .ok()
                    .and_then(|v| v.extract().ok())
                    .unwrap_or_default();
                let order_status_obj = match trade.getattr("orderStatus") {
                    Ok(o) => o,
                    Err(_) => continue,
                };
                let status_str: String = order_status_obj
                    .getattr("status")
                    .ok()
                    .and_then(|v| v.extract().ok())
                    .unwrap_or_default();
                let filled: f64 = order_status_obj
                    .getattr("filled")
                    .ok()
                    .and_then(|v| v.extract().ok())
                    .unwrap_or(0.0);
                let remaining: f64 = order_status_obj
                    .getattr("remaining")
                    .ok()
                    .and_then(|v| v.extract().ok())
                    .unwrap_or(0.0);
                let order_ref: String = order_obj
                    .getattr("orderRef")
                    .ok()
                    .and_then(|v| v.extract().ok())
                    .unwrap_or_default();
                out.push(OpenOrder {
                    order_id: OrderId(oid as u64),
                    contract,
                    side,
                    qty: qty as u32,
                    order_type: OrderType::Limit,
                    price,
                    tif: conversion::parse_tif(&tif_str),
                    gtd_until_utc: None,
                    client_order_ref: order_ref,
                    status: conversion::parse_order_status(&status_str),
                    filled_qty: filled as u32,
                    remaining_qty: remaining as u32,
                });
            }
            Ok(out)
        })
    }

    fn cmd_recent_fills(
        state: &PyState,
        since_ns: u64,
    ) -> Result<Vec<Fill>, BrokerError> {
        Python::with_gil(|py| -> Result<Vec<Fill>, BrokerError> {
            let ib = state.ib.bind(py);
            let ib_insync = py
                .import_bound("ib_insync")
                .map_err(|e| pyerr_to_broker(e, "import ib_insync"))?;
            let exec_filter_cls = ib_insync
                .getattr("ExecutionFilter")
                .map_err(|e| pyerr_to_broker(e, "getattr ExecutionFilter"))?;
            let exec_filter = exec_filter_cls
                .call0()
                .map_err(|e| pyerr_to_broker(e, "ExecutionFilter()"))?;
            // Sync version — ib.reqExecutions internally runs the loop.
            let result = ib
                .call_method1("reqExecutions", (exec_filter,))
                .map_err(|e| pyerr_to_broker(e, "reqExecutions"))?;
            let mut out = Vec::new();
            let iter = result
                .iter()
                .map_err(|e| pyerr_to_broker(e, "iter fills"))?;
            for f in iter {
                let fill = f.map_err(|e| pyerr_to_broker(e, "iter step fill"))?;
                match conversion::fill_from_py(&fill) {
                    Ok(parsed) if parsed.timestamp_ns >= since_ns => out.push(parsed),
                    Ok(_) => continue, // older than cutoff
                    Err(e) => log::warn!("skipping unparseable fill: {e:?}"),
                }
            }
            Ok(out)
        })
    }

    // ─── Contract resolution ─────────────────────────────────────

    fn cmd_qualify_future(
        state: &PyState,
        q: FutureQuery,
    ) -> Result<Contract, BrokerError> {
        Python::with_gil(|py| -> Result<Contract, BrokerError> {
            let ib = state.ib.bind(py);
            let ib_insync = py
                .import_bound("ib_insync")
                .map_err(|e| pyerr_to_broker(e, "import ib_insync"))?;
            let kwargs = PyDict::new_bound(py);
            kwargs
                .set_item("symbol", &q.symbol)
                .map_err(|e| pyerr_to_broker(e, "set symbol"))?;
            kwargs
                .set_item(
                    "lastTradeDateOrContractMonth",
                    conversion::format_yyyymmdd(q.expiry),
                )
                .map_err(|e| pyerr_to_broker(e, "set expiry"))?;
            kwargs
                .set_item("exchange", conversion::format_exchange(q.exchange))
                .map_err(|e| pyerr_to_broker(e, "set exchange"))?;
            kwargs
                .set_item("currency", conversion::format_currency(q.currency))
                .map_err(|e| pyerr_to_broker(e, "set currency"))?;
            let cls = ib_insync
                .getattr("Future")
                .map_err(|e| pyerr_to_broker(e, "getattr Future"))?;
            let unqualified = cls
                .call((), Some(&kwargs))
                .map_err(|e| pyerr_to_broker(e, "Future()"))?;
            // Sync version — ib.qualifyContracts internally runs the loop.
            let result = ib
                .call_method1("qualifyContracts", (&unqualified,))
                .map_err(|e| pyerr_to_broker(e, "qualifyContracts"))?;
            let qualified_obj = result
                .get_item(0)
                .map_err(|_| BrokerError::ContractNotFound(format!("Future {}", q.symbol)))?;
            let parsed = conversion::contract_from_py(&qualified_obj)?;
            state
                .contract_cache
                .borrow_mut()
                .insert(parsed.instrument_id.0, qualified_obj.into());
            Ok(parsed)
        })
    }

    fn cmd_qualify_option(
        state: &PyState,
        q: OptionQuery,
    ) -> Result<Contract, BrokerError> {
        Python::with_gil(|py| -> Result<Contract, BrokerError> {
            let ib = state.ib.bind(py);
            let ib_insync = py
                .import_bound("ib_insync")
                .map_err(|e| pyerr_to_broker(e, "import ib_insync"))?;
            let kwargs = PyDict::new_bound(py);
            kwargs
                .set_item("symbol", &q.symbol)
                .map_err(|e| pyerr_to_broker(e, "set symbol"))?;
            kwargs
                .set_item(
                    "lastTradeDateOrContractMonth",
                    conversion::format_yyyymmdd(q.expiry),
                )
                .map_err(|e| pyerr_to_broker(e, "set expiry"))?;
            kwargs
                .set_item("strike", q.strike)
                .map_err(|e| pyerr_to_broker(e, "set strike"))?;
            kwargs
                .set_item("right", q.right.as_char().to_string())
                .map_err(|e| pyerr_to_broker(e, "set right"))?;
            kwargs
                .set_item("exchange", conversion::format_exchange(q.exchange))
                .map_err(|e| pyerr_to_broker(e, "set exchange"))?;
            kwargs
                .set_item("currency", conversion::format_currency(q.currency))
                .map_err(|e| pyerr_to_broker(e, "set currency"))?;
            kwargs
                .set_item("multiplier", format!("{}", q.multiplier as i64))
                .map_err(|e| pyerr_to_broker(e, "set multiplier"))?;
            let cls = ib_insync
                .getattr("Option")
                .map_err(|e| pyerr_to_broker(e, "getattr Option"))?;
            let unqualified = cls
                .call((), Some(&kwargs))
                .map_err(|e| pyerr_to_broker(e, "Option()"))?;
            let result = ib
                .call_method1("qualifyContracts", (&unqualified,))
                .map_err(|e| pyerr_to_broker(e, "qualifyContracts"))?;
            let qualified_obj = result
                .get_item(0)
                .map_err(|_| BrokerError::ContractNotFound(format!("Option {} {}", q.symbol, q.expiry)))?;
            let parsed = conversion::contract_from_py(&qualified_obj)?;
            state
                .contract_cache
                .borrow_mut()
                .insert(parsed.instrument_id.0, qualified_obj.into());
            Ok(parsed)
        })
    }

    fn cmd_list_chain(
        state: &PyState,
        q: corsair_broker_api::ChainQuery,
    ) -> Result<Vec<Contract>, BrokerError> {
        Python::with_gil(|py| -> Result<Vec<Contract>, BrokerError> {
            let ib = state.ib.bind(py);
            let ib_insync = py
                .import_bound("ib_insync")
                .map_err(|e| pyerr_to_broker(e, "import ib_insync"))?;
            let kwargs = PyDict::new_bound(py);
            kwargs
                .set_item("symbol", &q.symbol)
                .map_err(|e| pyerr_to_broker(e, "set symbol"))?;
            kwargs
                .set_item("exchange", conversion::format_exchange(q.exchange))
                .map_err(|e| pyerr_to_broker(e, "set exchange"))?;
            kwargs
                .set_item("currency", conversion::format_currency(q.currency))
                .map_err(|e| pyerr_to_broker(e, "set currency"))?;
            let cls = ib_insync
                .getattr("Future")
                .map_err(|e| pyerr_to_broker(e, "getattr Future"))?;
            let stub = cls
                .call((), Some(&kwargs))
                .map_err(|e| pyerr_to_broker(e, "Future() stub"))?;
            // Sync version — ib.reqContractDetails internally runs the loop.
            let details_list = ib
                .call_method1("reqContractDetails", (stub,))
                .map_err(|e| pyerr_to_broker(e, "reqContractDetails"))?;

            let cutoff = q
                .min_expiry
                .map(conversion::format_yyyymmdd)
                .unwrap_or_default();
            let mut out = Vec::new();
            let iter = details_list
                .iter()
                .map_err(|e| pyerr_to_broker(e, "iter details"))?;
            for d in iter {
                let detail =
                    d.map_err(|e| pyerr_to_broker(e, "iter step detail"))?;
                let contract_obj = match detail.getattr("contract") {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let parsed = match conversion::contract_from_py(&contract_obj) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if !cutoff.is_empty() {
                    let exp_str = conversion::format_yyyymmdd(parsed.expiry);
                    if exp_str < cutoff {
                        continue;
                    }
                }
                state
                    .contract_cache
                    .borrow_mut()
                    .insert(parsed.instrument_id.0, contract_obj.into());
                out.push(parsed);
            }
            Ok(out)
        })
    }

    // ─── Market data ─────────────────────────────────────────────

    fn cmd_subscribe_ticks(
        state: &PyState,
        sub: TickSubscription,
    ) -> Result<TickStreamHandle, BrokerError> {
        Python::with_gil(|py| -> Result<TickStreamHandle, BrokerError> {
            let ib = state.ib.bind(py);
            let cache = state.contract_cache.borrow();
            let contract = cache.get(&sub.instrument_id.0).ok_or_else(|| {
                BrokerError::ContractNotFound(format!(
                    "no qualified contract for {} — qualify first",
                    sub.instrument_id
                ))
            })?;
            let ticker = ib
                .call_method1(
                    "reqMktData",
                    (contract.bind(py).clone(), "", false, false),
                )
                .map_err(|e| pyerr_to_broker(e, "reqMktData"))?;

            // Attach the per-ticker updateEvent callback so tick
            // updates push into the same event_queue.
            let attach = state
                .registrar_mod
                .bind(py)
                .getattr("attach_ticker")
                .map_err(|e| pyerr_to_broker(e, "getattr attach_ticker"))?;
            let callback = attach
                .call1((&ticker, state.event_queue.bind(py)))
                .map_err(|e| pyerr_to_broker(e, "attach_ticker(...)"))?;

            let mut next = state.next_handle.borrow_mut();
            let h = *next;
            *next += 1;
            state.ticker_cache.borrow_mut().insert(h, ticker.into());
            state
                .ticker_callbacks
                .borrow_mut()
                .insert(h, callback.into());
            Ok(TickStreamHandle(h))
        })
    }

    fn cmd_unsubscribe_ticks(
        state: &PyState,
        h: TickStreamHandle,
    ) -> Result<(), BrokerError> {
        Python::with_gil(|py| -> Result<(), BrokerError> {
            let ib = state.ib.bind(py);
            let mut tickers = state.ticker_cache.borrow_mut();
            if let Some(ticker) = tickers.remove(&h.0) {
                let contract = ticker
                    .bind(py)
                    .getattr("contract")
                    .map_err(|e| pyerr_to_broker(e, "getattr ticker.contract"))?;
                ib.call_method1("cancelMktData", (contract,))
                    .map_err(|e| pyerr_to_broker(e, "cancelMktData"))?;
            }
            // Drop the callback reference; ib_insync's weak-ref to it
            // stops firing once Python GCs the function.
            state.ticker_callbacks.borrow_mut().remove(&h.0);
            Ok(())
        })
    }

    /// Drain pending Python-side events onto Rust broadcast channels.
    /// Phase 2.5 implementation.
    pub fn pump(state: &PyState, streams: &BridgeStreams) {
        Python::with_gil(|py| {
            let queue = state.event_queue.bind(py);
            // Atomically swap with a fresh empty list to avoid holding
            // the GIL while broadcasting.
            let len: usize = queue
                .call_method0("__len__")
                .ok()
                .and_then(|v| v.extract().ok())
                .unwrap_or(0);
            if len == 0 {
                return;
            }
            // Pop all items into a Rust Vec while we hold the GIL.
            let mut items: Vec<(String, PyObject)> = Vec::with_capacity(len);
            for _ in 0..len {
                if let Ok(item) = queue.call_method0("pop") {
                    if let Ok(tag) = item
                        .get_item(0)
                        .and_then(|v| v.extract::<String>())
                    {
                        if let Ok(payload) = item.get_item(1) {
                            items.push((tag, payload.into()));
                        }
                    }
                }
            }
            // Now broadcast each item.
            for (tag, payload) in items {
                let p = payload.bind(py);
                match tag.as_str() {
                    "fill" => {
                        if let Ok(fill) = conversion::fill_from_py(&p) {
                            let _ = streams.fills.send(fill);
                        }
                    }
                    "status" => {
                        if let Ok(update) = order_status_from_py(&p) {
                            let _ = streams.status.send(update);
                        }
                    }
                    "tick" => {
                        if let Ok(tick) = tick_from_py(&p) {
                            let _ = streams.ticks.send(tick);
                        }
                    }
                    "error" => {
                        if let Ok(err) = error_from_py(&p) {
                            let _ = streams.errors.send(err);
                        }
                    }
                    "connect" => {
                        let _ = streams.connection.send(connection_event_from_py(
                            &p, true,
                        ));
                    }
                    "disconnect" => {
                        let _ = streams.connection.send(connection_event_from_py(
                            &p, false,
                        ));
                    }
                    other => {
                        log::warn!("bridge pump: unknown event tag {other}");
                    }
                }
            }
        });
    }

    // ─── Phase 2.5 helpers: parse payloads from Python events ────

    fn order_status_from_py(
        obj: &Bound<'_, PyAny>,
    ) -> Result<corsair_broker_api::OrderStatusUpdate, BrokerError> {
        // Payload shape: a Trade object (ib_insync emits the Trade
        // on orderStatusEvent).
        let order_obj = obj
            .getattr("order")
            .map_err(|e| pyerr_to_broker(e, "getattr trade.order"))?;
        let oid: i64 = order_obj
            .getattr("orderId")
            .ok()
            .and_then(|v| v.extract().ok())
            .unwrap_or(0);
        let order_status_obj = obj
            .getattr("orderStatus")
            .map_err(|e| pyerr_to_broker(e, "getattr trade.orderStatus"))?;
        let status_str: String = order_status_obj
            .getattr("status")
            .ok()
            .and_then(|v| v.extract().ok())
            .unwrap_or_default();
        let filled: f64 = order_status_obj
            .getattr("filled")
            .ok()
            .and_then(|v| v.extract().ok())
            .unwrap_or(0.0);
        let remaining: f64 = order_status_obj
            .getattr("remaining")
            .ok()
            .and_then(|v| v.extract().ok())
            .unwrap_or(0.0);
        let avg_price: f64 = order_status_obj
            .getattr("avgFillPrice")
            .ok()
            .and_then(|v| v.extract().ok())
            .unwrap_or(0.0);
        let last_price: Option<f64> = order_status_obj
            .getattr("lastFillPrice")
            .ok()
            .and_then(|v| v.extract().ok())
            .filter(|v: &f64| v.is_finite() && *v > 0.0);
        Ok(corsair_broker_api::OrderStatusUpdate {
            order_id: OrderId(oid as u64),
            status: conversion::parse_order_status(&status_str),
            filled_qty: filled as u32,
            remaining_qty: remaining as u32,
            avg_fill_price: avg_price,
            last_fill_price: last_price,
            timestamp_ns: conversion::now_ns(),
            reject_reason: None,
        })
    }

    fn tick_from_py(
        obj: &Bound<'_, PyAny>,
    ) -> Result<corsair_broker_api::Tick, BrokerError> {
        // Payload shape: a Ticker object. We extract the most-recently
        // updated price/size pair. ib_insync's Ticker has bid/ask/last/
        // bidSize/askSize fields.
        use corsair_broker_api::{InstrumentId, TickKind};
        let contract_obj = obj
            .getattr("contract")
            .map_err(|e| pyerr_to_broker(e, "getattr ticker.contract"))?;
        let con_id: i64 = contract_obj
            .getattr("conId")
            .ok()
            .and_then(|v| v.extract().ok())
            .unwrap_or(0);
        let bid: f64 = obj.getattr("bid").ok().and_then(|v| v.extract().ok()).unwrap_or(0.0);
        let ask: f64 = obj.getattr("ask").ok().and_then(|v| v.extract().ok()).unwrap_or(0.0);
        // Pick the best price as a representative tick. Phase 3 will
        // emit one Tick per kind change; this is a placeholder.
        let (kind, price) = if bid > 0.0 {
            (TickKind::Bid, Some(bid))
        } else if ask > 0.0 {
            (TickKind::Ask, Some(ask))
        } else {
            (TickKind::Last, None)
        };
        Ok(corsair_broker_api::Tick {
            instrument_id: InstrumentId(con_id as u64),
            kind,
            price,
            size: None,
            timestamp_ns: conversion::now_ns(),
        })
    }

    fn error_from_py(obj: &Bound<'_, PyAny>) -> Result<BrokerError, BrokerError> {
        // Payload: tuple (reqId, errorCode, errorString, contract)
        let code: i64 = obj
            .get_item(1)
            .ok()
            .and_then(|v| v.extract().ok())
            .unwrap_or(0);
        let msg: String = obj
            .get_item(2)
            .ok()
            .and_then(|v| v.extract().ok())
            .unwrap_or_default();
        Ok(BrokerError::Protocol {
            code: Some(code as i32),
            message: msg,
        })
    }

    fn connection_event_from_py(
        _obj: &Bound<'_, PyAny>,
        connected: bool,
    ) -> corsair_broker_api::events::ConnectionEvent {
        use corsair_broker_api::events::{ConnectionEvent, ConnectionState};
        ConnectionEvent {
            state: if connected {
                ConnectionState::Connected
            } else {
                ConnectionState::LostConnection
            },
            reason: None,
            timestamp_ns: conversion::now_ns(),
        }
    }

    // ─── Top-level command dispatch ──────────────────────────────

    pub fn dispatch(state: &PyState, cmd: Command, streams: &BridgeStreams) {
        match cmd {
            Command::Connect { reply } => {
                let r = cmd_connect(state);
                if r.is_ok() {
                    // Emit a Connected event on the connection stream
                    // so consumers waiting on it are unblocked.
                    use corsair_broker_api::events::{
                        ConnectionEvent, ConnectionState,
                    };
                    let _ = streams.connection.send(ConnectionEvent {
                        state: ConnectionState::Connected,
                        reason: Some("ibkr connect ok".into()),
                        timestamp_ns: conversion::now_ns(),
                    });
                }
                let _ = reply.send(r);
            }
            Command::Disconnect { reply } => {
                let r = cmd_disconnect(state);
                use corsair_broker_api::events::{
                    ConnectionEvent, ConnectionState,
                };
                let _ = streams.connection.send(ConnectionEvent {
                    state: ConnectionState::Closed,
                    reason: Some("ibkr disconnect".into()),
                    timestamp_ns: conversion::now_ns(),
                });
                let _ = reply.send(r);
            }
            Command::PlaceOrder { req, reply } => {
                let _ = reply.send(cmd_place_order(state, req));
            }
            Command::CancelOrder { id, reply } => {
                let _ = reply.send(cmd_cancel_order(state, id));
            }
            Command::ModifyOrder { id, req, reply } => {
                let _ = reply.send(cmd_modify_order(state, id, req));
            }
            Command::Positions { reply } => {
                let _ = reply.send(cmd_positions(state));
            }
            Command::AccountValues { reply } => {
                let _ = reply.send(cmd_account_values(state));
            }
            Command::OpenOrders { reply } => {
                let _ = reply.send(cmd_open_orders(state));
            }
            Command::RecentFills { since_ns, reply } => {
                let _ = reply.send(cmd_recent_fills(state, since_ns));
            }
            Command::QualifyFuture { q, reply } => {
                let _ = reply.send(cmd_qualify_future(state, q));
            }
            Command::QualifyOption { q, reply } => {
                let _ = reply.send(cmd_qualify_option(state, q));
            }
            Command::ListChain { q, reply } => {
                let _ = reply.send(cmd_list_chain(state, q));
            }
            Command::SubscribeTicks { sub, reply } => {
                let _ = reply.send(cmd_subscribe_ticks(state, sub));
            }
            Command::UnsubscribeTicks { h, reply } => {
                let _ = reply.send(cmd_unsubscribe_ticks(state, h));
            }
            Command::Shutdown { reply } => {
                let _ = reply.send(());
            }
        }
    }
}
