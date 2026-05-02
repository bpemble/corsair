//! Event-pumping plumbing — ib_insync callbacks → Rust broadcast channels.
//!
//! Phase 2.5 will populate this with Python-side hooks that:
//!   - subscribe to `IB.execDetailsEvent` and append (Fill) records to
//!     the bridge's event_queue
//!   - subscribe to `IB.orderStatusEvent` and append OrderStatusUpdate
//!     records
//!   - subscribe to `Ticker.updateEvent` per active subscription and
//!     append Tick records
//!   - subscribe to `IB.errorEvent` and append BrokerError records
//!   - subscribe to `IB.connectedEvent` and `IB.disconnectedEvent`
//!     and append ConnectionEvent records
//!
//! The bridge thread's `pump()` function drains the queue under the
//! GIL, dispatches each record to the appropriate broadcast::Sender,
//! and clears the queue.
//!
//! Why a Python-side queue rather than registering Rust closures as
//! callbacks: ib_insync's event system expects Python callables.
//! Wrapping Rust closures in PyO3 callable wrappers is possible but
//! complicates ownership; the queue approach is simpler and the
//! per-call overhead (one Python list append per event + one drain
//! per pump cycle, ~1 ms cadence) is negligible at our event rates.
