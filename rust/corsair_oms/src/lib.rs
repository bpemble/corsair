//! Order management for the v3 Rust runtime.
//!
//! Replaces the OMS portion of `src/quote_engine.py`. Owns:
//!   - `our_orders` index: per-key resting orders we placed
//!   - send-or-update logic with dead-band + GTD-refresh +
//!     cancel-before-replace (mirrors `_send_or_update`)
//!   - cycle counters (place / cancel / amend) for telemetry
//!
//! What's NOT here:
//!   - Order *placement* — caller (the runtime) takes [`OrderAction`]
//!     decisions and issues them via `Broker::place_order`.
//!   - Latency histograms — separate concern, can sit in the
//!     runtime or `corsair_snapshot`.
//!   - Layer C burst tracking — already in
//!     `corsair_pricing::quote_engine_helpers`-equivalent (kept in
//!     today's Python). Phase 3.5 omits it; the runtime can wire
//!     `FillBurstTracker` from a shared helpers crate if needed.
//!
//! # Send-or-update three-rule chain (CLAUDE.md §16)
//!
//! 1. **Dead-band**: skip if `|new_price - rest_price| < dead_band_ticks`
//!    AND age < `gtd_lifetime - gtd_refresh_lead`.
//! 2. **GTD-refresh override**: bypass dead-band when ≥
//!    `gtd_refresh_lead` has elapsed since the last place — keeps
//!    the quote alive before GTD-5s expiry.
//! 3. **Cooldown floor**: `min_send_interval_ms` hard minimum
//!    between sends per key (defensive backstop).

pub mod book;
pub mod key;

pub use book::{OrderBook, OrderRecord, OrderState};
pub use key::OurKey;
