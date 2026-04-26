# Operator handoff — Thread 3 deployment runbook

**Date:** 2026-04-26
**Audience:** Corsair-side Claude / corsair operator
**Status:** Code shipped on Alabaster commit `353925e` (2026-04-26 09:46 CDT). All flags default OFF. **Implementation complete; deployment is the remaining work.**

## References (read for context, do not re-derive)

- **Spec:** `crowsnest/docs/briefs/corsair_quote_staleness_burst_mitigation_2026-04-25.md` — Thread 3 brief. WHY, WHAT, per-pattern bite matrix, kill criteria. **This document does NOT repeat the brief; it sequences the deployment.**
- **Audit:** `crowsnest/docs/findings/corsair_v1_4_spec_vs_code_audit_2026-04-26.md` — flagged two compounding risks Thread 3 alone does not fix: (1) hedge mode observe leaves IBKR un-hedged, (2) margin/daily-halt is HALT not FLATTEN. Both interact with Thread 3.
- **Validation memos backing the +5.35 central Sharpe this deployment is designed to defend:** `defensible_sharpe_synthesis_2026-04-23.md`, `sharpe_inputs_validation_2026-04-26.md`, `walk_forward_validation_2026-04-26.md`.
- **Penny-jump-is-load-bearing finding** (don't accidentally undo): `tickjump_vs_passive_2026-04-26.md` — penny-jumping is +2.90 Sharpe over passive. Thread 3 gates penny-jump on burst conditions; misconfigured Layer B would erode this lift.

## Sequencing (gated, sequential)

```
  Phase 0  [REQUIRED]    hedge_manager first-tradeable-contract fix
  Phase 1  [REQUIRED]    Stage 0 hedge-drain synthetic burst test
  Phase 2  [REQUIRED]    §6 instrumentation baseline (≥10 days, all flags OFF)
  Phase 3  [REQUIRED]    §7 augmented baseline window (≥1 of each pattern)
  Phase 4  [Stage-3 gate] SABR parallelism fix
  Phase 5  Stage 1 enable: LEVER_C2_ANY_SIDE_ENABLED only
  Phase 6  Stage 2 enable: + LEVER_C1_SAME_SIDE_ENABLED
  Phase 7  Stage 3 enable: + LEVER_A_F_TICK_REFIT_ENABLED  (after Phase 4)
  Phase 8  Stage 4 enable: + LEVER_B_CANCEL_ON_DF_ENABLED  (after M_ticks calibration)
```

**Each phase has a deterministic gate to advance.** No phase is "judgment based" — if the gate doesn't pass mechanically, do not advance. If a rollback trigger fires at any phase, regress to the prior phase or fully off.

---

## Phase 0 — hedge_manager first-tradeable-contract fix (REQUIRED prerequisite)

**Why:** Audit §3.2 / §4.1. Currently `hedge_manager` resolves "front-month" to the next-expiring HG futures contract; during the ~1-2 weeks before expiry, IBKR's near-expiration physical-delivery lockout rejects retail orders on that contract. This forced hedge_mode reverted from execute → observe on 2026-04-22. **Until this is fixed, Thread 3's hedge-drain priority signal logs queued intent fast but doesn't move IBKR exposure** — leaving the audit §4.1 compound-risk failure mode live.

**Change:** in `src/hedge_manager.py` contract resolution, skip the front-month if it's within `near_expiry_lockout_days` (set to 7 as v1 default — calibrate against IBKR's actual lockout window, which empirically blocked HGJ6 ~6 days from expiry on 2026-04-22). Pick the first contract whose expiry is >= today + lockout_days.

**Gate to advance:**
- After deploy, set `hedging.mode: "execute"` in yaml.
- Run for ≥1 trading session.
- Confirm: at least one `hedge_trade` row in `logs-paper/hedge_trades-*.jsonl` has `mode: "execute"` (not "observe") and IBKR-side execDetailsEvent confirms the trade landed.
- Confirm: no Error 201 rejections in the same window.

**If gate fails:** revert hedging.mode to observe, file a fix iteration. Do NOT proceed to Phase 1 until execute mode is reliably firing.

---

## Phase 1 — Stage 0 hedge-drain synthetic burst test

**Why:** Brief §3 declares the hedge-drain priority signal a HARD requirement. The implementation is in commit 353925e but has been verified at unit-test level only (`tests/test_thread3.py`). This phase verifies it under live integration.

