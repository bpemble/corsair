# HG Front-Only Market-Making: v1.4 Full Specification & Corsair Handoff

Version: 1.4
Date: 2026-04-19
Author: Crowsnest research
Status: **APPROVED for Stage 0 execution.** Build-ready handoff.
Supersedes: v1.3 (`hg_front_only_full_spec.md`) for strategy, scope,
            capital, and defensive levers. v1.3's infrastructure
            sections (§§6-7 kill switch framework, §17 fill log
            schema, §22 artifact index) are structurally preserved
            and carried forward below.
Research basis: 6-panel OOS validation (2023-2024) + 1 OOS panel
            (Feb-Mar 2026). See §25 Artifact Index.

---

## 0. How to use this document

This spec is the authoritative handoff from Crowsnest research to
Corsair execution for the HG copper options market-making product.
It is build-ready: Corsair team should be able to implement Stage 0
infrastructure and Stage 1 paper deployment from this document alone,
without additional research-side clarification.

**Scope of ownership:**
- **Crowsnest owns**: strategy specification, economic thesis,
  defensive lever parameterization, kill switch thresholds,
  success/failure criteria, reconciliation pipeline on the research
  side.
- **Corsair owns**: IBKR integration, live SABR fitting against
  market data, order management state machine, kill switch
  implementation, paper/live execution, fill log generation.
- **Interface**: paper-trading fill log schema (§17), reconciliation
  ingestion path (§18).

**Document layout:**
- §§1-5: product, strategy, scope specification
- §6: defensive lever stack (PRIMARY SAFETY LAYER)
- §7: operational kill switches
- §8: capital structure
- §9: Gate 0 infrastructure checklist
- §§10-13: Stage 1 → Stage 2 deployment plan
- §§14-16: monitoring & reconciliation
- §§17-19: interfaces & schemas
- §§20-22: operational runbook
- §23: economic expectations & realistic haircuts
- §24: known limitations & open questions
- §25: artifact index & change log

---

## 1. Product

**Market**: CME COMEX HG (copper) options, financially-settled
European-style (symbol family HXE*).

**Underlying**: HG copper futures, 25,000 lbs per contract, priced in
USD/lb at $0.0005 tick. Contract months: F (Jan), G (Feb), H (Mar),
J (Apr), K (May), M (Jun), N (Jul), Q (Aug), U (Sep), V (Oct), X
(Nov), Z (Dec).

**Options**: HXE series. Expire approximately 4 business days before
the corresponding futures contract's last trading day. Financial
settlement at close.

**Contract multiplier**: 25,000 lbs (options inherit futures
multiplier).

**Tick size**: $0.0005/lb.

**Strike grid**: integer cents quoted as 4-digit prices in Databento
raw_symbol (e.g. "HXEK6 C485" = HG K6 call @ $4.85/lb).

**Active front-month at Stage 1 launch (target 2026-05)**: HXEK6
(May 2026 futures, options expire ~2026-04-27 per CME reference
dates). Verify the active front-month via CME calendar at launch
time.

---

## 2. Strategy summary

**Market-make both sides (bid + ask)** on a fixed set of nickel
strikes around ATM on the front-month HXE contract, continuously
re-centering ATM on the underlying HG futures forward. Short-vol
MM book structure: collect theta, hedge delta via HG futures,
accept limited vega/gamma exposure, defend tail with a daily P&L
halt at −5% of deployed capital.

**Not doing:**
- Directional trading
- Back-month quoting (DTE > 35)
- Wings-only quoting (no ATM)
- Asymmetric OTM scope (v1.3's original structure, superseded)
- Trend-filter-based quote gating
- Per-strike position caps beyond the SPAN-implicit limits

---

## 3. Scope

### 3.1 Expiry

**Front-month only.** DTE ≤ 35 days at quote time.

At any point in time, the active front-month is the nearest
non-expired monthly HXE contract. Roll happens automatically as
contracts expire (no manual intervention required).

**Do not quote** front+1 (DTE 35-65), front+2 (DTE 65-100), or
further. Evidence: Phase 4 U2 showed back-month combined-book
Sharpe deeply negative; exclusion is definitive. Re-evaluation
only after Stage 2 produces ≥6 months of live evidence that
changes the research picture.

### 3.2 Strike scope — Symmetric ATM ± 5 nickels (sym_5)

**Target**: 11 unique nickel strikes centered on ATM, quoted both
calls and puts at each strike (12 total instruments, ATM shared).

Grid at current forward F (e.g., F = $4.82):
- ATM nickel: round(F / 0.05) × 0.05 = $4.80
- Strikes quoted: $4.55, $4.60, $4.65, $4.70, $4.75, $4.80,
  $4.85, $4.90, $4.95, $5.00, $5.05
- Each strike quoted as both C and P (= 22 instruments total,
  where call + put at same strike are separate order books;
  book key is (strike, cp))

**Max resting orders at any moment**: 22 instruments × 2 sides
(bid, ask) = **44 resting orders**.

**Nickel filter on**: only strikes at exact $0.05 multiples.
Non-nickel strikes (penny-grid: $4.53, $4.57, etc.) are not
quoted. Rationale: 82.3% of HG options volume concentrates on
nickels; non-nickels are structurally thinner and push per-strike
TAM penetration into adversity zone.

**ATM re-centering**: recompute ATM at every quote cycle (30s
default). When new ATM differs from prior ATM by one nickel,
update the 11-strike window (drop the edge strike on one side,
add a new strike on the other). Pending orders on dropped strikes
cancel; new strikes begin quoting.

> **Deviation 2026-04-20 (operator override, commit `bde574b`)**: active
> paper config runs asymmetric OTM-only (calls ATM→ATM+5, puts ATM−5→ATM
> → 12 instruments, 24 max resting orders) rather than the 22/44
> specified above. Rationale: per-refresh gateway JVM serialization
> latency was dominant; halving order count halved serialization time.
> ITM strikes had thin flow / wide spreads, so pruning them freed order
> budget without losing fill volume. Accepted risk: short-C vs short-P
> inventory skew on persistent drift, monitored against the 5.0 delta
> kill; re-enable `inventory_balance` gate if skew sustains >30%. See
> `CLAUDE.md` §12 for revert procedure.

### 3.3 NOT quoted (exclusions)

- Any strike with DTE > 35 (back-month)
- Any strike more than 5 nickels from ATM (> $0.25 from forward)
- Non-nickel strikes (any strike not at an exact $0.05 multiple)
- Strikes where SABR RMSE > 0.05 (fit quality floor)
- Strikes outside the SABR calibrated range per
  `volatility.py::is_strike_calibrated`

### 3.4 Quote size & cadence

**Quote size**: 1 contract per side per instrument. Total quoted
notional per strike: ~25,000 × F × 1 = ~$120K per contract at
F≈$4.80.

**Quote refresh cadence**: every 30 seconds, OR on any of:
- ATM re-centers (forward moves past the nearest nickel boundary)
- Market bid/ask moves by ≥1 tick
- SABR parameters update (triggered by own schedule ≤30s)

**Penny-jump policy**: quote one tick inside the incumbent best
quote, subject to min-edge constraint below.

**Minimum edge**: `min_edge_points = 0.001` (2 ticks at HG's $0.0005
tick). Quote (theo − 0.001) on bid side, (theo + 0.001) on ask side.
If the penny-jumped price would violate min-edge, skip that side.

**Skip wide market**: if half-spread > 4× min_edge (i.e., > 0.004),
the market is considered too wide to quote into; skip both sides
until it tightens.

---

## 4. Pricing (theo)

**Theo source**: corsair's live SABR fit per (minute × expiry ×
side). At each quote cycle, use the most recent SABR parameters
for the front-month expiry to compute theo for each strike via:

