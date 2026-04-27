"""Operational kill-switch monitor for Corsair v2 (v1.4 §7).

Infrastructure-level protections that fire independently of the
strategy-level kills in RiskMonitor. These watch for failure modes that
the trading loop's health checks don't catch:

  - SABR calibration failure: RMSE > threshold sustained for N seconds
  - Quote latency breach: median place-RTT > threshold sustained for N seconds
  - Abnormal trade rate: fills/minute > multiplier × rolling-hour baseline
  - (Realized vol spike: logged but page-only per v1.4 §7; requires
    5-day historical rvol pipeline not yet wired)

IBKR disconnect and position-reconciliation failure are already wired in
main.py (on_disconnect callback) and watchdog/reconciliation code; this
module covers the remaining three.

Fires via risk.kill(reason, source="operational", kill_type="halt").
All operational kills are sticky (source="operational" is not cleared
by clear_disconnect_kill or clear_daily_halt) — they indicate a genuine
infrastructure problem that needs human review before re-quoting.

**Sustained-breach pattern**: each signal tracks ``first_breach_ts`` —
the monotonic timestamp when the current breach streak began. Reset to
None on first good sample. Fires when ``now - first_breach_ts >
window_sec``. This replaces an earlier "every-sample-in-window breaches"
check that relied on sample alignment against the window edge and
silently failed at moderately-slow check cadences.
"""

import logging
import time

logger = logging.getLogger(__name__)


