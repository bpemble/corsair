//! Corsair v3 broker daemon — Rust runtime.
//!
//! Production stack since the Phase 6.7 cutover (2026-05-02): this
//! daemon owns the IBKR connection (clientId=0 FA master), the
//! position book, risk, hedge, snapshot publisher, and the SHM IPC
//! server that the Rust trader binary connects to. The trader is a
//! sibling binary (`rust/corsair_trader`) — it never touches the
//! gateway directly.
//!
//! Topology:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │  Tokio multi-thread runtime (4 workers; cpu-pinned)      │
//! │                                                          │
//! │  ┌───────────────┐    Broker trait                       │
//! │  │ NativeBroker  │ ◄── corsair_broker_ibkr_native        │
//! │  └───┬───────────┘                                       │
//! │      │ subscribe_fills/status/ticks/depth/errors/conn    │
//! │      ▼                                                   │
//! │  ┌─────────────────────────────────────────────────┐    │
//! │  │ stream pumps (one tokio task each)              │    │
//! │  │   fills    → portfolio + hedge + risk.daily_pnl │    │
//! │  │   status   → oms                                │    │
//! │  │   ticks    → market_data                        │    │
//! │  │   depth    → market_data L2 book                │    │
//! │  │   errors   → log + risk                         │    │
//! │  │   connect  → risk.clear_disconnect              │    │
//! │  └─────────────────────────────────────────────────┘    │
//! │                                                          │
//! │  ┌─────────────────────────────────────────────────┐    │
//! │  │ periodic tasks                                  │    │
//! │  │   greek_refresh   (5s)    → portfolio           │    │
//! │  │   risk_check      (30s)   → risk                │    │
//! │  │   hedge           (30s)   → hedge fanout        │    │
//! │  │   snapshot        (250ms) → publisher           │    │
//! │  │   account_poll    (15s)   → AccountSnapshot     │    │
//! │  │   vol_surface     (60s)   → SABR fits → IPC     │    │
//! │  │   depth_rotator   (30s)   → L2 sub rotation     │    │
//! │  │   window_recenter (30s)   → strike sub window   │    │
//! │  │   risk_state      (1s)    → IPC → trader gates  │    │
//! │  └─────────────────────────────────────────────────┘    │
//! │                                                          │
//! │  Shared state: Mutex<...> per component                 │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! Runtime modes (see [`runtime::RuntimeMode`]):
//! - **live** (default since cutover): full operation, places orders.
//! - **shadow**: connects + ingests state, but does NOT place orders.
//!   Retained as an opt-in safety net (`CORSAIR_BROKER_SHADOW=1`) for
//!   debugging in production. Default is live.

pub mod config;
pub mod ipc;
pub mod jsonl;
pub mod latency;
pub mod notify;
pub mod runtime;
pub mod subscriptions;
pub mod tasks;
pub mod time;
pub mod vol_surface;

pub use config::{BrokerDaemonConfig, ProductConfig};
pub use ipc::{spawn_ipc, IpcConfig};
pub use runtime::{Runtime, RuntimeError};
pub use subscriptions::subscribe_market_data;
pub use vol_surface::spawn_vol_surface;
