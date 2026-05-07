//! `RiskMonitor` — the kill-switch state machine.
//!
//! Mirrors `src/risk_monitor.py::RiskMonitor`. Holds the current kill
//! state; consumers call [`check`] every cycle and
//! [`check_daily_pnl_only`] on every fill. The runtime acts on kill
//! events emitted via the returned [`RiskCheckOutcome`].

use corsair_hedge::{HedgeFanout, HedgeMode};
use corsair_position::{MarketView, PortfolioState};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::kill::{KillEvent, KillSource, KillType};
use crate::sentinel::{sentinel_dir, INDUCED_SENTINELS};

/// Sum hedge MTM + realized across managers using the hedge contract's
/// own mark when available, falling back to the options underlying.
/// Mirrors `corsair_snapshot::publisher::resolve_hedge_mark` so the
/// kill switch sees the same number the dashboard does.
///
/// This exists in `corsair_risk` because both the snapshot and the
/// risk monitor need the same hedge-aware P&L formula; duplicating
/// avoids a circular crate dep (snapshot depends on risk).
fn hedge_pnl(hedge: &HedgeFanout, market: &dyn MarketView) -> (f64, f64) {
    let mut mtm = 0.0;
    let mut realized = 0.0;
    for m in hedge.managers() {
        let cfg = m.config();
        // Audit T3-17: skip Observe-mode managers in the daily-halt
        // hedge_pnl sum. Observe-mode optimistically updates
        // hedge_qty + realized_pnl_usd as if trades filled at F, but
        // no real order was sent — folding that synthetic P&L into
        // the daily halt threshold means a "successful" observe-mode
        // shadow could fire (or suppress) the kill on a number that
        // doesn't reflect the real book. Execute-mode managers are
        // included; their state is fed by `apply_broker_fill` from
        // the broker's exec stream.
        if cfg.mode == HedgeMode::Observe {
            continue;
        }
        let f = market
            .hedge_underlying_price(&cfg.product)
            .or_else(|| market.underlying_price(&cfg.product))
            .unwrap_or(0.0);
        mtm += m.mtm_usd(f);
        realized += m.state().realized_pnl_usd;
    }
    (mtm, realized)
}

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
    ///
    /// Sign convention: callers should pass `max_daily_loss` as a
    /// NEGATIVE number (it represents a P&L floor). Audit T1-6: prior
    /// behavior silently dropped positive values to None — config
    /// typos that wrote "+2500" instead of "-2500" disabled the
    /// PRIMARY v1.4 defense without any operator warning. Now
    /// positives are accepted with a loud WARN and negated, matching
    /// likely operator intent. If you genuinely want the halt
    /// disabled, omit the field (pass None).
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
            if abs > 0.0 {
                log::warn!(
                    "max_daily_loss configured as positive value ${abs:.0} — \
                     interpreting as ${neg:.0} (P&L floor). Set explicitly negative \
                     to silence this warning.",
                    neg = -abs
                );
                return Some(-abs);
            }
            // abs == 0: leave halt disabled and let the `new()` path
            // log the CRITICAL "halt disabled" message.
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
    ///
    /// `hedge_qty_in_worst_delta` is the hedge contract count that the
    /// caller folded into `worst_delta`. When `hedge_fresh` is false,
    /// this monitor SUBTRACTS that contribution before evaluating the
    /// delta kill (CLAUDE.md §14 fail-closed). Set to 0.0 when the
    /// caller already passed options-only worst_delta.
    ///
    /// `hedge` is the multi-product fanout. The daily P&L halt sums
    /// `hedge_mtm + hedge_realized` into the kill threshold per
    /// CLAUDE.md §8 — without this the halt is options-only and lags
    /// reality whenever a hedge position is open. Pre-2026-05-05 the
    /// Rust port omitted hedge from the kill calc; this restores it.
    pub fn check(
        &mut self,
        portfolio: &PortfolioState,
        margin_used: f64,
        worst_delta: f64,
        worst_theta: f64,
        worst_vega: f64,
        market: &dyn MarketView,
        hedge: &HedgeFanout,
        hedge_qty_in_worst_delta: f64,
        hedge_fresh: bool,
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

        // §25: Strategy-level greek kills (delta_kill, theta_kill,
        // vega_kill) moved to the trader as improving-only gates.
        // Broker retains only operational/infrastructure kills (§7
        // spec — gateway disconnect, calibration failure, recon
        // failure, fill rate) plus the daily P&L halt below. Greek
        // values still flow to the trader via `risk_state` for the
        // trader's own gating; we just don't fire from here.
        //
        // Hedge-staleness fail-closed (CLAUDE.md §14) likewise moves
        // to the trader: the trader's `compute_risk_gates` strips
        // hedge from effective_delta when the hedge_state is stale.
        let _ = (worst_delta, worst_theta, worst_vega);
        let _ = (hedge_qty_in_worst_delta, hedge_fresh);

        // Daily P&L halt — primary v1.4 defense. Operator-override
        // kill_type=Halt (positions preserved).
        //
        // Components: realized_persisted (closed-leg accounting) +
        // options open MTM + hedge open MTM + hedge realized. Matches
        // the snapshot's daily_pnl emission so dashboard and kill
        // operate on the same number.
        if let Some(threshold) = self.cfg.daily_halt_threshold {
            let options_mtm = portfolio.compute_mtm_pnl(market);
            let (hedge_mtm, hedge_realized) = hedge_pnl(hedge, market);
            let daily = portfolio.realized_pnl_persisted
                + options_mtm
                + hedge_mtm
                + hedge_realized;
            if daily < threshold {
                let pct = (threshold.abs() / cap) * 100.0;
                let ev = KillEvent {
                    reason: format!(
                        "DAILY P&L HALT: ${:.0} < ${:.0} (-{:.1}% cap) \
                         [opts_mtm=${:.0} hedge_mtm=${:.0} hedge_real=${:.0} realized=${:.0}]",
                        daily,
                        threshold,
                        pct,
                        options_mtm,
                        hedge_mtm,
                        hedge_realized,
                        portfolio.realized_pnl_persisted
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
        hedge: &HedgeFanout,
    ) -> RiskCheckOutcome {
        if self.killed.is_some() {
            return RiskCheckOutcome::AlreadyKilled(self.killed.clone().unwrap());
        }
        let threshold = match self.cfg.daily_halt_threshold {
            Some(t) => t,
            None => return RiskCheckOutcome::Healthy,
        };
        let options_mtm = portfolio.compute_mtm_pnl(market);
        let (hedge_mtm, hedge_realized) = hedge_pnl(hedge, market);
        let daily = portfolio.realized_pnl_persisted
            + options_mtm
            + hedge_mtm
            + hedge_realized;
        if daily < threshold {
            let pct = (threshold.abs() / self.cfg.capital) * 100.0;
            let ev = KillEvent {
                reason: format!(
                    "DAILY P&L HALT (fill-path): ${:.0} < ${:.0} (-{:.1}% cap) \
                     [opts_mtm=${:.0} hedge_mtm=${:.0} hedge_real=${:.0} realized=${:.0}]",
                    daily,
                    threshold,
                    pct,
                    options_mtm,
                    hedge_mtm,
                    hedge_realized,
                    portfolio.realized_pnl_persisted
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
    ///
    /// `hedge_qty_included` is the hedge contract count folded into
    /// `effective_delta`. When `hedge_fresh` is false, this method
    /// strips that contribution before testing the kill threshold
    /// (CLAUDE.md §14 fail-closed). Pass 0.0/true for options-only
    /// callers.
    // §25: `check_per_fill_delta` removed. Per-fill delta gating
    // moved to the trader's improving-only logic
    // (`corsair_trader::decision::improving_passes`); broker only
    // forwards risk_state for the trader to act on.

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

    /// Transition to killed state and return the outcome. Called
    /// internally by `check` and externally by the broker watchdog
    /// (§25 trader_silent path).
    pub fn fire(&mut self, ev: KillEvent) -> RiskCheckOutcome {
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

    fn empty_fanout() -> HedgeFanout {
        HedgeFanout::new(vec![])
    }

    fn cfg_for_test() -> RiskConfig {
        // NOTE: vega_kill is enabled (1000.0) here so the vega-kill
        // tests exercise the threshold path. Production runs with
        // vega_kill=0 (DISABLED) per CLAUDE.md §13 — the Alabaster
        // characterization showed vega is a choke not a tail backstop
        // on HG. Don't copy 1000 into the live config without
        // re-characterizing.
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

    #[test]
    fn resolve_threshold_negates_positive_max_daily_loss() {
        // Audit T1-6: prior behavior silently dropped positive values.
        // New behavior accepts and negates with WARN, so a typo'd
        // "+2500" still arms the halt at -$2500.
        let t = RiskConfig::resolve_daily_halt_threshold(None, Some(2_500.0), 200_000.0);
        assert_eq!(t, Some(-2_500.0));
    }

    #[test]
    fn resolve_threshold_zero_max_daily_loss_is_disabled() {
        let t = RiskConfig::resolve_daily_halt_threshold(None, Some(0.0), 200_000.0);
        assert!(t.is_none());
    }

    // ── Kill firing ────────────────────────────────────────────

    #[test]
    fn margin_kill_fires_above_threshold() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(0.0, 0.0);
        let outcome = m.check(&p, 150_000.0, 0.0, 0.0, 0.0, &NoOpMarketView, &empty_fanout(), 0.0, true);
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
        let outcome = m.check(&p, 100_000.0, 0.0, 0.0, 0.0, &NoOpMarketView, &empty_fanout(), 0.0, true);
        matches!(outcome, RiskCheckOutcome::Healthy);
    }

    // §25: delta_kill / vega_kill / theta_kill tests removed —
    // strategy-level greek gating moved to the trader. Greek values
    // still flow through `check` but no longer fire kills from the
    // broker. The trader's `improving_passes` truth table is the
    // canonical test for the new gating behavior.
    #[test]
    fn strategy_greek_breaches_no_longer_fire_from_broker() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(0.0, 0.0);
        // Massive delta breach (would have fired pre-§25).
        let outcome = m.check(&p, 0.0, 50.0, 0.0, 0.0, &NoOpMarketView, &empty_fanout(), 0.0, true);
        matches!(outcome, RiskCheckOutcome::Healthy);
        // Massive theta breach.
        let outcome = m.check(&p, 0.0, 0.0, -10_000.0, 0.0, &NoOpMarketView, &empty_fanout(), 0.0, true);
        matches!(outcome, RiskCheckOutcome::Healthy);
        // Massive vega breach.
        let outcome = m.check(&p, 0.0, 0.0, 0.0, 99_999.0, &NoOpMarketView, &empty_fanout(), 0.0, true);
        matches!(outcome, RiskCheckOutcome::Healthy);
    }

    // ── Daily halt ──────────────────────────────────────────────

    #[test]
    fn daily_halt_fires_on_realized_breach() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(-3_000.0, 0.0);
        let outcome = m.check(&p, 0.0, 0.0, 0.0, 0.0, &NoOpMarketView, &empty_fanout(), 0.0, true);
        match outcome {
            RiskCheckOutcome::Killed(ev) => {
                assert!(ev.reason.contains("DAILY P&L HALT"));
                assert_eq!(ev.source, KillSource::DailyHalt);
            }
            other => panic!("expected daily halt, got {other:?}"),
        }
    }

    // §25: stale-hedge fail-closed tests removed — that fail-closed
    // logic moved to the trader's `compute_risk_gates` alongside the
    // improving-only delta gating. Trader-side test in
    // `corsair_trader::decision::tests`.

    #[test]
    fn check_daily_pnl_only_fires_on_breach() {
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(-3_000.0, 0.0);
        let outcome = m.check_daily_pnl_only(&p, &NoOpMarketView, &empty_fanout());
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
        let outcome = m.check_daily_pnl_only(&p, &NoOpMarketView, &empty_fanout());
        matches!(outcome, RiskCheckOutcome::Healthy);
        assert!(!m.is_killed());
    }

    // ── Stickiness ─────────────────────────────────────────────

    #[test]
    fn risk_kill_is_sticky() {
        // §25: drive the kill via `fire` directly since strategy-level
        // greek breaches no longer fire from the broker. Margin is
        // still broker-side, so this could equivalently use a margin
        // breach — fire is more direct.
        let mut m = RiskMonitor::new(cfg_for_test());
        m.fire(KillEvent {
            reason: "test".into(),
            source: KillSource::Risk,
            kill_type: KillType::Halt,
            timestamp_ns: 0,
        });
        let p = portfolio_with_pnl(0.0, 0.0);
        let outcome = m.check(&p, 0.0, 0.0, 0.0, 0.0, &NoOpMarketView, &empty_fanout(), 0.0, true);
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
        let outcome = m.check(&pf, 0.0, 0.0, 0.0, 0.0, &NoOpMarketView, &empty_fanout(), 0.0, true);
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
        let outcome = m.check_daily_pnl_only(&p, &view, &empty_fanout());
        matches!(outcome, RiskCheckOutcome::Killed(_));
    }

    // ── Hedge-aware daily P&L ────────────────────────────────────

    #[test]
    fn daily_halt_includes_hedge_mtm() {
        // Options book is healthy (no realized, no open MTM), but a
        // long hedge entered at 5.99 marks against 5.85 — −$3,500
        // hedge MTM, well past the −$2,500 threshold. Pre-fix this
        // would have been ignored (options-only daily P&L); post-fix
        // the kill must fire.
        use corsair_hedge::{HedgeConfig, HedgeManager, HedgeMode};
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(0.0, 0.0);
        let mut hm = HedgeManager::new(HedgeConfig {
            product: "HG".into(),
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
        });
        hm.reconcile_with_position(1, 5.99, false);
        let h = HedgeFanout::new(vec![hm]);
        let view = RecordingMarketView::new();
        view.set_underlying("HG", 5.85);
        view.set_hedge_underlying("HG", 5.85);
        let outcome = m.check(&p, 0.0, 0.0, 0.0, 0.0, &view, &h, 0.0, true);
        match outcome {
            RiskCheckOutcome::Killed(ev) => {
                assert!(ev.reason.contains("DAILY P&L HALT"));
                assert!(ev.reason.contains("hedge_mtm"));
            }
            other => panic!("expected daily halt with hedge MTM, got {other:?}"),
        }
    }

    #[test]
    fn daily_halt_fill_path_includes_hedge_mtm() {
        use corsair_hedge::{HedgeConfig, HedgeManager, HedgeMode};
        let mut m = RiskMonitor::new(cfg_for_test());
        let p = portfolio_with_pnl(0.0, 0.0);
        let mut hm = HedgeManager::new(HedgeConfig {
            product: "HG".into(),
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
        });
        hm.reconcile_with_position(1, 5.99, false);
        let h = HedgeFanout::new(vec![hm]);
        let view = RecordingMarketView::new();
        view.set_hedge_underlying("HG", 5.85);
        let outcome = m.check_daily_pnl_only(&p, &view, &h);
        matches!(outcome, RiskCheckOutcome::Killed(_));
        assert!(m.is_killed());
    }
}
