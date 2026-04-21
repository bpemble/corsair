"""Risk monitor for Corsair v2.

Continuous monitoring (every 5 minutes or more frequently):
- SPAN margin vs kill threshold
- Portfolio delta vs kill threshold
- Daily P&L vs kill threshold (v1.4 §6.1 PRIMARY defense; flattens book)
- Margin warning (above ceiling but below kill)

Kill taxonomy (v1.4 §6.2):
- kill_type="halt"       — cancel quotes, no flatten. Theta / vega halts.
- kill_type="flatten"    — cancel quotes + flatten options + flatten hedge.
                           Daily P&L halt and SPAN margin kill.
- kill_type="hedge_flat" — cancel quotes + force hedge to 0 (no options
                           flatten). Delta kill.

Kill source (governs auto-clear rules):
- source="risk"            — sticky; requires manual review.
- source="disconnect"      — cleared by watchdog after successful reconnect.
- source="daily_halt"      — cleared at next CME session rollover (17:00 CT)
                             per v1.4 §6.1 "automatic resume at next session".
- source="reconciliation"  — sticky.
- source="exception_storm" — sticky.
- source="operational"     — sticky (SABR, latency, etc.).
"""

import logging
import os

logger = logging.getLogger(__name__)

# v1.4 §9.4 induced-test sentinels. Each sentinel file drops the
# corresponding kill switch through its REAL firing path (cancel +
# flatten/halt + jsonl log) on the next risk check. Used for Gate 0
# pre-launch validation. See scripts/induce_kill_switch.py for the
# writer side.
#
# Dir is env-configurable so operators can point it at a docker-mounted
# volume when running induce scripts from the host (default /tmp only
# works inside the container via `docker compose exec`).
INDUCE_SENTINEL_DIR = os.environ.get("CORSAIR_INDUCE_DIR", "/tmp")
INDUCE_SENTINELS = {
    "daily_pnl": ("corsair_induce_daily_pnl",
                  "flatten", "daily_halt"),
    "margin":    ("corsair_induce_margin",
                  "flatten", "risk"),
    "delta":     ("corsair_induce_delta",
                  "hedge_flat", "risk"),
    "theta":     ("corsair_induce_theta",
                  "halt", "risk"),
    "vega":      ("corsair_induce_vega",
                  "halt", "risk"),
}


def _resolve_daily_halt_threshold(config):
    """Module-level helper for deriving the daily P&L halt threshold.

    v1.4 prefers daily_pnl_halt_pct × capital; falls back to the legacy
    absolute max_daily_loss. Exposed at module scope so main.py's
    snapshot emitter and RiskMonitor share one resolution path.
    Returns None if neither is configured (halt disabled).
    """
    try:
        k = config.kill_switch
        capital = float(config.constraints.capital)
    except AttributeError:
        return None
    pct = float(getattr(k, "daily_pnl_halt_pct", 0) or 0)
    if pct > 0 and capital > 0:
        return -(pct * capital)
    abs_val = float(getattr(k, "max_daily_loss", 0) or 0)
    if abs_val < 0:
        return abs_val
    return None


