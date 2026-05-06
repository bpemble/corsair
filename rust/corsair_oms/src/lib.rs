//! Minimal order-status book for the v3 Rust broker daemon.
//!
//! In the v3 cutover (Phase 6.7), the broker still owns a tiny
//! cache of "did this orderId terminate?" so per-status routing in
//! `corsair_broker/src/tasks.rs` can convert OrderStatus updates
//! into clean enum variants and bump termination counters. Trader
//! owns the heavyweight per-key dead-band / GTD / cancel-before-
//! replace logic — it does NOT live here, despite previous docs
//! saying otherwise.
//!
//! The crate is intentionally small. If you find yourself reaching
//! for `live_orders`, `link_order_id`, `mark_pending_cancel`, etc.
//! that isn't a sign you should add it back here — that's a sign
//! the work belongs in the trader (`corsair_trader/src/state.rs`).

pub mod book;

pub use book::{OrderBook, OrderState};