```
theo(K) = black76(F, K, T, svi_implied_vol(F, K, T, SABR_params), cp)
```

where SABR_params are (a, b, rho, m, sigma) from the most recent
`volatility.py` fit for (expiry, cp).

**Fallback chain** if SABR produces invalid theo:
1. SABR with ATM-only IV (no skew) — use ATM IV for all strikes
2. Market midpoint — use (bid + ask) / 2 for theo, widen min_edge
3. Flat vol (for deep OTM beyond SABR calibrated range) — skip the
   strike; do not fall back further.

**Theo refresh**: every quote cycle. SABR parameters update at
≤60s cadence; theo recomputes on every param update or forward
tick (whichever first). The trader's safety contract is the
staleness gate (skip quoting if no surface refresh in 120s) plus
the forward-drift guard (skip if `|spot - fit_forward| > 200 ticks`)
— cadence itself is not load-bearing, the gates are.

---

## 5. Hedging

**Delta hedge via HG futures.** Front-month HG futures contract
matching the active option expiry's underlying.

**Hedge target**: net book delta ≈ 0 at all times, measured at
quote cycle frequency (30s).

**Hedge tolerance band**: ±0.5 contract-deltas (= ±12,500 lbs
equivalent). No hedge trade if net delta is within ±0.5 of zero.

**Hedge trade size**: whole-contract rounding. Net delta 1.7 →
buy 2 futures. Net delta −0.6 → sell 1 future.

**Hedge rebalance cadence**: on any fill (hedge the new position's
delta immediately) OR when net book delta exceeds the tolerance
band on a 30s-sampled check.

**Hedge costs** expected per v1.3 U2 path-dependent model:
- Mean rebalance trade slippage: ~0.5-1.5 ticks per trade
- Aggregate hedge cost: ~$40-60 per option fill (measured on
  in-sample HG front-only)

---

## 6. Defensive lever stack (PRIMARY SAFETY LAYER)

This section supersedes v1.3 §§6-6b and replaces all trend-filter /
per-strike-cap / loss-limit-breaker specifications from prior
drafts. Evidence basis: `docs/findings/v1_4_spec_delta_2026-04-19.md`.

### 6.1 PRIMARY — Daily P&L halt

**Trigger**: cumulative intraday net P&L (options MTM + hedge MTM
+ realized flows + transaction costs, integrated from session start)
≤ **−5% of deployed capital**.

Concrete thresholds:
- Stage 1 ($75K capital): halt at **−$3,750**
- Stage 2 ($100K capital): halt at **−$5,000**
- Stage 2 variant ($150K): halt at −$7,500
- Stage 2 variant ($250K): halt at −$12,500

**Behavior on trigger**:
1. Immediately cancel all 44 resting option orders
2. Flatten options positions at best available market (market
   order or aggressive IOC limit at market ± 1 tick)
3. Flatten delta hedge (futures) at best available market
4. Log the halt with timestamp, P&L at trigger, book state
5. No new quoting for the remainder of the trading session
6. Automatic resume at next session start (unless a second
   Tier-1 switch also tripped — then escalate, see §7)

**P&L computation for the halt trigger**:
- **Options MTM**: sum over open options positions of
  `size × (current_mid − entry_theo) × 25000`
- **Hedge MTM**: `hedge_qty_contracts × (current_F − entry_F) × 25000`
- **Realized**: close fills on the day, summed with signs
- **Transaction costs**: exchange fee + commission per contract
  traded (one-sided), estimated $2.15/contract.

Sampling: every 30 seconds during session (same as quote cadence)
OR on every fill (immediate update).

**Implementation notes**:
- Running P&L must be maintained continuously, not computed at
  end-of-day. The halt must be able to fire at any moment during
  a session.
- "Cumulative intraday P&L" resets to zero at session start (not
  a running multi-day figure). The halt protects against a
  single-day loss regardless of prior days' P&L.
- In-panel testing approximated the halt as an end-of-day floor
  on daily P&L. Live implementation must be faster (intraday).

**Expected effect based on validation**:
- Max tail-event DD reduced from 70-83% of capital to 17-25%
  across all tested panels + capital tiers.
- Halt fires approximately 0-10 times per 4-month panel
  depending on regime. Typical non-tail panel: 2-5 halt days
  per 4mo.