**Test runbook** (write as `scripts/test_thread3_stage0.py`):

1. With master flag OFF and all layer flags OFF, start corsair in paper mode.
2. Inject 5 synthetic fills via a sentinel mechanism analogous to `scripts/induce_kill_switch.py`:
   - Sentinel file: `/tmp/corsair_inject_burst` containing a JSON array of 5 fill records spaced ≤500 ms apart, mixed sides/strikes.
   - `quote_engine.update_quotes()` checks for the sentinel at the top of each cycle, if present injects the records as fills via the existing `fill_handler`.
3. **Even with master flag OFF**, the FillBurstTracker still updates its rolling window (per the `fills.burst_1s_at_fill` instrumentation field which emits regardless of flag state). Verify it counts 5 fills in <1s.
4. Flip `thread3.master_enabled: true` and `lever_c2_any_side_enabled: true` (only).
5. Repeat the injection.
6. Verify in logs:
   - `burst_events-*.jsonl` emits a row with `trigger: "C2"`, `K2_count: 3` (or higher) at the timestamp of the 3rd injected fill.
   - Hedge engine's queue-depth observable (add `queue_depth` field to `hedge_trades` rows OR a new `hedge_drain-*.jsonl` stream emitted on flag-raise) shows the queue draining within ≤500 ms of the C2 trigger.
   - `burst_pull` event has `cooldown_until` 3.0 s past the trigger.
7. Verify: during the 3.0 s cooldown, no new quotes are placed (`quotes-*.jsonl` if/when stood up, or proxy via `quote_engine.update_quotes()` returning early).

**Pass criteria** (all required):
- 5/5 injected fills emit `burst_1s_at_fill` correctly.
- C2 trigger fires at the 3rd-of-5 injected fill (i.e., when `burst_1s` count reaches 3).
- Hedge engine drains its queue (length goes from N to 0) within ≤500 ms of the trigger.
- Hedge throughput during drain ≥3 hedges/sec (i.e., a queue of 3+ items clears in ≤1 sec).
- Cooldown clears the priority flag only when (queue empty AND cooldown expired).

**Gate to advance:** all 5 pass criteria green.

**If gate fails:** file a corsair fix; do not proceed.

---

## Phase 2 — §6 instrumentation baseline (≥10 trading days, all flags OFF)

**Why:** Brief §6 introduces three new streams (`order_lifecycle-*.jsonl`, `burst_events-*.jsonl`, `fills.burst_1s_at_fill`). Confirm they're emitting under flag-OFF baseline before any flag flip.

