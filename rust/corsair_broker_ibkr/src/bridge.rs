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
    use pyo3::prelude::*;

    /// Holds Python objects that live for the lifetime of the bridge:
    ///   - the asyncio loop
    ///   - the ib_insync.IB instance
    ///   - the event-pumping bookkeeping
    ///
    /// PyObject is Send (under GIL acquisition rules); we only access
    /// these fields under Python::with_gil from the bridge thread.
    /// Fields are `pub` and `#[allow(dead_code)]` because Phase 2.4
    /// will read them from `dispatch`; until then the constructor is
    /// the only writer.
    #[allow(dead_code)]
    pub struct PyState {
        pub ib: PyObject,
        pub asyncio_loop: PyObject,
        // Pending event queue — populated by registered ib_insync
        // callbacks, drained by `pump` and `dispatch`.
        pub event_queue: PyObject,
    }

    impl PyState {
        pub fn new(_cfg: &BridgeConfig) -> Result<Self, BridgeError> {
            Python::with_gil(|py| -> Result<Self, BridgeError> {
                // Import asyncio + ib_insync.
                let asyncio = py.import_bound("asyncio")
                    .map_err(|e| BridgeError::PythonInit(format!("import asyncio: {e}")))?;
                let ib_insync = py.import_bound("ib_insync")
                    .map_err(|e| BridgeError::PythonInit(format!("import ib_insync: {e}")))?;
                let util = py.import_bound("ib_insync.util")
                    .map_err(|e| BridgeError::PythonInit(format!("import ib_insync.util: {e}")))?;

                // ib_insync.util.startLoop() sets up the asyncio loop in a
                // way compatible with ib_insync's nested-loop pattern.
                let _ = util.call_method0("startLoop");

                let asyncio_loop = asyncio
                    .call_method0("get_event_loop")
                    .map_err(|e| BridgeError::PythonInit(format!("get_event_loop: {e}")))?
                    .into();

                let ib_class = ib_insync.getattr("IB")
                    .map_err(|e| BridgeError::PythonInit(format!("getattr IB: {e}")))?;
                let ib = ib_class.call0()
                    .map_err(|e| BridgeError::PythonInit(format!("IB(): {e}")))?
                    .into();

                // Create a Python list to serve as the event queue.
                let event_queue = py
                    .import_bound("builtins")
                    .map_err(|e| BridgeError::PythonInit(format!("import builtins: {e}")))?
                    .call_method0("list")
                    .map_err(|e| BridgeError::PythonInit(format!("list(): {e}")))?
                    .into();

                Ok(PyState {
                    ib,
                    asyncio_loop,
                    event_queue,
                })
            })
        }
    }

    /// Drain pending Python-side events onto Rust broadcast channels.
    /// Called both opportunistically (poll timeout) and after each
    /// command dispatch.
    pub fn pump(_state: &PyState, _streams: &BridgeStreams) {
        // Phase 2.5 will populate this. For now we just acknowledge
        // the call and return; ib_insync callbacks aren't wired yet.
        // TODO(phase 2.5): drain `state.event_queue` and forward to
        // streams.fills / streams.status / streams.ticks / etc.
    }

    /// Execute one command on the Python thread.
    pub fn dispatch(_state: &PyState, cmd: Command, _streams: &BridgeStreams) {
        match cmd {
            Command::Connect { reply } => {
                // TODO(phase 2.4): call self.ib.connect(host, port, clientId)
                // with the lean-bypass pattern from src/connection.py.
                // For now stub to InvalidRequest so the test path can
                // see the bridge architecture but knows real IB isn't
                // wired.
                let _ = reply.send(Err(BrokerError::Internal(
                    "connect not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::Disconnect { reply } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "disconnect not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::PlaceOrder { reply, .. } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "place_order not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::CancelOrder { reply, .. } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "cancel_order not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::ModifyOrder { reply, .. } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "modify_order not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::Positions { reply } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "positions not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::AccountValues { reply } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "account_values not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::OpenOrders { reply } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "open_orders not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::RecentFills { reply, .. } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "recent_fills not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::QualifyFuture { reply, .. } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "qualify_future not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::QualifyOption { reply, .. } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "qualify_option not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::ListChain { reply, .. } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "list_chain not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::SubscribeTicks { reply, .. } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "subscribe_ticks not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::UnsubscribeTicks { reply, .. } => {
                let _ = reply.send(Err(BrokerError::Internal(
                    "unsubscribe_ticks not yet implemented (phase 2.4)".into(),
                )));
            }
            Command::Shutdown { reply } => {
                let _ = reply.send(());
            }
        }
    }
}