class RiskMonitor:
    """Monitors portfolio risk and triggers kill switch on breaches."""

    def __init__(self, portfolio, margin_checker, quote_manager, csv_logger, config,
                 flatten_callback=None, hedge_manager=None):
        self.portfolio = portfolio
        # Multi-product: this is normally a MarginCoordinator, which exposes
        # combined-across-products get_current_margin() / update_cached_margin().
        # The single-engine IBKRMarginChecker is also accepted (for tests /
        # legacy paths) — both shape-compat the methods we call.
        self.margin = margin_checker
        self.quotes = quote_manager
        self.csv_logger = csv_logger
        self.config = config
        # v1.4 §6.1 flatten-on-halt: injected by main.py because flatten must
        # iterate every registered engine (multi-product). When None, kill()
        # with kill_type=="flatten" falls back to cancel-only and logs a warn
        # — an explicit misconfiguration signal, not a silent fallback.
        self.flatten_callback = flatten_callback
        # v1.4 §6.2 delta kill: force hedge to 0 without flattening options.
        # When None, delta kills degrade to a bare halt.
        self.hedge_manager = hedge_manager
        self.killed = False
        self._kill_reason: str = ""
        self._kill_source: str = ""
        self._kill_type: str = ""

        # v1.4 §6.1 PRIMARY DEFENSE: resolve once at init and cache. If
        # neither daily_pnl_halt_pct nor max_daily_loss is configured the
        # halt is disabled — log CRITICAL so an operator can't miss it.
        # check() and check_daily_pnl_only() both bail on None rather
        # than re-resolving per-call (avoids silent drift if config is
        # hot-reloaded).
        self.daily_halt_threshold: float | None = (
            _resolve_daily_halt_threshold(config)
        )
        if self.daily_halt_threshold is None:
            logger.critical(
                "DAILY P&L HALT DISABLED: neither kill_switch.daily_pnl_halt_pct "
                "nor kill_switch.max_daily_loss is configured. The primary v1.4 "
                "defense will NOT fire. Set daily_pnl_halt_pct (e.g. 0.05) in "
                "the active config before resuming live orders."
            )
        else:
            _cap = float(getattr(config.constraints, "capital", 0)) or 0.0
            _pct = (
                abs(self.daily_halt_threshold / _cap) * 100 if _cap > 0 else 0.0
            )
            logger.info(
                "Daily P&L halt armed: threshold=$%.0f (-%.1f%% of $%.0f capital)",
                self.daily_halt_threshold, _pct, _cap,
            )

    def check(self, market_state):
        """Run all risk checks. Called every greek_refresh_seconds."""
        if self.killed:
            return

        # v1.4 §9.4 induced-test hook. If the operator dropped a
        # sentinel file, fire the corresponding kill through the real
        # firing path (cancel + flatten + paper log), then delete the
        # sentinel so the test doesn't re-trigger every cycle. Source
        # is tagged with "induced_" prefix so reconciliation can
        # distinguish induced from genuine breaches.
        if self._check_induced_sentinels():
            return

        # Refresh Greeks
        self.portfolio.refresh_greeks()

        # Update cached margin
        if hasattr(self.margin, 'update_cached_margin'):
            self.margin.update_cached_margin()

        current_margin = self.margin.get_current_margin()
        capital = self.config.constraints.capital

        # Multi-product safety: compute per-product delta/vega/theta and
        # take the worst (max abs / min). Mixing contract-equivalent
        # numbers across products with different multipliers (ETH 50 vs
        # HG 25000) is meaningless, so the kill switches gate on the
        # most-exposed single product instead of a nonsense sum.
        products = list(self.portfolio.products()) or ["__all__"]
        worst_delta = 0.0
        worst_delta_prod = ""
        worst_vega = 0.0
        worst_vega_prod = ""
        worst_theta = 0.0  # most-negative
        worst_theta_prod = ""
        if products == ["__all__"]:
            worst_delta = self.portfolio.net_delta
            worst_vega = self.portfolio.net_vega
            worst_theta = self.portfolio.net_theta
        else:
            for prod in products:
                d = self.portfolio.delta_for_product(prod)
                v = self.portfolio.vega_for_product(prod)
                t = self.portfolio.theta_for_product(prod)
                if abs(d) > abs(worst_delta):
                    worst_delta, worst_delta_prod = d, prod
                if abs(v) > abs(worst_vega):
                    worst_vega, worst_vega_prod = v, prod
                if t < worst_theta:
                    worst_theta, worst_theta_prod = t, prod

        # Log risk snapshot
        self.csv_logger.log_risk_snapshot(
            underlying_price=market_state.underlying_price,
            margin_used=current_margin,
            margin_pct=current_margin / capital if capital > 0 else 0,
            net_delta=self.portfolio.net_delta,
            net_theta=self.portfolio.net_theta,
            net_vega=self.portfolio.net_vega,
            long_count=self.portfolio.long_count,
            short_count=self.portfolio.short_count,
            gross_positions=self.portfolio.gross_positions,
            unrealized_pnl=self.portfolio.compute_mtm_pnl(),
            daily_spread_capture=self.portfolio.spread_capture_today,
        )

        logger.info(
            "RISK: margin=$%.0f (%.0f%%) worst_delta=%.2f[%s] "
            "worst_theta=$%.0f[%s] worst_vega=$%.0f[%s] "
            "positions=%d (L%d/S%d) pnl=$%.0f",
            current_margin, (current_margin / capital * 100) if capital > 0 else 0,
            worst_delta, worst_delta_prod or "?",
            worst_theta, worst_theta_prod or "?",
            worst_vega, worst_vega_prod or "?",
            self.portfolio.gross_positions,
            self.portfolio.long_count, self.portfolio.short_count,
            self.portfolio.compute_mtm_pnl(),
        )

        # SPAN margin kill (v1.4 §6.2): FLATTEN book on breach.
        margin_kill = capital * self.config.kill_switch.margin_kill_pct
        if current_margin > margin_kill:
            self.kill(
                f"MARGIN KILL: ${current_margin:,.0f} > ${margin_kill:,.0f}",
                source="risk", kill_type="flatten",
            )
            return

        # Delta kill (v1.4 §6.2): force hedge to 0, halt new opens.
        if abs(worst_delta) > self.config.kill_switch.delta_kill:
            self.kill(
                f"DELTA KILL [{worst_delta_prod}]: {worst_delta:.2f} > "
                f"±{self.config.kill_switch.delta_kill}",
                source="risk", kill_type="hedge_flat",
            )
            return

        # Vega halt (v1.4 §6.2): halt new opens only, no flatten.
        vega_kill = float(getattr(self.config.kill_switch, "vega_kill", 0) or 0)
        if vega_kill > 0 and abs(worst_vega) > vega_kill:
            self.kill(
                f"VEGA HALT [{worst_vega_prod}]: ${worst_vega:+,.0f} > "
                f"±${vega_kill:,.0f}",
                source="risk", kill_type="halt",
            )
            return

        # Theta halt (v1.4 §6.2): halt new opens only.
        theta_kill = float(getattr(self.config.kill_switch, "theta_kill", 0) or 0)
        if theta_kill < 0 and worst_theta < theta_kill:
            self.kill(
                f"THETA HALT [{worst_theta_prod}]: ${worst_theta:.0f} < "
                f"${theta_kill:.0f}",
                source="risk", kill_type="halt",
            )
            return

        # Daily P&L halt (v1.4 §6.1 PRIMARY DEFENSE): FLATTEN on breach.
        # Threshold resolved+cached at __init__; None means the halt is
        # disabled (CRITICAL log already emitted).
        if self.daily_halt_threshold is not None:
            self.portfolio.daily_pnl = (
                self.portfolio.realized_pnl_persisted
                + self.portfolio.compute_mtm_pnl()
                + self._hedge_mtm_usd()
            )
            if self.portfolio.daily_pnl < self.daily_halt_threshold:
                self.kill(
                    f"DAILY P&L HALT: ${self.portfolio.daily_pnl:,.0f} < "
                    f"${self.daily_halt_threshold:,.0f} "
                    f"(-{abs(self.daily_halt_threshold/capital)*100:.1f}% cap)",
                    source="daily_halt", kill_type="flatten",
                )
                return

        # Margin warning (above ceiling but below kill) — log only; the
        # constraint checker already blocks margin-increasing opens.
        margin_ceiling = capital * self.config.constraints.margin_ceiling_pct
        if current_margin > margin_ceiling:
            logger.warning(
                "MARGIN WARNING: $%.0f above ceiling $%.0f. "
                "Constraint checker blocking margin-increasing orders.",
                current_margin, margin_ceiling,
            )

    def check_daily_pnl_only(self) -> bool:
        """Lightweight halt check callable from the fill path without
        doing a full risk sweep. Used by FillHandler per v1.4 §6.1
        "OR on every fill (immediate update)" so the halt fires between
        5-minute risk checks.

        Returns True if the halt fired (caller may want to short-circuit).
        """
        if self.killed or self.daily_halt_threshold is None:
            return False
        self.portfolio.daily_pnl = (
            self.portfolio.realized_pnl_persisted
            + self.portfolio.compute_mtm_pnl()
            + self._hedge_mtm_usd()
        )
        if self.portfolio.daily_pnl < self.daily_halt_threshold:
            capital = self.config.constraints.capital
            self.kill(
                f"DAILY P&L HALT (fill-path): ${self.portfolio.daily_pnl:,.0f} < "
                f"${self.daily_halt_threshold:,.0f} "
                f"(-{abs(self.daily_halt_threshold/capital)*100:.1f}% cap)",
                source="daily_halt", kill_type="flatten",
            )
            return True
        return False

    def _hedge_mtm_usd(self) -> float:
        """Sum the hedge-manager fanout's MTM, or 0 if no hedge is wired.
        v1.4 §6.1 includes hedge MTM in the daily P&L halt calc.

        Exception-safe: if MTM computation breaks (e.g., stale product
        registry, NaN underlying) we return 0 rather than propagating —
        the halt must keep running. But we log once per session with
        exc_info so a silent under-reading doesn't go unnoticed.
        """
        hm = self.hedge_manager
        if hm is None or not hasattr(hm, "mtm_usd"):
            return 0.0
        try:
            return float(hm.mtm_usd())
        except Exception:
            if not getattr(self, "_hedge_mtm_err_logged", False):
                logger.warning(
                    "hedge mtm_usd() raised; daily P&L halt will use 0 "
                    "for hedge MTM until recovered. Under-reports true "
                    "exposure.",
                    exc_info=True,
                )
                self._hedge_mtm_err_logged = True
            return 0.0

    def kill(self, reason: str, source: str = "risk",
             kill_type: str = "halt"):
        """Emergency shutdown.

        Args:
            reason: human-readable message for logs + operator.
            source: governs auto-clear rules (see module docstring).
            kill_type: one of "halt", "flatten", "hedge_flat" — controls
                whether positions are forcibly closed. See v1.4 §6.2
                precedence table.
        """
        logger.critical(
            "KILL SWITCH ACTIVATED [%s / %s]: %s", source, kill_type, reason
        )
        # Always cancel resting quotes first. This is the minimum action
        # for every kill type and matches IBKR "safe-first" semantics:
        # even if flatten flips a failure, the book is at least quiet.
        try:
            self.quotes.cancel_all_quotes()
        except Exception as e:
            logger.error("kill: cancel_all_quotes failed: %s", e)

        if kill_type == "flatten":
            if self.flatten_callback is not None:
                try:
                    self.flatten_callback(reason)
                except Exception as e:
                    logger.error("kill: flatten_callback failed: %s", e)
            else:
                logger.warning(
                    "kill_type=flatten but no flatten_callback wired — "
                    "degrading to halt. Configure RiskMonitor(flatten_callback=...)."
                )
        elif kill_type == "hedge_flat":
            if self.hedge_manager is not None:
                try:
                    self.hedge_manager.force_flat(reason)
                except Exception as e:
                    logger.error("kill: hedge_manager.force_flat failed: %s", e)
            else:
                logger.warning(
                    "kill_type=hedge_flat but no hedge_manager wired — "
                    "degrading to halt."
                )

        # Emit paper kill_switch JSONL event with a book snapshot.
        self._log_kill_switch_event(reason, source, kill_type)

        self.killed = True
        self._kill_reason = reason
        self._kill_source = source
        self._kill_type = kill_type

    def _log_kill_switch_event(self, reason: str, source: str,
                                kill_type: str) -> None:
        """Emit a kill_switch.jsonl row with a full book snapshot per
        v1.4 §9.4 "logs trigger event with complete book state"."""
        if self.csv_logger is None:
            return
        try:
            positions_snapshot = [
                {
                    "product": p.product,
                    "strike": float(p.strike),
                    "expiry": p.expiry,
                    "cp": p.put_call,
                    "qty": int(p.quantity),
                    "avg_fill_price": float(p.avg_fill_price),
                    "current_price": float(p.current_price),
                    "delta": float(p.delta),
                    "theta": float(p.theta),
                    "vega": float(p.vega),
                }
                for p in self.portfolio.positions
            ]
            current_margin = 0.0
            try:
                current_margin = float(self.margin.get_current_margin())
            except Exception:
                pass
            # kill_type is set inside book_state BEFORE log emission so
            # reconciliation sees flatten vs halt vs hedge_flat without
            # parsing the reason string.
            book_state = {
                "kill_type": kill_type,
                "positions": positions_snapshot,
                "positions_count": len(positions_snapshot),
                "current_margin_usd": current_margin,
                "capital_usd": float(self.config.constraints.capital),
                "daily_pnl_usd": float(getattr(self.portfolio, "daily_pnl", 0.0)),
                "realized_pnl_usd": float(
                    getattr(self.portfolio, "realized_pnl_persisted", 0.0)),
                "mtm_pnl_usd": float(self.portfolio.compute_mtm_pnl()),
                "net_delta": float(self.portfolio.net_delta),
                "net_theta": float(self.portfolio.net_theta),
                "net_vega": float(self.portfolio.net_vega),
            }
            self.csv_logger.log_paper_kill_switch(
                switch_name=self._switch_name_from_reason(reason),
                reason=reason, source=source, book_state=book_state,
            )
        except Exception as e:
            logger.warning("kill switch event emit failed: %s", e)

    @staticmethod
    def _switch_name_from_reason(reason: str) -> str:
        """Extract the canonical switch name from the reason prefix.

        Reasons are constructed as "<SWITCH NAME>: <detail>" in kill().
        Strip to the first colon for a clean switch label on the log line.
        """
        if ":" in reason:
            head = reason.split(":", 1)[0].strip()
            return head
        return reason[:40]

    def _check_induced_sentinels(self) -> bool:
        """v1.4 §9.4 induced-breach test hook. Check /tmp for sentinel
        files and fire the matching kill if present. Sentinels are
        removed after firing so each touch triggers exactly one kill.

        Returns True if an induced kill fired (caller should short-
        circuit its own checks to avoid double-firing).
        """
        for switch_key, (fname, kill_type, source) in INDUCE_SENTINELS.items():
            path = os.path.join(INDUCE_SENTINEL_DIR, fname)
            if not os.path.exists(path):
                continue
            # Remove the sentinel BEFORE firing so the kill can't
            # re-trigger on the next cycle if the remove fails after
            # the fact (e.g., permissions, NFS). If remove fails, log
            # WARNING and skip the fire: operator needs the signal,
            # not a pinned kill that re-fires on daily_halt rollover.
            try:
                os.remove(path)
            except OSError as e:
                logger.warning(
                    "Induced sentinel %s present but os.remove failed (%s); "
                    "refusing to fire kill to avoid a stuck re-trigger loop. "
                    "Remove the sentinel manually before the next check.",
                    path, e,
                )
                return False
            reason = (
                f"INDUCED TEST [{switch_key.upper()}]: sentinel {path} — "
                f"exercising kill_type={kill_type} source={source}"
            )
            induced_source = f"induced_{source}"
            self.kill(reason, source=induced_source, kill_type=kill_type)
            logger.warning("INDUCED TEST fired: %s", reason)
            return True
        return False

    def clear_disconnect_kill(self) -> bool:
        """Clear a kill IFF it was caused by a disconnect. Returns True if
        cleared. Risk-induced kills remain sticky."""
        if self.killed and self._kill_source == "disconnect":
            logger.info("Clearing disconnect-induced kill: %s", self._kill_reason)
            self.killed = False
            self._kill_reason = ""
            self._kill_source = ""
            self._kill_type = ""
            return True
        return False

    def clear_daily_halt(self) -> bool:
        """v1.4 §6.1: automatic resume at next session start. Called from
        main.py at CME session rollover (17:00 CT).

        Also clears induced_daily_halt so the induced test exercises the
        full auto-clear path (Gate 0 §9.4 verifies resume-on-rollover).
        Non-halt induced sources (induced_risk) remain sticky so the
        operator must restart to validate post-test state.

        Returns True if the kill was cleared. Does nothing for sticky
        sources (manual review required)."""
        if self.killed and self._kill_source in ("daily_halt",
                                                  "induced_daily_halt"):
            logger.warning(
                "Clearing daily P&L halt at session rollover: %s",
                self._kill_reason,
            )
            self.killed = False
            self._kill_reason = ""
            self._kill_source = ""
            self._kill_type = ""
            return True
        return False

    @property
    def kill_reason(self) -> str:
        return self._kill_reason

    @property
    def kill_source(self) -> str:
        return self._kill_source

    @property
    def kill_type(self) -> str:
        return self._kill_type