**Procedure:**
1. After Phase 1 passes, run corsair as normal with `thread3.master_enabled: false`.
2. Daily check: each of the three streams has ≥1 row written for the day. Stream paths:
   - `logs-paper/order_lifecycle-YYYY-MM-DD.jsonl`
   - `logs-paper/burst_events-YYYY-MM-DD.jsonl` (will be empty most days — only emits on flag-on triggers; under flag-OFF, this stream may stay empty all baseline. That's fine. Confirm the FILE EXISTS at session-end.)
   - `fills-YYYY-MM-DD.jsonl` rows include the `burst_1s_at_fill` field (≥1 fill row per fill day).

**Gate to advance:**
- ≥10 calendar days of baseline collected.
- All three streams exist for ≥80% of trading sessions (some may be empty if no fills that day; the file emission itself is the check).
- `order_lifecycle` shows expected per-order placement+cancel cycles (each fill should have 1 placement and 1 cancel/fill row at minimum).

---

## Phase 3 — §7 augmented baseline window

**Why:** Brief §7 baseline rule: feasibility window (2026-04-19 → 24, already captured in `~/feasibility/data/parsed/`) PLUS new instrumented data, until ≥1 of each pattern is observed in the new instrumented portion.

**Procedure:**
1. From Phase 2 baseline, look for cluster events using the new `burst_events` stream (which records C1/C2 triggers, but with master flag OFF the triggers fire as INFORMATIONAL only — they don't take cancel actions, but they note when the conditions WOULD fire).

Wait — re-check: brief §6.3 says `burst_events` "fires on Layer C burst-pull fires." With flags OFF, no fires happen, so no rows emit. This is a problem for Phase 3 detection.

**Instrumentation correction needed:** the FillBurstTracker should emit observational rows to `burst_events-*.jsonl` whenever the C1/C2 thresholds are CROSSED, regardless of master flag state. The flag should gate the ACTION (cancel quotes), not the OBSERVATION (log the event). Patch `quote_engine.py` to log the event on threshold cross even with master OFF; then the flag gates whether the cancel + cooldown actually execute.

**Gate to advance:**
- ≥1 P1-shaped event observed (burst with `dF_since_sabr` ≥ 5 ticks AND ≥3 same-side fills in 1s).
- ≥1 P2-shaped event observed (burst with ≥3 any-side fills in 1s, mixed BUY/SELL across strikes).
- If after 30 days neither pattern has observed, document the absence and proceed with whichever pattern data IS available + the feasibility-window evidence for the missing pattern. (Brief §7's fallback rule.)

---

## Phase 4 — SABR parallelism fix (Stage-3 gate only)

**Why:** Audit §3.1. Layer A's F-tick refit shares the same calibration-collision bottleneck as the existing `sabr_fast_recal_dollars` path. With the current single-in-flight calibration architecture, Layer A's threshold trigger is throttled to the elapsed-time path (60s observed median on 04-20). Without parallelism, Layer A is theater — the trigger fires but the calibration doesn't actually run faster.

**Change:** in `src/sabr.py`, replace the single-in-flight `calibrate_async` queue with a per-(expiry, side) parallel queue. A forward-tick refit on HXEJ6/C should not block on an unrelated HXEK6/P calibration. Use `concurrent.futures.ProcessPoolExecutor(max_workers=6)` (one slot per surface) or equivalent.

**Gate to advance (only required before Phase 7 / Stage 3):**
- After deploy, observe `sabr_fits-*.jsonl` cadence for HXEJ6/C specifically. Median Δt between refits should drop from 60s observed → ≤30s (matching spec §4) on volatile sessions.
- Verify: a `trigger_reason: "F_tick"` row appears at least once when |F_now − F_at_last_fit| > 5 HG ticks ($0.0025) — confirms Layer A path can fire.

**Safe to skip if:** Stages 1+2 (C2 + C1) provide enough lift and Layer A is deferred. Stage 3 specifically depends on this.

---

## Phase 5 — Stage 1 enable: C2 only

**Why:** C2 is the load-bearing P2 lever per brief §5 (the only lever that bites the 04-22-style aggressor sweep). Its hedge-drain coupling also addresses the 04-20 mechanism partially. Validate first in isolation.

**Procedure:**
1. Set `thread3.master_enabled: true` and `thread3.lever_c2_any_side_enabled: true`. Leave C1, A, B all OFF.
2. Run for ≥10 trading days.
3. At day 10, run the §8 evaluation harness (spec below).

**Gate to advance to Phase 6:**
- §8 four criteria all PASS (negative-Phase-1 dollars reduced ≥50%, tier-1 margin trips reduced ≥50%, halt frequency NOT increased, burst_1s rate NOT increased).
- No rollback trigger fired during the 10 days.

**If gate is AMBIGUOUS** (point estimate shows reduction but bootstrap CI crosses zero on any criterion): per kill memo, **extend to 30 days, do NOT renegotiate criteria**. Same for Stages 2-4.

**If any rollback trigger fires** during the 10 days: revert master_enabled → false, regress to Phase 4 baseline. Investigate before re-attempting.

---

## Phase 6 — Stage 2 enable: + C1

Procedure mirrors Phase 5: add `lever_c1_same_side_enabled: true`. ≥10 days. Run §8 evaluation. Same gate. Same rollback rule.

---

## Phase 7 — Stage 3 enable: + Layer A (after Phase 4)

**Prerequisite:** Phase 4 SABR parallelism fix landed and verified (refit cadence ≤30s on volatile sessions).

Procedure mirrors Phase 5: add `lever_a_f_tick_refit_enabled: true`. ≥10 days. Run §8 evaluation.

**Additional Stage-3 specific check:** verify `sabr_fits.trigger_reason: "F_tick"` rows exist in the post-deploy data — confirms Layer A is firing. If trigger_reason is always "elapsed", Layer A is silent and Phase 4's fix didn't take.

---

## Phase 8 — Stage 4 enable: + Layer B (after M_ticks calibration)

**Prerequisite:** M_ticks calibration completed (procedure below). Calibrated value must produce expected cancel rate ≤30% of placements on the §7 baseline data — brief's pre-deploy guardrail.

Procedure mirrors Phase 5: add `lever_b_cancel_on_df_enabled: true`. ≥10 days. Run §8 evaluation.

---

## §8 evaluation harness specification

**Build as:** `crowsnest/scripts/thread3_post_deployment_eval.py`. Operator runs it at end of each stage's evaluation period.

**Inputs:**
- Pre-deployment baseline window: 5-day feasibility window (`~/feasibility/data/parsed/`) PLUS Phase 2/3 instrumented baseline (≥10 days flag-OFF).
- Post-deployment window: stage-specific (≥10 days with stage flags ON).

**Computed metrics (with bootstrap 95% CI, 5000 resamples):**

```
M1 = total $ amount of negative-Phase-1 fills per day
     (sum over fills with signed_edge_dollars < 0, /sessions)
M2 = tier-1 margin trip count per session
     (count from kill_switch-*.jsonl where source contains "MARGIN", /sessions)
M3 = daily P&L halt count per session
     (count from kill_switch-*.jsonl where source="daily_halt", /sessions)
M4 = burst_1s ≥ 2 fill rate per session
     (count of fills with burst_1s_at_fill >= 2, /sessions)
```

**Pass/Fail logic per criterion:**

```
C1: M1_post ≤ 0.5 × M1_pre  (≥50% reduction in negative-Phase-1 $)
    PASS if (M1_pre - M1_post) bootstrap 95% CI lower bound > 0.5 × M1_pre
    AMBIGUOUS if CI overlaps the 0.5× threshold
    FAIL if upper bound < 0.5 × M1_pre

C2: M2_post ≤ 0.5 × M2_pre  (≥50% reduction in margin trips)
    Same bootstrap rule.

C3: M3_post ≤ M3_pre  (NOT increased)
    PASS if upper bound of (M3_post - M3_pre) ≤ 0
    FAIL otherwise.

C4: M4_post ≤ M4_pre  (burst rate NOT increased — adversarial guard)
    Same as C3.
```

**Overall verdict:**
- **PASS:** all four criteria PASS individually.
- **AMBIGUOUS:** any criterion AMBIGUOUS, none FAIL → extend window to 30 days additional, re-run. Per kill memo: "do NOT renegotiate criteria. Keep the 50% target."
- **FAIL:** any criterion FAIL → kill that stage, regress to prior stage.

**60-day extension rule (frozen, do not modify):** if 30-day extension still produces AMBIGUOUS, kill the thread on that criterion. If 60-day reverses to FAIL direction, kill immediately.

---

## M_ticks calibration procedure (Phase 8 prerequisite)

**Build as:** `crowsnest/scripts/calibrate_layer_b_m_ticks.py`. Run on Phase 2/3 baseline data before enabling Layer B.

**Procedure:**

1. Load `order_lifecycle-*.jsonl` from baseline period.
2. Per resting order, compute `|F_now − F_at_placement|` at the order's cancel/fill timestamp (whichever came first). Distribution measured in HG ticks ($0.0005).
3. Per fill specifically: also compute the order's `signed_edge_dollars` outcome (positive = profitable, negative = paid through theo).
4. Build the empirical CDF of (|ΔF| since placement) at fill events, separately for negative-edge (toxic) and positive-edge (profitable) fills.
5. Find the M* value where:
   - For toxic fills (signed_edge < 0): ≥80% have |ΔF| ≥ M* (i.e., setting the threshold here cancels most toxic fills before they fill).
   - For profitable fills (signed_edge > 0): ≤20% have |ΔF| ≥ M* (i.e., we don't cancel many profitable fills).
6. Output the recommended `layer_b_m_ticks` = M*.

**Guardrail check:**
- Apply M* to all baseline orders. Compute hypothetical cancel rate: (orders with |ΔF| ≥ M* before cancel/fill) / (total placements).
- If cancel rate > 30% of placements → **DO NOT ENABLE LAYER B**. Layer B at this M_ticks is approaching blanket-passive territory; per `tickjump_vs_passive_2026-04-26.md`, blanket-passive loses 2.9 Sharpe.
- If cancel rate ≤ 30%, set `thread3.layer_b_m_ticks: M*` in yaml and proceed to Phase 8.

**Default starting value (v1):** M_ticks = 3 (per brief §3). Treat empirical M* as the calibrated replacement only if ≥30 days of baseline data are available; otherwise stay at 3.

---

## Rollback trigger monitoring

**Build as:** `corsair/scripts/thread3_rollback_monitor.py`, run on a daily cron (after session-rollover at 17:00 CT).

**Triggers (from brief §9, encoded mechanically):**

```
T1: rolling 5-day daily-realized-P&L mean is worse than baseline mean by >2σ
    Source: pnl_snapshots-*.jsonl daily totals
    σ from §7 augmented baseline (pooled feasibility + new instrumented)
    Skip this trigger until pooled baseline has ≥15 days
T2: tier-1 margin trip count in any rolling 5-day post-deploy window > 5-day baseline count
    Source: kill_switch-*.jsonl filtered to source*="MARGIN"
T3: aggregate fill count in rolling 5-day window < 0.6 × baseline rolling 5-day fill count
    Source: fills-*.jsonl
T4: ≥3 distinct burst_pull events within any 30-second window
    Source: burst_events-*.jsonl, time-windowed
    OR: ≥1 fill landing within 100 ms of cooldown expiry on the side just unlocked
T5: SABR refit rate increases by >20× baseline (CPU runaway)
    Source: sabr_fits-*.jsonl, count per minute vs baseline
    OR: refit RMSE increases by >50% (refit triggering on noise)
T6: order cancel rate exceeds 90% of placements in any rolling-day window
    Source: order_lifecycle-*.jsonl, cancel:placement ratio per day
```

**On any trigger fire:**
1. Log a `rollback_triggered-YYYY-MM-DD.jsonl` row with the specific trigger ID and supporting data.
2. Set master flag → false in yaml.
3. `docker compose up -d --build corsair` (per CLAUDE.md §0 — restart alone doesn't pick up code; rebuild needed for config changes if rebuild path is required, otherwise restart suffices for yaml-only).
4. Page operator (Discord per `discord_notify.py`).
5. Write a follow-up finding doc explaining what happened.

---

## Open questions still pending operator answer

These were §11 of the original brief; some are still open:

1. **Order lifecycle stream choice** — the commit shipped `order_lifecycle-*.jsonl` as a new stream. Confirm this is the intended path vs augmenting `requote_cycles`.
2. **`quotes-*.jsonl` capture status** — separate task #9 in the crowsnest queue. The 30-day post-deploy clock for §8 starts in earnest only when this is live. **If quotes-*.jsonl never gets stood up, the §8 evaluation uses `order_lifecycle` + `fills` as proxies — workable but coarser.**
3. **`F_at_placement` field availability** — verify this is populated on all orders in `order_lifecycle-*.jsonl`. Without it, Layer B can't fire correctly. Phase 2 baseline check must confirm field non-null on ≥99% of placements.
4. **Cooldown clock source** — monotonic system clock per spec, not IBKR timestamps (avoids clock skew). Confirm the implementation uses `time.monotonic()`, not `datetime.now()`.
5. **Hedge-engine concurrency posture** — Phase 0 fix changes which contract is hedged but not the concurrency model. The hedge-drain priority signal interrupts the cadence sleep (per brief). Verify the implementation does not require a queue-rewrite.

---

## Pass-to-ship-live criteria (after all phases complete)

Thread 3 is considered DEPLOYED and stable when:
- All 4 stages (C2, C1, A, B) are flag-ON.
- 30-day post-Stage-4 §8 evaluation passes all four criteria.
- No rollback triggers fired in any 5-day rolling window during the 4 stage evaluations.
- M_ticks calibration's cancel-rate guardrail held at ≤30% throughout Stage 4.

Total calendar timeline: **6-10 weeks** depending on how aggressively phases are sequenced and whether Phase 4 SABR fix lands quickly.

---

## What this handoff does NOT cover

- The corsair v1.4 audit's other open items (delta_kill scaling, kill-flatten-vs-halt revisit, vega_kill re-enable). Those are independent of Thread 3 and have their own decision paths.
- ETH product re-enable. Tabled per CLAUDE.md.
- Strike scope revert to sym_5. Operator's call independent of Thread 3; FIX/colo migration is the prerequisite per CLAUDE.md §12.

## Final note

**The deployment process IS the validation process.** Every gate is mechanical; every criterion is pre-committed. Resist the temptation to soften thresholds at evaluation time — the kill memo's "renegotiating criteria mid-evaluation is the failure mode we are guarding against" applies throughout. Default to extending the window over softening the threshold; default to regressing a stage over relaxing the rollback triggers.
