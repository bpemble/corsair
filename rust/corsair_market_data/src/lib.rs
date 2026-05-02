//! Market data state — tick stream → option book + underlying.
//!
//! Mirrors `src/market_data.py`. Consumes ticks from the broker
//! stream and maintains:
//!   - underlying price per product
//!   - option state per (product, strike, expiry, right):
//!     bid/ask/last/sizes, derived IV
//!   - ATM tracking + subscription window
//!
//! Implements `corsair_position::MarketView` so PortfolioState can
//! call refresh_greeks against this state directly.

pub mod atm;
pub mod option_state;
pub mod state;
pub mod view;

pub use atm::{AtmTracker, StrikeWindow};
pub use option_state::OptionTick;
pub use state::{MarketDataState, OptionMeta};
pub use view::MarketDataView;
