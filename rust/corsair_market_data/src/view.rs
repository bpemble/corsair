//! `MarketDataView` adapter — implements
//! [`corsair_position::MarketView`] over [`MarketDataState`].
//!
//! Two impls of `MarketView`:
//! 1. **Direct** on `&MarketDataState` — preferred; works under
//!    `Mutex<MarketDataState>` lock.
//! 2. **Wrapped** in `MarketDataView` for `Rc<RefCell<...>>` callers
//!    (single-threaded test fixtures).

use chrono::NaiveDate;
use corsair_broker_api::Right;
use corsair_position::MarketView;
use std::cell::RefCell;
use std::rc::Rc;

use crate::state::MarketDataState;

// Direct impl — the runtime locks `Mutex<MarketDataState>`, gets
// `&MarketDataState`, and passes it as `&dyn MarketView` without
// extra allocation.
impl MarketView for MarketDataState {
    fn underlying_price(&self, product: &str) -> Option<f64> {
        MarketDataState::underlying_price(self, product)
    }

    fn iv_for(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
    ) -> Option<f64> {
        let t = self.option(product, strike, expiry, right)?;
        if t.iv > 0.0 { Some(t.iv) } else { None }
    }

    fn current_price(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
    ) -> Option<f64> {
        let t = self.option(product, strike, expiry, right)?;
        let p = t.current_price();
        if p > 0.0 { Some(p) } else { None }
    }
}

/// Wraps an `Rc<RefCell<MarketDataState>>` for legacy callers that
/// share state via interior mutability. Production runtime should
/// use the direct impl on `&MarketDataState`.
pub struct MarketDataView {
    state: Rc<RefCell<MarketDataState>>,
}

impl MarketDataView {
    pub fn new(state: Rc<RefCell<MarketDataState>>) -> Self {
        Self { state }
    }
}

impl MarketView for MarketDataView {
    fn underlying_price(&self, product: &str) -> Option<f64> {
        self.state.borrow().underlying_price(product)
    }

    fn iv_for(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
    ) -> Option<f64> {
        MarketView::iv_for(&*self.state.borrow(), product, strike, expiry, right)
    }

    fn current_price(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
    ) -> Option<f64> {
        MarketView::current_price(&*self.state.borrow(), product, strike, expiry, right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use corsair_broker_api::InstrumentId;

    #[test]
    fn view_proxies_state() {
        let state = Rc::new(RefCell::new(MarketDataState::new()));
        state
            .borrow_mut()
            .register_underlying("HG", InstrumentId(1));
        state.borrow_mut().set_underlying("HG", 6.05);
        let view = MarketDataView::new(state.clone());
        assert_eq!(view.underlying_price("HG"), Some(6.05));
        assert!(view.underlying_price("UNKNOWN").is_none());
    }

    #[test]
    fn current_price_returns_mid_when_available() {
        let state = Rc::new(RefCell::new(MarketDataState::new()));
        let exp = chrono::NaiveDate::from_ymd_opt(2026, 6, 26).unwrap();
        state
            .borrow_mut()
            .register_option("HG", 6.05, exp, Right::Call, InstrumentId(100));
        state.borrow_mut().update_bid(InstrumentId(100), 0.024, 5, 0);
        state.borrow_mut().update_ask(InstrumentId(100), 0.026, 5, 0);
        let view = MarketDataView::new(state);
        assert_eq!(
            view.current_price("HG", 6.05, exp, Right::Call),
            Some(0.025)
        );
    }
}
