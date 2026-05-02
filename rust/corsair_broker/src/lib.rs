//! Corsair v3 broker daemon — Rust runtime.
//!
//! Wires the Phase 1-3 crates into a single async process:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │  Tokio multi-thread runtime                              │
//! │                                                          │
//! │  ┌───────────────┐    Broker trait                       │
//! │  │ IbkrAdapter   │ ◄─────────────────                    │
//! │  └───┬───────────┘                                       │
//! │      │ subscribe_fills/status/ticks/errors/connection    │
//! │      ▼                                                   │
//! │  ┌─────────────────────────────────────────────────┐    │
//! │  │ stream pumps (one tokio task each)              │    │
//! │  │   fills    → portfolio + hedge                  │    │
//! │  │   status   → oms                                │    │
//! │  │   ticks    → market_data                        │    │
//! │  │   errors   → log + risk                         │    │
//! │  │   connect  → watchdog + risk.clear_disconnect   │    │
//! │  └─────────────────────────────────────────────────┘    │
//! │                                                          │
//! │  ┌─────────────────────────────────────────────────┐    │
//! │  │ periodic tasks                                  │    │
//! │  │   greek_refresh (5min)   → portfolio            │    │
//! │  │   risk_check    (5min)   → risk                 │    │
//! │  │   hedge         (30s)    → hedge fanout         │    │
//! │  │   snapshot      (250ms)  → publisher            │    │
//! │  │   account_poll  (5min)   → broker.account_values│    │
//! │  └─────────────────────────────────────────────────┘    │
//! │                                                          │
//! │  Shared state: Arc<Mutex<...>> per component            │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! Runtime modes:
//! - **shadow**: connects + ingests state, but does NOT place orders.
//!   Used to validate Phase 4 against the live Python broker.
//! - **live**: full runtime; eventual production replacement.
//!
//! Selected via env var `CORSAIR_BROKER_SHADOW=1` (default: 1 during
//! Phase 4 development; flip when Phase 5 cut-over lands).

pub mod config;
pub mod runtime;
pub mod tasks;

pub use config::{BrokerDaemonConfig, ProductConfig};
pub use runtime::{Runtime, RuntimeError};
