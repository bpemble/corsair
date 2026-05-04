//! Per-product hedge state — quantity, average entry, realized P&L.

use serde::{Deserialize, Serialize};

/// State for one product's hedge position. Updated by
/// `apply_broker_fill` from broker exec events; queried by
/// risk/snapshot/constraint via [`HedgeManager`] accessors.
///
/// Sign convention: positive `hedge_qty` = long underlying exposure
/// (matches IBKR's signed position). avg_entry_F is the volume-
/// weighted average futures price at which we're holding the current
/// position; resets to 0 when hedge_qty returns to 0.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HedgeState {
    pub hedge_qty: i32,
    pub avg_entry_f: f64,
    /// Cumulative realized P&L from closed hedge cycles (in-memory;
    /// resets on restart, not persisted). Surfaced via snapshot so
    /// the dashboard's account row can fold in hedge realized.
    pub realized_pnl_usd: f64,
    /// SystemTime ns at the most recent reconcile-or-fill that updated
    /// this state. Audit T1-5: effective-delta gating in the trader
    /// trusts `hedge_qty`, but that trust depends on a recent
    /// reconcile against IBKR. This field lets the gate fail closed
    /// when the most recent update is too old (>300s by default —
    /// 10× the periodic reconcile cadence). 0 = never updated.
    /// SystemTime (not Instant) so the timestamp can be compared
    /// across crates without sharing a process-static T0.
    #[serde(default)]
    pub last_update_ns: u64,
}

impl HedgeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a fill (from the broker exec stream OR from observe-mode
    /// optimistic update). Updates hedge_qty + avg_entry_f and
    /// crystallizes realized P&L on close/flip.
    ///
    /// Sign convention: `qty` is unsigned, `is_buy` carries direction.
    /// `multiplier` is the futures contract multiplier ($/point).
    ///
    /// Mirrors `_apply_local_fill` in Python.
    pub fn apply_fill(&mut self, is_buy: bool, qty: u32, price: f64, multiplier: f64) {
        self.last_update_ns = systemtime_ns();
        let effect = if is_buy { qty as i32 } else { -(qty as i32) };
        let new_qty = self.hedge_qty + effect;

        if new_qty == 0 {
            // Hedge flat — realize the entire prior position.
            let sign = if self.hedge_qty > 0 { 1.0 } else { -1.0 };
            self.realized_pnl_usd +=
                sign * (price - self.avg_entry_f) * (self.hedge_qty.unsigned_abs() as f64) * multiplier;
            self.hedge_qty = 0;
            self.avg_entry_f = 0.0;
            return;
        }
        // Same-direction: size-weighted average. No realization.
        if (self.hedge_qty >= 0 && effect >= 0) || (self.hedge_qty <= 0 && effect <= 0) {
            let total_notional =
                self.avg_entry_f * (self.hedge_qty as f64) + price * (effect as f64);
            self.hedge_qty = new_qty;
            self.avg_entry_f = total_notional / (new_qty as f64);
            return;
        }
        // Partial reverse: close |effect| at current price.
        if effect.unsigned_abs() < self.hedge_qty.unsigned_abs() {
            let sign = if self.hedge_qty > 0 { 1.0 } else { -1.0 };
            self.realized_pnl_usd +=
                sign * (price - self.avg_entry_f) * (effect.unsigned_abs() as f64) * multiplier;
            self.hedge_qty = new_qty;
            return;
        }
        // Full reverse + flip: close entire prior at price, then open
        // residual on opposite side at the same price.
        let sign = if self.hedge_qty > 0 { 1.0 } else { -1.0 };
        self.realized_pnl_usd +=
            sign * (price - self.avg_entry_f) * (self.hedge_qty.unsigned_abs() as f64) * multiplier;
        self.hedge_qty = new_qty;
        self.avg_entry_f = price;
    }

    /// Mark-to-market in dollars. Returns 0 when flat or when F<=0.
    pub fn mtm_usd(&self, current_f: f64, multiplier: f64) -> f64 {
        if self.hedge_qty == 0 || current_f <= 0.0 {
            return 0.0;
        }
        (current_f - self.avg_entry_f) * (self.hedge_qty as f64) * multiplier
    }

    /// Reset state — used on bootstrap reconcile when broker reports
    /// a different position than we held locally.
    pub fn replace(&mut self, qty: i32, avg_f: f64) {
        self.last_update_ns = systemtime_ns();
        self.hedge_qty = qty;
        self.avg_entry_f = avg_f;
    }

    /// Audit T1-5: returns true when the most recent reconcile/fill
    /// happened within `max_age_ns`. Callers (effective-delta gates)
    /// should treat `hedge_qty` as 0 when this returns false (fail
    /// closed).
    pub fn is_fresh(&self, now_ns: u64, max_age_ns: u64) -> bool {
        if self.last_update_ns == 0 {
            // Never updated — assume freshly-booted runtime that
            // hasn't yet seen its first reconcile. Conservative.
            return false;
        }
        now_ns.saturating_sub(self.last_update_ns) <= max_age_ns
    }
}

