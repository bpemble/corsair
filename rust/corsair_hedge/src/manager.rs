//! `HedgeManager` — per-product hedge orchestration.
//!
//! Responsibilities:
//! - Track hedge_qty + avg_entry_f via [`HedgeState`]
//! - Decide rebalance trades from options delta drift
//! - Apply broker exec confirmations to update state
//! - Compute hedge MTM for risk's daily P&L halt
//!
//! What's NOT here:
//! - Order placement: caller (the runtime) takes [`HedgeAction::Place`]
//!   and calls `Broker::place_order`.
//! - Contract resolution: caller resolves the hedge contract via
//!   `Broker::list_chain` and passes it to [`HedgeManager::new`].
//! - Lockout-skip logic: lives in the runtime's contract resolver,
//!   driven by [`HedgeConfig::lockout_days`].

use corsair_broker_api::Contract;
use corsair_position::PortfolioState;
use serde::{Deserialize, Serialize};

use crate::state::HedgeState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HedgeMode {
    /// Compute and log; do not place real orders. Used for shadow
    /// validation. Optimistic-on-placement state updates apply.
    Observe,
    /// Place real IOC limit orders against the hedge contract.
    /// State updates only on broker exec confirmation.
    Execute,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HedgeConfig {
    pub product: String,
    pub multiplier: f64,
    pub mode: HedgeMode,
    /// |effective_delta| <= tolerance → no trade.
    pub tolerance_deltas: f64,
    pub rebalance_on_fill: bool,
    /// Minimum interval between periodic rebalance attempts.
    pub rebalance_cadence_sec: f64,
    pub include_in_daily_pnl: bool,
    pub flatten_on_halt_enabled: bool,
    /// IOC limit aggression in ticks past F.
    pub ioc_tick_offset: i32,
    pub hedge_tick_size: f64,
    /// Phase 0 lockout: skip contracts within N days of expiry.
    /// 0 disables. The runtime's contract resolver enforces this.
    pub lockout_days: i32,
}

/// Action the [`HedgeManager`] decides on each rebalance call. The
/// runtime acts on these against the broker.
#[derive(Debug, Clone)]
pub enum HedgeAction {
    /// Within tolerance — no trade needed.
    Idle,
    /// Place an IOC limit order with the given side and quantity.
    /// Caller computes the final limit price from the live hedge
    /// contract's bid/ask (corsair_market_data exposes this).
    Place {
        /// `true` = BUY, `false` = SELL.
        is_buy: bool,
        qty: u32,
        /// Reason for the trade — propagated to the JSONL log.
        reason: String,
        /// Effective delta the hedge is correcting (for telemetry).
        effective_delta_pre: f64,
        /// Target hedge_qty after the trade fills (for telemetry).
        target_qty: i32,
    },
    /// Observe-mode log entry only. Caller shouldn't place a real
    /// order; in observe mode the manager updates state optimistically
    /// AS IF the trade filled at F.
    LogOnly {
        is_buy: bool,
        qty: u32,
        reason: String,
        forward: f64,
    },
}

pub struct HedgeManager {
    cfg: HedgeConfig,
    pub(crate) state: HedgeState,
    /// Resolved hedge contract from the runtime's contract resolver.
    /// Used for filtering broker fill events to "ours".
    hedge_contract: Option<Contract>,
    /// Monotonic timestamp of last periodic rebalance.
    last_periodic_ns: u64,
    /// Layer C burst-pull cooldown — when nonzero and now < this,
    /// rebalance_periodic bypasses the cadence gate.
    priority_drain_until_ns: u64,
    /// Dedupe set for execDetails — prevents double-counting.
    processed_exec_ids: std::collections::HashSet<String>,
    /// Bounded order for FIFO eviction.
    processed_exec_ids_order: std::collections::VecDeque<String>,
}

const PROCESSED_EXEC_IDS_MAX: usize = 10_000;

impl HedgeManager {
    pub fn new(cfg: HedgeConfig) -> Self {
        Self {
            cfg,
            state: HedgeState::new(),
            hedge_contract: None,
            last_periodic_ns: 0,
            priority_drain_until_ns: 0,
            processed_exec_ids: std::collections::HashSet::new(),
            processed_exec_ids_order: std::collections::VecDeque::new(),
        }
    }

    pub fn config(&self) -> &HedgeConfig {
        &self.cfg
    }

    pub fn state(&self) -> &HedgeState {
        &self.state
    }

    pub fn hedge_qty(&self) -> i32 {
        self.state.hedge_qty
    }

    pub fn mtm_usd(&self, current_f: f64) -> f64 {
        self.state.mtm_usd(current_f, self.cfg.multiplier)
    }

    /// Set the resolved hedge contract. Caller (runtime) qualifies
    /// the contract via `Broker::list_chain` with lockout-skip and
    /// passes it here.
    pub fn set_hedge_contract(&mut self, c: Contract) {
        log::info!(
            "HedgeManager [{}]: hedge contract set to {} (instrument_id={})",
            self.cfg.product,
            c.local_symbol,
            c.instrument_id
        );
        self.hedge_contract = Some(c);
    }

    pub fn hedge_contract(&self) -> Option<&Contract> {
        self.hedge_contract.as_ref()
    }

    /// On every option fill: trigger an immediate rebalance check.
    pub fn rebalance_on_fill(&mut self, portfolio: &PortfolioState) -> HedgeAction {
        if !self.cfg.rebalance_on_fill {
            return HedgeAction::Idle;
        }
        self.maybe_rebalance(portfolio, "fill", 6.0)
    }

    /// Periodic check from the main loop. Caller passes the current
    /// underlying price (forward) for observe-mode optimistic updates
    /// and any logs.
    pub fn rebalance_periodic(
        &mut self,
        portfolio: &PortfolioState,
        forward: f64,
        now_ns: u64,
    ) -> HedgeAction {
        let priority_active = self.priority_drain_until_ns > 0
            && now_ns < self.priority_drain_until_ns;
        if priority_active {
            self.last_periodic_ns = now_ns;
            return self.maybe_rebalance(portfolio, "priority_drain_periodic", forward);
        }
        if self.priority_drain_until_ns > 0 && now_ns >= self.priority_drain_until_ns {
            // Cooldown expired
            self.priority_drain_until_ns = 0;
            self.last_periodic_ns = now_ns;
            return self.maybe_rebalance(portfolio, "priority_drain_clear", forward);
        }
        let elapsed = (now_ns.saturating_sub(self.last_periodic_ns)) as f64 / 1e9;
        if elapsed < self.cfg.rebalance_cadence_sec {
            return HedgeAction::Idle;
        }
        self.last_periodic_ns = now_ns;
        self.maybe_rebalance(portfolio, "periodic", forward)
    }

    /// v1.4 §6.2 delta kill: bring net delta to exactly 0.
    pub fn force_flat(&mut self, portfolio: &PortfolioState, forward: f64) -> HedgeAction {
        // Bypass tolerance: aim for exactly 0.
        self.rebalance(portfolio, Some(0.0), "delta_kill_force_flat", forward)
    }

    /// Set the priority-drain deadline (used by Layer C burst-pull).
    pub fn request_priority_drain(&mut self, cooldown_until_mono_ns: u64) {
        let prev = self.priority_drain_until_ns;
        self.priority_drain_until_ns = prev.max(cooldown_until_mono_ns);
    }

    /// On daily-P&L halt or margin kill: close out the hedge.
    pub fn flatten_on_halt(&mut self, forward: f64) -> HedgeAction {
        if !self.cfg.flatten_on_halt_enabled {
            return HedgeAction::Idle;
        }
        if self.state.hedge_qty == 0 {
            return HedgeAction::Idle;
        }
        let close_qty = self.state.hedge_qty.unsigned_abs();
        let is_buy = self.state.hedge_qty < 0;
        match self.cfg.mode {
            HedgeMode::Observe => HedgeAction::LogOnly {
                is_buy,
                qty: close_qty,
                reason: "halt_flatten".into(),
                forward,
            },
            HedgeMode::Execute => HedgeAction::Place {
                is_buy,
                qty: close_qty,
                reason: "halt_flatten".into(),
                effective_delta_pre: self.state.hedge_qty as f64,
                target_qty: 0,
            },
        }
    }

    /// Internal: tolerance-gated rebalance.
    fn maybe_rebalance(
        &mut self,
        portfolio: &PortfolioState,
        reason: &str,
        forward: f64,
    ) -> HedgeAction {
        let net_delta = portfolio.delta_for_product(&self.cfg.product);
        let effective = net_delta + (self.state.hedge_qty as f64);
        if effective.abs() <= self.cfg.tolerance_deltas {
            return HedgeAction::Idle;
        }
        self.rebalance(portfolio, None, reason, forward)
    }

    /// Compute and dispatch the hedge trade. tolerance_override=Some(0.0)
    /// means bypass tolerance (for force_flat).
    fn rebalance(
        &mut self,
        portfolio: &PortfolioState,
        tolerance_override: Option<f64>,
        reason: &str,
        forward: f64,
    ) -> HedgeAction {
        let net_delta = portfolio.delta_for_product(&self.cfg.product);
        let effective = net_delta + (self.state.hedge_qty as f64);
        // Target hedge_qty: -round(net_delta) so options + hedge ≈ 0.
        // Equivalent: target = -round(effective - hedge_qty) so
        // applying delta = (target - hedge_qty) brings effective to 0.
        let target_qty = -(effective - (self.state.hedge_qty as f64)).round() as i32;
        let desired_change = target_qty - self.state.hedge_qty;
        if tolerance_override.is_none() && desired_change.abs() < 1 {
            return HedgeAction::Idle;
        }
        if desired_change == 0 {
            return HedgeAction::Idle;
        }
        let is_buy = desired_change > 0;
        let qty = desired_change.unsigned_abs();
        match self.cfg.mode {
            HedgeMode::Observe => {
                // Optimistic update for observe mode (Python parity).
                self.state.apply_fill(is_buy, qty, forward, self.cfg.multiplier);
                HedgeAction::LogOnly {
                    is_buy,
                    qty,
                    reason: reason.into(),
                    forward,
                }
            }
            HedgeMode::Execute => HedgeAction::Place {
                is_buy,
                qty,
                reason: reason.into(),
                effective_delta_pre: effective,
                target_qty,
            },
        }
    }

    /// Apply a fill confirmation from the broker's exec stream. Filters
    /// to the resolved hedge contract by instrument_id; deduplicates by
    /// exec_id; ignores in observe mode (the optimistic path already
    /// applied at placement). Returns true if state was updated.
    pub fn apply_broker_fill(&mut self, fill: &corsair_broker_api::events::Fill) -> bool {
        if self.cfg.mode != HedgeMode::Execute {
            return false;
        }
        let target = match self.hedge_contract.as_ref() {
            Some(c) => c.instrument_id,
            None => return false,
        };
        if fill.instrument_id != target {
            return false;
        }
        if fill.exec_id.is_empty() || self.processed_exec_ids.contains(&fill.exec_id) {
            return false;
        }
        if self.processed_exec_ids_order.len() >= PROCESSED_EXEC_IDS_MAX {
            if let Some(old) = self.processed_exec_ids_order.pop_front() {
                self.processed_exec_ids.remove(&old);
            }
        }
        self.processed_exec_ids.insert(fill.exec_id.clone());
        self.processed_exec_ids_order.push_back(fill.exec_id.clone());
        let pre = self.state.hedge_qty;
        let is_buy = matches!(fill.side, corsair_broker_api::Side::Buy);
        self.state
            .apply_fill(is_buy, fill.qty, fill.price, self.cfg.multiplier);
        log::warn!(
            "HEDGE fill confirmed [{}]: {} {} @ {} — hedge_qty {:+} → {:+} (execId={})",
            self.cfg.product,
            if is_buy { "BUY" } else { "SELL" },
            fill.qty,
            fill.price,
            pre,
            self.state.hedge_qty,
            fill.exec_id
        );
        true
    }

    /// Reconcile our local hedge_qty against broker positions. Caller
    /// (runtime) calls Broker::positions and filters for the hedge
    /// contract; we compare and take their numbers as authoritative.
    /// Returns true on actual divergence (state was changed).
    pub fn reconcile_with_position(
        &mut self,
        broker_qty: i32,
        broker_avg_cost: f64,
        silent: bool,
    ) -> bool {
        // Bump freshness even on no-op (already-matched) reconciles.
        // Audit T1-1: without this, the freshness gate
        // (HedgeState::is_fresh) silently disengages after 300s in
        // steady state, and CLAUDE.md §14's effective-delta gate
        // falls back to options-only without operator-visible signal.
        self.state.touch_freshness();
        if broker_qty == self.state.hedge_qty {
            return false;
        }
        let prior = self.state.hedge_qty;
        self.state.replace(broker_qty, broker_avg_cost);
        if !silent {
            log::warn!(
                "HEDGE reconcile [{}]: divergence — local={:+} → broker={:+} (diff={:+})",
                self.cfg.product,
                prior,
                broker_qty,
                broker_qty - prior
            );
        }
        true
    }

    /// Mark this manager's hedge state as freshly observed even when
    /// no reconcile/fill happened. Used by the periodic hedge-tick
    /// loop to keep the freshness gate alive on FLAT legs (which
    /// otherwise never visit `reconcile_with_position`).
    pub fn touch_freshness(&mut self) {
        self.state.touch_freshness();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use corsair_broker_api::{
        events::Fill, ContractKind, Currency, Exchange, InstrumentId, Right, Side,
    };
    use corsair_position::{Position, ProductInfo, ProductRegistry};

    fn cfg(mode: HedgeMode) -> HedgeConfig {
        HedgeConfig {
            product: "HG".into(),
            multiplier: 25_000.0,
            mode,
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

    fn hedge_contract() -> Contract {
        Contract {
            instrument_id: InstrumentId(987654),
            kind: ContractKind::Future,
            symbol: "HG".into(),
            local_symbol: "HGM6".into(),
            expiry: NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
            strike: None,
            right: None,
            multiplier: 25_000.0,
            exchange: Exchange::Comex,
            currency: Currency::Usd,
            trading_class: "HG".into(),
        }
    }

    fn portfolio_with_delta(d: f64) -> PortfolioState {
        let mut r = ProductRegistry::new();
        r.register(ProductInfo {
            product: "HG".into(),
            multiplier: 25_000.0,
            default_iv: 0.30,
        });
        let mut p = PortfolioState::new(r);
        // Replace positions with a single position whose delta * qty = d.
        let pos = Position {
            product: "HG".into(),
            strike: 6.05,
            expiry: NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
            right: Right::Call,
            quantity: 1,
            avg_fill_price: 0.025,
            fill_time: chrono::Utc::now(),
            multiplier: 25_000.0,
            delta: d,
            gamma: 0.0,
            theta: 0.0,
            vega: 0.0,
            current_price: 0.025,
        };
        p.replace_positions(vec![pos]);
        p
    }

    // ── Idle within tolerance ─────────────────────────────────

    #[test]
    fn within_tolerance_idle() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Execute));
        let p = portfolio_with_delta(0.3); // < tolerance 0.5
        let action = h.rebalance_on_fill(&p);
        matches!(action, HedgeAction::Idle);
    }

    // ── Trade outside tolerance ───────────────────────────────

    #[test]
    fn outside_tolerance_places_order_in_execute() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Execute));
        let p = portfolio_with_delta(2.7);
        let action = h.rebalance_on_fill(&p);
        match action {
            HedgeAction::Place { is_buy, qty, target_qty, .. } => {
                assert!(!is_buy, "should sell to reduce positive delta");
                assert_eq!(qty, 3); // round(2.7) = 3
                assert_eq!(target_qty, -3);
            }
            other => panic!("expected Place, got {other:?}"),
        }
    }

    #[test]
    fn observe_mode_logs_and_updates_optimistically() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Observe));
        let p = portfolio_with_delta(2.7);
        let action = h.rebalance_on_fill(&p);
        match action {
            HedgeAction::LogOnly { is_buy, qty, .. } => {
                assert!(!is_buy);
                assert_eq!(qty, 3);
            }
            other => panic!("expected LogOnly, got {other:?}"),
        }
        // Optimistic update applied
        assert_eq!(h.hedge_qty(), -3);
    }

    // ── Force flat ────────────────────────────────────────────

    #[test]
    fn force_flat_targets_zero() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Execute));
        // Pretend we're already long 4 hedge contracts.
        h.state.replace(4, 6.00);
        let p = portfolio_with_delta(0.0);
        let action = h.force_flat(&p, 6.05);
        match action {
            HedgeAction::Place { is_buy, qty, target_qty, .. } => {
                assert!(!is_buy); // sell to flatten long
                assert_eq!(qty, 4);
                assert_eq!(target_qty, 0);
            }
            other => panic!("expected Place, got {other:?}"),
        }
    }

    // ── Flatten on halt ───────────────────────────────────────

    #[test]
    fn flatten_on_halt_when_long() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Execute));
        h.state.replace(3, 6.00);
        let action = h.flatten_on_halt(6.05);
        match action {
            HedgeAction::Place { is_buy, qty, .. } => {
                assert!(!is_buy);
                assert_eq!(qty, 3);
            }
            other => panic!("expected Place, got {other:?}"),
        }
    }

    #[test]
    fn flatten_on_halt_idle_when_flat() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Execute));
        let action = h.flatten_on_halt(6.05);
        matches!(action, HedgeAction::Idle);
    }

    #[test]
    fn flatten_on_halt_disabled_when_off() {
        let mut cf = cfg(HedgeMode::Execute);
        cf.flatten_on_halt_enabled = false;
        let mut h = HedgeManager::new(cf);
        h.state.replace(3, 6.00);
        let action = h.flatten_on_halt(6.05);
        matches!(action, HedgeAction::Idle);
    }

    // ── Broker fill confirmation ──────────────────────────────

    #[test]
    fn apply_broker_fill_updates_state() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Execute));
        h.set_hedge_contract(hedge_contract());
        let f = Fill {
            exec_id: "exec-1".into(),
            order_id: corsair_broker_api::OrderId(1),
            instrument_id: InstrumentId(987654),
            side: Side::Buy,
            qty: 3,
            price: 6.05,
            timestamp_ns: 0,
            commission: None,
        };
        let updated = h.apply_broker_fill(&f);
        assert!(updated);
        assert_eq!(h.hedge_qty(), 3);
    }

    #[test]
    fn apply_broker_fill_dedup() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Execute));
        h.set_hedge_contract(hedge_contract());
        let f = Fill {
            exec_id: "exec-dup".into(),
            order_id: corsair_broker_api::OrderId(1),
            instrument_id: InstrumentId(987654),
            side: Side::Buy,
            qty: 1,
            price: 6.05,
            timestamp_ns: 0,
            commission: None,
        };
        assert!(h.apply_broker_fill(&f));
        assert!(!h.apply_broker_fill(&f), "second apply should dedup");
    }

    #[test]
    fn apply_broker_fill_filters_wrong_instrument() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Execute));
        h.set_hedge_contract(hedge_contract());
        let f = Fill {
            exec_id: "exec-other".into(),
            order_id: corsair_broker_api::OrderId(1),
            instrument_id: InstrumentId(999), // not our contract
            side: Side::Buy,
            qty: 1,
            price: 6.05,
            timestamp_ns: 0,
            commission: None,
        };
        assert!(!h.apply_broker_fill(&f));
        assert_eq!(h.hedge_qty(), 0);
    }

    #[test]
    fn observe_mode_ignores_broker_fills() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Observe));
        h.set_hedge_contract(hedge_contract());
        let f = Fill {
            exec_id: "exec-obs".into(),
            order_id: corsair_broker_api::OrderId(1),
            instrument_id: InstrumentId(987654),
            side: Side::Buy,
            qty: 1,
            price: 6.05,
            timestamp_ns: 0,
            commission: None,
        };
        assert!(!h.apply_broker_fill(&f));
    }

    // ── Reconcile with broker position ────────────────────────

    #[test]
    fn reconcile_no_op_when_matching() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Execute));
        h.state.replace(3, 6.00);
        assert!(!h.reconcile_with_position(3, 6.00, true));
    }

    #[test]
    fn reconcile_replaces_on_divergence() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Execute));
        h.state.replace(3, 6.00);
        assert!(h.reconcile_with_position(-2, 6.10, true));
        assert_eq!(h.hedge_qty(), -2);
    }

    // ── MTM ─────────────────────────────────────────────────

    #[test]
    fn mtm_via_manager_uses_multiplier() {
        let mut h = HedgeManager::new(cfg(HedgeMode::Execute));
        h.state.replace(3, 6.00);
        // (6.05-6.00) * 3 * 25000 = 3750
        assert!((h.mtm_usd(6.05) - 3750.0).abs() < 1e-6);
    }
}
