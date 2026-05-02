//! Multi-product hedge fanout — dispatches force_flat / flatten_on_halt
//! / mtm_usd / hedge_qty_for_product across all registered managers.
//!
//! Mirrors `HedgeFanout` in Python. Exception isolation: a failure in
//! one product's hedge path doesn't block the others.

use crate::manager::{HedgeAction, HedgeManager};
use corsair_position::PortfolioState;

/// Multi-product hedge dispatcher.
pub struct HedgeFanout {
    managers: Vec<HedgeManager>,
}

impl HedgeFanout {
    pub fn new(managers: Vec<HedgeManager>) -> Self {
        Self { managers }
    }

    pub fn managers(&self) -> &[HedgeManager] {
        &self.managers
    }

    /// Look up by product key. None if not registered.
    pub fn for_product(&self, product: &str) -> Option<&HedgeManager> {
        self.managers
            .iter()
            .find(|m| m.config().product == product)
    }

    pub fn for_product_mut(&mut self, product: &str) -> Option<&mut HedgeManager> {
        self.managers
            .iter_mut()
            .find(|m| m.config().product == product)
    }

    /// Per-product hedge qty (used by effective_delta_gating).
    /// Returns 0 if product isn't registered.
    pub fn hedge_qty_for_product(&self, product: &str) -> i32 {
        self.for_product(product)
            .map(|m| m.hedge_qty())
            .unwrap_or(0)
    }

    /// Sum hedge MTM across products. `forwards` provides each
    /// product's current underlying price; missing entries use 0
    /// (manager returns 0 when F<=0).
    pub fn mtm_usd_total(
        &self,
        forwards: &dyn Fn(&str) -> Option<f64>,
    ) -> f64 {
        self.managers
            .iter()
            .map(|m| {
                let f = forwards(&m.config().product).unwrap_or(0.0);
                m.mtm_usd(f)
            })
            .sum()
    }

    /// Force-flat all hedges (delta kill). Returns one action per
    /// manager. Caller routes each Place/LogOnly to the broker.
    pub fn force_flat_all(
        &mut self,
        portfolio: &PortfolioState,
        forward_for: &dyn Fn(&str) -> Option<f64>,
    ) -> Vec<(String, HedgeAction)> {
        let mut out = Vec::new();
        for m in self.managers.iter_mut() {
            let f = forward_for(&m.config().product).unwrap_or(0.0);
            let action = m.force_flat(portfolio, f);
            out.push((m.config().product.clone(), action));
        }
        out
    }

    /// Close out hedges on halt. Returns one action per manager.
    pub fn flatten_on_halt_all(
        &mut self,
        forward_for: &dyn Fn(&str) -> Option<f64>,
    ) -> Vec<(String, HedgeAction)> {
        let mut out = Vec::new();
        for m in self.managers.iter_mut() {
            let f = forward_for(&m.config().product).unwrap_or(0.0);
            let action = m.flatten_on_halt(f);
            out.push((m.config().product.clone(), action));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::{HedgeConfig, HedgeMode};

    fn cfg(product: &str) -> HedgeConfig {
        HedgeConfig {
            product: product.into(),
            multiplier: 25_000.0,
            mode: HedgeMode::Execute,
            tolerance_deltas: 0.5,
            rebalance_on_fill: true,
            rebalance_cadence_sec: 30.0,
            include_in_daily_pnl: true,
            flatten_on_halt_enabled: true,
            ioc_tick_offset: 2,
            hedge_tick_size: 0.0005,
            lockout_days: 30,
        }
    }

    #[test]
    fn for_product_returns_correct_manager() {
        let m1 = HedgeManager::new(cfg("HG"));
        let m2 = HedgeManager::new(cfg("ETH"));
        let f = HedgeFanout::new(vec![m1, m2]);
        assert!(f.for_product("HG").is_some());
        assert!(f.for_product("ETH").is_some());
        assert!(f.for_product("UNKNOWN").is_none());
    }

    #[test]
    fn hedge_qty_per_product() {
        let mut m1 = HedgeManager::new(cfg("HG"));
        m1.state.replace(4, 6.0);
        let mut m2 = HedgeManager::new(cfg("ETH"));
        m2.state.replace(-1, 3000.0);
        let f = HedgeFanout::new(vec![m1, m2]);
        assert_eq!(f.hedge_qty_for_product("HG"), 4);
        assert_eq!(f.hedge_qty_for_product("ETH"), -1);
        assert_eq!(f.hedge_qty_for_product("UNKNOWN"), 0);
    }

    #[test]
    fn mtm_total_sums_across() {
        let mut m1 = HedgeManager::new(cfg("HG"));
        m1.state.replace(3, 6.00);
        let mut m2 = HedgeManager::new(cfg("ETH"));
        m2.state.replace(-1, 3000.0);
        let f = HedgeFanout::new(vec![m1, m2]);
        let forwards = |p: &str| -> Option<f64> {
            match p {
                "HG" => Some(6.05),    // +0.05 * 3 * 25k = +3750
                "ETH" => Some(2950.0), // (2950-3000) * -1 * 25k = +1_250_000
                _ => None,
            }
        };
        let total = f.mtm_usd_total(&forwards);
        assert!((total - (3750.0 + 1_250_000.0)).abs() < 1e-6);
    }
}