### 6.2 SECONDARY — Tier-1 kill switches

Carried forward from v1.3 §6 with thresholds recalibrated for
Stage 1 / Stage 2 capital tiers:

| Switch | Stage 1 ($75K) | Stage 2 ($100K) | Trigger mechanism |
|---|---|---|---|
| SPAN margin kill | **$52,500** (70% × $75K) | **$70,000** (70% × $100K) | Absolute halt; flatten book |
| Delta kill | ±5.0 contract-deltas | ±5.0 | Halt new opens, force-hedge to 0 |
| Theta kill | **−$200** (2/3 of v1.3) | **−$250** | Halt new opens |
| \|Net vega\| halt | ~~**$500**~~ **DISABLED** (see below) | ~~**$750**~~ TBD | Halt new opens |

**\|Net vega\| halt — DISABLED 2026-04-23.** Backtest characterization
(HG, 2023 Q1 / 2023 Q3 / OOS 2026 panels at $250K tier) showed the $500
threshold would halt on 67-88% of fills — operationally a choke, not a
tail backstop. In the OOS 2026 panel, only 3.7% of $500-breaches coincide
with margin > 80% of cap; rescaling to $3000 fixes OOS (0.0% co-occurrence)
but breaks 2024 Q3 (97.4%). Disabling is the only choice that's clean
across all three panels, and is consistent with §87 ("accept limited
vega/gamma exposure, defend tail with daily P&L halt") and with SPAN
already pricing a 25% vol scan. Evidence:
`docs/findings/vega_kill_characterization_2026-04-23.md`. Applied at
`config/hg_v1_4_paper.yaml:64` (`vega_kill: 0` — 0 is a no-op guard in
`risk_monitor.py:244`). HG-specific; re-characterize before any ETH
capital bump.

**All Tier-1 kills**:
- Override the daily P&L halt (i.e., if SPAN breaches before
  daily P&L halt, flatten immediately without waiting)
- Log trigger with full book state
- Require manual review before next-session resume

### 6.3 Trigger precedence

If multiple conditions fire simultaneously, precedence order:
1. SPAN margin kill (hardest — instant flatten)
2. Delta/theta/vega kill (flatten if position-level)
3. Daily P&L halt (session-level halt + flatten)
4. Operational kill switches (§7)

**Any Tier-1 trigger before daily P&L halt = escalation**. Do not
auto-resume; require manual review.

### 6.4 NOT in v1.4 defensive stack (tested and refuted)

- **Trend filter** (ts_10d < −1 put-skip): tested across 6 panels,
  0/6 pass pre-specified validation. Provides no value-add.
- **Per-strike position cap N=5**: tested across 7 panels (6 + OOS),
  hurts Sharpe in every non-tail panel (mean −1.94), does not
  mitigate tail events, makes Feb-Mar 2026 DD *worse* (65% → 130%
  at $100K). Definitively refuted.
- **Daily loss-limit breaker** (pause fills day-after-loss): tested
  earlier, superseded by the direct daily P&L halt flatten
  mechanism which addresses the tail damage at source.

---

## 7. Operational kill switches (infrastructure-level)

These are not strategy levers; they protect against infrastructure
failure. Carried forward from v1.3 §7 unchanged:

| Switch | Trigger | Action |
|---|---|---|
| IBKR gateway disconnect | Any loss of connection | Cancel all resting orders; halt quoting |
| SABR calibration failure | RMSE > 0.05 for 5+ min on front-month | Halt quoting on affected strikes |
| Quote latency breach | Median quote-update latency > 2s for 60s | Halt all quoting, page operator |
| Position reconciliation failure | IBKR-reported position ≠ corsair's internal state | Halt, reconcile, page operator |
| Realized vol spike | 5-day rolling rvol > 0.50 (historical p90 is 31%) | Page operator; do NOT auto-halt |
| Abnormal trade-rate | Fills per minute > 10× rolling-hour baseline | Halt, investigate, page |

These fire independently of strategy-level kills and should be
implemented as separate subsystem (watchdog layer).

---

## 8. Capital structure

### 8.1 Stage 1 allocated capital

**$75K total allocated to HG paper deployment.**

Breakdown:
- Paper margin cap: $75K (this is the cap the halt is sized against)
- Working buffer: $0 (paper; no intra-day margin swings)
- Kill-switch reserve: $0 (paper; no real capital)

### 8.2 Stage 2 allocated capital (base case)

**$100K total allocated.** Post Stage-1 pass.

Breakdown:
- Margin cap: $100K
- Working buffer (intra-day margin swings): $40K
- Kill-switch reserve (locked, never deployed): $20K
- Operator-reserve account: $40K (for Stage 3+ expansion or
  unanticipated liquidity events)

**Total committed to HG strategy**: $200K at Stage 2
($100K active + $100K buffer/reserve).

### 8.3 SPAN margin expectations

Based on Phase 4 and 6-panel measurements with sym_5 scope:

| Capital | Typical margin usage | Peak margin usage |
|---|---|---|
| $75K Stage 1 | 60-70% of cap | 85-95% |
| $100K Stage 2 | 55-65% of cap | 80-90% |

**Implication**: at Stage 1 $75K, expect to operate near margin
ceiling most of the time. Margin kill at 70% × $75K = $52.5K
will fire regularly on volatility spikes. This is by design.

**Practical: Corsair team should verify via IBKR SPAN calculator
on a representative sym_5 book before Stage 1 launch.**

### 8.4 Expected returns (Stage 2 base case, $100K)

From 6-panel + OOS validation with halt active:
- Mean annualized P&L: ~$740K (projection, before haircut)
- Mean Sharpe: +5.91
- Max observed tail DD: ~$20K (19.7% of capital)
- IRR: ~740% (projection, before haircut)

**Realistic haircut for live deployment**: 30-50% reduction from
backtest projections. Sources of haircut:
- Halt flattening slippage not modeled (expect +10-30% haircut on
  prevented-loss)
- Halt-trigger latency (intraday monitoring vs end-of-day
  approximation)
- AS behavior in live counterparty mix
- Regime distribution uncertainty (N=2 tail events)

**Realistic Stage 2 expectation**: $370K–$520K/yr on $100K
capital. Still strong risk-adjusted.

---

## 9. Gate 0 — Pre-Stage-1 infrastructure checklist

**Owner: Corsair execution team.** All items must be complete and
verified before Stage 1 paper launch.

### 9.1 IBKR integration

- [ ] IBKR Gateway installed on corsair production host
- [ ] Paper account authenticated and stable
- [ ] Market data subscription active for CME COMEX:
  - HG futures (all active months)
  - HXE options (European-style copper options)
- [ ] Gateway connection monitoring + auto-reconnect logic
  - Induced disconnect: kill gateway, confirm reconnect ≤ 60s
  - No orphaned orders across reconnect (induced test)
  - Order-state reconciliation matches IBKR post-reconnect
- [ ] Active front-month at launch: **HXEK6** (May 2026 options,
      expires ~2026-04-27). Verify market data is flowing on
      HXEK6 call + put strikes spanning ATM ± 5 nickels before
      launch (typically 22 instruments visible).

### 9.2 SABR calibration loop

- [ ] SABR calibration live, running against HXEK6 quotes
  - Parameters (a, b, rho, m, sigma) per (expiry, cp)
  - Update interval ≤ 30s
  - RMSE tracking per fit
- [ ] Theo pricing engine using latest SABR params
- [ ] Theo produced for all 22 instruments (C+P at 11 strikes)
- [ ] Fallback chain implemented per §4
- [ ] Calibration quality gate: RMSE > 0.05 trips SABR kill
      switch (§7) — induced test by forcing noisy input

### 9.3 Order management state machine

- [ ] Place quote → IBKR acknowledges
- [ ] Market moves → quote cancels + re-places
- [ ] Fill event received → position tracker updates
- [ ] Fill event received → hedge order placed (delta offset)
- [ ] Partial fill handling: residual cancels cleanly
- [ ] ATM re-center event: old-edge strike cancels; new-edge
      begins quoting

### 9.4 Kill switch implementation & verification

All kill switches must be implemented AND induced-breach tested
on paper account before Stage 1 launch.

- [ ] **Daily P&L halt at −$3,750** (NEW, v1.4 primary defense)
  - Live intraday P&L tracking (options MTM + hedge MTM + realized)
  - Running-total sampled every 30s OR on fill
  - Breach triggers: cancel all, flatten options, flatten hedge
  - Session-end halt: no re-quote until next session
  - **Induced test**: manually accumulate −$3,750 paper loss
        (via intentional adverse trade sequence); verify halt
        fires + flatten completes + no re-quote same session
- [ ] **SPAN margin kill at $52,500** (70% × $75K)
  - Induced test: deliberately approach margin ceiling;
        confirm halt + flatten at trigger
- [ ] **Delta kill at ±5.0**
  - Induced: accumulate 5+ contract-deltas one-sided;
        confirm force-hedge to 0
- [ ] **Theta kill at −$200**
  - Induced: accumulate sufficient short theta;
        confirm halt new opens
- [ ] **|Net vega| halt at $500**
  - Induced: accumulate sufficient net vega;
        confirm halt new opens
- [ ] Each kill switch logs trigger event with complete book
      state (positions, margin, P&L, F, timestamp)
- [ ] Kill switch test log written to
      `~/corsair/logs-paper/kill_switch_tests.jsonl` for
      crowsnest review

### 9.5 Logging requirements

Required log streams, one JSONL file per stream, written to
`~/corsair/logs-paper/`:

- `fills.jsonl`: every fill (§17 schema)
- `skips.jsonl`: every non-fill quote event with skip reason
- `kill_switch.jsonl`: every kill switch trigger (with pre/post state)
- `reconnects.jsonl`: every gateway disconnect/reconnect event
- `sabr_fits.jsonl`: SABR params at each update + RMSE
- `margin_snapshots.jsonl`: SPAN margin every 1 minute during session
- `pnl_snapshots.jsonl`: intraday P&L trajectory (every 30s)
- `hedge_trades.jsonl`: every futures hedge trade

### 9.6 Reconciliation pipeline

- [ ] Paper fill log written in §17 schema
- [ ] Daily EOD summary export (§18 schema)
- [ ] Crowsnest-side ingestion script reads corsair logs and
      writes to `~/crowsnest/data/live_paper/` (crowsnest team to
      implement separately)
- [ ] First-day reconciliation: at Stage 1 day 1 close, manual
      check that crowsnest simulator run for same day produces
      fills within ±50% of live paper count

### 9.7 Config file

Required config at `~/corsair/config/hg_v1_4_paper.yaml`:

```yaml
product: HG
mode: paper
stage: 1
capital_usd: 75000

scope:
  expiry: front_only
  max_dte_days: 35
  strike_filter: sym_5
  strike_window_nickels: 5
  nickel_only: true

quote:
  tick_size: 0.0005
  penny_jump_ticks: 1
  quote_size: 1
  min_edge_points: 0.001
  skip_if_spread_over_edge_mul: 4.0
  refresh_cadence_seconds: 30
  atm_recompute_cadence_seconds: 30

hedging:
  enabled: true
  instrument: HG_futures_front
  tolerance_deltas: 0.5
  rebalance_on_fill: true
  rebalance_cadence_seconds: 30

kill_switches:
  daily_pnl_halt_pct: 0.05
  daily_pnl_halt_usd: 3750  # = 0.05 × 75000
  span_margin_kill_usd: 52500  # = 0.70 × 75000
  delta_kill: 5.0
  theta_kill_usd: -200
  vega_halt_usd: 500

operational_kills:
  ibkr_disconnect: true
  sabr_rmse_threshold: 0.05
  quote_latency_max_ms: 2000
  rvol_alert_5d_threshold: 0.50

logging:
  dir: ~/corsair/logs-paper
  fills: true
  skips: true
  kill_switch: true
  reconnects: true
  sabr_fits: true
  margin_snapshots_cadence_min: 1
  pnl_snapshots_cadence_sec: 30
  hedge_trades: true
```

### 9.8 Gate 0 acceptance criteria

All checklist items complete. Induced kill-switch tests pass
(log artifacts produced). Crowsnest review sign-off on:
- Kill switch test log (confirm all 5 switches fired correctly)
- First-day paper operation dry run (30-60 min live with no
  exceptions)
- Reconciliation pipeline end-to-end (live fill written → crowsnest
  ingestion → simulator comparison)

**Target: Gate 0 complete within 2-3 weeks of v1.4 sign-off
(2026-04-19).** Target Gate 0 completion: **2026-05-10**.

---

## 10. Stage 1 — Paper operation ($75K capital)

### 10.1 Scope

- **All Gate 0 items complete and verified.**
- Stage 1 is OPERATIONAL VALIDATION, not economic validation.
- No P&L targets. No Sharpe claim attached.
- Purpose: confirm infrastructure runs cleanly at realistic size.

### 10.2 Duration

**1-2 weeks of continuous paper operation during CME HG options
session hours.** Target: 2026-05-11 → 2026-05-25.

### 10.3 Stage 1 pass criteria (ALL must hold)

Operational criteria only:

- [ ] Zero orphaned orders across any reconnect
- [ ] All kill switch trips logged with full state
- [ ] No kill switch fires EXCEPT during induced tests (Gate 0)
      OR on genuine market events (e.g., SABR RMSE spike during
      real vol move, daily P&L halt on genuine loss day)
- [ ] Improving-fill logic works (test by approaching halt
      thresholds deliberately)
- [ ] Reconnection logic passes under real network conditions
- [ ] Fills occur on BOTH call and put sides (not one-sided book)
- [ ] MtM tracking infrastructure operates without drift
- [ ] Simulator-vs-live fill-rate divergence ≤ 50% on any single
      day, ≤ 30% on the 10-day average
- [ ] No data loss in any of the 8 log streams

### 10.4 Stage 1 economics observation (not a pass criterion)

Report for post-Stage-1 review (NOT used to gate Stage 2 advance):
- Fills/day (range)
- Per-fill gross edge (median, p10, p90)
- Halt frequency (how often daily P&L halt fires)
- Margin utilization (mean, peak)
- Per-moneyness fill distribution (ATM, near-OTM, deep-OTM)
- Any tail event observed (day with P&L breach of −5% cap)

### 10.5 Stage 1 failure modes

If Stage 1 pass criteria are NOT all met at end of window:

**Operational failure**: infrastructure issue (reconnect,
reconciliation, kill switch malfunction). Do NOT advance. Fix
and re-run Stage 1 for another 1-2 weeks.

**Structural failure**: simulator-vs-live divergence > 50%
sustained, or fills concentrated one-sided. Requires research-
side investigation before re-running Stage 1.

**Catastrophic failure**: multiple Tier-1 kills fire spuriously,
or data loss in log streams. Halt, full manual review.

---

## 11. Stage 2 — Live operation ($100K capital)

### 11.1 Trigger

**Stage 1 pass criteria (§10.3) all met AND 2 weeks of
operational stability at end of Stage 1.** Target Stage 2
launch: **2026-05-25 or later**.

### 11.2 Scope

Same config as Stage 1, with:
- Capital increased to $100K
- Kill switch thresholds recalibrated (§6.1, §6.2):
  - Daily P&L halt: −$5,000
  - SPAN margin kill: $70,000 (70% × $100K)
  - Other Tier-1 thresholds slightly scaled per §6.2 table
- Config file: `~/corsair/config/hg_v1_4_live.yaml`
- Mode: live (not paper)
- Stage: 2

### 11.3 Stage 2 monitoring & weekly review

**Daily**:
- All 8 log streams captured
- Daily EOD summary written to reconciliation ingestion path
- Automated alerts on kill switch trips, log stream gaps

**Weekly** (Crowsnest research reviews):
- Live vs simulator fills comparison
- P&L attribution: gross edge + inventory MTM + hedge cost
- Tail-event count (if any)
- Margin utilization profile
- Any halt event (P&L halt trigger) — full post-mortem

### 11.4 Stage 2 success criteria (for internal tracking; not
         a "pass" gate)

