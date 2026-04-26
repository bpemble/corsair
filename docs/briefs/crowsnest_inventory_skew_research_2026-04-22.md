# Research Brief — Inventory-Skew Pricing for Margin Relief on HG

Author: Corsair execution team
Date: 2026-04-22
Target: Crowsnest research
Status: Request-for-backtest. No corsair-side implementation until a
        variant passes the validation gate in §7.
Source observations:
  - 2026-04-21 P&L post-mortem (−$1,234 day, 55% attributable to 3×
    6.00P short accumulation on a −0.02 forward drift; internal)
  - 2026-04-22 margin decomposition (94% of $43K SPAN margin carried by
    3 short ATM puts; 25% of gross contracts)
  - 2026-04-21 behind-incumbent distance distribution (this brief does
    NOT duplicate the min_edge/tie-breaking research — those are the
    pricing-aggression levers; this brief is the position-aware lever)

---

## 1. Executive summary

Corsair's v1.4 quote engine prices both sides of each nickel strike
symmetrically around theo regardless of current inventory on that
strike. On 2026-04-21, incremental fills on the short side of 6.00P
compounded to a 3-contract position carrying ~$40K of SPAN margin
(94% of total book margin) without any self-correcting pricing
response. This brief asks crowsnest to backtest **inventory-skew
pricing** — adjusting bid and ask targets as a function of position
on the same strike — to quantify the margin relief and P&L impact
across a menu of variants.

The per-unit arithmetic is extremely favorable in principle:
- 1 tick = 0.0005 × 25,000 = **$12.50** of edge given up
- Closing 1× 6.00P frees **~$13,000** of SPAN margin
- Ratio: **~1,000:1** in favor of paying ticks to close
- BUT this ignores adverse-selection, path-dependency, and regime
  distribution. Empirical validation is required.

Corsair owns the implementation (config-gated knob, paper experiment
harness). Crowsnest owns the backtest, the variant selection, and
the deployment-gate decision.

---

## 2. Why SPAN + daily halt don't already catch this

This follows the methodology of the 2026-04-21 hardburst-skip brief
(ex-ante skip rules prior — default is rejection; the mechanism
argument must carry the weight).

- **SPAN** is a portfolio stress test. It sizes margin for the
  present book, but does nothing to prevent inventory accumulation
  into high-margin configurations. Once the book is in the bad state,
  SPAN holds us there unless margin breaches a threshold.
- **Daily P&L halt (−5%)** catches the session loss if inventory
  concentration turns into a realized loss — but by the time it
  fires, the damage is done. A $687 realized loss + $547 MtM on one
  strike line (2026-04-21) was well inside the halt boundary yet
  consumed 28% of daily P&L.
- **Margin escape** (constraint_checker.py:574) allows margin-
  *reducing* fills only AFTER margin exceeds the ceiling. It's a
  filter, not a pricing signal — it doesn't encourage closes before
  we hit the ceiling.
- **Per-strike position cap** was refuted in v1.4 (§6.4): hurts
  Sharpe, doesn't mitigate tail events. Inventory-skew pricing is
  distinct — it's continuous rather than binary, and it adjusts
  PRICING not QUOTING-PERMISSION.

The gap: nothing in the v1.4 stack creates a *pricing response* when
position builds up. Inventory skew is a direct address.

---

## 3. Mechanism

At the close of a short-option fill, the system's private valuation
of that contract diverges from theo by the **shadow price of the
margin it consumes**. For a deep-gamma position like short ATM puts,
that shadow price is large and strictly positive on the closing side.

Inventory skew operationalizes this by shifting the quote targets:

```
current:
  BUY  target = theo − min_edge
  SELL target = theo + min_edge

with inventory skew (schematic):
  BUY  target = theo − min_edge + f_buy(position)
  SELL target = theo + min_edge + f_sell(position)

where f_buy, f_sell depend on position sign/magnitude and the chosen
variant (§4).
```

When net short on a strike, `f_buy > 0` (raise our bid — more likely
to close) and `f_sell > 0` (raise our ask — less likely to add).
When net long, mirror.

Key consequences to measure:
- **Fill count change** — does the more-aggressive close side actually
  get filled more, or do we just post a worse bid that no one crosses?
