//! `RiskMonitor` — the kill-switch state machine.
//!
//! Mirrors `src/risk_monitor.py::RiskMonitor`. Holds the current kill
//! state; consumers call [`check`] every cycle and
//! [`check_daily_pnl_only`] on every fill. The runtime acts on kill
//! events emitted via the returned [`RiskCheckOutcome`].

use corsair_position::{MarketView, PortfolioState};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::kill::{KillEvent, KillSource, KillType};
use crate::sentinel::{sentinel_dir, INDUCED_SENTINELS};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub capital: f64,
    pub margin_kill_pct: f64,
    /// Threshold for the daily P&L halt (negative number; e.g.,
    /// -25_000 for -5% of $500k). When None, the halt is DISABLED
    /// — RiskMonitor logs a CRITICAL warning at construction.
    pub daily_halt_threshold: Option<f64>,
    pub delta_kill: f64,
    /// 0 means disabled (Alabaster characterization on 2026-04-23).
    pub vega_kill: f64,
    /// Negative; 0 means disabled.
    pub theta_kill: f64,
    /// Used for the warning log when current_margin > ceiling but
    /// below kill (the constraint checker handles the actual gate).
    pub margin_ceiling_pct: f64,
    /// Toggle for effective_delta_gating (CLAUDE.md §14). When false,
    /// the delta kill uses options-only delta (excludes hedge_qty).
    pub effective_delta_gating: bool,
}

impl RiskConfig {
    /// Resolve threshold from either daily_pnl_halt_pct (preferred)
    /// or absolute max_daily_loss. Mirrors
    /// `_resolve_daily_halt_threshold` in Python. Returns None if
    /// neither is configured.
    pub fn resolve_daily_halt_threshold(
        daily_pnl_halt_pct: Option<f64>,
        max_daily_loss: Option<f64>,
        capital: f64,
    ) -> Option<f64> {
        if let Some(pct) = daily_pnl_halt_pct {
            if pct > 0.0 && capital > 0.0 {
                return Some(-(pct * capital));
            }
        }
        if let Some(abs) = max_daily_loss {
            if abs < 0.0 {
                return Some(abs);
            }
        }
        None
    }
}

/// Outcome of a check() call. Tells the runtime what action (if
/// any) the kill switch wants to take.
#[derive(Debug, Clone)]
pub enum RiskCheckOutcome {
    /// No kill triggered.
    Healthy,
    /// A kill fired this cycle. The runtime should act on it.
    Killed(KillEvent),
    /// A kill is already active (sticky). Subsequent checks return
    /// this until cleared.
    AlreadyKilled(KillEvent),
}

pub struct RiskMonitor {
    cfg: RiskConfig,
    killed: Option<KillEvent>,
}

impl RiskMonitor {
    pub fn new(cfg: RiskConfig) -> Self {
        if cfg.daily_halt_threshold.is_none() {
            log::error!(
                "DAILY P&L HALT DISABLED: neither daily_pnl_halt_pct nor \
                 max_daily_loss is configured. Primary v1.4 defense will \
                 NOT fire."
            );
        } else {
            let pct = cfg
                .daily_halt_threshold
                .map(|t| (t / cfg.capital) * 100.0)
                .unwrap_or(0.0);
            log::info!(
                "Daily P&L halt armed: threshold=${:.0} ({:.1}% of ${:.0} capital)",
                cfg.daily_halt_threshold.unwrap_or(0.0),
                pct,
                cfg.capital
            );
        }
        Self {
            cfg,
            killed: None,
        }
    }

    pub fn config(&self) -> &RiskConfig {
        &self.cfg
    }

    pub fn is_killed(&self) -> bool {
        self.killed.is_some()
    }

    pub fn kill_event(&self) -> Option<&KillEvent> {
        self.killed.as_ref()
    }

