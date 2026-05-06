//! [`MarketView`] trait — the read interface that
//! [`PortfolioState::refresh_greeks`] needs from market data.
//!
//! Decouples the position book from corsair_market_data so both can
//! be developed/tested independently. Production wires
//! `corsair_market_data::MarketDataState` (Phase 3.6) as the
//! implementation; tests use `NoOpMarketView` or
//! `RecordingMarketView` for deterministic behavior.

use chrono::NaiveDate;
use corsair_broker_api::Right;
use std::cell::RefCell;
use std::collections::HashMap;

/// Read-only access to market state for a position. Each position
/// queries this for the inputs to greek computation.
pub trait MarketView {
    /// Current underlying price (futures forward) for a product.
    /// `None` means market data hasn't populated yet — caller skips
    /// greek refresh for this position.
    fn underlying_price(&self, product: &str) -> Option<f64>;

    /// Implied vol for the given option. The implementation is
    /// responsible for the resolution policy: live brentq IV, fall
    /// back to fitted SABR/SVI vol, fall back to a default (typical
    /// 30%). `None` means no usable vol — caller skips.
    fn iv_for(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
    ) -> Option<f64>;

    /// Current observed price (mid or last) — used for MTM and to
    /// populate Position.current_price during greek refresh. `None`
    /// means no recent observation.
    fn current_price(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
    ) -> Option<f64>;

    /// Last-tick price for the resolved hedge contract for `product`.
    /// Distinct from `underlying_price`: the options-engine underlying
    /// (e.g. HGK6) is generally NOT the hedge contract (e.g. HGM6).
    /// Returns `None` if no hedge subscription has been registered or
    /// no tick has landed yet — callers performing hedge MTM should
    /// fall through loudly (WARN), not silently substitute the options
    /// underlying.
    ///
    /// Default impl returns `None` so existing stub `MarketView` impls
    /// (test fixtures) compile unchanged.
    fn hedge_underlying_price(&self, _product: &str) -> Option<f64> {
        None
    }
}

/// Test stub: every query returns `None`. Used for unit-testing
/// PortfolioState methods that don't depend on market data
/// (registry, fill merging, daily reset).
pub struct NoOpMarketView;

impl MarketView for NoOpMarketView {
    fn underlying_price(&self, _product: &str) -> Option<f64> {
        None
    }
    fn iv_for(
        &self,
        _product: &str,
        _strike: f64,
        _expiry: NaiveDate,
        _right: Right,
    ) -> Option<f64> {
        None
    }
    fn current_price(
        &self,
        _product: &str,
        _strike: f64,
        _expiry: NaiveDate,
        _right: Right,
    ) -> Option<f64> {
        None
    }
}

/// Test stub: returns canned values seeded by the test. Use
/// `set_underlying`, `set_iv`, `set_current_price` to populate
/// before calling refresh_greeks.
#[derive(Default)]
pub struct RecordingMarketView {
    pub underlying: RefCell<HashMap<String, f64>>,
    pub iv: RefCell<HashMap<(String, i64, NaiveDate, char), f64>>,
    pub price: RefCell<HashMap<(String, i64, NaiveDate, char), f64>>,
    pub hedge_underlying: RefCell<HashMap<String, f64>>,
}

impl RecordingMarketView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_underlying(&self, product: &str, price: f64) {
        self.underlying.borrow_mut().insert(product.into(), price);
    }

    pub fn set_hedge_underlying(&self, product: &str, price: f64) {
        self.hedge_underlying
            .borrow_mut()
            .insert(product.into(), price);
    }

    pub fn set_iv(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
        iv: f64,
    ) {
        self.iv
            .borrow_mut()
            .insert((product.into(), strike_key(strike), expiry, right.as_char()), iv);
    }

    pub fn set_current_price(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
        price: f64,
    ) {
        self.price
            .borrow_mut()
            .insert((product.into(), strike_key(strike), expiry, right.as_char()), price);
    }
}

impl MarketView for RecordingMarketView {
    fn underlying_price(&self, product: &str) -> Option<f64> {
        self.underlying.borrow().get(product).copied()
    }
    fn iv_for(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
    ) -> Option<f64> {
        self.iv
            .borrow()
            .get(&(product.into(), strike_key(strike), expiry, right.as_char()))
            .copied()
    }
    fn current_price(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
    ) -> Option<f64> {
        self.price
            .borrow()
            .get(&(product.into(), strike_key(strike), expiry, right.as_char()))
            .copied()
    }
    fn hedge_underlying_price(&self, product: &str) -> Option<f64> {
        self.hedge_underlying.borrow().get(product).copied()
    }
}

/// Convert a float strike to a stable integer key.
/// Convention: round to 4 decimal places * 10000 (matches the
/// `strike_key` in the trader binary).
pub(crate) fn strike_key(strike: f64) -> i64 {
    (strike * 10_000.0).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_returns_none() {
        let v = NoOpMarketView;
        assert!(v.underlying_price("HG").is_none());
        assert!(v
            .iv_for(
                "HG",
                6.0,
                NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
                Right::Call
            )
            .is_none());
    }

    #[test]
    fn recording_round_trips_values() {
        let v = RecordingMarketView::new();
        v.set_underlying("HG", 6.05);
        let exp = NaiveDate::from_ymd_opt(2026, 6, 26).unwrap();
        v.set_iv("HG", 6.05, exp, Right::Call, 0.45);
        v.set_current_price("HG", 6.05, exp, Right::Call, 0.025);

        assert_eq!(v.underlying_price("HG"), Some(6.05));
        assert_eq!(v.iv_for("HG", 6.05, exp, Right::Call), Some(0.45));
        assert_eq!(v.current_price("HG", 6.05, exp, Right::Call), Some(0.025));
    }
}
