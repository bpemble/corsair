//! Aggregate market data state — underlying + option book.

use chrono::NaiveDate;
use corsair_broker_api::{InstrumentId, Right};
use std::collections::HashMap;

use crate::option_state::OptionTick;

// `hashbrown::HashMap` is API-compatible with `std::collections::HashMap`
// for everything we use, plus exposes `raw_entry` (still unstable in
// std). We use raw_entry on the `options` map only so the hot lookup
// path can hash a borrowed (product: &str) key without an owning
// `OptionKey { product: String, … }` temporary. Other maps in this
// struct stay on std HashMap — only the `options` map is hot.
type HBHashMap<K, V> = hashbrown::HashMap<K, V>;

/// Market data state for a single broker connection. Holds
/// underlying price per product and the option book keyed by
/// (product, strike_key, expiry, right_char).
///
/// The runtime drives this by consuming `Broker::subscribe_ticks_stream`
/// and calling `update_*` methods. Risk / position / OMS read it
/// via the [`MarketDataView`](crate::view::MarketDataView) trait.
pub struct MarketDataState {
    underlying: HashMap<String, f64>,
    options: HBHashMap<OptionKey, OptionTick>,
    /// Fast lookup from broker InstrumentId → option key.
    by_instrument: HashMap<InstrumentId, OptionKey>,
    /// Underlying instrument id → product (resolved via runtime
    /// at qualification time).
    underlying_instruments: HashMap<InstrumentId, String>,
    /// Hedge contract last-tick price per product. Distinct from
    /// `underlying` because the resolved hedge contract (e.g. HGM6)
    /// is generally NOT the same future as the options-engine
    /// underlying (e.g. HGK6) — calendar spread between them is non-
    /// trivial. Marking the hedge MTM against the options underlying
    /// produces sign-flipping errors of `multiplier × calendar_spread`
    /// per contract (CLAUDE.md §10's 2026-04-27 Python fix; the
    /// Phase 6.7 cutover did not carry it forward — see this audit).
    hedge_underlying: HashMap<String, f64>,
    /// Hedge instrument id → product (registered alongside the hedge
    /// tick subscription at boot).
    hedge_underlying_instruments: HashMap<InstrumentId, String>,
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
            options: HBHashMap::new(),
            by_instrument: HashMap::new(),
            underlying_instruments: HashMap::new(),
            hedge_underlying: HashMap::new(),
            hedge_underlying_instruments: HashMap::new(),
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

    /// Register the resolved hedge contract's instrument for a
    /// product. Subsequent bid/ask/last ticks for this `instrument_id`
    /// land in `hedge_underlying[product]` rather than (or in addition
    /// to) the options-engine `underlying[product]`. Same product can
    /// have a distinct hedge instrument from its options underlying —
    /// e.g. HG options anchor on HGK6 but hedges trade in HGM6.
    pub fn register_hedge_underlying(&mut self, product: &str, instrument_id: InstrumentId) {
        self.hedge_underlying_instruments
            .insert(instrument_id, product.to_string());
    }

    /// Reverse lookup: which product (if any) does this instrument
    /// represent the underlying of? Used by the broker's tick fast-path
    /// to fork underlying ticks into a separate "underlying_tick"
    /// event for the trader.
    pub fn product_for_underlying(&self, iid: InstrumentId) -> Option<String> {
        self.underlying_instruments.get(&iid).cloned()
    }

    /// Reverse lookup for hedge contract instruments. Audit T4-19:
    /// parallel to `product_for_underlying`. Returns the product whose
    /// hedge contract is `iid`, or None if not registered. Provided
    /// for symmetry; broker tick paths may want to publish hedge-tick
    /// events distinct from underlying-tick events.
    pub fn product_for_hedge_underlying(&self, iid: InstrumentId) -> Option<String> {
        self.hedge_underlying_instruments.get(&iid).cloned()
    }

    pub fn underlying_price(&self, product: &str) -> Option<f64> {
        self.underlying.get(product).copied().filter(|&p| p > 0.0)
    }