class OperationalKillMonitor:
    """Watches infrastructure health signals and trips RiskMonitor on
    sustained breaches.

    Construct ONE instance per corsair process (not per-product) because
    the kill signal is global — if SABR degrades on the primary, we
    halt everything.

    Inputs it pulls each cycle via public accessors:
      - engines[*]["sabr"].latest_rmse(expiry)
      - engines[*]["quotes"].get_latency_snapshot() → place_rtt_us.p50
      - portfolio.fills_today (rolling fill-rate)

    Public API:
      - check(): call from the main loop at 5-10s cadence
    """

    def __init__(self, engines: list, portfolio, risk_monitor, config):
        self.engines = engines
        self.portfolio = portfolio
        self.risk = risk_monitor
        self.config = config

        op = getattr(config, "operational_kills", None)
        self.enabled: bool = bool(getattr(op, "enabled", True))
        self.rmse_threshold: float = float(
            getattr(op, "sabr_rmse_threshold", 0.05))
        self.rmse_window_sec: float = float(
            getattr(op, "sabr_rmse_window_sec", 300.0))
        self.latency_ms_max: float = float(
            getattr(op, "quote_latency_max_ms", 2000.0))
        self.latency_window_sec: float = float(
            getattr(op, "quote_latency_window_sec", 60.0))
        self.fill_rate_mul: float = float(
            getattr(op, "abnormal_fill_rate_mul", 10.0))
        self.fill_rate_baseline_window_sec: float = float(
            getattr(op, "abnormal_fill_baseline_window_sec", 3600.0))
        # Minimum actual coverage of the baseline window before the ratio
        # check runs. Fresh-boot scenarios otherwise trip at market open
        # when a tiny baseline (built over ~49 min of pre-market quiet)
        # gets compared against a legitimate open-auction fill burst —
        # observed 2026-04-21 at 08:37 CT with 4 fills in 110ms ratio'd
        # 29× against a 0.1/min baseline. 30min coverage makes the
        # denominator meaningful.
        self.fill_rate_min_baseline_sec: float = float(
            getattr(op, "abnormal_fill_baseline_min_coverage_sec", 1800.0))
        # Floor on the effective baseline rate (fills/min). Without a
        # floor, a quiet baseline (e.g. 0.08/min from overnight) × 10×
        # multiplier gives an effective threshold (0.8/min) that a
        # perfectly normal legitimate fill cluster can breach. Observed
        # 2026-04-23 07:47 CDT: 4 fills in 20s = 12/min short rate,
        # vs 0.08/min hour baseline → 150× ratio, kill fired. Floor
        # sets the minimum effective baseline so the kill only fires
        # on genuinely abnormal burst rates, not regime transitions.
        # Bumped 0.5 → 2.5 on 2026-04-27 after a 5-fills-in-60s normal
        # burst at 15:25 UTC tripped 5/0.5 = 10× ratio, halted ~3.5h and
        # cost us the April settlement window. New floor lifts trigger
        # to ~25 fills/min absolute during quiet baselines, well above
        # any realistic non-runaway burst.
        self.fill_rate_baseline_floor_per_min: float = float(
            getattr(op, "abnormal_fill_baseline_floor_per_min", 2.5))
        # Absolute floor on the short-window fill count. Independent of
        # ratio: even a 100× ratio can't fire if fewer than this many
        # fills landed in the short window. Defends against the
        # "tiny-numbers ratio explodes" failure mode where 5 vs 0.5 is
        # mathematically 10× but operationally just normal flow.
        self.fill_rate_min_short_count: int = int(
            getattr(op, "abnormal_fill_min_short_count", 15))
        self.rvol_alert_threshold: float = float(
            getattr(op, "rvol_alert_5d_threshold", 0.50))

        # Hardburst-skip overlay kill (spec Change 3.6). Trips when the
        # daily suppression rate exceeds this fraction — indicating the
        # detector is firing constantly (regime shift or mis-calibrated
        # threshold). On trip: disables the gate for the rest of the day
        # and emits a Discord alert. Read from top-level quoting: block
        # with a floor on minimum attempts so a fresh-boot can't trip at
        # the first skip.
        q = getattr(config, "quoting", None)
        self.hardburst_kill_rate: float = float(
            getattr(q, "hardburst_suppression_kill_rate", 0.20))
        self.hardburst_kill_min_attempts: int = int(
            getattr(q, "hardburst_suppression_kill_min_attempts", 100))

        # Sustained-breach tracking. Per-signal monotonic ts of when the
        # current breach streak began; None means no active breach.
        # RMSE keyed by (engine_name, expiry) — different expiries
        # calibrate independently.
        self._rmse_first_breach_ts: dict = {}
        self._latency_first_breach_ts: float | None = None

        # Fill-count snapshots: (ts, fills_today). The short-term rate
        # vs rolling-hour baseline comparison uses a sliding window here
        # (not a breach-streak), so we keep the sample history.
        self._fill_hist: list = []

    def check(self) -> None:
        """Evaluate all operational kill switches. No-op if already
        killed. Fires risk.kill() on sustained breach."""
        if not self.enabled or self.risk.killed:
            return

        now = time.monotonic()

        self._check_sabr_rmse(now)
        if self.risk.killed:
            return

        self._check_quote_latency(now)
        if self.risk.killed:
            return

        self._check_fill_rate(now)

        # Hardburst-suppression-rate kill runs regardless of other kill
        # state — it's a FEATURE-DISABLE, not a full halt. Safe to evaluate
        # even after other kills have fired.
        self._check_hardburst_suppression()

    # ── Per-switch checks ─────────────────────────────────────────────
    def _check_sabr_rmse(self, now: float) -> None:
        """Front-month RMSE > threshold continuously for ``window_sec`` ⇒
        kill. Streak is tracked via ``_rmse_first_breach_ts[key]``; a
        single sample at-or-below threshold resets the streak to None.
        """
        for eng in self.engines:
            sabr = eng.get("sabr")
            md = eng.get("md")
            if sabr is None or md is None:
                continue
            if not hasattr(sabr, "latest_rmse"):
                # SABR version predates the public API; skip rather than
                # silently disarm. Log once so the operator sees it.
                if not getattr(self, "_sabr_api_warned", False):
                    logger.warning(
                        "operational_kills: engine %s SABR has no "
                        "latest_rmse() — RMSE kill disabled for this engine.",
                        eng.get("name", "?"),
                    )
                    self._sabr_api_warned = True
                continue

            expiries = getattr(md.state, "expiries", []) or []
            if not expiries:
                continue
            front = expiries[0]

            rmse = sabr.latest_rmse(front)
            if rmse is None:
                continue  # no fit has landed yet

            key = (eng["name"], front)
            if rmse > self.rmse_threshold:
                first = self._rmse_first_breach_ts.get(key)
                if first is None:
                    self._rmse_first_breach_ts[key] = now
                elif (now - first) > self.rmse_window_sec:
                    self.risk.kill(
                        f"SABR RMSE SUSTAINED BREACH [{eng['name']}/{front}]: "
                        f"rmse={rmse:.4f} > {self.rmse_threshold:.3f} for "
                        f"{now - first:.0f}s (window {self.rmse_window_sec:.0f}s)",
                        source="operational", kill_type="halt",
                    )
                    return
            else:
                self._rmse_first_breach_ts[key] = None

    def _check_quote_latency(self, now: float) -> None:
        """Median place-RTT > threshold sustained ⇒ kill.

        Samples the primary engine's latency snapshot. Latency above 2s
        indicates a wire-protocol or gateway problem (not a strategy
        issue), so we halt quoting and page operator.
        """
        if not self.engines:
            return
        primary = self.engines[0]
        quotes = primary.get("quotes")
        if quotes is None or not hasattr(quotes, "get_latency_snapshot"):
            return

        snap = quotes.get_latency_snapshot() or {}
        p50_us = (snap.get("place_rtt_us") or {}).get("p50")
        if p50_us is None:
            return  # no samples yet
        median_ms = float(p50_us) / 1000.0

        if median_ms > self.latency_ms_max:
            if self._latency_first_breach_ts is None:
                self._latency_first_breach_ts = now
            elif (now - self._latency_first_breach_ts) > self.latency_window_sec:
                self.risk.kill(
                    f"QUOTE LATENCY BREACH: place-RTT p50={median_ms:.0f}ms "
                    f"> {self.latency_ms_max:.0f}ms for "
                    f"{now - self._latency_first_breach_ts:.0f}s "
                    f"(window {self.latency_window_sec:.0f}s)",
                    source="operational", kill_type="halt",
                )
        else:
            self._latency_first_breach_ts = None

    def _check_fill_rate(self, now: float) -> None:
        """Fills/minute > multiplier × baseline ⇒ kill.

        Baseline = fill-rate across the past ``baseline_window_sec``
        (default 1h). Short-term rate (last ~60s) must exceed the
        baseline by ``rate_mul`` (default 10×).

        Early-session handling: if baseline window has <2 snapshots or
        <5 total fills, skip — the ratio is meaningless when numbers
        are small.
        """
        fills_today = int(getattr(self.portfolio, "fills_today", 0))
        self._fill_hist.append((now, fills_today))
        # Evict samples older than the baseline window + buffer.
        cutoff = now - (self.fill_rate_baseline_window_sec + 60)
        while self._fill_hist and self._fill_hist[0][0] < cutoff:
            self._fill_hist.pop(0)

        if len(self._fill_hist) < 3:
            return

        # Short window: last 60s.
        short_cutoff = now - 60.0
        short_samples = [s for s in self._fill_hist if s[0] >= short_cutoff]
        if len(short_samples) < 2:
            return
        short_fills = max(0, short_samples[-1][1] - short_samples[0][1])
        short_span = max(1.0, short_samples[-1][0] - short_samples[0][0])
        short_rate_per_min = (short_fills / short_span) * 60.0

        # Baseline: past `baseline_window_sec`.
        long_cutoff = now - self.fill_rate_baseline_window_sec
        long_samples = [s for s in self._fill_hist if s[0] >= long_cutoff]
        if len(long_samples) < 2:
            return
        long_fills = max(0, long_samples[-1][1] - long_samples[0][1])
        if long_fills < 5:
            return  # not enough fills to trust the ratio
        long_span = max(1.0, long_samples[-1][0] - long_samples[0][0])
        if long_span < self.fill_rate_min_baseline_sec:
            return  # baseline window not yet populated enough to trust
        long_rate_per_min = (long_fills / long_span) * 60.0
        effective_baseline = max(long_rate_per_min, self.fill_rate_baseline_floor_per_min)
        if effective_baseline <= 0:
            return

        ratio = short_rate_per_min / effective_baseline
        if ratio > self.fill_rate_mul and short_fills >= self.fill_rate_min_short_count:
            floored = effective_baseline > long_rate_per_min
            floor_note = f" [floored from {long_rate_per_min:.2f}]" if floored else ""
            self.risk.kill(
                f"ABNORMAL FILL RATE: {short_rate_per_min:.1f}/min (short, "
                f"{short_fills} fills in window) vs {effective_baseline:.2f}/min "
                f"(hour baseline{floor_note}) — ratio {ratio:.1f}× > "
                f"{self.fill_rate_mul:.0f}×",
                source="operational", kill_type="halt",
            )

    def _check_hardburst_suppression(self) -> None:
        """Hardburst overlay runaway-suppression guard (spec Change 3.6).

        Disables the hardburst_skip gate for the rest of the session when
        the daily suppression rate exceeds ``hardburst_kill_rate`` with
        at least ``hardburst_kill_min_attempts`` samples. This is a
        FEATURE-DISABLE, not a risk.kill() — the rest of the trading
        path is still healthy.

        Idempotent: QuoteManager.disable_hardburst_skip_for_session is a
        no-op after first call, so repeated `check()` invocations post-
        trip don't flood the Discord channel."""
        for eng in self.engines:
            quotes = eng.get("quotes")
            if quotes is None:
                continue
            # Already killed for this session? Nothing to do.
            if getattr(quotes, "_hardburst_skip_disabled_by_kill", False):
                continue
            attempts = getattr(quotes, "_hardburst_update_attempts", 0)
            if attempts < self.hardburst_kill_min_attempts:
                continue
            rate = quotes.get_hardburst_suppression_rate()
            if rate <= self.hardburst_kill_rate:
                continue
            # Trip
            reason = (f"suppression_rate={rate:.3f} "
                      f"> kill_rate={self.hardburst_kill_rate:.2f} "
                      f"(attempts={attempts}, "
                      f"skips={quotes._hardburst_skip_count}, "
                      f"engine={eng.get('name', '?')})")
            quotes.disable_hardburst_skip_for_session(reason)
            try:
                from .discord_notify import send_alert
                send_alert(
                    "HARDBURST OVERLAY AUTO-DISABLED",
                    f"Runaway suppression — {reason}. "
                    f"Gate OFF until session rollover. "
                    f"Review detector threshold / window vs current regime.",
                    color=0xF39C12,  # orange — feature kill, not full halt
                )
            except Exception as e:
                logger.debug("hardburst kill discord alert failed: %s", e)
