//! Library facade for benches and integration tests.
//!
//! corsair_trader is primarily a binary (`src/main.rs` produces
//! `/usr/local/bin/corsair_trader_rust`). This file exists so that
//! `benches/*.rs` (and external integration tests, if added) can
//! reference public items like `decision::compute_risk_gates`,
//! `decision::improving_passes`, `state::ScalarSnapshot`, and
//! `state::TheoGreeks` without including source files via `#[path]`.
//!
//! Both the bin (rooted at `main.rs`) and the lib (rooted here)
//! compile the same module sources independently. There is no shared
//! global state between them — they are separate compilation units.
//! In particular, the bin's `#[global_allocator] mimalloc` directive
//! lives only in `main.rs` and does NOT apply to bench builds. That's
//! intentional: benches measure relative cost, and a different
//! allocator only shifts the baseline, not the delta between
//! variants.
//!
//! Only modules whose pub items are referenced from outside the crate
//! are re-exported here. Internal modules (jsonl, msgpack_*, ipc) are
//! deliberately omitted.

pub mod decision;
pub mod messages;
pub mod pricing;
pub mod state;
pub mod tte_cache;
pub mod types;
