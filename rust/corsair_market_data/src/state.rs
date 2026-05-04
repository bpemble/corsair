//! Aggregate market data state — underlying + option book.

use chrono::NaiveDate;
use corsair_broker_api::{InstrumentId, Right};
use std::collections::HashMap;

use crate::option_state::OptionTick;

/// Market data state for a single broker connection. Holds
/// underlying price per product and the option book keyed by
/// (product, strike_key, expiry, right_char).
///
/// The runtime drives this by consuming `Broker::subscribe_ticks_stream`
/// and calling `update_*` methods. Risk / position / OMS read it
/// via the [`MarketDataView`](crate::view::MarketDataView) trait.
pub struct MarketDataState {
    underlying: HashMap<String, f64>,
    options: HashMap<OptionKey, OptionTick>,
    /// Fast lookup from broker InstrumentId → option key.
    by_instrument: HashMap<InstrumentId, OptionKey>,
    /// Underlying instrument id → product (resolved via runtime
    /// at qualification time).
    underlying_instruments: HashMap<InstrumentId, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OptionKey {
    product: String,
    strike_key: i64,
    expiry: NaiveDate,
    right: Right,
}

/// Public read-only view of an option's contract metadata, returned
/// by [`MarketDataState::option_meta`]. Owned so callers don't have
/// to hold the state lock past the lookup.
#[derive(Debug, Clone)]
pub struct OptionMeta {
    pub product: String,
    pub strike: f64,
    pub expiry: NaiveDate,
    pub right: Right,
}

fn strike_key(s: f64) -> i64 {
    (s * 10_000.0).round() as i64
}

impl Default for MarketDataState {
    fn default() -> Self {
        Self::new()
    }
}

impl MarketDataState {
    pub fn new() -> Self {
        Self {
            underlying: HashMap::new(),
            options: HashMap::new(),
            by_instrument: HashMap::new(),
            underlying_instruments: HashMap::new(),
        }
    }

    /// Register an option contract from the runtime's qualification.
    /// After this, ticks for `instrument_id` route to the option
    /// state at (product, strike, expiry, right).
    pub fn register_option(
        &mut self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
        instrument_id: InstrumentId,
    ) {
        let k = OptionKey {
            product: product.to_string(),
            strike_key: strike_key(strike),
            expiry,
            right,
        };
        self.options.entry(k.clone()).or_insert_with(|| {
            let mut t = OptionTick::new(strike, expiry, right);
            t.instrument_id = Some(instrument_id);
            t
        });
        self.by_instrument.insert(instrument_id, k);
    }

    /// Register the underlying instrument for a product.
    pub fn register_underlying(&mut self, product: &str, instrument_id: InstrumentId) {
        self.underlying_instruments
            .insert(instrument_id, product.to_string());
    }

    /// Reverse lookup: which product (if any) does this instrument
    /// represent the underlying of? Used by the broker's tick fast-path
    /// to fork underlying ticks into a separate "underlying_tick"
    /// event for the trader.
    pub fn product_for_underlying(&self, iid: InstrumentId) -> Option<String> {
        self.underlying_instruments.get(&iid).cloned()
    }

    pub fn underlying_price(&self, product: &str) -> Option<f64> {
        self.underlying.get(product).copied().filter(|&p| p > 0.0)
    }

    pub fn set_underlying(&mut self, product: &str, price: f64) {
        if price > 0.0 {
            self.underlying.insert(product.to_string(), price);
        }
    }

    /// Update bid for an option keyed by instrument id (broker
    /// stream path). No-op if the instrument isn't registered.
    pub fn update_bid(&mut self, iid: InstrumentId, price: f64, size: u64, ts_ns: u64) {
        if let Some(k) = self.by_instrument.get(&iid).cloned() {
            if let Some(t) = self.options.get_mut(&k) {
                if price > 0.0 {
                    t.bid = price;
                }
                t.bid_size = size;
                t.last_updated_ns = ts_ns;
            }
        } else if let Some(product) = self.underlying_instruments.get(&iid).cloned() {
            // Underlying tick.
            if price > 0.0 {
                self.underlying.insert(product, price);
            }
        }
    }

