//! `PortfolioState` — aggregate position book.
//!
//! Maps to today's `src/position_manager.py::PortfolioState` but with
//! v3 architecture: the position book consumes a [`MarketView`] trait
//! for greek refresh, doesn't directly own market data or vol surface
//! state.

use chrono::{DateTime, NaiveDate, Utc};
use corsair_broker_api::Right;
use corsair_pricing::greeks::compute_greeks;
use std::collections::HashMap;

/// Audit T4-20: total positions excluded from `compute_mtm_pnl` since
/// process boot due to a non-positive `live` price (no quote, no
/// cached value). Snapshot publisher reads this for telemetry; a
/// non-zero value flags one or more stale-quote contracts.
pub static MTM_SKIPPED_POSITIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

use crate::aggregation::{AggregateResult, PortfolioGreeks};
use crate::fill::FillOutcome;
use crate::market_view::{strike_key as strike_key_fn, MarketView};
use crate::position::{Position, PositionKey, ProductRegistry};

/// Aggregate portfolio state.
///
/// Owns:
/// - Vec<Position> (ordered by insertion)
/// - HashMap<PositionKey, usize> (index for fast lookup)
/// - daily accounting (fills_today, spread_capture, daily_pnl,
///   realized_pnl_persisted, session_open_nlv)
/// - ProductRegistry
///
/// Does NOT own market data, vol surfaces, kill switches, or order
/// lifecycle — those are in their own crates.
pub struct PortfolioState {
    positions: Vec<Position>,
    /// Index for O(1) lookup on add_fill. Values are indices into
    /// `positions`. Rebuilt after position removal to maintain
    /// consistency.
    index: HashMap<PositionKey, usize>,
    registry: ProductRegistry,

    // Daily accounting
    pub fills_today: u32,
    pub spread_capture_today: f64,
    pub spread_capture_mid_today: f64,
    pub daily_pnl: f64,
    pub realized_pnl_persisted: f64,
    pub session_open_nlv: Option<f64>,
}

impl PortfolioState {
    pub fn new(registry: ProductRegistry) -> Self {
        Self {
            positions: Vec::new(),
            index: HashMap::new(),
            registry,
            fills_today: 0,
            spread_capture_today: 0.0,
            spread_capture_mid_today: 0.0,
            daily_pnl: 0.0,
            realized_pnl_persisted: 0.0,
            session_open_nlv: None,
        }
    }

    /// Construct an empty `PortfolioState` with no products. Mostly
    /// for tests; production should always register at least one.
    pub fn empty() -> Self {
        Self::new(ProductRegistry::new())
    }

    pub fn registry(&self) -> &ProductRegistry {
        &self.registry
    }

    pub fn registry_mut(&mut self) -> &mut ProductRegistry {
        &mut self.registry
    }

    pub fn positions(&self) -> &[Position] {
        &self.positions
    }