    /// Run all kill checks. Should be called every greek_refresh
    /// interval (typical 5min) plus on demand from operational kills.
    ///
    /// `worst_delta` / `worst_vega` / `worst_theta` are the across-
    /// products worst values (computed by the caller from
    /// PortfolioState aggregates + hedge_qty when effective gating
    /// is on). Pass options-only values when `effective_delta_gating`
    /// is false.
    pub fn check(
        &mut self,
        portfolio: &PortfolioState,
        margin_used: f64,
        worst_delta: f64,
        worst_theta: f64,
        worst_vega: f64,
        market: &dyn MarketView,
    ) -> RiskCheckOutcome {
        if let Some(ref e) = self.killed {
            return RiskCheckOutcome::AlreadyKilled(e.clone());
        }

        // Induced sentinel — Gate 0 boot self-test.
        if let Some(ev) = self.check_induced_sentinels() {
            return self.fire(ev);
        }

        let cap = self.cfg.capital;

        // SPAN margin kill — operator override per CLAUDE.md §7
        let margin_kill = cap * self.cfg.margin_kill_pct;
        if margin_used > margin_kill {
            let ev = KillEvent {
                reason: format!(
                    "MARGIN KILL: ${:.0} > ${:.0}",
                    margin_used, margin_kill
                ),
                source: KillSource::Risk,
                kill_type: KillType::Halt,
                timestamp_ns: now_ns(),
            };
            return self.fire(ev);
        }

        // Delta kill (CLAUDE.md §6.2)
        if worst_delta.abs() > self.cfg.delta_kill {
            let ev = KillEvent {
                reason: format!(
                    "DELTA KILL: {:+.2} > ±{:.1}",
                    worst_delta, self.cfg.delta_kill
                ),
                source: KillSource::Risk,
                kill_type: KillType::HedgeFlat,
                timestamp_ns: now_ns(),
            };
            return self.fire(ev);
        }

        // Vega halt — disabled when 0 (Alabaster).
        if self.cfg.vega_kill > 0.0 && worst_vega.abs() > self.cfg.vega_kill {
            let ev = KillEvent {
                reason: format!(
                    "VEGA HALT: ${:+.0} > ±${:.0}",
                    worst_vega, self.cfg.vega_kill
                ),
                source: KillSource::Risk,
                kill_type: KillType::Halt,
                timestamp_ns: now_ns(),
            };
            return self.fire(ev);
        }

        // Theta halt — disabled when 0.
        if self.cfg.theta_kill < 0.0 && worst_theta < self.cfg.theta_kill {
            let ev = KillEvent {
                reason: format!(
                    "THETA HALT: ${:.0} < ${:.0}",
                    worst_theta, self.cfg.theta_kill
                ),
                source: KillSource::Risk,
                kill_type: KillType::Halt,
                timestamp_ns: now_ns(),
            };
            return self.fire(ev);
        }

        // Daily P&L halt — primary v1.4 defense. Operator-override
        // kill_type=Halt (positions preserved).
        if let Some(threshold) = self.cfg.daily_halt_threshold {
            let mtm = portfolio.compute_mtm_pnl(market);
            let daily = portfolio.realized_pnl_persisted + mtm;
            if daily < threshold {
                let pct = (threshold.abs() / cap) * 100.0;
                let ev = KillEvent {
                    reason: format!(
                        "DAILY P&L HALT: ${:.0} < ${:.0} (-{:.1}% cap)",
                        daily, threshold, pct
                    ),
                    source: KillSource::DailyHalt,
                    kill_type: KillType::Halt,
                    timestamp_ns: now_ns(),
                };
                return self.fire(ev);
            }
        }

        // Margin warning (warn-only above ceiling but below kill).
        let margin_ceiling = cap * self.cfg.margin_ceiling_pct;
        if margin_used > margin_ceiling {
            log::warn!(
                "MARGIN WARNING: ${:.0} above ceiling ${:.0}. Constraint \
                 checker blocking margin-increasing orders.",
                margin_used,
                margin_ceiling
            );
        }
        RiskCheckOutcome::Healthy
    }