    pub fn update_ask(&mut self, iid: InstrumentId, price: f64, size: u64, ts_ns: u64) {
        if let Some(k) = self.by_instrument.get(&iid).cloned() {
            if let Some(t) = self.options.get_mut(&k) {
                if price > 0.0 {
                    t.ask = price;
                }
                t.ask_size = size;
                t.last_updated_ns = ts_ns;
            }
        }
    }

    pub fn update_last(&mut self, iid: InstrumentId, price: f64, ts_ns: u64) {
        if let Some(k) = self.by_instrument.get(&iid).cloned() {
            if let Some(t) = self.options.get_mut(&k) {
                if price > 0.0 {
                    t.last = price;
                }
                t.last_updated_ns = ts_ns;
            }
        } else if let Some(product) = self.underlying_instruments.get(&iid).cloned() {
            if price > 0.0 {
                self.underlying.insert(product, price);
            }
        }
    }

    /// Update bid SIZE only (TickKind::BidSize tick). The price-side
    /// update path doesn't carry size — IBKR sends size as separate
    /// tickSize messages from the price ticks. Without these methods
    /// bid_size/ask_size stay at 0 and the trader's dark-book guard
    /// blocks every place attempt.
    pub fn update_bid_size(&mut self, iid: InstrumentId, size: u64, ts_ns: u64) {
        if let Some(k) = self.by_instrument.get(&iid).cloned() {
            if let Some(t) = self.options.get_mut(&k) {
                t.bid_size = size;
                t.last_updated_ns = ts_ns;
            }
        }
    }

    /// Update ask SIZE only (TickKind::AskSize tick).
    pub fn update_ask_size(&mut self, iid: InstrumentId, size: u64, ts_ns: u64) {
        if let Some(k) = self.by_instrument.get(&iid).cloned() {
            if let Some(t) = self.options.get_mut(&k) {
                t.ask_size = size;
                t.last_updated_ns = ts_ns;
            }
        }
    }

    /// Direct lookup by InstrumentId. Used by the broker daemon's
    /// tick fast-path to attach depth state to outbound TickEvents
    /// without re-resolving by (product, strike, expiry, right).
    pub fn option_by_iid(&self, iid: InstrumentId) -> Option<&crate::option_state::OptionTick> {
        let k = self.by_instrument.get(&iid)?;
        self.options.get(k)
    }

    /// Apply an IBKR depth operation to the option's L2 book.
    /// `is_bid` is true when IBKR sent side=1 (bid), false for side=0
    /// (ask). `op` is 0=insert, 1=update, 2=delete. No-op for non-
    /// options or unregistered instruments.
    pub fn apply_depth(
        &mut self,
        iid: InstrumentId,
        is_bid: bool,
        position: i32,
        op: i32,
        price: f64,
        size: u64,
        ts_ns: u64,
    ) {
        if let Some(k) = self.by_instrument.get(&iid).cloned() {
            if let Some(t) = self.options.get_mut(&k) {
                t.depth.apply(is_bid, position, op, price, size);
                t.last_updated_ns = ts_ns;
            }
        }
    }

    /// Update option open interest for the leg keyed by instrument id.
    /// Pushed by IBKR's generic tick "101". No-op if not an option.
    pub fn update_open_interest(&mut self, iid: InstrumentId, oi: u64, ts_ns: u64) {
        if let Some(k) = self.by_instrument.get(&iid).cloned() {
            if let Some(t) = self.options.get_mut(&k) {
                t.open_interest = oi;
                t.last_updated_ns = ts_ns;
            }
        }
    }

    /// Update option session volume. Pushed by IBKR's generic tick
    /// "100". No-op if not an option.
    pub fn update_option_volume(&mut self, iid: InstrumentId, vol: u64, ts_ns: u64) {
        if let Some(k) = self.by_instrument.get(&iid).cloned() {
            if let Some(t) = self.options.get_mut(&k) {
                t.volume = vol;
                t.last_updated_ns = ts_ns;
            }
        }
    }