- **Edge per close fill** — we expect lower per-fill edge on closes
  (we're paying up), potentially zero or slightly negative edge in
  units of theo. The margin relief must justify this.
- **Inventory decay rate** — how quickly do positions mean-revert
  toward zero under each variant?
- **Margin profile over time** — peak, median, time-at-ceiling
- **Tail behavior** — does the skew reduce the magnitude of bad-regime
  drawdowns (where inventory compounds fastest)?

---

## 4. Variants to backtest

Five axes. Crowsnest should sweep a pre-specified grid and report
best-per-axis to reduce overfit risk. **Do not cherry-pick the top
raw-Sharpe variant; pick the highest Sharpe-per-parameter-count.**

### 4.1 Skew magnitude (k)

Scalar multiplier on the skew function. Units: ticks per unit inventory.

| Variant | k | Interpretation |
|---|---|---|
| V0-baseline | 0 | Current v1.4 behavior |
| V1-gentle | 0.5 | Half-tick per unit (rounds to nearest tick; kicks in at 2× inventory) |
| V2-base | 1 | One tick per unit — 3× 6.00P short → bid +3t / ask +3t |
| V3-firm | 2 | Two ticks per unit |
| V4-aggressive | 4 | Four ticks per unit — saturates quickly |

### 4.2 Skew shape

How skew grows with |position|.

| Variant | Shape | Notes |
|---|---|---|
| S-linear | k × \|pos\| | Proportional |
| S-stepped | 0 if \|pos\| ≤ 1; k × \|pos\| otherwise | Tolerates small single-unit positions |
| S-capped | min(k × \|pos\|, k_max) | Upper bound, e.g., k_max = 5 ticks |
| S-exponential | k × \|pos\|² | Aggressive escalation past 2+ contracts |

### 4.3 Side application

Which side the skew applies to.

| Variant | BUY side | SELL side |
|---|---|---|
| A-close-only | +skew when short | +skew when long (close longs) |
| A-both | close side tightens AND open side widens (pull toward flat) | same |
| A-symmetric | equal skew both sides | only magnitude matters |

A-both is the most aggressive: when short 3× 6.00P, we'd raise BOTH
bid and ask by 3 ticks. Bid raise → more likely to close. Ask raise
→ less likely to add more shorts (fills on the ask would be someone
CROSSING our ask to buy from us — which ADDS to our short position).

### 4.4 Trigger gating

When skew activates.

| Variant | Trigger |
|---|---|
| G-always | Active for all non-zero positions |
| G-position | Active when \|pos\| ≥ N (e.g., N=2) |
| G-margin | Active when margin > M% of ceiling (e.g., M=50) |
| G-hybrid | Active when \|pos\| ≥ N AND margin > M% |

G-margin is theoretically cleanest (skew only when margin is
actually binding) but has a cliff effect at the threshold.
G-hybrid avoids pathological "skew everything, everywhere" and
"no skew when it matters" extremes.

### 4.5 Granularity

What "position" means for computing skew.

| Variant | Position definition |
|---|---|
| P-strike | Net position on this exact (strike, expiry, right) |
| P-expiry | Net position on this expiry, all strikes + rights |
| P-greek | Use the dominant greek stress (e.g., pull toward net-delta-zero) |

P-strike is the simplest and addresses the direct concentration case
(today's 3× 6.00P). P-expiry or P-greek would handle the book-level
view (e.g., we're gamma-short across multiple strikes but flat on
each individually).

---

## 5. Proposed baseline grid

Not every combination is interesting. Suggested initial grid:

| # | k | Shape | Side | Trigger | Granularity |
|---|---|---|---|---|---|
| 1 | 0 | — | — | — | — | (V0 baseline; current) |
| 2 | 1 | linear | close-only | always | strike |
| 3 | 1 | linear | both | always | strike |
| 4 | 2 | linear | both | always | strike |
| 5 | 2 | stepped@2 | both | always | strike |
| 6 | 2 | capped@5 | both | always | strike |
| 7 | 4 | exponential | both | always | strike |
| 8 | 2 | linear | both | margin≥50% | strike |
| 9 | 2 | linear | both | hybrid (\|pos\|≥2, margin≥40%) | strike |
| 10 | 2 | linear | both | always | expiry |

10 variants = manageable. Add/subtract per crowsnest's judgment.

---

## 6. Panels & metrics

### 6.1 Panels

Same 6-panel + OOS structure as v1.4's validation:
- 6 panels from 2023-2024 HG front-only
- 1 OOS panel from Feb-Mar 2026
- A second OOS panel pulled from 2026-04-xx paper-trade (corsair
  provides the fills.jsonl + quotes.csv once 2+ weeks of the
  tie_is_behind experiment have run — gives same-regime but
  realistic counterparty behavior)

### 6.2 Required metrics per (variant × panel)

Margin profile:
- Peak SPAN margin
- Median SPAN margin
- Time-at-ceiling-or-above (fraction of session)
- Margin-kill fire count
- Daily-halt fire count

Inventory profile:
- Peak |position| on any single strike
- Time-with-|position|≥3 on any strike
- Per-strike position half-life (how fast inventory decays)

P&L:
- Gross edge captured (sum of per-fill (fill_px − theo))
- Realized P&L (closed round-trips only)
- MTM change on open positions
- Net daily P&L
- Sharpe over panel

Fill characteristics:
- Fill count (close-side vs open-side)
- Per-fill edge (distribution; expect lower on close side)
- Fill-to-close-ratio = close fills / total fills

Adverse selection on the skew variants:
- Signed_bias on close-side fills (expect positive — we pay for closes)
- Forward move at t+60s conditioned on close fill (expect forward
  moved AGAINST us — the flow that hit our raised bid was informed
  short-pressure)
- Net: does the adverse movement on close fills exceed the margin
  relief value of closing?

### 6.3 Comparison format (required output)

Request a table like:

| Variant | Peak margin | Median margin | Ceiling% | Kill-fires | Net P&L | Sharpe | Close-fill AS |
|---|---|---|---|---|---|---|---|
| V0 | $42K | $29K | 18% | 0 | −$580 | +2.1 | — |
| V2 | $35K | $23K | 8% | 0 | +$120 | +3.4 | +$18/fill |
| V5 | $31K | $21K | 5% | 0 | +$340 | +4.1 | +$22/fill |
| V7 | $24K | $19K | 1% | 0 | −$200 | +1.8 | +$38/fill |

The pattern above is hypothetical but illustrates what decisions the
table should enable: find the knee where margin relief is strong but
close-fill AS isn't eating the gains.

---

## 7. Deployment gate (ex-ante skip rules prior)

Same methodology discipline as the hardburst-skip overlay brief.
**Default position: rejection.** At least one variant must pass ALL of:

- (a) Peak SPAN margin ≥ 15% reduction vs V0 baseline across ≥ 5 of 7
      panels (confirms margin relief is real, not panel-specific)
- (b) Net P&L neutral or positive vs V0 across ≥ 5 of 7 panels
      (confirms we're not just paying for capital efficiency with P&L)
- (c) Median close-fill signed_bias ≥ +0.20 (confirms the closes are
      "correctly informed" — we're paying for real insurance, not
      arbitrary premium)
- (d) No increase in daily-P&L-halt fire count vs baseline (confirms
      we're not trading margin-peak-reduction for tail increase)
- (e) Grid diversity: at least 2 variants pass (a)–(d), so the
      selected config isn't a lone overfit point

Failure on any criterion for any variant → do NOT ship any variant;
write methodology memo; cooling-off of 3 months before retry with
tweaked grid, same as hardburst-skip prior.

Tie-breaking among variants that pass:
1. Sharpe-per-parameter-count (penalize variants with more moving parts)
2. Simpler-is-better on structural tie (prefer linear to exponential,
   always-on to conditional, strike to expiry)

---

## 8. Expected outcome range

Setting expectations before results arrive (don't treat these as
targets; deviation is information):

- **Peak margin reduction:** 20–40% across variants V2–V5; 50%+ on V6–V7
- **Median margin reduction:** 15–30% across variants V2–V5
- **P&L impact:** −5% to +10% vs baseline. This is a defensive
  overlay; P&L is a *secondary* goal after margin efficiency.
- **Sharpe:** plausibly flat to +0.2 lift (small but positive via
  margin freeing capital for more fills)
- **Close-fill signed_bias:** strongly positive, +0.20 to +0.50.
  If it's near zero, the overlay isn't working (we're paying for
  nothing). If it's strongly negative, the overlay is making us
  close INTO informed counter-flow (bad).

Outcomes materially outside this range = investigate before acting.
Much smaller margin reduction than expected = backtest has a bug OR
the margin structure is less inventory-sensitive than we assume.
Much larger P&L impact (either direction) = mechanism we don't
understand.

---

## 9. Interface & handoff

### 9.1 Corsair provides (ready now)

- `logs/quotes.csv` — complete quote-decision history with 4dp prices
  from 2026-04-22 onward
- `logs-paper/fills-*.jsonl` — paper fills with theo_at_fill, margin
  trajectory, position_pre/post
- `logs-paper/margin_snapshots-*.jsonl` — SPAN margin @ 1min cadence
- `logs-paper/pnl_snapshots-*.jsonl` — P&L @ 30s cadence
- `logs-paper/requote_cycles-*.jsonl` — per-cycle instrumentation
  (cycle_id, orders_evaluated, orders_skipped_reasons, per-cycle
  latency) — useful for reconstructing "what would each variant
  have done at each decision point"

### 9.2 Corsair will ship (after variant selected)

- Config-gated knob: `inventory_skew.{k, shape, side, trigger, granularity}`
- Default: V0 (current behavior). Flag OFF in production until
  crowsnest hands back the validated variant selection.
- Implementation lives in `_process_side` in `src/quote_engine.py`
  (same hot-path location as the tie_is_behind knob from experiment B)
- Estimated: ~100 LOC code + config + unit tests. Same shape as
  the hardburst-skip overlay (see corsair-side brief 2026-04-21).

### 9.3 Crowsnest owns

- Simulator-side variant implementation (overlay on existing sim
  that replays fills under the skew-adjusted quote prices)
- Panel runs + metrics per §6.2
- Comparison table per §6.3
- Variant selection recommendation
- Writeup of results + per-variant analysis

### 9.4 Shared

- Decision on deploying any selected variant (requires joint sign-off
  on the validation gate §7)
- Integration with v1.5 spec (if a variant lands, it goes into the
  v1.5 defensive lever stack)

---

## 10. Non-goals / out-of-scope

- **Portfolio-level skew** beyond per-strike granularity — tested
  only as V10 in the grid; complexity not warranted for first pass
- **Greek-aware skew** (pulling portfolio delta/gamma/vega toward zero
  via pricing). Arguably a cleaner formulation but much more complex.
  Defer to v1.6+ research.
- **Dynamic skew** (k adjusts based on realized fill-rate feedback).
  Same — too complex for first pass.
- **Any change to §6.2 Tier-1 kills or §6.1 daily halt.** Inventory
  skew is ADDITIVE; the kill stack stays exactly as v1.4 specifies.
- **ETH or other products.** HG only. Any port to other products
  requires re-backtesting on that product's panel data.

---

## 11. Timeline request

Not urgent. This is a defensive lever for a problem we've already
got a direct observation of (2026-04-21) but can be kept off the
hot path while we prioritize:

1. The tie_is_behind (Experiment B) validation currently running on
   corsair-side paper
2. The min_edge-reduction experiment (Experiment A; pending Exp B
   settling)
3. The hardburst-skip overlay validation (2-4 week paper shadow)

Suggested crowsnest timeline:
- **Week 1:** variant grid scoping, simulator implementation
- **Week 2-3:** panel runs, metrics, validation-gate application
- **Week 4:** writeup + variant recommendation (or rejection memo)

No deployment-ready decision until both Experiment A and B have
settled, so crowsnest can see the post-experiment corsair baseline
and avoid layering overlays on a shifting baseline.

---

## 12. Change log & related artifacts

- **2026-04-21 internal post-mortem** (corsair session notes; not
  committed) — identified inventory-skew as recommendation #1
  after day's 3× 6.00P accumulation loss
- **2026-04-22 margin decomposition** (this session) — quantified
  94% margin concentration on 25% of gross contracts (3 short ATM
  puts of 12 total open)
- **Hardburst-skip overlay brief** (crowsnest 2026-04-21) —
  methodology template this brief follows
- **ex_ante_skip_rules_prior** (crowsnest research_principles/) —
  governing methodology for validation gate §7
- **v1.4 §6.4** — refuted per-strike position cap (distinct from
  this overlay; cap is binary, skew is continuous)

## Approval & sign-off

- Corsair execution: drafted 2026-04-22
- Crowsnest research: pending review

Questions during backtest to be routed via weekly check-in. Material
scope changes to the grid require amendment to this brief before
running.
