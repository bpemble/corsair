//! Delta-hedge manager for the v3 Rust runtime.
//!
//! Mirrors `src/hedge_manager.py`. Tracks hedge_qty + avg_entry_F per
//! product, rebalances against options delta drift, applies fills via
//! the broker's exec stream. Place orders go through the
//! `Broker::place_order` trait — never directly to ib_insync.
//!
//! # CLAUDE.md §10 reconciliation
//!
//! v3 maintains the same three-layer reconciliation discipline:
//! 1. Boot reconcile — `reconcile_from_broker()` reads broker positions
//!    on startup and seeds hedge_qty + avg_entry_F.
//! 2. Periodic reconcile — `rebalance_periodic` calls
//!    `reconcile_from_broker(silent=true)` every cycle.
//! 3. Fill confirmation — `apply_broker_fill` updates hedge_qty +
//!    avg_entry_F + realized_pnl_usd from broker exec events
//!    (Broker::subscribe_fills stream, filtered by FUT contract +
//!    matching instrument_id). Replaces optimistic-on-placement.

pub mod fanout;
pub mod manager;
pub mod state;

pub use fanout::HedgeFanout;
pub use manager::{HedgeAction, HedgeConfig, HedgeManager, HedgeMode};
pub use state::HedgeState;