    /// Direct lookup for OptionTick.
    pub fn option(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
    ) -> Option<&OptionTick> {
        self.options.get(&OptionKey {
            product: product.to_string(),
            strike_key: strike_key(strike),
            expiry,
            right,
        })
    }

    /// All options for a product. Used by snapshot serialization
    /// and SABR refit drivers.
    pub fn options_for_product(&self, product: &str) -> Vec<&OptionTick> {
        self.options
            .iter()
            .filter(|(k, _)| k.product == product)
            .map(|(_, v)| v)
            .collect()
    }

    pub fn option_count(&self) -> usize {
        self.options.len()
    }

    /// Public accessor for tick-forwarding paths that need to enrich
    /// per-instrument-id ticks with option metadata. Returns None for
    /// unregistered ids (e.g. an underlying tick or a stream race
    /// before `register_option` ran).
    pub fn option_meta(&self, iid: InstrumentId) -> Option<OptionMeta> {
        let key = self.by_instrument.get(&iid)?;
        let tick = self.options.get(key)?;
        Some(OptionMeta {
            product: key.product.clone(),
            strike: tick.strike,
            expiry: tick.expiry,
            right: tick.right,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exp() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 26).unwrap()
    }

    #[test]
    fn register_and_lookup_option() {
        let mut s = MarketDataState::new();
        s.register_option("HG", 6.05, exp(), Right::Call, InstrumentId(100));
        let t = s.option("HG", 6.05, exp(), Right::Call).unwrap();
        assert_eq!(t.strike, 6.05);
        assert_eq!(t.instrument_id, Some(InstrumentId(100)));
    }

    #[test]
    fn update_bid_routes_via_instrument_id() {
        let mut s = MarketDataState::new();
        s.register_option("HG", 6.05, exp(), Right::Call, InstrumentId(100));
        s.update_bid(InstrumentId(100), 0.025, 5, 1234);
        let t = s.option("HG", 6.05, exp(), Right::Call).unwrap();
        assert_eq!(t.bid, 0.025);
        assert_eq!(t.bid_size, 5);
    }

    #[test]
    fn update_underlying_via_registered_instrument() {
        let mut s = MarketDataState::new();
        s.register_underlying("HG", InstrumentId(50));
        s.update_last(InstrumentId(50), 6.05, 1234);
        assert_eq!(s.underlying_price("HG"), Some(6.05));
    }

    #[test]
    fn options_for_product_isolates() {
        let mut s = MarketDataState::new();
        s.register_option("HG", 6.05, exp(), Right::Call, InstrumentId(100));
        s.register_option("ETH", 3000.0, exp(), Right::Call, InstrumentId(101));
        assert_eq!(s.options_for_product("HG").len(), 1);
        assert_eq!(s.options_for_product("ETH").len(), 1);
    }

    #[test]
    fn unregistered_instrument_is_silently_ignored() {
        let mut s = MarketDataState::new();
        s.update_bid(InstrumentId(999), 0.025, 5, 1234);
        // No panic, no spurious option in the book.
        assert_eq!(s.option_count(), 0);
    }

    #[test]
    fn mid_returns_none_when_one_side_missing() {
        let mut s = MarketDataState::new();
        s.register_option("HG", 6.05, exp(), Right::Call, InstrumentId(100));
        s.update_bid(InstrumentId(100), 0.025, 5, 0);
        let t = s.option("HG", 6.05, exp(), Right::Call).unwrap();
        assert!(t.mid().is_none());
    }

    #[test]
    fn mid_returns_average_when_both_sides_present() {
        let mut s = MarketDataState::new();
        s.register_option("HG", 6.05, exp(), Right::Call, InstrumentId(100));
        s.update_bid(InstrumentId(100), 0.024, 5, 0);
        s.update_ask(InstrumentId(100), 0.026, 5, 0);
        let t = s.option("HG", 6.05, exp(), Right::Call).unwrap();
        assert_eq!(t.mid(), Some(0.025));
    }
}