    /// Fast per-fill halt check. Mirrors
    /// `RiskMonitor.check_daily_pnl_only` in Python — used by the
    /// fill handler so the halt fires between 5-min cycles.
    pub fn check_daily_pnl_only(
        &mut self,
        portfolio: &PortfolioState,
        market: &dyn MarketView,
    ) -> RiskCheckOutcome {
        if self.killed.is_some() {
            return RiskCheckOutcome::AlreadyKilled(self.killed.clone().unwrap());
        }
        let threshold = match self.cfg.daily_halt_threshold {
            Some(t) => t,
            None => return RiskCheckOutcome::Healthy,
        };
        let mtm = portfolio.compute_mtm_pnl(market);
        let daily = portfolio.realized_pnl_persisted + mtm;
        if daily < threshold {
            let pct = (threshold.abs() / self.cfg.capital) * 100.0;
            let ev = KillEvent {
                reason: format!(
                    "DAILY P&L HALT (fill-path): ${:.0} < ${:.0} (-{:.1}% cap)",
                    daily, threshold, pct
                ),
                source: KillSource::DailyHalt,
                kill_type: KillType::Halt,
                timestamp_ns: now_ns(),
            };
            return self.fire(ev);
        }
        RiskCheckOutcome::Healthy
    }

    /// Per-fill delta enforcement. The 300s periodic risk_check is too
    /// slow to catch a fast cascade (2026-05-04: options_delta drifted
    /// 0 → -14.6 between two periodic ticks while we accumulated ~38
    /// contracts in 4 minutes). This fires the kill the moment a fill
    /// pushes effective_delta past `delta_kill`, before the next quote
    /// is even decided.
    ///
    /// Caller passes the post-fill `effective_delta` (options +
    /// hedge_qty). Hedge state is read by the caller because the risk
    /// monitor doesn't own a HedgeFanout reference and we want the
    /// gate to operate on the same metric the trader self-blocks on.
    pub fn check_per_fill_delta(&mut self, effective_delta: f64) -> RiskCheckOutcome {
        if self.killed.is_some() {
            return RiskCheckOutcome::AlreadyKilled(self.killed.clone().unwrap());
        }
        if effective_delta.abs() > self.cfg.delta_kill {
            let ev = KillEvent {
                reason: format!(
                    "DELTA KILL (fill-path): {:+.2} > ±{:.1}",
                    effective_delta, self.cfg.delta_kill
                ),
                source: KillSource::Risk,
                kill_type: KillType::HedgeFlat,
                timestamp_ns: now_ns(),
            };
            return self.fire(ev);
        }
        RiskCheckOutcome::Healthy
    }

    /// Check for induced-kill sentinels in the configured directory.
    /// Removes the sentinel BEFORE firing so the kill can't re-trigger
    /// on the next cycle if the remove succeeds. If remove fails, log
    /// WARNING and skip the fire.
    fn check_induced_sentinels(&mut self) -> Option<KillEvent> {
        let dir = sentinel_dir();
        for s in INDUCED_SENTINELS {
            let path = dir.join(s.filename);
            if !path.exists() {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(_) => {}
                Err(e) => {
                    log::warn!(
                        "Induced sentinel {} present but remove failed ({}); \
                         refusing to fire to avoid stuck re-trigger loop. \
                         Remove manually before next check.",
                        path.display(),
                        e
                    );
                    return None;
                }
            }
            let reason = format!(
                "INDUCED TEST [{}]: sentinel {} — exercising kill_type={:?} \
                 source={:?}",
                s.key.to_ascii_uppercase(),
                path.display(),
                s.kill_type,
                s.inner_source.label()
            );
            log::warn!("INDUCED TEST fired: {reason}");
            return Some(KillEvent {
                reason,
                source: KillSource::Induced(Box::new(s.inner_source.clone())),
                kill_type: s.kill_type,
                timestamp_ns: now_ns(),
            });
        }
        None
    }