    pub fn position_count(&self) -> usize {
        self.positions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    fn make_key(product: &str, strike: f64, expiry: NaiveDate, right: Right) -> PositionKey {
        PositionKey {
            product: product.to_string(),
            strike_key: strike_key_fn(strike),
            expiry,
            right,
        }
    }

    /// Replace the entire position book. Used after `seed_from_broker`
    /// reconciliation. Preserves daily accounting fields.
    pub fn replace_positions(&mut self, positions: Vec<Position>) {
        self.positions = positions;
        self.rebuild_index();
    }

    fn rebuild_index(&mut self) {
        self.index.clear();
        for (i, p) in self.positions.iter().enumerate() {
            let key = Self::make_key(&p.product, p.strike, p.expiry, p.right);
            self.index.insert(key, i);
        }
    }

    /// Apply a fill to the book. Returns the outcome describing what
    /// happened (for the caller's logging / risk-callback decisions).
    ///
    /// Mirrors `position_manager.add_fill`. Audit T1-3 (2026-05-05):
    /// avg_fill_price now uses a running VWAP over same-direction
    /// fills (and resets to fill_price on a flip), instead of the
    /// prior "set to last fill price" simplification. realized_on_close
    /// reads avg_fill_price as the cost basis, so a true VWAP
    /// improves realized accuracy between periodic IBKR reconciles —
    /// reconcile still re-syncs against IBKR's avgCost periodically
    /// to correct any residual drift from partial fills the broker
    /// reports differently than us.
    pub fn add_fill(
        &mut self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
        quantity: i32,
        fill_price: f64,
        spread_captured: f64,
        spread_captured_mid: f64,
    ) -> FillOutcome {
        if quantity == 0 {
            return FillOutcome::Rejected("zero quantity".into());
        }

        let multiplier = match self.registry.multiplier_for(product) {
            Some(m) => m,
            None => {
                log::warn!(
                    "add_fill: product {} not registered; defaulting multiplier to 100",
                    product
                );
                100.0
            }
        };

        let key = Self::make_key(product, strike, expiry, right);
        if let Some(&idx) = self.index.get(&key) {
            // Existing position. Merge. Read fields up front so we
            // don't hold a mutable borrow across record_fill_accounting.
            let prev_qty = self.positions[idx].quantity;
            let prev_avg = self.positions[idx].avg_fill_price;
            let new_qty = prev_qty + quantity;

            // Realized accounting on close/partial-close/flip. Note:
            // `prev_avg` is the position's last-fill price, not the
            // true IBKR VWAP — the existing simplification (see line
            // ~118) means this number can drift between periodic
            // reconciles. Periodic reconcile re-syncs avg_fill_price
            // to IBKR's avg_cost / multiplier, after which subsequent
            // closes are accurate. Without this accumulator,
            // realized_pnl_persisted stays at 0.0 forever and the
            // daily P&L kill misses every closed-leg P&L (Finding B).
            let realized_delta =
                realized_on_close(prev_qty, prev_avg, quantity, fill_price, multiplier);
            self.realized_pnl_persisted += realized_delta;

            if new_qty == 0 {
                self.positions.remove(idx);
                self.rebuild_index();
                self.record_fill_accounting(quantity, spread_captured, spread_captured_mid);
                return FillOutcome::Closed;
            }
            // Mutate fields, drop the borrow, then update accounting.
            //
            // VWAP rule: when same-direction (open or increase), blend
            // the fill into the running average using ABSOLUTE qty so
            // the math is symmetric for longs and shorts. When the
            // fill closes / partially closes / flips, prev_avg stays
            // attached to the residual position — closing fills don't
            // change the average. On a flip, the residual on the new
            // side opens at fill_price (the leg that crosses zero
            // realized at prev_avg, then the residual opens fresh).
            //
            // Audit T1-3: prior behavior `pos.avg_fill_price = fill_price`
            // overwrote on every same-side fill, so the position's
            // average drifted to whatever the last fill happened at.
            // With this fix, realized_on_close (which reads prev_avg)
            // gets a stable VWAP between IBKR reconciles.
            {
                let pos = &mut self.positions[idx];
                let same_dir = prev_qty.signum() == quantity.signum();
                let flipped = prev_qty.signum() != new_qty.signum() && prev_qty.signum() != 0;
                if same_dir {
                    // Open or increase: VWAP across absolute qtys.
                    let prev_abs = prev_qty.unsigned_abs() as f64;
                    let fill_abs = quantity.unsigned_abs() as f64;
                    let new_abs = new_qty.unsigned_abs() as f64;
                    if new_abs > 0.0 {
                        pos.avg_fill_price =
                            (prev_avg * prev_abs + fill_price * fill_abs) / new_abs;
                    }
                } else if flipped {
                    // Crossed zero: residual opens at fill_price.
                    pos.avg_fill_price = fill_price;
                }
                // else: partial close — keep prev_avg attached to the
                // residual position; closing fills don't shift VWAP.
                pos.quantity = new_qty;
                pos.fill_time = Utc::now();
            }
            self.record_fill_accounting(quantity, spread_captured, spread_captured_mid);

            if (prev_qty > 0 && new_qty > 0 && new_qty > prev_qty)
                || (prev_qty < 0 && new_qty < 0 && new_qty < prev_qty)
            {
                FillOutcome::Increased
            } else if prev_qty.signum() != new_qty.signum() && prev_qty.signum() != 0 {
                FillOutcome::Flipped
            } else {
                FillOutcome::PartiallyClosed
            }
        } else {
            // New position.
            let pos = Position {
                product: product.to_string(),
                strike,
                expiry,
                right,
                quantity,
                avg_fill_price: fill_price,
                fill_time: Utc::now(),
                multiplier,
                delta: 0.0,
                gamma: 0.0,
                theta: 0.0,
                vega: 0.0,
                current_price: 0.0,
            };
            self.positions.push(pos);
            self.index.insert(key, self.positions.len() - 1);
            self.record_fill_accounting(quantity, spread_captured, spread_captured_mid);
            FillOutcome::Opened
        }
    }

    fn record_fill_accounting(
        &mut self,
        quantity: i32,
        spread_captured: f64,
        spread_captured_mid: f64,
    ) {
        self.fills_today += quantity.unsigned_abs();
        self.spread_capture_today += spread_captured;
        self.spread_capture_mid_today += spread_captured_mid;
    }

    /// Recompute Greeks for every position using the supplied
    /// [`MarketView`]. Skips positions whose product has no
    /// underlying yet, or whose IV resolution returns None — those
    /// positions retain their previous Greeks until next refresh.
    pub fn refresh_greeks(&mut self, market: &dyn MarketView) {
        for pos in self.positions.iter_mut() {
            let f = match market.underlying_price(&pos.product) {
                Some(v) if v > 0.0 => v,
                _ => continue,
            };
            let iv = market
                .iv_for(&pos.product, pos.strike, pos.expiry, pos.right)
                .unwrap_or_else(|| {
                    self.registry
                        .get(&pos.product)
                        .map(|p| p.default_iv)
                        .unwrap_or(0.30)
                });
            let tte = time_to_expiry_years(pos.expiry);
            if tte <= 0.0 {
                continue;
            }
            let g = compute_greeks(f, pos.strike, tte, iv, 0.0, pos.right.as_char(), pos.multiplier);
            pos.delta = g.delta;
            pos.gamma = g.gamma;
            pos.theta = g.theta;
            pos.vega = g.vega;
            if let Some(p) = market.current_price(&pos.product, pos.strike, pos.expiry, pos.right)
            {
                if p > 0.0 {
                    pos.current_price = p;
                }
            }
        }
    }

    /// Aggregate Greeks per-product AND across all products.
    pub fn aggregate(&self) -> AggregateResult {
        let mut total = PortfolioGreeks::default();
        let mut per_product: HashMap<String, PortfolioGreeks> = HashMap::new();
        for p in &self.positions {
            total.add(p.quantity, p.delta, p.theta, p.vega, p.gamma);
            per_product
                .entry(p.product.clone())
                .or_default()
                .add(p.quantity, p.delta, p.theta, p.vega, p.gamma);
        }
        AggregateResult { total, per_product }
    }

    /// Convenience: net delta across all products. Use
    /// `aggregate().per_product[product].net_delta` for per-product.
    pub fn net_delta(&self) -> f64 {
        self.positions
            .iter()
            .map(|p| p.delta * p.quantity as f64)
            .sum()
    }

    pub fn net_theta(&self) -> f64 {
        self.positions
            .iter()
            .map(|p| p.theta * p.quantity as f64)
            .sum()
    }

    pub fn net_vega(&self) -> f64 {
        self.positions
            .iter()
            .map(|p| p.vega * p.quantity as f64)
            .sum()
    }

    pub fn net_gamma(&self) -> f64 {
        self.positions
            .iter()
            .map(|p| p.gamma * p.quantity as f64)
            .sum()
    }

    pub fn long_count(&self) -> u32 {
        self.positions.iter().filter(|p| p.quantity > 0).count() as u32
    }

    pub fn short_count(&self) -> u32 {
        self.positions.iter().filter(|p| p.quantity < 0).count() as u32
    }

    pub fn gross_positions(&self) -> u32 {
        self.positions.iter().filter(|p| p.quantity != 0).count() as u32
    }

    /// Per-product delta (signed, qty-weighted).
    pub fn delta_for_product(&self, product: &str) -> f64 {
        self.positions
            .iter()
            .filter(|p| p.product == product)
            .map(|p| p.delta * p.quantity as f64)
            .sum()
    }

    pub fn theta_for_product(&self, product: &str) -> f64 {
        self.positions
            .iter()
            .filter(|p| p.product == product)
            .map(|p| p.theta * p.quantity as f64)
            .sum()
    }

    pub fn vega_for_product(&self, product: &str) -> f64 {
        self.positions
            .iter()
            .filter(|p| p.product == product)
            .map(|p| p.vega * p.quantity as f64)
            .sum()
    }

    pub fn positions_for_product(&self, product: &str) -> Vec<&Position> {
        self.positions.iter().filter(|p| p.product == product).collect()
    }

    /// Mark-to-market P&L using the supplied [`MarketView`] for
    /// fresh prices, falling back to each position's cached
    /// `current_price` when no live observation exists.
    ///
    /// MTM = sum_pos(quantity * (current_price - avg_fill_price) * multiplier)
    ///
    /// Audit T4-20: positions with `live <= 0.0` (no current quote
    /// AND no cached price) are silently skipped. That is correct
    /// behavior — pricing them at zero would over-report MTM losses
    /// — but the operator should know when a position is excluded.
    /// We bump a counter (`MTM_SKIPPED_POSITIONS`) on every skip and
    /// log a throttled WARN; the snapshot publisher / dashboard can
    /// surface this number to flag stale-quote contracts.
    pub fn compute_mtm_pnl(&self, market: &dyn MarketView) -> f64 {
        let mut total = 0.0;
        let mut skipped = 0u64;
        for p in &self.positions {
            let live = market
                .current_price(&p.product, p.strike, p.expiry, p.right)
                .unwrap_or(p.current_price);
            if live <= 0.0 {
                skipped += 1;
                continue;
            }
            total += (p.quantity as f64) * (live - p.avg_fill_price) * p.multiplier;
        }
        if skipped > 0 {
            let total_skipped = MTM_SKIPPED_POSITIONS
                .fetch_add(skipped, std::sync::atomic::Ordering::Relaxed)
                + skipped;
            // Throttle: log every power-of-two count so the operator
            // sees a steady stream early then it backs off.
            if total_skipped.is_power_of_two() {
                log::warn!(
                    "compute_mtm_pnl: {} position(s) skipped this call due to no \
                     live or cached price; running total since boot = {}",
                    skipped,
                    total_skipped
                );
            }
        }
        total
    }

    /// Reset daily counters at session rollover (CME 17:00 CT).
    /// Does NOT touch positions — those persist across sessions.
    /// `realized_pnl_persisted` is also retained until daily_state
    /// is explicitly reset by the caller.
    pub fn reset_daily(&mut self) {
        self.fills_today = 0;
        self.spread_capture_today = 0.0;
        self.spread_capture_mid_today = 0.0;
        self.daily_pnl = 0.0;
    }

    /// Find a position by key (for snapshot / risk inspection).
    pub fn get_position(
        &self,
        product: &str,
        strike: f64,
        expiry: NaiveDate,
        right: Right,
    ) -> Option<&Position> {
        let key = Self::make_key(product, strike, expiry, right);
        self.index.get(&key).map(|&i| &self.positions[i])
    }
}

/// Compute time-to-expiry in years from a date. Uses CME's calendar
/// convention: 365.25 days/year.
///
/// COMEX HG options expire at 13:00 ET (18:00 UTC in winter / 17:00
/// UTC in summer DST; we approximate with 18:00 UTC). If the codebase
/// grows to other products with different settlement times (e.g. ETH
/// 16:00 CT), generalize this to a per-product cutoff. The prior
/// 20:00 UTC anchor was wrong for HG and overstated TTE on the last
/// trading day — small pricing error on the wing where TTE matters
/// most.
pub fn time_to_expiry_years(expiry: NaiveDate) -> f64 {
    let now: DateTime<Utc> = Utc::now();
    let expiry_end = expiry.and_hms_opt(18, 0, 0).unwrap(); // 18:00 UTC ≈ 13:00 ET (HG)
    let expiry_dt = DateTime::<Utc>::from_naive_utc_and_offset(expiry_end, Utc);
    let secs = (expiry_dt - now).num_seconds() as f64;
    if secs <= 0.0 {
        0.0
    } else {
        secs / (365.25 * 24.0 * 3600.0)
    }
}

/// Realized P&L contribution of a fill that closes (fully, partially,
/// or by flipping) a prior position. Returns 0.0 when the fill is
/// purely opening or increasing. Sign convention matches MTM:
/// positive = gain.
///
/// `prev_avg` should be the position's `avg_fill_price` BEFORE this
/// fill mutates it. Post-T1-3 (2026-05-05) that's a running VWAP
/// across same-direction fills, so realized P&L accumulates against
/// a stable cost basis. Periodic IBKR reconciles still re-sync
/// against avgCost to correct any drift from partial fills.
pub(crate) fn realized_on_close(
    prev_qty: i32,
    prev_avg: f64,
    fill_qty: i32,
    fill_price: f64,
    multiplier: f64,
) -> f64 {
    if prev_qty == 0 || fill_qty == 0 {
        return 0.0;
    }
    if prev_qty.signum() == fill_qty.signum() {
        return 0.0; // increasing — no realized
    }
    let closed_abs = fill_qty.abs().min(prev_qty.abs());
    // Sign of the realized contribution: long close (prev>0) realizes
    // (fill - avg) per share; short close (prev<0) realizes (avg - fill).
    // Combined: (avg - fill) × closed_abs × sign(prev_qty<0 ? +1 : -1).
    let signed = if prev_qty > 0 {
        -(closed_abs as f64)
    } else {
        closed_abs as f64
    };
    (prev_avg - fill_price) * signed * multiplier
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_view::RecordingMarketView;
    use crate::position::ProductInfo;
    use chrono::NaiveDate;

    fn registry() -> ProductRegistry {
        let mut r = ProductRegistry::new();
        r.register(ProductInfo {
            product: "HG".into(),
            multiplier: 25_000.0,
            default_iv: 0.30,
        });
        r
    }

    fn exp_2026_06() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 26).unwrap()
    }

    // ── add_fill ────────────────────────────────────────────────

    #[test]
    fn fill_into_empty_book_opens_position() {
        let mut p = PortfolioState::new(registry());
        let outcome = p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.025, 0.0, 0.0);
        assert_eq!(outcome, FillOutcome::Opened);
        assert_eq!(p.position_count(), 1);
        assert_eq!(p.fills_today, 1);
    }

    #[test]
    fn fill_increasing_long_position() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 2, 0.025, 0.0, 0.0);
        let outcome = p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.030, 0.0, 0.0);
        assert_eq!(outcome, FillOutcome::Increased);
        assert_eq!(p.positions()[0].quantity, 3);
    }

    #[test]
    fn opposite_fill_partially_closes() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 3, 0.025, 0.0, 0.0);
        let outcome = p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, -1, 0.030, 0.0, 0.0);
        assert_eq!(outcome, FillOutcome::PartiallyClosed);
        assert_eq!(p.positions()[0].quantity, 2);
    }

    #[test]
    fn opposite_fill_closes_completely() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 2, 0.025, 0.0, 0.0);
        let outcome = p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, -2, 0.030, 0.0, 0.0);
        assert_eq!(outcome, FillOutcome::Closed);
        assert_eq!(p.position_count(), 0);
    }

    #[test]
    fn over_close_flips_to_short() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.025, 0.0, 0.0);
        let outcome = p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, -3, 0.030, 0.0, 0.0);
        assert_eq!(outcome, FillOutcome::Flipped);
        assert_eq!(p.positions()[0].quantity, -2);
    }

    #[test]
    fn zero_quantity_rejected() {
        let mut p = PortfolioState::new(registry());
        let outcome = p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 0, 0.025, 0.0, 0.0);
        matches!(outcome, FillOutcome::Rejected(_));
        assert_eq!(p.position_count(), 0);
    }

    #[test]
    fn unregistered_product_uses_fallback_multiplier() {
        let mut p = PortfolioState::empty();
        p.add_fill("ZZZ", 10.0, exp_2026_06(), Right::Call, 1, 0.5, 0.0, 0.0);
        assert_eq!(p.position_count(), 1);
        assert_eq!(p.positions()[0].multiplier, 100.0);
    }

    // ── Greeks aggregation ────────────────────────────────────

    #[test]
    fn refresh_greeks_with_market_view_populates_position() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.025, 0.0, 0.0);
        let view = RecordingMarketView::new();
        view.set_underlying("HG", 6.05);
        view.set_iv("HG", 6.05, exp_2026_06(), Right::Call, 0.45);
        view.set_current_price("HG", 6.05, exp_2026_06(), Right::Call, 0.026);
        p.refresh_greeks(&view);
        let pos = &p.positions()[0];
        assert!(pos.delta != 0.0, "delta populated");
        assert!(pos.vega > 0.0, "vega positive");
        assert_eq!(pos.current_price, 0.026);
    }

    #[test]
    fn refresh_skips_positions_with_no_underlying() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.025, 0.0, 0.0);
        let view = RecordingMarketView::new(); // empty
        p.refresh_greeks(&view);
        assert_eq!(p.positions()[0].delta, 0.0);
    }

    #[test]
    fn aggregate_sums_across_products() {
        let mut r = ProductRegistry::new();
        r.register(ProductInfo {
            product: "HG".into(),
            multiplier: 25_000.0,
            default_iv: 0.30,
        });
        r.register(ProductInfo {
            product: "ETHUSDRR".into(),
            multiplier: 50.0,
            default_iv: 0.50,
        });
        let mut p = PortfolioState::new(r);
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.025, 0.0, 0.0);
        p.add_fill("ETHUSDRR", 3000.0, exp_2026_06(), Right::Put, -2, 50.0, 0.0, 0.0);
        let agg = p.aggregate();
        assert_eq!(agg.total.long_count, 1);
        assert_eq!(agg.total.short_count, 1);
        assert_eq!(agg.per_product.len(), 2);
    }

    #[test]
    fn delta_for_product_isolates_correctly() {
        let mut r = ProductRegistry::new();
        r.register(ProductInfo {
            product: "HG".into(),
            multiplier: 25_000.0,
            default_iv: 0.30,
        });
        r.register(ProductInfo {
            product: "ETH".into(),
            multiplier: 50.0,
            default_iv: 0.40,
        });
        let mut p = PortfolioState::new(r);
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.025, 0.0, 0.0);
        p.add_fill("ETH", 3000.0, exp_2026_06(), Right::Call, 2, 50.0, 0.0, 0.0);
        let view = RecordingMarketView::new();
        view.set_underlying("HG", 6.05);
        view.set_underlying("ETH", 3000.0);
        view.set_iv("HG", 6.05, exp_2026_06(), Right::Call, 0.40);
        view.set_iv("ETH", 3000.0, exp_2026_06(), Right::Call, 0.60);
        p.refresh_greeks(&view);
        let dh = p.delta_for_product("HG");
        let de = p.delta_for_product("ETH");
        assert!(dh > 0.0 && de > 0.0);
        // HG and ETH should not contaminate each other.
        assert_ne!(dh, de);
    }

    // ── MTM ────────────────────────────────────────────────────

    #[test]
    fn mtm_uses_live_market_price() {
        let mut p = PortfolioState::new(registry());
        // Long 1 call @ 0.025; current 0.030; multiplier 25000.
        // MTM = (0.030 - 0.025) * 1 * 25000 = $125
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.025, 0.0, 0.0);
        let view = RecordingMarketView::new();
        view.set_current_price("HG", 6.05, exp_2026_06(), Right::Call, 0.030);
        let mtm = p.compute_mtm_pnl(&view);
        assert!((mtm - 125.0).abs() < 1e-6, "got {mtm}");
    }

    #[test]
    fn mtm_zero_when_no_price_and_no_cached() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.025, 0.0, 0.0);
        let view = RecordingMarketView::new(); // no current price
        let mtm = p.compute_mtm_pnl(&view);
        assert_eq!(mtm, 0.0);
    }

    // ── Realized P&L accumulation (Finding B) ──────────────────

    #[test]
    fn realized_on_close_short_then_buy_to_close() {
        // Short 2 @ 0.085, BUY 2 @ 0.082 → realized = +$150.
        let r = realized_on_close(-2, 0.085, 2, 0.082, 25_000.0);
        assert!((r - 150.0).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn realized_on_close_long_then_sell_to_close_at_loss() {
        // Long 2 @ 0.085, SELL 2 @ 0.082 → realized = -$150.
        let r = realized_on_close(2, 0.085, -2, 0.082, 25_000.0);
        assert!((r - -150.0).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn realized_on_close_partial() {
        // Short 4 @ 0.085, BUY 1 @ 0.082 (partial close) → +$75.
        let r = realized_on_close(-4, 0.085, 1, 0.082, 25_000.0);
        assert!((r - 75.0).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn realized_on_close_increasing_returns_zero() {
        // Short 2 @ 0.085, SELL 1 @ 0.084 (more short) → 0.
        let r = realized_on_close(-2, 0.085, -1, 0.084, 25_000.0);
        assert_eq!(r, 0.0);
    }

    #[test]
    fn realized_on_close_flip_realizes_only_closed_portion() {
        // Short 2 @ 0.085, BUY 5 @ 0.082 (closes 2, opens 3 long).
        // Realized portion: 2 contracts of short closed at +$0.003 each
        // = +$150. The new long opens at fill_price; not part of this fn.
        let r = realized_on_close(-2, 0.085, 5, 0.082, 25_000.0);
        assert!((r - 150.0).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn add_fill_accumulates_realized_on_close() {
        let mut p = PortfolioState::new(registry());
        // Short 2 @ 0.085, then close at 0.082 → realized_pnl_persisted += $150.
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, -2, 0.085, 0.0, 0.0);
        assert_eq!(p.realized_pnl_persisted, 0.0, "no realized on open");
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 2, 0.082, 0.0, 0.0);
        assert!(
            (p.realized_pnl_persisted - 150.0).abs() < 1e-6,
            "got {}",
            p.realized_pnl_persisted
        );
    }

    // ── VWAP cost-basis (Audit T1-3) ───────────────────────────

    #[test]
    fn vwap_blends_same_direction_fills() {
        let mut p = PortfolioState::new(registry());
        // Long 2 @ 0.025, then long 1 @ 0.030. VWAP = (0.025*2 + 0.030*1)/3 = 0.02667.
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 2, 0.025, 0.0, 0.0);
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.030, 0.0, 0.0);
        let pos = &p.positions()[0];
        assert!(
            (pos.avg_fill_price - 0.026666666666666665).abs() < 1e-9,
            "VWAP mismatch: got {}",
            pos.avg_fill_price
        );
    }

    #[test]
    fn vwap_short_side_blends_correctly() {
        let mut p = PortfolioState::new(registry());
        // Short 2 @ 0.085, then short 3 @ 0.090. VWAP = (0.085*2 + 0.090*3)/5 = 0.088.
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, -2, 0.085, 0.0, 0.0);
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, -3, 0.090, 0.0, 0.0);
        let pos = &p.positions()[0];
        assert!((pos.avg_fill_price - 0.088).abs() < 1e-9, "got {}", pos.avg_fill_price);
        assert_eq!(pos.quantity, -5);
    }

    #[test]
    fn vwap_partial_close_keeps_avg() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 4, 0.025, 0.0, 0.0);
        // Partial close — avg should stay at 0.025.
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, -1, 0.030, 0.0, 0.0);
        let pos = &p.positions()[0];
        assert!((pos.avg_fill_price - 0.025).abs() < 1e-9);
        assert_eq!(pos.quantity, 3);
    }

    #[test]
    fn vwap_flip_resets_to_fill_price() {
        let mut p = PortfolioState::new(registry());
        // Long 1 @ 0.025, sell 3 @ 0.030 → flip to short 2 at fill_price 0.030.
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.025, 0.0, 0.0);
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, -3, 0.030, 0.0, 0.0);
        let pos = &p.positions()[0];
        assert_eq!(pos.quantity, -2);
        assert!((pos.avg_fill_price - 0.030).abs() < 1e-9);
    }

    #[test]
    fn vwap_realized_uses_blended_cost_basis() {
        let mut p = PortfolioState::new(registry());
        // Two opens at different prices, then close.
        // Long 2 @ 0.025, long 2 @ 0.035 → VWAP 0.030.
        // Sell 4 @ 0.040 → realized = (0.040 - 0.030) * 4 * 25000 = $1000.
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 2, 0.025, 0.0, 0.0);
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 2, 0.035, 0.0, 0.0);
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, -4, 0.040, 0.0, 0.0);
        assert!(
            (p.realized_pnl_persisted - 1000.0).abs() < 1e-6,
            "expected $1000 realized via VWAP, got {}",
            p.realized_pnl_persisted
        );
    }

    #[test]
    fn add_fill_no_realized_on_pure_open() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.025, 0.0, 0.0);
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.027, 0.0, 0.0);
        assert_eq!(p.realized_pnl_persisted, 0.0);
    }

    // ── Daily reset ───────────────────────────────────────────

    #[test]
    fn reset_daily_clears_counters_keeps_positions() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 1, 0.025, 1.5, 1.0);
        assert!(p.fills_today > 0);
        p.reset_daily();
        assert_eq!(p.fills_today, 0);
        assert_eq!(p.spread_capture_today, 0.0);
        assert_eq!(p.position_count(), 1, "positions persist across day reset");
    }

    // ── Index integrity ───────────────────────────────────────

    #[test]
    fn lookup_after_close_returns_none() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 2, 0.025, 0.0, 0.0);
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, -2, 0.030, 0.0, 0.0);
        assert!(p
            .get_position("HG", 6.05, exp_2026_06(), Right::Call)
            .is_none());
    }

    #[test]
    fn lookup_finds_existing_position() {
        let mut p = PortfolioState::new(registry());
        p.add_fill("HG", 6.05, exp_2026_06(), Right::Call, 2, 0.025, 0.0, 0.0);
        let pos = p
            .get_position("HG", 6.05, exp_2026_06(), Right::Call)
            .unwrap();
        assert_eq!(pos.quantity, 2);
    }

    #[test]
    fn replace_positions_rebuilds_index() {
        let mut p = PortfolioState::new(registry());
        let pos = Position {
            product: "HG".into(),
            strike: 6.05,
            expiry: exp_2026_06(),
            right: Right::Call,
            quantity: 5,
            avg_fill_price: 0.025,
            fill_time: Utc::now(),
            multiplier: 25_000.0,
            delta: 0.0,
            gamma: 0.0,
            theta: 0.0,
            vega: 0.0,
            current_price: 0.0,
        };
        p.replace_positions(vec![pos]);
        assert_eq!(p.position_count(), 1);
        let found = p.get_position("HG", 6.05, exp_2026_06(), Right::Call);
        assert!(found.is_some());
    }
}
