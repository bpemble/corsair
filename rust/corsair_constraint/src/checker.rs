//! `ConstraintChecker` — combined-budget gate logic.

use corsair_broker_api::Right;
use corsair_position::PortfolioState;
use serde::{Deserialize, Serialize};

use crate::gates::GateOutcome;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstraintConfig {
    /// IBKR underlying symbol; bounds delta/theta gating to this
    /// product only.
    pub product: String,
    pub capital: f64,
    pub margin_ceiling_pct: f64,
    pub delta_ceiling: f64,
    /// Negative; lower bound on net theta.
    pub theta_floor: f64,
    /// Hard kill limits — final backstop even when margin-escape is on.
    pub margin_kill_pct: f64,
    pub delta_kill: f64,
    pub theta_kill: f64,
    /// Toggle for effective_delta_gating (CLAUDE.md §14). False
    /// reverts to options-only.
    pub effective_delta_gating: bool,
    /// Tier-1 margin priority escape — allows fills that strictly
    /// reduce margin even when they'd breach delta/theta soft caps,
    /// as long as no hard kill limit is crossed.
    pub margin_escape_enabled: bool,
}

impl ConstraintConfig {
    pub fn margin_ceiling(&self) -> f64 {
        self.capital * self.margin_ceiling_pct
    }
    pub fn margin_kill(&self) -> f64 {
        self.capital * self.margin_kill_pct
    }
}

/// Pre-fill check input. Caller computes:
/// - `cur_margin` and `post_margin` from corsair_pricing::span (with
///   IBKR scale already applied)
/// - `cur_long_premium` and `post_long_premium` for the solvency check
/// - `option_delta_per_contract` and `option_theta_per_contract` from
///   greek refresh; sign carries side direction
#[derive(Debug, Clone)]
pub struct ConstraintCheck {
    pub option_strike: f64,
    pub option_right: Right,
    pub option_expiry: chrono::NaiveDate,
    pub option_delta_per_contract: f64,
    pub option_theta_per_contract: f64,
    pub fill_qty_signed: i32,
    pub multiplier: f64,
    pub cur_margin: f64,
    pub post_margin: f64,
    pub cur_long_premium: f64,
    pub post_long_premium: f64,
    /// Hedge contract qty (signed), needed for effective_delta_gating.
    /// Caller passes 0 if no hedge or gating disabled.
    pub hedge_qty: i32,
    /// Freshness of `hedge_qty` per CLAUDE.md §14. The caller is
    /// responsible for computing this against `HedgeState::is_fresh`
    /// (or equivalent). Encoding:
    ///   - `None`     = legacy behavior; hedge_qty is trusted iff
    ///                  effective_delta_gating is on (back-compat)
    ///   - `Some(true)`  = hedge state was reconciled recently
    ///   - `Some(false)` = hedge state is stale → gate FAILS CLOSED
    ///                     (treat hedge_qty as 0 + log WARN once;
    ///                     reject any order pushing options-only
    ///                     delta past the ceiling)
    /// TODO-NOTE for callers in OTHER crates (corsair_broker::tasks):
    /// you must compute freshness via HedgeState::is_fresh(now_ns,
    /// freshness_ns) and pass Some(bool) here. Default `None` keeps
    /// compile-compat but disables the fail-closed guarantee.
    pub hedge_state_fresh: Option<bool>,
}

/// Calibration bounds on the synthetic-vs-IBKR scale factor (CLAUDE.md
/// §3). Values outside the band fall back to 1.0 so the scale never
/// hides a real risk-model failure.
pub const IBKR_SCALE_LOWER: f64 = 0.5;
pub const IBKR_SCALE_UPPER: f64 = 1.25;

pub struct ConstraintChecker {
    cfg: ConstraintConfig,
    /// Synthetic-vs-IBKR margin scale factor (CLAUDE.md §3). Synthetic
    /// SPAN runs ~25-30% high vs IBKR for short strangles; this scale
    /// brings outputs to IBKR-equivalent. Updated every ~5min by
    /// `update_cached_margin`. Default 1.0 (no scaling) until first
    /// calibration lands.
    ibkr_scale: f64,
    /// Last-observed raw / IBKR / scale tuple. Useful for telemetry.
    last_calibration: Option<MarginCalibration>,
    /// Audit T1-1 / CLAUDE.md §14: log a stale-hedge WARN at most
    /// once per process; stale-hedge events are noisy and the warn
    /// should be loud-but-not-spammy.
    stale_hedge_warned: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone, Copy)]