No formal advance gate to Stage 3 is specified in v1.4. Stage 3
decision requires ≥6 months of Stage 2 live data and separate
memo.

Internal tracking metrics (not action-triggering):
- Live daily Sharpe > +2 on 30-day rolling window
- No catastrophic drawdown (> 25% of capital)
- Fill rate within ±30% of simulator projection

### 11.5 Stage 2 abort conditions

**Automatic halt + investigation trigger** (not auto-abort, but
operator paged for immediate review):

- Cumulative P&L ≤ −30% of capital in any rolling 30-day window
- Three P&L halts fire within 10 trading days
- Any operational kill switch fires on genuine market event
  (not induced)
- Simulator-vs-live divergence exceeds 100% on any single week

**Hard abort** (immediate Stage 2 suspension, manual review):
- Two Tier-1 kill switches fire on genuine market events within
  the same day
- Single-day loss exceeds 7% of capital (halt mechanism failure
  or latency issue)

---

## 12. Stages 3-4 (NOT specified in v1.4)

Beyond Stage 2, no capital scaling or product expansion is
specified. Re-evaluation gate:
- ≥ 6 months of live Stage 2 data
- ≥ 3 additional tail events observed (current N=2)
- Fresh spec delta memo (v1.5) justifying any expansion

---

## 13. Reconciliation & monitoring (Crowsnest-side)