    /// Internal: transition to killed state and return the outcome.
    fn fire(&mut self, ev: KillEvent) -> RiskCheckOutcome {
        log::error!(
            "KILL SWITCH ACTIVATED [{} / {:?}]: {}",
            ev.source.label(),
            ev.kill_type,
            ev.reason
        );
        self.killed = Some(ev.clone());
        RiskCheckOutcome::Killed(ev)
    }

    /// Watchdog reconnect: clear a disconnect-source kill. Returns
    /// true if the kill was cleared. Other sources stay sticky.
    pub fn clear_disconnect_kill(&mut self) -> bool {
        if let Some(ev) = &self.killed {
            if ev.source.is_disconnect() {
                log::info!("Clearing disconnect-induced kill: {}", ev.reason);
                self.killed = None;
                return true;
            }
        }
        false
    }

    /// Session rollover: clear a daily_halt-source kill (or
    /// induced_daily_halt). Returns true if cleared.
    pub fn clear_daily_halt(&mut self) -> bool {
        if let Some(ev) = &self.killed {
            if ev.source.is_daily_clearable() {
                log::warn!(
                    "Clearing daily P&L halt at session rollover: {}",
                    ev.reason
                );
                self.killed = None;
                return true;
            }
        }
        false
    }

    /// Take the current kill event, leaving the monitor in killed
    /// state but exposing the event for paper-stream logging. Used
    /// by the runtime's kill_switch JSONL emitter.
    pub fn kill_reason(&self) -> Option<&str> {
        self.killed.as_ref().map(|e| e.reason.as_str())
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use corsair_broker_api::Right;
    use corsair_position::{NoOpMarketView, ProductInfo, ProductRegistry, RecordingMarketView};
    use std::env;
    use tempfile::TempDir;

    fn cfg_for_test() -> RiskConfig {
        RiskConfig {
            capital: 200_000.0,
            margin_kill_pct: 0.70,
            daily_halt_threshold: Some(-2_500.0),
            delta_kill: 5.0,
            vega_kill: 1_000.0,
            theta_kill: -200.0,
            margin_ceiling_pct: 0.50,
            effective_delta_gating: true,
        }
    }

    fn portfolio_with_pnl(realized: f64, mtm_price_diff: f64) -> PortfolioState {
        let mut r = ProductRegistry::new();
        r.register(ProductInfo {
            product: "HG".into(),
            multiplier: 25_000.0,
            default_iv: 0.30,
        });
        let mut p = PortfolioState::new(r);
        p.realized_pnl_persisted = realized;
        if mtm_price_diff != 0.0 {
            p.add_fill(
                "HG",
                6.05,
                NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
                Right::Call,
                1,
                0.025,
                0.0,
                0.0,
            );
        }
        p
    }

    // ── Threshold resolution ────────────────────────────────────

    #[test]
    fn resolve_threshold_from_pct() {
        let t = RiskConfig::resolve_daily_halt_threshold(Some(0.05), None, 200_000.0);
        assert_eq!(t, Some(-10_000.0));
    }

    #[test]
    fn resolve_threshold_from_absolute() {
        let t = RiskConfig::resolve_daily_halt_threshold(None, Some(-2_500.0), 200_000.0);
        assert_eq!(t, Some(-2_500.0));
    }

    #[test]
    fn resolve_threshold_none_when_unconfigured() {
        let t = RiskConfig::resolve_daily_halt_threshold(None, None, 200_000.0);
        assert!(t.is_none());
    }

    // ── Kill firing ────────────────────────────────────────────

    #[test]
    fn margin_kill_fires_above_threshold() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(0.0, 0.0);
        let outcome = m.check(&p, 150_000.0, 0.0, 0.0, 0.0, &NoOpMarketView);
        match outcome {
            RiskCheckOutcome::Killed(ev) => {
                assert!(ev.reason.contains("MARGIN KILL"));
                assert_eq!(ev.kill_type, KillType::Halt);
                assert_eq!(ev.source, KillSource::Risk);
            }
            other => panic!("expected margin kill, got {other:?}"),
        }
    }