pub struct MarginCalibration {
    pub raw_synthetic: f64,
    pub ibkr_actual: f64,
    pub scale: f64,
    pub timestamp_ns: u64,
}

impl ConstraintChecker {
    pub fn new(cfg: ConstraintConfig) -> Self {
        Self {
            cfg,
            ibkr_scale: 1.0,
            last_calibration: None,
            stale_hedge_warned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn config(&self) -> &ConstraintConfig {
        &self.cfg
    }

    /// Current synthetic-vs-IBKR scale factor. Multiply raw synthetic
    /// SPAN by this to get IBKR-equivalent margin.
    pub fn ibkr_scale(&self) -> f64 {
        self.ibkr_scale
    }

    /// Apply the IBKR scale to a raw synthetic SPAN value.
    pub fn scale(&self, raw_synthetic: f64) -> f64 {
        raw_synthetic * self.ibkr_scale
    }

    /// Re-calibrate the IBKR scale from a fresh account-values poll.
    /// Skip the update if either input is non-positive (degenerate
    /// state) or if the resulting scale is outside the safety band —
    /// the bound prevents masking a real risk model failure (CLAUDE.md
    /// §3). Returns the scale actually adopted.
    pub fn update_cached_margin(
        &mut self,
        ibkr_actual: f64,
        raw_synthetic: f64,
        timestamp_ns: u64,
    ) -> f64 {
        if ibkr_actual <= 0.0 || raw_synthetic <= 0.0 {
            // Don't update on degenerate inputs — keep prior scale.
            return self.ibkr_scale;
        }
        let proposed = ibkr_actual / raw_synthetic;
        let bounded = if proposed < IBKR_SCALE_LOWER || proposed > IBKR_SCALE_UPPER {
            log::warn!(
                "ibkr_scale: proposed={:.3} outside [{:.2}, {:.2}] — falling back to 1.0",
                proposed,
                IBKR_SCALE_LOWER,
                IBKR_SCALE_UPPER
            );
            1.0
        } else {
            proposed
        };
        self.ibkr_scale = bounded;
        self.last_calibration = Some(MarginCalibration {
            raw_synthetic,
            ibkr_actual,
            scale: bounded,
            timestamp_ns,
        });
        log::info!(
            "ibkr_scale recalibrated: raw=${:.0} ibkr=${:.0} → scale={:.3}",
            raw_synthetic,
            ibkr_actual,
            bounded
        );
        bounded
    }

    pub fn last_calibration(&self) -> Option<MarginCalibration> {
        self.last_calibration
    }

    /// Evaluate all gates against the proposed fill.
    pub fn check(&self, portfolio: &PortfolioState, c: &ConstraintCheck) -> GateOutcome {
        let cap = self.cfg.capital;
        let margin_ceiling = self.cfg.margin_ceiling();
        let margin_kill = self.cfg.margin_kill();

        // Pre-compute current state for this product.
        let cur_options_delta = portfolio.delta_for_product(&self.cfg.product);
        let cur_theta = portfolio.theta_for_product(&self.cfg.product);

        // Effective delta = options + hedge, when gating is on.
        // Audit T1-1 / CLAUDE.md §14: if the caller declares the hedge
        // state STALE (Some(false)), zero out hedge_qty and fail
        // closed — the gate then operates on options-only delta. WARN
        // once so the operator sees the degradation. Some(true) trusts
        // hedge_qty as fresh; None keeps legacy behavior for callers
        // that haven't been updated to compute freshness yet.
        let trust_hedge = self.cfg.effective_delta_gating
            && match c.hedge_state_fresh {
                Some(true) => true,
                Some(false) => {
                    use std::sync::atomic::Ordering;
                    if !self.stale_hedge_warned.swap(true, Ordering::Relaxed) {
                        log::warn!(
                            "ConstraintChecker: hedge state STALE — gating falls back \
                             to options-only delta (fail-closed). Subsequent stale \
                             events suppressed."
                        );
                    }
                    false
                }
                None => true, // legacy callers — preserve prior behavior
            };
        let effective_hedge_qty = if trust_hedge { c.hedge_qty } else { 0 };
        let cur_delta = cur_options_delta + effective_hedge_qty as f64;
        let post_delta =
            cur_delta + c.option_delta_per_contract * (c.fill_qty_signed as f64);

        // Theta math.
        let option_theta_total = c.option_theta_per_contract * c.multiplier;
        let post_theta = cur_theta + option_theta_total * (c.fill_qty_signed as f64);

        // ── Tier-1 hard kill limits (always binding) ──────────────
        if c.post_margin > margin_kill {
            return GateOutcome::Rejected {
                reason: "margin_kill".into(),
                detail: format!(
                    "margin_kill: post=${:.0} > kill=${:.0}",
                    c.post_margin, margin_kill
                ),
            };
        }
        if post_delta.abs() > self.cfg.delta_kill {
            return GateOutcome::Rejected {
                reason: "delta_kill".into(),
                detail: format!(
                    "delta_kill: post={:+.2} > ±{:.1}",
                    post_delta, self.cfg.delta_kill
                ),
            };
        }
        if self.cfg.theta_kill < 0.0 && post_theta < self.cfg.theta_kill {
            return GateOutcome::Rejected {
                reason: "theta_kill".into(),
                detail: format!(
                    "theta_kill: post=${:.0} < ${:.0}",
                    post_theta, self.cfg.theta_kill
                ),
            };
        }

        // ── Tier-1 margin priority escape ─────────────────────────
        if self.cfg.margin_escape_enabled
            && c.cur_margin > margin_ceiling
            && c.post_margin < c.cur_margin
        {
            // Long-premium capital check is a solvency constraint
            // (cash outlay), not a risk metric — enforce it even in
            // escape mode, with the same improving-fill exception.
            if c.post_long_premium > cap && c.post_long_premium > c.cur_long_premium {
                return GateOutcome::Rejected {
                    reason: "long_premium".into(),
                    detail: format!(
                        "long_premium: post=${:.0} > cap=${:.0} (escape mode)",
                        c.post_long_premium, cap
                    ),
                };
            }
            return GateOutcome::AcceptedMarginEscape {
                detail: format!(
                    "margin escape: cur=${:.0} > ceiling=${:.0}, post=${:.0} reduces breach",
                    c.cur_margin, margin_ceiling, c.post_margin
                ),
            };
        }

        // ── Strict mode: per-constraint with improving-fill ────────

        // Margin
        if c.post_margin > margin_ceiling {
            // Improving exception: if currently breached, allow if post < cur.
            if c.cur_margin > margin_ceiling && c.post_margin >= c.cur_margin {
                return GateOutcome::Rejected {
                    reason: "margin".into(),
                    detail: format!(
                        "margin: post=${:.0} > ceiling=${:.0} (currently breached, fill not improving)",
                        c.post_margin, margin_ceiling
                    ),
                };
            }
            if c.cur_margin <= margin_ceiling {
                return GateOutcome::Rejected {
                    reason: "margin".into(),
                    detail: format!(
                        "margin: post=${:.0} > ceiling=${:.0}",
                        c.post_margin, margin_ceiling
                    ),
                };
            }
        }

        // Delta
        if post_delta.abs() > self.cfg.delta_ceiling {
            // Improving: if abs(post) < abs(cur), allow — BUT only when
            // post does not flip sign relative to cur. A sign flip
            // means the fill rotated us through zero into the opposite
            // exposure, which is not "improving" — it's overshooting.
            // Without this check, a fill from cur=+3.5 to post=-3.4
            // gets accepted as "improving" even though we're now short
            // delta past the ceiling.
            let sign_eq = (post_delta >= 0.0) == (cur_delta >= 0.0);
            if post_delta.abs() >= cur_delta.abs() || !sign_eq {
                return GateOutcome::Rejected {
                    reason: "delta".into(),
                    detail: format!(
                        "delta: post={:+.2} > ±{:.1} (cur={:+.2}{})",
                        post_delta,
                        self.cfg.delta_ceiling,
                        cur_delta,
                        if !sign_eq { ", sign-flip" } else { "" }
                    ),
                };
            }
        }

        // Theta
        if post_theta < self.cfg.theta_floor {
            if post_theta <= cur_theta {
                return GateOutcome::Rejected {
                    reason: "theta".into(),
                    detail: format!(
                        "theta: post=${:.0} < ${:.0} (cur=${:.0})",
                        post_theta, self.cfg.theta_floor, cur_theta
                    ),
                };
            }
        }

        // Long-premium solvency
        if c.post_long_premium > cap && c.post_long_premium > c.cur_long_premium {
            return GateOutcome::Rejected {
                reason: "long_premium".into(),
                detail: format!(
                    "long_premium: post=${:.0} > cap=${:.0}",
                    c.post_long_premium, cap
                ),
            };
        }

        GateOutcome::Accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use corsair_position::{ProductInfo, ProductRegistry};

    fn cfg() -> ConstraintConfig {
        ConstraintConfig {
            product: "HG".into(),
            capital: 200_000.0,
            margin_ceiling_pct: 0.50,    // → 100k
            delta_ceiling: 3.0,
            theta_floor: -200.0,
            margin_kill_pct: 0.70,        // → 140k
            delta_kill: 5.0,
            theta_kill: -500.0,
            effective_delta_gating: true,
            margin_escape_enabled: false,
        }
    }

    fn portfolio_empty() -> PortfolioState {
        let mut r = ProductRegistry::new();
        r.register(ProductInfo {
            product: "HG".into(),
            multiplier: 25_000.0,
            default_iv: 0.30,
        });
        PortfolioState::new(r)
    }

    fn check(post_margin: f64, opt_delta: f64, opt_theta: f64, qty: i32) -> ConstraintCheck {
        ConstraintCheck {
            option_strike: 6.05,
            option_right: Right::Call,
            option_expiry: NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
            option_delta_per_contract: opt_delta,
            option_theta_per_contract: opt_theta,
            fill_qty_signed: qty,
            multiplier: 25_000.0,
            cur_margin: 0.0,
            post_margin,
            cur_long_premium: 0.0,
            post_long_premium: 0.0,
            hedge_qty: 0,
            hedge_state_fresh: None,
        }
    }

    // ── Margin gate ────────────────────────────────────────────

    #[test]
    fn under_margin_ceiling_accepted() {
        let cc = ConstraintChecker::new(cfg());
        let p = portfolio_empty();
        let outcome = cc.check(&p, &check(50_000.0, 0.5, -0.001, 1));
        assert_eq!(outcome, GateOutcome::Accepted);
    }

    #[test]
    fn margin_breach_rejects() {
        let cc = ConstraintChecker::new(cfg());
        let p = portfolio_empty();
        let outcome = cc.check(&p, &check(110_000.0, 0.5, -0.001, 1));
        assert_eq!(outcome.rejection_reason(), Some("margin"));
    }

    #[test]
    fn margin_kill_takes_precedence() {
        let cc = ConstraintChecker::new(cfg());
        let p = portfolio_empty();
        let outcome = cc.check(&p, &check(150_000.0, 0.5, -0.001, 1));
        assert_eq!(outcome.rejection_reason(), Some("margin_kill"));
    }

    // ── Improving-fill exception ────────────────────────────────

    #[test]
    fn improving_margin_when_breached_accepted() {
        let cc = ConstraintChecker::new(cfg());
        let p = portfolio_empty();
        let mut c = check(105_000.0, 0.5, -0.001, -1); // sell, reducing
        c.cur_margin = 110_000.0; // currently breached
        let outcome = cc.check(&p, &c);
        // Margin breached but improving — accepted (no margin reject).
        assert!(matches!(outcome, GateOutcome::Accepted));
    }

    #[test]
    fn worsening_margin_when_breached_rejected() {
        let cc = ConstraintChecker::new(cfg());
        let p = portfolio_empty();
        let mut c = check(115_000.0, 0.5, -0.001, 1);
        c.cur_margin = 110_000.0;
        let outcome = cc.check(&p, &c);
        assert_eq!(outcome.rejection_reason(), Some("margin"));
    }

    // ── Delta gate ─────────────────────────────────────────────

    #[test]
    fn delta_breach_rejects_when_increasing() {
        let cc = ConstraintChecker::new(cfg());
        // Build portfolio with cur_delta = 2.5
        let mut r = ProductRegistry::new();
        r.register(ProductInfo {
            product: "HG".into(),
            multiplier: 25_000.0,
            default_iv: 0.30,
        });
        let mut p = PortfolioState::new(r);
        // Hand-set positions; simulating a 5-position book with
        // pre-computed greeks summing to 2.5 delta.
        use corsair_position::Position;
        use chrono::Utc;
        p.replace_positions(vec![Position {
            product: "HG".into(),
            strike: 6.05,
            expiry: NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
            right: Right::Call,
            quantity: 5,
            avg_fill_price: 0.025,
            fill_time: Utc::now(),
            multiplier: 25_000.0,
            delta: 0.5,
            gamma: 0.0,
            theta: 0.0,
            vega: 0.0,
            current_price: 0.025,
        }]);
        let outcome = cc.check(&p, &check(50_000.0, 0.5, 0.0, 2));
        // cur=2.5; post=2.5+0.5*2=3.5; ceiling=3.0 → reject.
        assert_eq!(outcome.rejection_reason(), Some("delta"));
    }

    #[test]
    fn delta_kill_rejects() {
        let cc = ConstraintChecker::new(cfg());
        let p = portfolio_empty();
        let outcome = cc.check(&p, &check(50_000.0, 6.0, 0.0, 1));
        assert_eq!(outcome.rejection_reason(), Some("delta_kill"));
    }

    // ── Effective delta gating ─────────────────────────────────

    #[test]
    fn effective_gating_on_credits_hedge() {
        let cc = ConstraintChecker::new(cfg());
        let p = portfolio_empty();
        let mut c = check(50_000.0, 2.0, 0.0, 1);
        c.hedge_qty = -3; // short hedge cancels long delta
        let outcome = cc.check(&p, &c);
        // post_delta = 0 + 2*1 + (-3) = -1 → within ceiling → accept
        assert_eq!(outcome, GateOutcome::Accepted);
    }

    #[test]
    fn effective_gating_off_uses_options_only() {
        let mut cf = cfg();
        cf.effective_delta_gating = false;
        let cc = ConstraintChecker::new(cf);
        let p = portfolio_empty();
        let mut c = check(50_000.0, 4.0, 0.0, 1);
        c.hedge_qty = -10; // would offset under gating, but we're off
        let outcome = cc.check(&p, &c);
        // post_delta options-only = 4.0 → exceeds delta_kill 5.0? no.
        // exceeds ceiling 3.0 — yes. cur_delta=0, abs(post)=4 > abs(0).
        assert_eq!(outcome.rejection_reason(), Some("delta"));
    }

    // ── Theta gate ─────────────────────────────────────────────

    #[test]
    fn theta_breach_rejects() {
        let cc = ConstraintChecker::new(cfg());
        let p = portfolio_empty();
        // theta_floor = -200, theta_kill = -500.
        // Use option_theta_per_contract = -0.012; multiplier = 25000
        // → option_theta_total = -300/contract.
        // Buying 1 → post_theta = -300, which breaches floor -200
        // but stays above kill -500 (so soft "theta" gate fires).
        let outcome = cc.check(&p, &check(50_000.0, 0.5, -0.012, 1));
        assert_eq!(outcome.rejection_reason(), Some("theta"));
    }

    // ── Long-premium solvency ──────────────────────────────────

    #[test]
    fn long_premium_breach_rejects() {
        let cc = ConstraintChecker::new(cfg());
        let p = portfolio_empty();
        let mut c = check(50_000.0, 0.5, 0.0, 1);
        c.post_long_premium = 250_000.0; // > capital 200k
        let outcome = cc.check(&p, &c);
        assert_eq!(outcome.rejection_reason(), Some("long_premium"));
    }

    // ── Margin escape ──────────────────────────────────────────

    #[test]
    fn margin_escape_accepts_improving_fill_with_soft_breach() {
        let mut cf = cfg();
        cf.margin_escape_enabled = true;
        let cc = ConstraintChecker::new(cf);
        let p = portfolio_empty();
        // Currently above ceiling; fill reduces margin AND breaches
        // delta soft cap (which would normally reject).
        let mut c = check(105_000.0, 4.0, 0.0, 1);
        c.cur_margin = 110_000.0;
        let outcome = cc.check(&p, &c);
        // post_delta = 4.0 → breaches ceiling 3.0 but escape on.
        // Should accept under escape (margin reducing).
        assert!(matches!(outcome, GateOutcome::AcceptedMarginEscape { .. }));
    }

    // ── Sign-flip improving delta exception ────────────────────

    #[test]
    fn delta_improving_with_sign_flip_rejected() {
        // cur_delta = +3.5 (over ceiling 3.0). A SELL of 4 contracts
        // at delta 1.7 → post = +3.5 - 4*1.7 = -3.3. abs(post)=3.3 <
        // abs(cur)=3.5, so old logic would call this "improving" and
        // accept. New logic rejects on sign flip — we crossed zero.
        let cc = ConstraintChecker::new(cfg());
        let mut r = ProductRegistry::new();
        r.register(ProductInfo {
            product: "HG".into(),
            multiplier: 25_000.0,
            default_iv: 0.30,
        });
        let mut p = PortfolioState::new(r);
        use chrono::Utc;
        use corsair_position::Position;
        // Pre-existing options at delta 0.7 × 5 = 3.5
        p.replace_positions(vec![Position {
            product: "HG".into(),
            strike: 6.05,
            expiry: NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
            right: Right::Call,
            quantity: 5,
            avg_fill_price: 0.025,
            fill_time: Utc::now(),
            multiplier: 25_000.0,
            delta: 0.7,
            gamma: 0.0,
            theta: 0.0,
            vega: 0.0,
            current_price: 0.025,
        }]);
        let outcome = cc.check(&p, &check(50_000.0, 1.7, 0.0, -4));
        assert_eq!(outcome.rejection_reason(), Some("delta"));
    }

    #[test]
    fn delta_improving_same_sign_still_accepted() {
        // cur_delta = +3.5, post = +3.0 (under ceiling) — accept.
        let cc = ConstraintChecker::new(cfg());
        let mut r = ProductRegistry::new();
        r.register(ProductInfo {
            product: "HG".into(),
            multiplier: 25_000.0,
            default_iv: 0.30,
        });
        let mut p = PortfolioState::new(r);
        use chrono::Utc;
        use corsair_position::Position;
        p.replace_positions(vec![Position {
            product: "HG".into(),
            strike: 6.05,
            expiry: NaiveDate::from_ymd_opt(2026, 6, 26).unwrap(),
            right: Right::Call,
            quantity: 5,
            avg_fill_price: 0.025,
            fill_time: Utc::now(),
            multiplier: 25_000.0,
            delta: 0.7,
            gamma: 0.0,
            theta: 0.0,
            vega: 0.0,
            current_price: 0.025,
        }]);
        // Post = 3.5 + (-0.5)*1 = 3.0; under ceiling; accept.
        let outcome = cc.check(&p, &check(50_000.0, -0.5, 0.0, 1));
        assert!(matches!(outcome, GateOutcome::Accepted));
    }

    // ── Stale-hedge fail-closed ────────────────────────────────

    #[test]
    fn stale_hedge_zeros_out_hedge_credit() {
        // gating on, hedge -3 (would offset +4 options to +1), but
        // hedge_state_fresh = Some(false) → ignore hedge → post=+4
        // breaches ceiling → reject.
        let cc = ConstraintChecker::new(cfg());
        let p = portfolio_empty();
        let mut c = check(50_000.0, 4.0, 0.0, 1);
        c.hedge_qty = -3;
        c.hedge_state_fresh = Some(false);
        let outcome = cc.check(&p, &c);
        assert_eq!(outcome.rejection_reason(), Some("delta"));
    }

    #[test]
    fn fresh_hedge_credits_normally() {
        let cc = ConstraintChecker::new(cfg());
        let p = portfolio_empty();
        let mut c = check(50_000.0, 4.0, 0.0, 1);
        c.hedge_qty = -3;
        c.hedge_state_fresh = Some(true);
        // post = 4 + (-3) = 1 → under ceiling → accept
        let outcome = cc.check(&p, &c);
        assert_eq!(outcome, GateOutcome::Accepted);
    }

    #[test]
    fn margin_escape_does_not_bypass_hard_kill() {
        let mut cf = cfg();
        cf.margin_escape_enabled = true;
        let cc = ConstraintChecker::new(cf);
        let p = portfolio_empty();
        let mut c = check(105_000.0, 6.0, 0.0, 1);
        c.cur_margin = 110_000.0;
        // post_delta=6 > delta_kill=5 — escape doesn't help.
        let outcome = cc.check(&p, &c);
        assert_eq!(outcome.rejection_reason(), Some("delta_kill"));
    }
}