Crowsnest team maintains the following infrastructure during
Stages 1-2:

### 13.1 Ingestion

Script: `scripts/ingest_corsair_logs.py` (to be written by
crowsnest).
- Reads `~/corsair/logs-paper/*.jsonl` (Stage 1) or
  `~/corsair/logs-live/*.jsonl` (Stage 2)
- Writes normalized parquet to
  `~/crowsnest/data/live_paper/hg/` or `~/crowsnest/data/live/hg/`
- Cadence: hourly incremental, daily full consolidation

### 13.2 Simulator comparison

Script: `scripts/live_vs_sim_daily.py` (to be written).
- Loads live fills + hedges for the day
- Runs crowsnest simulator on same trade events (from Databento
  or IBKR TBBO if available) for same day
- Produces divergence metrics:
  - Fill count delta
  - P&L delta
  - Per-moneyness fill distribution delta
  - Per-contract delta
- Writes daily report to
  `~/crowsnest/reports/hg_reconciliation_<date>.json`

### 13.3 Alerts

Configured in `~/crowsnest/config/live_divergence_alerts.yaml`:

```yaml
divergence_alerts:
  fill_count_pct_max: 30  # %; paged above
  pnl_dollar_divergence_max: 10000  # $; paged above
  tail_event_day: true  # always paged
  kill_switch_fire: true  # always paged
  reconnect_spike: 5  # reconnects/hour; paged above
```