fn systemtime_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULT: f64 = 25_000.0; // HG

    #[test]
    fn open_long_from_flat() {
        let mut s = HedgeState::new();
        s.apply_fill(true, 3, 6.05, MULT);
        assert_eq!(s.hedge_qty, 3);
        assert!((s.avg_entry_f - 6.05).abs() < 1e-9);
        assert_eq!(s.realized_pnl_usd, 0.0);
    }

    #[test]
    fn open_short_from_flat() {
        let mut s = HedgeState::new();
        s.apply_fill(false, 2, 6.05, MULT);
        assert_eq!(s.hedge_qty, -2);
    }

    #[test]
    fn same_direction_averages() {
        let mut s = HedgeState::new();
        s.apply_fill(true, 2, 6.00, MULT);
        s.apply_fill(true, 2, 6.10, MULT);
        assert_eq!(s.hedge_qty, 4);
        assert!((s.avg_entry_f - 6.05).abs() < 1e-9);
    }

    #[test]
    fn full_close_realizes_full_pnl() {
        let mut s = HedgeState::new();
        s.apply_fill(true, 3, 6.00, MULT);
        s.apply_fill(false, 3, 6.10, MULT);
        assert_eq!(s.hedge_qty, 0);
        assert_eq!(s.avg_entry_f, 0.0);
        // (6.10 - 6.00) * 3 * 25000 = 7500
        assert!((s.realized_pnl_usd - 7500.0).abs() < 1e-6);
    }

    #[test]
    fn partial_close_realizes_proportional() {
        let mut s = HedgeState::new();
        s.apply_fill(true, 4, 6.00, MULT);
        s.apply_fill(false, 1, 6.05, MULT);
        assert_eq!(s.hedge_qty, 3);
        assert!((s.avg_entry_f - 6.00).abs() < 1e-9);
        // (6.05 - 6.00) * 1 * 25000 = 1250
        assert!((s.realized_pnl_usd - 1250.0).abs() < 1e-6);
    }

    #[test]
    fn over_close_flips() {
        let mut s = HedgeState::new();
        s.apply_fill(true, 2, 6.00, MULT);
        s.apply_fill(false, 5, 6.10, MULT);
        // Closes 2 at 6.10 (PnL 5000), flips short 3 at 6.10.
        assert_eq!(s.hedge_qty, -3);
        assert!((s.avg_entry_f - 6.10).abs() < 1e-9);
        assert!((s.realized_pnl_usd - 5000.0).abs() < 1e-6);
    }

    #[test]
    fn short_close_realizes_correctly() {
        let mut s = HedgeState::new();
        s.apply_fill(false, 4, 6.10, MULT);
        s.apply_fill(true, 4, 6.00, MULT);
        // Short profit = (6.00 - 6.10) * -4 * 25000 = $10,000
        assert_eq!(s.hedge_qty, 0);
        assert!((s.realized_pnl_usd - 10000.0).abs() < 1e-6);
    }

    #[test]
    fn mtm_zero_when_flat() {
        let s = HedgeState::new();
        assert_eq!(s.mtm_usd(6.05, MULT), 0.0);
    }

    #[test]
    fn mtm_long_position_profits_on_rise() {
        let mut s = HedgeState::new();
        s.apply_fill(true, 3, 6.00, MULT);
        // (6.05 - 6.00) * 3 * 25000 = 3750
        assert!((s.mtm_usd(6.05, MULT) - 3750.0).abs() < 1e-6);
    }

    #[test]
    fn mtm_short_position_profits_on_drop() {
        let mut s = HedgeState::new();
        s.apply_fill(false, 2, 6.00, MULT);
        // (5.95 - 6.00) * -2 * 25000 = 2500
        assert!((s.mtm_usd(5.95, MULT) - 2500.0).abs() < 1e-6);
    }

    #[test]
    fn replace_overrides_state() {
        let mut s = HedgeState::new();
        s.apply_fill(true, 5, 6.00, MULT);
        s.replace(-2, 6.10);
        assert_eq!(s.hedge_qty, -2);
        assert_eq!(s.avg_entry_f, 6.10);
        // realized untouched
    }
}