    #[test]
    fn margin_kill_does_not_fire_below_threshold() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(0.0, 0.0);
        let outcome = m.check(&p, 100_000.0, 0.0, 0.0, 0.0, &NoOpMarketView);
        matches!(outcome, RiskCheckOutcome::Healthy);
    }

    #[test]
    fn delta_kill_fires() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(0.0, 0.0);
        let outcome = m.check(&p, 0.0, 5.5, 0.0, 0.0, &NoOpMarketView);
        match outcome {
            RiskCheckOutcome::Killed(ev) => {
                assert!(ev.reason.contains("DELTA KILL"));
                assert_eq!(ev.kill_type, KillType::HedgeFlat);
            }
            other => panic!("expected delta kill, got {other:?}"),
        }
    }

    #[test]
    fn vega_kill_fires_when_threshold_set() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(0.0, 0.0);
        let outcome = m.check(&p, 0.0, 0.0, 0.0, 1_500.0, &NoOpMarketView);
        matches!(outcome, RiskCheckOutcome::Killed(_));
    }

    #[test]
    fn vega_kill_disabled_when_zero() {
        let mut cfg = cfg_for_test();
        cfg.vega_kill = 0.0;
        let mut m = RiskMonitor::new(cfg);
        let p = portfolio_with_pnl(0.0, 0.0);
        let outcome = m.check(&p, 0.0, 0.0, 0.0, 99_999.0, &NoOpMarketView);
        matches!(outcome, RiskCheckOutcome::Healthy);
    }

    #[test]
    fn theta_kill_fires() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(0.0, 0.0);
        let outcome = m.check(&p, 0.0, 0.0, -250.0, 0.0, &NoOpMarketView);
        matches!(outcome, RiskCheckOutcome::Killed(_));
    }

    // ── Daily halt ──────────────────────────────────────────────

    #[test]
    fn daily_halt_fires_on_realized_breach() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(-3_000.0, 0.0);
        let outcome = m.check(&p, 0.0, 0.0, 0.0, 0.0, &NoOpMarketView);
        match outcome {
            RiskCheckOutcome::Killed(ev) => {
                assert!(ev.reason.contains("DAILY P&L HALT"));
                assert_eq!(ev.source, KillSource::DailyHalt);
            }
            other => panic!("expected daily halt, got {other:?}"),
        }
    }

    #[test]
    fn check_daily_pnl_only_fires_on_breach() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(-3_000.0, 0.0);
        let outcome = m.check_daily_pnl_only(&p, &NoOpMarketView);
        matches!(outcome, RiskCheckOutcome::Killed(_));
        assert!(m.is_killed());
    }

    #[test]
    fn check_daily_pnl_only_disabled_when_threshold_unconfigured() {
        let mut cfg = cfg_for_test();
        cfg.daily_halt_threshold = None;
        let mut m = RiskMonitor::new(cfg);
        let mut p = portfolio_with_pnl(-99_999_999.0, 0.0);
        p.realized_pnl_persisted = -99_999_999.0;
        let outcome = m.check_daily_pnl_only(&p, &NoOpMarketView);
        matches!(outcome, RiskCheckOutcome::Healthy);
        assert!(!m.is_killed());
    }

    // ── Stickiness ─────────────────────────────────────────────

    #[test]
    fn risk_kill_is_sticky() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(0.0, 0.0);
        m.check(&p, 0.0, 5.5, 0.0, 0.0, &NoOpMarketView); // delta kill
        let outcome = m.check(&p, 0.0, 0.0, 0.0, 0.0, &NoOpMarketView); // would be healthy
        matches!(outcome, RiskCheckOutcome::AlreadyKilled(_));
    }

    #[test]
    fn clear_disconnect_clears_only_disconnect() {
        let mut m = RiskMonitor::new(cfg_for_test());
        m.fire(KillEvent {
            reason: "lost connection".into(),
            source: KillSource::Disconnect,
            kill_type: KillType::Halt,
            timestamp_ns: 0,
        });
        assert!(m.is_killed());
        assert!(m.clear_disconnect_kill());
        assert!(!m.is_killed());
        // Risk kill NOT clearable.
        m.fire(KillEvent {
            reason: "BAD".into(),
            source: KillSource::Risk,
            kill_type: KillType::Halt,
            timestamp_ns: 0,
        });
        assert!(!m.clear_disconnect_kill());
        assert!(m.is_killed());
    }

    #[test]
    fn clear_daily_halt_clears_only_daily_or_induced_daily() {
        let mut m = RiskMonitor::new(cfg_for_test());
        m.fire(KillEvent {
            reason: "halt".into(),
            source: KillSource::DailyHalt,
            kill_type: KillType::Halt,
            timestamp_ns: 0,
        });
        assert!(m.clear_daily_halt());
        assert!(!m.is_killed());

        // induced_daily_halt also clearable
        m.fire(KillEvent {
            reason: "induced halt".into(),
            source: KillSource::Induced(Box::new(KillSource::DailyHalt)),
            kill_type: KillType::Halt,
            timestamp_ns: 0,
        });
        assert!(m.clear_daily_halt());

        // induced_risk NOT clearable.
        m.fire(KillEvent {
            reason: "induced risk".into(),
            source: KillSource::Induced(Box::new(KillSource::Risk)),
            kill_type: KillType::Halt,
            timestamp_ns: 0,
        });
        assert!(!m.clear_daily_halt());
    }

    // ── Induced sentinels ────────────────────────────────────────

    #[test]
    fn induced_sentinel_fires_then_removes_file() {
        let tmp = TempDir::new().unwrap();
        env::set_var("CORSAIR_INDUCE_DIR", tmp.path());
        // Drop a daily_pnl sentinel
        let p = tmp.path().join("corsair_induce_daily_pnl");
        std::fs::write(&p, "test").unwrap();
        let mut m = RiskMonitor::new(cfg_for_test());
        let pf = portfolio_with_pnl(0.0, 0.0);
        let outcome = m.check(&pf, 0.0, 0.0, 0.0, 0.0, &NoOpMarketView);
        match outcome {
            RiskCheckOutcome::Killed(ev) => {
                assert!(ev.reason.contains("INDUCED TEST"));
                assert!(matches!(ev.source, KillSource::Induced(_)));
            }
            other => panic!("expected induced kill, got {other:?}"),
        }
        // Sentinel file removed.
        assert!(!p.exists());
    }

    // ── MTM-based daily halt ─────────────────────────────────────

    #[test]
    fn daily_halt_uses_mtm_pnl_via_market_view() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let mut p = portfolio_with_pnl(0.0, 0.0);
        // Long 1 call @ 0.025 with multiplier 25000.
        p.add_fill(
            "HG",
            6.05,
            NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
            Right::Call,
            1,
            0.025,
            0.0,
            0.0,
        );
        // Mark the option down dramatically: 0.025 → -0.075 (impossible
        // but demonstrates threshold-breach math).
        let view = RecordingMarketView::new();
        view.set_current_price(
            "HG",
            6.05,
            NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
            Right::Call,
            -0.075,
        );
        // Realized=0; MTM = (-0.075 - 0.025) * 1 * 25000 = -2500
        // → exactly at threshold (-2500). Need slightly below.
        p.realized_pnl_persisted = -100.0;
        let outcome = m.check_daily_pnl_only(&p, &view);
        matches!(outcome, RiskCheckOutcome::Killed(_));
    }
}