Alert routing: slack webhook to `#crowsnest-hg-live` channel.

### 13.4 Weekly memo

Crowsnest research produces weekly summary memo:
`docs/findings/hg_stage1_weekly_<YYYYMMDD>.md` (or `stage2_weekly`).

Contents:
- Week's live Sharpe, P&L, DD, fill count
- Comparison vs simulator projection
- Any tail event post-mortem
- Any operational issue
- Advance/hold/abort recommendation if relevant

---

## 14. Performance monitoring metrics (live-side)

Daily metrics tracked in corsair logs and ingested by crowsnest:

| Metric | Computation | Trigger if |
|---|---|---|
| Fills/day | Count of active fills (non-cancelled) | < 20% of simulator |
| Gross edge/fill | Mean of \|fill_price − theo_at_fill\| × size × mult | < $100 |
| Net daily P&L | Gross edge + inventory MTM + hedge − costs | < −5% cap (triggers halt) |
| Margin utilization | SPAN / capital | > 80% sustained |
| Halt count | Daily P&L halts fired that day | > 0 (always log) |
| Call:put fill ratio | n_call_fills / n_put_fills | < 0.3 or > 3.0 |
| Reconnect count | Gateway reconnects that day | > 3 |
| SABR RMSE median | Median RMSE across fits that day | > 0.03 |

---

## 15. Logging & audit trail

All logs retained indefinitely. Daily rotation + compression for
files > 100MB. Full audit trail:
- Every fill: input quote state, theo, fill price, edge, position
  deltas pre/post
- Every kill switch trip: complete book state snapshot
- Every reconnect: pre-reconnect pending orders, post-reconnect
  reconciled state

---

## 16. Economic expectations & realistic haircuts

### 16.1 Stage 1 paper ($75K)

**No economic target.** Operational validation only.
Observed fills/day, P&L, halt frequency will be reported but are
NOT the Stage 1 pass criteria.

Expected direction (for context): baseline projection at $75K per
6-panel evidence (with halt active, ~linear downscaling from
$100K results):
- Mean Sharpe: ~+5.9
- Annualized P&L: ~$555K (projection)
- Max tail DD: ~$15K (20% of cap)

Realistic (with halt slippage + paper-to-live haircuts): half
the above figures. Still positive.

### 16.2 Stage 2 live ($100K)

Backtest-based projection (all 7 panels with halt active):
- Annualized P&L: ~$740K
- Sharpe: +5.91
- Max tail DD: ~$20K (20% of cap)
- IRR: ~740%

Realistic haircut (30-50% reduction):
- Annualized P&L: $370K – $520K
- Sharpe: +3.5 to +4.5 realistic range

Still strong risk-adjusted return. At the lower end ($370K on
$100K), still 370% IRR.

### 16.3 Haircut sources

| Source | Estimated impact |
|---|---|
| Halt flattening slippage (not modeled) | +10-30% on prevented-loss during halt events |
| Halt-trigger latency (intraday vs daily approximation) | +5-15% on halt-day losses |
| AS behavior in live counterparty mix | ±20% on fill count |
| Regime distribution (N=2 tail events) | unknown; dominant risk |
| Exchange fees + commissions | baked into backtest |
| IBKR market-data / API costs | $5-15K/year on capital base |

---

## 17. Interface — paper fill log schema

File: `~/corsair/logs-paper/fills.jsonl`. One JSON record per
line, per fill.

```json
{
  "timestamp_utc": "2026-05-12T14:32:17.342Z",
  "order_id": "CORSAIR-P-12345",
  "ibkr_perm_id": "IBKR-678901",
  "symbol": "HXEK6 C485",
  "contract_sym": "HXEK6",
  "cp": "C",
  "strike": 4.85,
  "side": "SELL",
  "quantity": 1,
  "fill_price": 0.0425,
  "fill_price_usd": 1062.50,
  "theo_at_quote": 0.0420,
  "theo_at_fill": 0.0421,
  "edge_usd": 12.50,
  "forward_at_fill": 4.8075,
  "moneyness": 1.0087,
  "days_to_expiry": 15.2,
  "position_pre": -2,
  "position_post": -3,
  "book_delta_pre": 0.45,
  "book_delta_post": -0.12,
  "hedge_trade_triggered": false,
  "margin_pre_usd": 38500,
  "margin_post_usd": 39250,
  "sabr_rmse_at_fill": 0.0023
}
```

**Required fields**: timestamp_utc, symbol, cp, strike, side,
quantity, fill_price, theo_at_fill, edge_usd, forward_at_fill,
days_to_expiry, margin_post_usd.

**Optional fields**: everything else (helpful for reconciliation
but not required).

---

## 18. Interface — daily EOD summary

File: `~/corsair/logs-paper/daily_summary_<YYYY-MM-DD>.json`.