    /// Last-tick price for the resolved hedge contract. Returns None
    /// when no hedge subscription has been registered or no tick has
    /// landed yet — callers performing hedge MTM should fall through
    /// loudly (WARN) rather than silently substituting the options
    /// underlying, which is a different futures contract.
    pub fn hedge_underlying_price(&self, product: &str) -> Option<f64> {
        self.hedge_underlying
            .get(product)
            .copied()
            .filter(|&p| p > 0.0)
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
            return;
        }
        if let Some(product) = self.underlying_instruments.get(&iid).cloned() {
            if price > 0.0 {
                self.underlying.insert(product, price);
            }
        }
        // Hedge underlying is independent of options underlying —
        // separate map, separate insert. Same iid may appear in both
        // when hedge_contract == options_underlying (single-contract
        // products); both inserts are idempotent.
        if let Some(product) = self.hedge_underlying_instruments.get(&iid).cloned() {
            if price > 0.0 {
                self.hedge_underlying.insert(product, price);
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
            return;
        }
        // Audit T1-4: update_bid and update_last update the underlying
        // and hedge_underlying maps when the iid is registered there;
        // update_ask did not, so the hedge tick never refreshed when
        // the only price change was on the ask side. That made
        // hedge_underlying stale and the snapshot's calendar-spread
        // mark drifted off-quote until a bid or last tick landed.
        if let Some(product) = self.underlying_instruments.get(&iid).cloned() {
            if price > 0.0 {
                self.underlying.insert(product, price);
            }
        }
        if let Some(product) = self.hedge_underlying_instruments.get(&iid).cloned() {
            if price > 0.0 {
                self.hedge_underlying.insert(product, price);
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
            return;
        }
        if let Some(product) = self.underlying_instruments.get(&iid).cloned() {
            if price > 0.0 {
                self.underlying.insert(product, price);
            }
        }
        if let Some(product) = self.hedge_underlying_instruments.get(&iid).cloned() {
            if price > 0.0 {
                self.hedge_underlying.insert(product, price);
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

    /// Direct lookup for OptionTick. Zero-alloc: hashes a borrowed
    /// (product, strike_key, expiry, right) without constructing an
    /// owning `OptionKey` (which would `String`-alloc product).
    ///
    /// Hashing must mirror the `Hash` derive on `OptionKey` field-by-
    /// field in declaration order. The hash-consistency unit test
    /// below pins this — if a maintainer reorders OptionKey fields
    /// without updating this function, the test fails immediately.
    pub fn option(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
    ) -> Option<&OptionTick> {
        use std::hash::{BuildHasher, Hash, Hasher};
        let strike_key = strike_key(strike);
        let mut hasher = self.options.hasher().build_hasher();
        // Order matches OptionKey field declaration order. DO NOT
        // reorder without also updating OptionKey + the consistency
        // test in `mod tests`.
        product.hash(&mut hasher);
        strike_key.hash(&mut hasher);
        expiry.hash(&mut hasher);
        right.hash(&mut hasher);
        let h = hasher.finish();
        self.options
            .raw_entry()
            .from_hash(h, |k| {
                k.product == product
                    && k.strike_key == strike_key
                    && k.expiry == expiry
                    && k.right == right
            })
            .map(|(_, v)| v)
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
    fn raw_entry_hash_matches_owning_optionkey_hash() {
        // The zero-alloc `option()` lookup hashes a borrowed key
        // field-by-field. This MUST produce the same hash as the
        // owning `OptionKey { product: String, … }` — otherwise the
        // raw_entry probe lands in a different bucket and silently
        // misses. Pin the invariant so a maintainer reordering
        // OptionKey fields (or changing the field types) gets a
        // loud compile-test failure rather than silent prod misses.
        use std::hash::{BuildHasher, Hash, Hasher};

        let s = MarketDataState::new();
        let strike = 6.05_f64;
        let expiry = exp();
        let right = Right::Call;
        let strike_k = strike_key(strike);
        let owning = OptionKey {
            product: "HG".to_string(),
            strike_key: strike_k,
            expiry,
            right,
        };

        let mut h_owning = s.options.hasher().build_hasher();
        owning.hash(&mut h_owning);
        let owning_hash = h_owning.finish();

        let mut h_borrowed = s.options.hasher().build_hasher();
        "HG".hash(&mut h_borrowed);
        strike_k.hash(&mut h_borrowed);
        expiry.hash(&mut h_borrowed);
        right.hash(&mut h_borrowed);
        let borrowed_hash = h_borrowed.finish();

        assert_eq!(
            owning_hash, borrowed_hash,
            "raw_entry hash must match OptionKey derive(Hash) — \
             field order or type changed in OptionKey?"
        );
    }

    #[test]
    fn raw_entry_lookup_matches_owning_lookup_across_products() {
        // Multiple products + strikes + rights — sanity check the
        // raw_entry path resolves correctly across collision domains.
        let mut s = MarketDataState::new();
        s.register_option("HG", 6.05, exp(), Right::Call, InstrumentId(1));
        s.register_option("HG", 6.05, exp(), Right::Put, InstrumentId(2));
        s.register_option("HG", 6.10, exp(), Right::Call, InstrumentId(3));
        s.register_option("ETH", 6.05, exp(), Right::Call, InstrumentId(4));

        assert_eq!(s.option("HG", 6.05, exp(), Right::Call).unwrap().instrument_id, Some(InstrumentId(1)));
        assert_eq!(s.option("HG", 6.05, exp(), Right::Put).unwrap().instrument_id, Some(InstrumentId(2)));
        assert_eq!(s.option("HG", 6.10, exp(), Right::Call).unwrap().instrument_id, Some(InstrumentId(3)));
        assert_eq!(s.option("ETH", 6.05, exp(), Right::Call).unwrap().instrument_id, Some(InstrumentId(4)));
        // Negatives.
        assert!(s.option("HG", 6.15, exp(), Right::Call).is_none());
        assert!(s.option("XX", 6.05, exp(), Right::Call).is_none());
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
    fn hedge_underlying_routes_independently_of_options_underlying() {
        let mut s = MarketDataState::new();
        // Options underlying = HGK6, hedge contract = HGM6 — distinct
        // instrument ids, distinct prices.
        s.register_underlying("HG", InstrumentId(50));
        s.register_hedge_underlying("HG", InstrumentId(60));
        s.update_last(InstrumentId(50), 5.96, 1);
        s.update_bid(InstrumentId(60), 6.07, 1, 2);
        assert_eq!(s.underlying_price("HG"), Some(5.96));
        assert_eq!(s.hedge_underlying_price("HG"), Some(6.07));
    }

    #[test]
    fn hedge_underlying_price_none_when_unregistered() {
        let s = MarketDataState::new();
        assert!(s.hedge_underlying_price("HG").is_none());
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
