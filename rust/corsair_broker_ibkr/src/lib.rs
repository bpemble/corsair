//! IBKR adapter for the corsair_broker_api Broker trait.
//!
//! # Phase 2 — PyO3 bridge over ib_insync
//!
//! The fastest path to a working `Broker` impl is to bridge through the
//! existing Python `ib_insync` library. ib_insync is battle-tested
//! against IBKR's quirks (FA-orderKey rewriting, multiple Trade objects
//! per orderId, account-field-required, Error 320 socket reads, etc.);
//! reimplementing the wire protocol from scratch in Rust is Phase 6
//! work and shouldn't gate Phases 3-5 of the v3 migration.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────┐
//! │ Tokio runtime (multi-thread)          │
//! │                                       │
//! │  IbkrAdapter::place_order(req).await  │
//! │                  │                    │
//! │           tokio mpsc → Command        │
//! │                  ▼                    │
//! │  ┌──────────────────────────────┐    │
//! │  │ Python event-loop thread     │    │
//! │  │ (single OS thread)            │    │
//! │  │  - asyncio loop               │    │
//! │  │  - ib_insync.IB instance      │    │
//! │  │  - dispatches commands        │    │
//! │  │  - pumps events to broadcast  │    │
//! │  └──────────────────────────────┘    │
//! │                  │                    │
//! │       tokio broadcast → Fills/Status  │
//! │                  ▼                    │
//! │  Consumers in Rust (OMS, position,   │
//! │  risk, hedge, market_data)            │
//! └──────────────────────────────────────┘
//! ```
//!
//! Why a dedicated Python thread:
//! - ib_insync is built around asyncio. Sharing one event loop simplifies
//!   reasoning about callbacks (no cross-loop dispatch).
//! - The GIL is acquired per-call from this thread; no contention with
//!   tokio worker threads.
//! - When Phase 6 ships (native Rust client), we delete the bridge thread
//!   and replace with native I/O — the trait surface and consumers are
//!   unchanged.
//!
//! # Phase status
//!
//! Phase 2 (this crate, current):
//! - ✅ Crate skeleton + Cargo wiring
//! - 🔄 Python event-loop thread bridge (Phase 2.2)
//! - 🔄 ib_insync ↔ trait type conversion (Phase 2.3)
//! - 🔄 IbkrAdapter Broker trait impl (Phase 2.4)
//! - 🔄 Event pumping (Phase 2.5)
//! - 🔄 Integration test (Phase 2.6)
//!
//! Phase 6 (future): replace `bridge` module with `native` module that
//! talks IBKR API V100+ directly. The IbkrAdapter struct and its trait
//! impl are unchanged.

pub mod bridge;
pub mod commands;
pub mod conversion;
pub mod events;
pub mod adapter;

pub use adapter::IbkrAdapter;
pub use bridge::{Bridge, BridgeConfig, BridgeError};

/// Initialize the embedded Python interpreter. The corsair_broker
/// daemon binary calls this once at startup before constructing any
/// `IbkrAdapter`. Safe to call multiple times (idempotent under
/// pyo3::prepare_freethreaded_python).
///
/// In test code use `init_python_for_test()` instead — it adds the
/// repo's src/ to sys.path so `import src.connection` works.
pub fn init_python() {
    pyo3::prepare_freethreaded_python();
}

#[cfg(test)]
pub fn init_python_for_test() {
    use pyo3::prelude::*;
    init_python();
    // Add the repo root to sys.path so `import src.*` resolves.
    Python::with_gil(|py| {
        let sys = py.import_bound("sys").expect("import sys");
        let path = sys.getattr("path").expect("sys.path");
        // Tests run from the rust workspace root or repo root;
        // try both common locations.
        for candidate in [".", "..", "/work", "/home/ethereal/corsair"] {
            let _ = path.call_method1("insert", (0, candidate));
        }
    });
}