```json
{
  "date": "2026-05-12",
  "capital_deployed_usd": 75000,
  "fills_count": 42,
  "fills_call": 19,
  "fills_put": 23,
  "gross_edge_total_usd": 18450.00,
  "inventory_mtm_total_usd": 12300.00,
  "hedge_mtm_total_usd": -2180.00,
  "realized_pnl_total_usd": 28570.00,
  "transaction_costs_usd": 180.60,
  "net_pnl_usd": 28389.40,
  "net_pnl_pct_of_cap": 37.85,
  "margin_usage_mean_usd": 48200,
  "margin_usage_peak_usd": 54300,
  "margin_usage_peak_pct": 72.4,
  "halt_events": [],
  "kill_switch_events": [],
  "reconnect_events": [
    {"timestamp": "2026-05-12T09:15:03Z", "duration_sec": 12}
  ],
  "sabr_rmse_median": 0.0021,
  "sabr_rmse_p90": 0.0039,
  "active_strikes_count": 22,
  "fills_per_moneyness": {
    "deep_otm_call": 3,
    "near_otm_call": 7,
    "atm": 12,
    "near_otm_put": 11,
    "deep_otm_put": 9
  }
}
```

---

## 19. Runbook — common scenarios

### 19.1 Daily P&L halt fires

1. Log trigger event with complete state (automatic)
2. Flatten all options + hedge (automatic)
3. No new quoting until next session (automatic)
4. Operator: review halt log within 4 hours
5. If cause is genuine market event → resume next session
   (no action needed)
6. If cause is calibration/infrastructure bug → do NOT resume;
   escalate to crowsnest research
7. Record in weekly memo

### 19.2 Kill switch fires (Tier-1 other than daily halt)

1. All automatic actions complete (flatten, log)
2. Operator: immediate review (within 1 hour)
3. Determine root cause BEFORE next session
4. Hard abort conditions (§11.5) → full manual review, crowsnest
   involvement, potentially pause Stage 2

### 19.3 Reconnection / gateway issue

1. Gateway auto-reconnects (target ≤60s)
2. Order state reconciliation automatic
3. If reconcile fails → halt quoting, page operator
4. If reconnects > 3 in single day → operator review

### 19.4 SABR calibration degradation

1. RMSE > 0.05 for 5+ min → SABR kill fires, halt affected strikes
2. Operator: check input data quality, market conditions
3. If calibration won't recover after 30 min → halt full session,
   escalate

### 19.5 Simulator-vs-live divergence

1. Divergence > 30% daily: operator note, no action
2. Divergence > 50% daily: operator review within 24h
3. Divergence > 100% sustained 3+ days: pause Stage 2, crowsnest
   investigation

---

## 20. Escalation & contacts

| Event | Responsible team | Escalation trigger |
|---|---|---|
| Daily P&L halt (routine) | Corsair ops | None (automatic) |
| Tier-1 kill fire (genuine) | Corsair ops | Manual review before resume |
| Infrastructure failure | Corsair ops | Crowsnest ping if impacts research pipeline |
| Divergence > 50% | Corsair ops → Crowsnest | Joint review |
| Divergence > 100% sustained | Crowsnest research | Pause Stage 2, full memo |
| Catastrophic loss (> 7% cap) | Both | Hard abort; full manual review |

---

## 21. Review cadence

- **Daily**: Automated reports + alerts
- **Weekly**: Crowsnest research memo (Stage 1 and Stage 2)
- **Monthly**: Cross-team review of Stage 2 trajectory
- **Quarterly**: Formal spec delta if material changes proposed

---

## 22. Out-of-scope for v1.4

Explicitly NOT part of v1.4:
- Back-month (front+1, front+2) quoting
- Wings-only or asymmetric OTM scope variants
- Trend filter or any put-skip/call-skip defensive lever
- Per-strike position caps beyond SPAN-implicit
- ETH options (retracted per v4 spec retraction 2026-04-18)
- Additional products (silver, gold, other metals)
- Stage 3+ capital scaling

Any of the above would require a fresh spec delta memo (v1.5 or
later) with research justification.

---

## 23. Economic summary — what we are deploying

At Stage 2 ($100K capital, v1.4 halt-on config):

| Dimension | Value | Source |
|---|---|---|
| Sharpe (central, realistic) | +5.2 | audit §2 — L1+L2 fill realism, halt slippage $3K/trigger, as_rate=0.35 |
| Sharpe (95% CI) | +4.0 to +6.5 | audit §3 — bounded by halt-slippage × AS-rate uncertainty |
| Annualized P&L (central) | ~$820K | audit, panel-mean pnl_median × 12/3.71 months |
| Annualized P&L (95% CI) | ~$600K to ~$1.0M | scales ≈ linearly with Sharpe over tested range |
| Max tail DD (measured) | 23% of capital | audit per-panel §5 |
| Max tail DD (adverse realistic) | up to 29% at S=$3K halt slippage | audit §2 |
| Halt trigger rate | ~5–6 days per ~4-month panel | audit §5 |

Source: `~/crowsnest/docs/findings/defensible_sharpe_synthesis_2026-04-23.md`.

### 23.1 How this number is constructed

1. Baseline pipeline output: +5.91 (reproduces byte-identical).
2. Apply realistic halt slippage ($3K/trigger central): +5.27.
3. Apply L1+L2 fill realism: negligible (within noise). Central ≈ +5.35.
4. Rounded: +5.0 to +5.5 central, +4.0 to +6.5 95% CI.

### 23.2 Top-3 priced sensitivities

| Knob | Low | Central | High | Sharpe range |
|---|---|---|---|---|
| Halt slippage | $1K/trigger | $3K | $8K | +5.81 → +5.27 → +4.29 |
| AS rate | 0.20 | 0.35 (current) | 0.50 | ~+6.4 → +5.91 → +5.03 |
| Fill realism (L1+L2) | Off | Baseline | L1+L2 on | +5.91 → +5.91 → +6.03 |

Dominant uncertainty is halt slippage. Fill-realism moves the headline by <1 cross-panel SE.

### 23.3 Risks NOT in the 95% CI

The CI covers what is priceable from 2023–2026 HG panel data. It does NOT cover:

- Decision-to-exchange latency — pipeline zero-latency, live ~10–200 ms.
- Queue-below-top — TBBO only; behind-touch fills invisible.
- Live AS mix — as_intensity=0.25 is literature-benchmark, not corsair-calibrated.
- Regime coverage — 26 months of HG history; pre-2023 + 2024-Q4 + 2025-H1 absent.
- Paper-to-live operational gap — IBKR throttling, rejections, reconnect mechanics.

See audit §6 for magnitudes. Combined plausible magnitude is ±0.5–1.0 Sharpe — post-live reality may land outside +4.0 to +6.5 in either direction without any single assumption being wrong.

### 23.4 Prior framing (retained for reference)

| Dimension | Prior value |
|---|---|
| Annualized P&L (projection) | ~$740K |
| Annualized P&L (realistic haircut) | $370K – $520K |
| Sharpe (projection) | +5.91 |
| Sharpe (realistic) | +3.5 – +4.5 |

The prior "realistic" haircut was a 30–50% judgement-call reduction. The audit's per-dimension measurement finds that range more conservative than measured realism supports — primarily because as_intensity=0.25 was already modeling a significant fraction of what the haircut informally reserved for.

---

## 24. Known limitations & open questions

1. **Halt flattening slippage** — not modeled in backtest. Stage 1
   paper operation will measure this directly. Expect 10-30%
   reduction in halt-prevention efficacy.

2. **Tail event frequency and regime dependence** — N=2 events
   over 26 months of backtest data. Insufficient to characterize
   frequency at scale. Stage 2 live operation is the
   tail-event observation vehicle.

3. **Capital-DD relationship regime-dependence** — the two tail
   events observed showed OPPOSITE capital-DD relationships
   (Feb-Mar 2026 favored small capital; Panel 5 2024-Q2 favored
   larger). Halt mitigates both, but the underlying regime
   uncertainty remains. Additional events needed to bound.

4. **Halt-and-restart dynamics** — backtest assumes clean next-day
   resume. Live market may show persistent regime effects across
   halt boundaries. Monitor.

5. **AS behavior in live counterparty mix** — AS model calibrated
   on historical Databento; live counterparty behavior may differ.
   Simulator-vs-live reconciliation catches this.

---

## 25. Artifact index & change log

### v1.4 research artifacts (2026-04-19)

- `docs/findings/v1_4_spec_delta_2026-04-19.md` — this handoff's
  delta memo, capturing what changed from v1.3
- `docs/findings/v1_3_advisory_retraction_2026-04-19.md` — pending-
  revision advisory for v1.3, now superseded
- `docs/findings/trend_filter_validation_2026-04-19.md` — trend
  filter refutation + strategy validation (revised afternoon
  version)
- `docs/findings/scope_matrix_and_backmonth_audit_2026-04-19.md` —
  scope matrix supporting sym_5 choice
- `docs/findings/span_aware_capital_sweep_2026-04-19.md` — SPAN
  binding capital analysis (earlier)
- `scripts/build_2023_2024_pipeline.py` — 2023-2024 pipeline builder
- `scripts/panel_trend_filter_test.py` — 6-panel trend filter test
- `scripts/panel_capital_sensitivity.py` — capital-tier sensitivity
- `scripts/panel_defensive_levers_v2.py` — per-strike cap + halt test
- `data/backtest/hg_2023_2024/*.json` — validation results

### Historical artifacts (still relevant)

- `docs/findings/hg_front_only_deployment_prep_2026-04-18.md` —
  Gate 0 checklist origin (v1.3)
- `docs/findings/hg_tam_analysis_2026-04-18.md` — TAM analysis
  supporting front-month choice
- `docs/findings/realism_upgrades/phase4_hg_front_only_headline_2026-04-18.md` —
  Phase 4 headline finding
- `docs/findings/realism_upgrades/phase4_u2_hedge_cost_2026-04-18.md` —
  Phase 4 U2 per-expiry breakdown

### Change log vs v1.3

| Section | v1.3 | v1.4 |
|---|---|---|
| Strike scope | Asymmetric OTM ATM ± 5 | Symmetric ATM ± 5 (sym_5) |
| Trend filter | Not specified | Explicitly excluded (refuted) |
| Per-strike cap | N=5 proposed | Not in spec (refuted) |
| Daily P&L halt | Not specified | REQUIRED at −5% cap |
| Stage 1 capital | $100K | $75K |
| Stage 2 capital | $250K (Variant A-C) | $100K (base case) |
| Kill switch tuning | Fixed ($140K margin, −$500 theta, $1K vega) | Scaled to capital (§6.2) |
| Economic projection | $190K ± $130K base | $370K – $520K realistic |
| Gate 0 infrastructure | Per v1.3 §9 | Unchanged in content; parameters updated |

---

## Appendix A — Stage 0 timeline (target)

| Week | Activity | Deliverable |
|---|---|---|
| 1 (2026-04-20 → 04-26) | IBKR gateway + market data | Live HXEK6 data flowing |
| 2 (2026-04-27 → 05-03) | SABR calibration + pricing engine | Theo produced for 22 instruments |
| 3 (2026-05-04 → 05-10) | Order management + kill switches + induced tests | All 5 kills verified |
| Week 4 (2026-05-11 → 05-17) | Stage 1 launch | Paper op week 1 |
| Week 5 (2026-05-18 → 05-24) | Stage 1 continues | Paper op week 2 |
| End Week 5 | Stage 1 pass review | Advance decision |

Target Stage 2 launch: **2026-05-25** (contingent on Stage 1 pass).

---

## Approval & sign-off

- Research (Crowsnest): **APPROVED 2026-04-19**
- Execution (Corsair): pending Gate 0 completion
- Deployment authorization: v1.4 is the active spec. Corsair team
  may proceed with Stage 0 infrastructure buildout immediately.

Questions from Corsair team during buildout should be directed to
Crowsnest research via weekly check-in. Material spec changes
require v1.5 delta memo.
