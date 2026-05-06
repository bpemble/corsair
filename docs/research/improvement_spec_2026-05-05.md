# Corsair v1.4 — Adverse-Selection Reduction & Profitability Spec

Date: 2026-05-05
Author: Quant research advisor session, paper-data review (logs-paper/2026-05-04, logs-paper/2026-05-05)
Status: Research handoff. Concrete recommendations for operator + crowsnest scoping.
Active spec: `docs/hg_spec_v1.4.md` (APPROVED 2026-04-19).
Active config: `config/runtime_v3.yaml`.
Hot path: `corsair-broker-rs` + `corsair_trader_rust` (Phase 6.7 cutover 2026-05-02).

## Executive summary

Live paper data (2026-05-05, n=430 option fills, n=659 SABR refits over a single
session) tells a sharp story:

1. **Quote pickoff at sub-place_ack latency is the dominant adverse-selection
   pattern.** 75% of option fills land within 1 second of place_ack (66% within
   100 ms, p50 = 40 ms). Our place_ack RTT is p50 56 ms / p99 ~1 s. The
   counterparty is hitting the quote within the same RTT envelope it took us
   to place it. This is not "wide-market drift fills"; it's "stale quote vs
   moving theo at sub-RTT scale". Treatment vector: replace polling-based
   refresh and the static 60 s SABR refit with an event-triggered partial
   refit (forward delta, ATM IV) on every underlying tick big enough to
   matter. **Top P&L lever in this report.**

2. **The SABR fit is materially unstable on the wings, even on clean fits.**
   Per-refit `nu` jumps up to **2.47** (vol-of-vol drifting from ~2 to ~5
   between two consecutive 60 s fits), `rho` jumps up to **+0.95**. RMSE is
   fine (median 0.0041) — the surface is locally well-fit but not temporally
   smooth. Wing theos move 5–20% between refits with no underlying news
   driving it. Treatment: switch to **eSSVI / SSVI calendar-arbitrage-free
   parameterization** with a **smoothness regularizer** on (ρ, ψ) between
   refits, OR enforce a Tikhonov penalty on `‖θ_t − θ_{t-1}‖` in the
   bounded-LM solver. Both approaches eliminate the wing-pickoff incident
   class without flattening the smile.

3. **The strike-window OTM+ATM filter is hardcoded against `snap.underlying_price`
   (spot), not `vp_msg.forward` (parity-implied F).** Today HGK6 spot and
   HGM6 parity-F differ by **~$0.05 (≈100 ticks)** consistently. The two
   anchors disagree by an entire nickel. Strike-window decisions, ITM-OTM
   classification, and the forward-drift guard all use spot — but pricing
   uses fit_F. This is a silent bias that systematically widens spreads on
   one side of the chain (whichever side the carry pulls). Treatment: route
   ALL quoting decisions through fit_F. Subtle but high-leverage.

4. **Order-flow signature is bursty and one-sided: 36% of option fills happen
   within 0.5 s of another option fill, BUYs lead SELLs 327:118.** This is a
   classic Hawkes self-excitation pattern in the fill stream. We have zero
   flow-toxicity feedback today (CLAUDE.md §16 dark-book guards are
   structural, not state-based). Treatment: a lightweight VPIN-style toxicity
   estimator on a 60 s rolling fill window, gated to widen the half-spread
   (or pull quotes) when toxicity exceeds a threshold. Implementable in
   <100 LoC of Rust.

5. **The 8 ATM fills net −$144K (out of +$152K total realized edge); 374
   wing fills net +$298K.** Two structural conclusions: (a) most of our edge
   IS being captured at the wings — the dark-book guard from §16 is doing
   its job there; (b) ATM is where adverse selection lives, and we're under-
   defended there because (i) ATM theo is most sensitive to fit_F and (ii)
   ATM gamma means a small underlying tick moves theo by more than a tick.
   The wide-spread skip (`skip_if_spread_over_edge_mul`) is currently
   **disabled** in production (`runtime_v3.yaml:74`, set to 0.0 with TEMP
   comment). Re-enabling it and restoring the spec's 4× multiplier protects
   exactly the ATM region.

Primary metric to track: **realized edge per fill, partitioned by (right,
side, |log-moneyness|, fit_age, fill latency from place_ack)**. The 6-tuple
is sufficient to retrospectively classify every fill as "earned" vs "picked
off" and feed any of the proposals below.

---

## Recommendation 1 — Event-triggered ATM IV bump between SABR refits

**Lane:** Vol modeling (applied math) / Order flow (signal processing).
**Status:** **needs backtesting** for the trigger threshold; the mechanism
itself is ship-ready and falsifiable.

### Problem (live evidence)

- Per `trader_events-2026-05-05.jsonl`: 659 SABR refits over the session.
  Forward jumps in a single 60 s window have median 0.8 ticks, p99 8.8
  ticks, max 16.2 ticks (= $0.008 in F). This is small but non-trivial,
  and it's **not the issue**.
- The issue is `nu` jumps: median 0.04, **p99 0.62, max 2.47**. `rho`
  jumps: median 0.006, **p99 0.088, max 0.95**. These are between two
  consecutive 60s fits with no obvious underlying news. The fitter is
  simply finding a different local minimum because three or four ATM
  IVs ticked.
- The trader's `MAX_FORWARD_DRIFT_TICKS` = 100 (was 200 until 2026-05-05;
  tightened today after exactly this issue, per `decision.rs:46-53`).
  But the median spot-vs-fit_forward in our run is **100.5 ticks**
  (the carry between HGK6 spot and HGM6 parity-F). The gate is operating
  at its threshold — false-positive city.
- Fill resting time after place_ack: **median 40 ms, 75% within 1 s**.
  Place_ack RTT: median 56 ms, p99 ~1 s. The market is hitting our
  quotes faster than we can refresh on a tick.

### Current implementation

`vol_surface.rs:90-101`: SABR refit on a fixed 60 s `tokio::time::interval`.
Trader's `decide_on_tick` reads the most recent surface from
`state.vol_surfaces` (decision.rs:206-228) and uses `vp_msg.forward` (the
fit-time F) as the anchor. Between refits, theo is recomputed but always
against the SABR params from the last fit.

### Why the current approach is sub-optimal

A fixed 60 s cadence is wrong for two distinct reasons:

1. **Compute-throughput sub-optimal**: most refits don't need to happen.
   When the underlying is quiet and quotes are stable, the previous fit
   is still valid; the refit burns ~5 ms of LM solver budget with no
   information change.
2. **Latency-of-information sub-optimal**: when the underlying moves >5
   ticks in a window, theo for ATM strikes (where vega and gamma are
   highest) is genuinely stale, and our quotes become picked-off-able.
   By the time the next 60 s fit lands, the move is over and we've
   already eaten the cost.

A fixed cadence is the wrong control structure. The right structure is
*variable-rate sampling*: refit when something changed enough to matter,
not before.

### Proposed alternative: event-triggered fast-path ATM IV update

**Two-tier cascade:**

1. **Slow path (existing 60 s SABR refit)**: produces a full
   (α, β, ρ, ν, F) per expiry, calibrated against the full strike chain.
   Source of truth for ρ, ν, F, β.

2. **Fast path (per-tick ATM IV update)**: on every underlying-tick or
   ATM-option-tick, compute a **delta-α adjustment** that re-anchors the
   slow-path surface to the current ATM mid:
   ```
   ATM_mid_iv_t = brent(black76(F_now, K_atm, T, σ) − ATM_mid_t = 0)
   target_ATM_iv_slow = sabr_iv(F_now, K_atm, T, α, β, ρ, ν)
   Δα = (ATM_mid_iv_t / target_ATM_iv_slow) − 1
   α_t = α_slow × (1 + Δα)
   F_t = F_now (use put-call parity at the ATM strike if both sides live)
   ```
   Compute cost: **one Brent solve (~3 µs) + one sabr_iv (~1 µs)**. Fits
   easily in the trader hot path. The skew/wing geometry (β, ρ, ν) stays
   anchored to the slow-path fit; only the level α drifts.

3. **Trigger gate on the slow-path refit**: skip the refit if both
   (a) |F_t − F_slow| < 5 ticks and (b) |α_t − α_slow| / α_slow < 5%.
   This eliminates wasted refits in calm markets, freeing the broker
   60 s tick to refit MORE eagerly when conditions warrant.

### Literature anchors

- **Andreasen-Huge (2011) "ZABR — Expansions for the Masses"**: the
  one-step calibration with ATM-anchor + skew structure decoupling is
  the modern standard for fast vol-surface refresh.
- **Carr-Wu (2016) "Static Hedging of Standard Options"**: the
  parsimonious "smile-anchor + wing-multiplier" decomposition Δα uses
  is the same idea applied to dynamic refresh.
- Hagan SABR (2002) is what Corsair already uses; the α-rescale is
  the "ATM correction" that any Hagan implementation has at hand.

### Expected P&L impact

Best case: drops the 8 ATM fills with mean −$18K each (= $144K loss) by
having current-tick ATM IV available within ~5 µs of the underlying
movement, well inside the place_ack RTT envelope.
Expected case: cuts ATM adverse selection by 30–50% (the share that's
attributable to *stale ATM IV*; the rest is genuine information
asymmetry). Translates to ~$50–80K saved over the equivalent of one
session like 2026-05-05.
Worst case: the Δα update is itself noisy (if ATM mid jumps from a
single trader's IOC), drives spurious quote refreshes, increases place
volume by ~2× without earning any additional edge. Falsifiable: monitor
`place / fill` ratio and `place_total_per_session`. If `place_total_per_
session` triples and per-fill edge stays flat, the proposal is wrong.

### Spec deviation implications

v1.4 §4 specifies "Theo refresh: every quote cycle. SABR parameters
update at ≤60s cadence; theo recomputes on every param update or
forward tick (whichever first)." This proposal IS the spec — it ships
the "or forward tick" clause that's currently missing. Expected: no
spec change. Document as `§ 4.A — fast-path ATM IV update` deviation
note.

Interacts with §16 Layer 5 (forward-drift guard): if Δα is live, the
spot-vs-fit_F drift becomes much less load-bearing, since theo is
already accounting for it. Recommend keeping the guard but loosening
it to 200 ticks once Δα is in place.

### Validation plan

1. **Backtest panel** (crowsnest-side): re-run the OOS 2026 panel with
   the simulator implementing Δα adjustment between fits. Compare
   per-fill realized edge (close at next mid + 30 s) and tail loss.
   Pass criterion: ATM adverse rate (fills with edge_usd < 0 in the
   |lm|<0.01 bucket) drops by ≥30%, total edge improves by ≥$0.
2. **Shadow run**: log Δα and the resulting α_t to a new JSONL stream
   for one paper session WITHOUT acting on it. Verify Δα magnitude
   stays bounded (|Δα| < 20% of α, 99% of the time) and that α_t is
   well-correlated with what the next slow-fit produces.
3. **Live-paper A/B**: 1-week paper deployment with effective_delta
   gating off (CLAUDE.md §14 fallback) to isolate the change from
   §10/§14 regression risk. Treatment: Δα active. Control: Δα off.
   Both run on alternate hours.

### Implementation sketch

In `corsair_trader/src/decision.rs::compute_theo`:
```
fn compute_theo(forward_now, strike, tte, right, params, atm_iv_now)
 -> (iv, theo) {
    if let Some(atm_iv_obs) = atm_iv_now {
      let atm_iv_slow = sabr_iv(forward_now, atm_strike(forward_now),
                                 tte, params.alpha, params.beta,
                                 params.rho, params.nu);
      let dalpha = (atm_iv_obs - atm_iv_slow) / atm_iv_slow;
      let alpha_eff = params.alpha * (1.0 + dalpha.clamp(-0.20, 0.20));
      let iv = sabr_iv(forward_now, strike, tte, alpha_eff,
                        params.beta, params.rho, params.nu);
      return (iv, black76(forward_now, strike, tte, iv, right));
    }
    // fall back to current path
}
```
ATM IV is computed once per tick at the broker side (already has
chain mids), shipped on the same `tick` IPC as a new `atm_iv_now` field.

### Open questions / data needs

- ATM strike pinning: which strike counts as "the ATM" for the Δα
  reference? Likely `round(F_now/0.05)*0.05`, but at the strike-rolling
  boundary (F crosses a nickel), Δα would jump discontinuously. Need
  a smoothed reference (linear interp between the two nearest nickels).
- What's the *lower* bound on Δα magnitude where we should NOT update?
  Empirically <5% is just noise from BBO half-tick jitter. Need
  histogram of Δα observed in shadow run before setting threshold.

---

## Recommendation 2 — Smoothness penalty on consecutive SABR fits

**Lane:** Vol modeling (applied math).
**Status:** **needs backtesting**. Mechanism is well-established; the
penalty weight is the only knob that needs tuning.

### Problem

Per-fit jumps of `rho` up to 0.95 and `nu` up to 2.47 between consecutive
60s fits (logs-paper/2026-05-05). Each refit independently solves an LM
problem with no memory of the previous solution. When the residual surface
has multiple local minima of similar cost, the LM picks whichever is
closer to its initial guess — and CLAUDE.md / `calibrate.rs:320-325` shows
4 fixed initial guesses with no prior-aware seeding.

A jump from `rho=−0.5` to `rho=+0.4` between two 60 s fits means deep-OTM
call wings that were trading rich one minute become trading cheap the
next. Our quotes sit in those wings unmoved (deal-band gates don't
trigger because theo barely moved at the strike — but the *forward
generalization* of the surface to other strikes is wildly different).
This is exactly how SELL-CALL @ 5.95 fills get edge=−$1683 (worst on
2026-05-05, theo flips from 0.10 to 0.04 between fits).

### Current implementation

`calibrate.rs::calibrate_sabr` runs LM from 4 fixed initial guesses,
picks lowest cost, applies a flat `max_rmse` filter. No prior. No
warm-start from previous fit.

### Why sub-optimal

Bounded-LM is a local optimizer. With ATM-only data (which is most of
our liquid quote book), the (β=0.5, ρ, ν) subspace has a manifold of
near-equivalent solutions — a small change in one ATM IV can flip the
solver between two valid fits with very different wing predictions.
This is well-documented for SABR (cf. Lesniewski 2008 "SABR for the
Impatient", §5).

### Proposed alternative

**Tikhonov-regularized warm-start:**

Add to the LM residual vector a smoothness term:
```
r_smooth = λ × diag(w_α, w_ρ, w_ν) × (θ - θ_prev)
```
where `θ_prev` is the previous accepted fit (per expiry), `w_*` are
parameter-specific scales (e.g., `w_α = 1/0.1`, `w_ρ = 1/0.3`,
`w_ν = 1/1.0` to put α, ρ, ν on comparable units), and λ is the
penalty weight.

The LM solver already supports residual stacking — this is a one-line
change in `calibrate_sabr`. The smoothness term costs zero extra
function evaluations.

**λ tuning:** the right scale is "what one full ν jump costs in IV
space." Empirically: `Δν = 1.0` → ATM IV change ~0.5%. λ should be
chosen such that `λ × Δθ` is about half a typical residual magnitude
(`rmse ~ 0.005`). Plausible λ ≈ 0.005 for default setting; needs
backtest.

**Alternative (cleaner, more theory-grounded):** SSVI parameterization
(Gatheral-Jacquier 2014, "Arbitrage-free SVI volatility surfaces").

### Literature anchors

- **Gatheral & Jacquier (2014)** — arbitrage-free SVI. The eSSVI
  parameterization in (ρ_t, ψ(θ_t)) is explicitly designed to be
  smooth in time AND arbitrage-free at every t.
- **Lesniewski (2008)** — SABR ill-posedness on ATM-only data.
- **Carr & Pelts (2018) "Stochastic Volatility for the Masses"** —
  consistency conditions for sequential calibration of SVI/SABR.

### Expected P&L impact

Best case: eliminates the wing-rho-flip incident class; saves ~$5K
worst-case per session (from the 2026-05-05 worst fills, mostly
attributable to ν / ρ flip).
Expected case: +5–10% on per-fill edge for non-ATM strikes from
better wing pricing. Modest but consistent.
Worst case: λ too high → fits lag real regime shifts (e.g. an
overnight gap), and the surface stays anchored to yesterday for
the first 30 minutes of the new session. Falsifiable: regression
on rmse during regime shifts (e.g., daily session transitions);
if rmse stays >0.01 for >5 minutes after a session start, λ is
too tight.

### Spec deviation implications

None at v1.4 level. Internal to broker's vol_surface fitter.

### Validation plan

1. Backtest: replay 2026-05-04, 2026-05-05 chain data through
   the broker's fitter with various λ ∈ {0, 0.001, 0.005, 0.01, 0.05}.
   Measure: (a) median |Δrho|, |Δnu| between consecutive fits;
   (b) sum of |theo(t) − theo(t-60s)| at every quoted strike.
   The sweet spot is where (a) drops by ≥50% and (b) drops by
   ≥30% with no rmse degradation >25%.
2. Live shadow: deploy with λ active, log new params alongside
   what would have been the un-regularized fit. Run for one
   week; compare implied per-fill edge in retrospective sim.

### Implementation sketch

In `corsair_pricing/src/calibrate.rs::calibrate_sabr`:
```
pub fn calibrate_sabr_warm(
    f, t, strikes, market_ivs, weights,
    beta, max_rmse,
    prior: Option<(alpha_prev, rho_prev, nu_prev)>,
    lambda_smooth: f64,  // 0 = no penalty, ~0.005 default
) -> Option<SabrFit> { ... }
```
Add penalty terms to the residual closure inside the LM call. Persist
last fit per `(product, expiry)` in `vol_surface_cache` (already
exists).

### Open questions

- Is β fixed at 0.5 the right anchor? Hagan suggests β=1 for
  log-normal regimes (currencies) and β=0 for normal regimes
  (rates near zero). HG copper at $5–6 in a high-vol environment
  is closer to log-normal — β=0.7–0.8 may be a better anchor.
  Worth a separate one-off backtest sweep.

---

## Recommendation 3 — Route ALL strike-window decisions through fit_F, not spot

**Lane:** Vol modeling / Risk systems.
**Status:** **ship-ready** (deterministic improvement, observable bug).

### Problem (smoking gun)

`decision.rs:137-181`:
```
let forward = snap.underlying_price;       // <-- spot of HGK6 (front)
...
if (strike - forward).abs() > MAX_STRIKE_OFFSET_USD { skip; }  // ATM window
if r_char == 'C' && strike < forward - ATM_TOL_USD { skip; }   // ITM-call gate
if r_char == 'P' && strike > forward + ATM_TOL_USD { skip; }   // ITM-put gate
...
let fit_forward = vp_msg.forward;          // <-- parity-F of HGM6 (option underlying)
let drift = (forward - fit_forward).abs(); // spot-vs-fit drift
```

**The two `forward` values systematically differ by ~$0.05 (≈100 ticks)**
because vol_surface.rs:226-269 derives F via put-call parity from the
**option mids** — i.e., the HGM6 carry-implied F. The trader's
`snap.underlying_price` is the futures tick of HGK6 (front-month).

This means:
- The ATM-window restriction (MAX_STRIKE_OFFSET_USD = 0.30) is anchored
  ~$0.05 too low (or too high, depending on contango sign). Strikes that
  ARE inside the ±0.30 window of the option's true forward get rejected.
- The ITM-call gate (skip if `K < spot − 0.025`) is rejecting calls
  that aren't actually ITM in the option's pricing world — it's
  rejecting calls with `K < spot − 0.025` while the put-call-parity
  forward is `spot + 0.05`, so the call at `K = spot − 0.025` is
  actually OTM by 7.5 ticks.
- The forward-drift guard `|spot − fit_F| > 100t` is operating at
  exactly its threshold (median drift = 100.5t), which is why the gate
  was tightened from 200→100 today (`decision.rs:46-53`) and is
  presumably still firing constantly.

This is the v3 echo of CLAUDE.md §16's root cause (passing current spot
to SVI's compute_theo — fixed for theo, NOT yet fixed for the gates).

### Current implementation

The trader stores `underlying_price` (spot, from underlying_tick events)
and `vp_msg.forward` (parity-F, from vol_surface events) as separate
quantities. Pricing uses fit_F (correct). Gating uses spot (incorrect).

### Why sub-optimal

- Strike asymmetry: with HGM6 contango, the chain's ATM is at a higher
  K than HGK6 spot. Quoting in `K ∈ [spot−0.30, spot+0.30]` means we
  miss strikes near the put-call-parity ATM, while including strikes
  that are deep ITM in the option's pricing.
- Wider RMSE on the wings the trader DOES quote, because fit_F is
  the IV anchor; quoting strikes far from fit_F means quoting where
  SABR extrapolation is least reliable.
- The ITM-call/put gate is fundamentally checking "is this strike on
  the wrong side of *spot*"; what we want is "is this strike on the
  wrong side of *option-pricing forward*."

### Proposed alternative

Replace `snap.underlying_price` with `vp_msg.forward` for ALL gating
decisions. Keep `snap.underlying_price` for the forward-drift guard
ONLY (whose job IS to detect spot vs fit divergence). Equivalently:

```
let pricing_forward = vp_msg.forward;
let spot = snap.underlying_price;

// ATM window — anchored on pricing forward
if (strike - pricing_forward).abs() > MAX_STRIKE_OFFSET_USD { skip; }

// OTM-only — anchored on pricing forward
if r_char == 'C' && strike < pricing_forward - ATM_TOL_USD { skip; }
if r_char == 'P' && strike > pricing_forward + ATM_TOL_USD { skip; }

// Forward-drift guard — uses BOTH (this is its job)
if (spot - pricing_forward).abs() > MAX_FORWARD_DRIFT_TICKS * tick { skip; }
```

The forward-drift guard then becomes meaningful: if spot-vs-fit_F
exceeds the threshold, it's because the chain's parity-F has stopped
tracking spot reliably (broker's fitter sanity-check at vol_surface.rs:262
already falls back to spot when sample count < 2; the gate fires when
parity is breaking down).

### Literature

This is foundational microstructure: futures-options pricing is anchored
on the *underlying-of-the-option*, not the *front-month-future*. Hull
chapter 17 (futures options) makes the distinction explicit; CME's
SPAN documentation does too. Not novel — just a bug fix.

### Expected P&L impact

Hard to quantify cleanly, but bounds: today's strike scope is
asymmetric by exactly the carry. With fit_F anchoring, we'd be
quoting a different (and structurally more correct) chunk of the
chain. Expected: 5–15% improvement in fill rate AND per-fill edge,
especially for the right-side strikes (whichever side the carry
puts farther from spot).

### Spec deviation implications

This is a bug-fix, not a deviation. v1.4 §3.2 specifies "Symmetric ATM ± 5
nickels" without specifying anchor; the natural reading is "anchored on
the option's pricing forward." Spec doesn't change.

Interacts with CLAUDE.md §16 Layer 3 (strike window restrictions) by
correcting the anchor — strengthens the protection.

### Validation plan

1. Replay logs-paper/2026-05-05 with the patch applied, compute the
   set of (strike, side, tick) triples that would have decided
   differently. Spot-check a sample to confirm the new behavior is
   correct.
2. Backtest on 2026-Q1 and 2026-Q3 panels — measure per-fill edge
   change, fill count change, and whether the imbalance (BUY:SELL,
   currently 327:118 = 2.77×) drops.
3. Live shadow: a separate flag-gated branch logging "would-have-
   skipped-but-did-place" and "would-have-placed-but-did-skip" to
   confirm the count of differing decisions is non-zero before
   shipping.

### Implementation sketch

`corsair_trader/src/decision.rs::decide_on_tick`: lift `vp_msg`
acquisition above the strike-window gates; rename the local `forward`
to `pricing_forward` from `vp_msg.forward`; keep `spot =
snap.underlying_price` for the forward-drift guard at line ~263.

Touchpoints: line 137-181 (gating block), line 263-274 (drift guard).
~10 LoC delta.

### Open questions

- The `vp_msg` is per-expiry; if multiple expiries exist (Stage 1
  is front-only, but Stage 2+ may add), the per-expiry F differs
  by carry. Already handled (vol_surface lookup is keyed on expiry).
- Boot before first vol_surface arrives: trader has no fit_F. Today's
  code returns early on missing surface (`skip_no_vol_surface`),
  which is fail-safe — keep that behavior.

---

## Recommendation 4 — Re-enable wide-spread skip; restore §3.4 spec multiplier

**Lane:** Risk systems / Hedging.
**Status:** **ship-ready** (deterministic improvement, restoring spec).

### Problem

`runtime_v3.yaml:74`:
```
skip_if_spread_over_edge_mul: 0.0   # TEMP 2026-05-04 — was 4.0;
                                     # disabled for latency measurement
```

Spec v1.4 §3.4 specifies `skip_if_spread_over_edge_mul = 4.0` (skip if
half-spread > 4 × min_edge = $0.004). The TEMP comment says paper
spreads are 18–28 ticks at this hour and the gate was filtering every
quote, so it was disabled for **latency measurement**. That measurement
is presumably done.

This is the ATM-pickoff defense. When half-spread is wide, our theo±edge
target lands deep inside the BBO; our quote becomes the most aggressive
in the book; market taker hits us on the mean-reversion of the wide
spread. This is exactly the pattern of the 8 ATM fills with mean −$18K
edge each on 2026-05-05.

### Current implementation

`decision.rs:386-396`: gate is wired but disabled by config (`mul = 0`).
Spec says `mul = 4`.

### Why sub-optimal

The wide-spread skip is the **only structural defense at the ATM**:
- ATM-window (±$0.30) is a strike filter, not a market-state filter
- Dark-book guard fires on bid=0 or size=0; doesn't fire on a wide
  but two-sided market
- Forward-drift guard fires on spot-vs-fit; doesn't fire on a single-
  strike spread blow-out
- Effective-delta gate (CLAUDE.md §14) is portfolio-level, not
  quote-level

The `skip_if_spread_over_edge_mul` IS the per-strike, per-tick
quote-aggressiveness governor. With it disabled, our quote is the
inside of the spread for as long as the spread is wide.

### Proposed alternative

1. Restore `skip_if_spread_over_edge_mul: 4.0` for production.
2. Make it dynamic: `mul = max(4.0, k_vol × realized_vol_60s)` where
   `k_vol` is calibrated. When realized vol spikes, the threshold
   widens (we tolerate wider books). When markets are quiet, the
   threshold tightens. Avoids the all-or-nothing "filter every
   quote" failure mode the operator hit on 2026-05-04.
3. Alternative finer-grained gate: per-side wide-spread check — skip
   the side where the half-spread (mid − bid for SELL, ask − mid for
   BUY) exceeds the threshold, while keeping the other side. Today's
   gate is symmetric.

### Literature

- **Avellaneda-Stoikov (2008)**: optimal market-making with a
  reservation price proves that as inventory and vol increase, the
  optimal half-spread widens; quoting inside a wide BBO when our
  own optimal half-spread is even wider is dominated.
- **Glosten-Milgrom (1985)**: in the presence of informed traders,
  market makers must widen quotes to break even; the BBO width is a
  noisy signal of information asymmetry.

### Expected P&L impact

Best case: kills the 8 ATM picks (−$144K) on a session like 2026-05-05.
Expected case: drops the worst-edge tail by 50% with marginal hit to
fill volume. Fill volume DOES drop (these aren't free skips), but
realized edge per fill goes up by more than enough.
Worst case: in a chronically-wide HG environment (e.g. overnight Asia
hours), gate fires 100% and we make zero. Per the operator note that
already burned us — needs the dynamic-mul mitigation.

### Spec deviation implications

Restores spec §3.4. Removes a current deviation.

### Validation plan

1. Re-run 2026-05-05 paper with `mul = 4.0` and check whether the 8
   ATM losing fills would have been skipped. If yes → ship.
2. Live for 1 session with metrics: count of `skip_wide_spread`
   firings, fill count, mean realized edge. Compare to today's
   session. Pass: edge per fill up >20%; fill count not down by
   more than 30%.

### Implementation sketch

Step 1 (immediate): edit YAML to 4.0, remove TEMP comment.
Step 2 (follow-up): rolling 60s realized vol of underlying tracked in
the broker, shipped on `risk_state` IPC, consumed in
`compute_risk_gates`. New `dynamic_mul` field on `ScalarSnapshot`.

### Open questions

- What's the right `k_vol`? Empirically, HG ATM realized vol over 60s
  varies between 0.0001 and 0.001 (5 bps to 50 bps in F space).
  Half-spread ranges from 1 to 10 ticks. Plausible: `k_vol` ≈ 1000.
  Backtest sweep needed.

---

## Recommendation 5 — Inventory-skew pricing (per the existing crowsnest brief)

**Lane:** Inventory management & quoting.
**Status:** **needs backtesting**. Brief
`docs/briefs/crowsnest_inventory_skew_research_2026-04-22.md` already
exists; this entry confirms the operator's hypothesis with current data
and recommends prioritizing the backtest.

### Problem (live confirmation)

The existing brief (Apr 22) called this from a single-day −$1,234
post-mortem. Today's data confirms:

- **BUY-PUT 280 fills, total realized edge −$568K** (mean −$2,030;
  outliers excluded mean +$20). Heavy short-put accumulation.
- **BUY-CALL 39 fills, total −$284K** (mean −$7,281). Same problem,
  call side, smaller volume.
- **SELL-PUT 46 fills, total +$576K**, **SELL-CALL 78 fills, total
  +$429K**. The SELL side is profitable on average.
- The 2.77:1 BUY:SELL imbalance (327:118) means we accumulate net
  short-options inventory steadily, with no quote-side response.

A pricing-side response (inventory skew) is the right lever per the
brief's argument (§3 of that brief).

### Current implementation

None. `decision.rs:392-435` computes target = theo ± edge with no
inventory dependency.

### Why sub-optimal

The marginal value to the strategy of closing one short ATM put is
~$13,000 of margin freed (per the Apr 22 brief §1) — vs ~$12.50 cost
per tick of edge given up. The 1000:1 ratio means we should be willing
to give up multiple ticks of edge to get back to flat. Currently we
give up zero.

This is well-understood theory (Avellaneda-Stoikov reservation price,
Cartea-Jaimungal-Penalva chapter 10). Corsair's lack of any inventory
response is a clear deviation from optimal MM — the operator's note
in §12 even lists it as an accepted deviation pending the
`inventory_balance` gate.

### Proposed alternative

Endorse the existing brief. Specific recommendations to the researcher
on top of what the brief specifies:

1. **Add per-fill latency-from-ack as a feature in the backtest.** Today's
   data shows 75% of fills land within 1 s of place_ack — these are
   different in kind from fills that land 30+ seconds later (a longer-
   resting quote that finally got hit by spontaneous flow). The
   inventory-skew lever may have very different impact on the two
   populations. The brief currently doesn't disaggregate.
2. **Use the Avellaneda-Stoikov reservation price form** rather than
   ad-hoc tick-shift. Concretely:
   ```
   reservation = theo - q × γ × σ² × (T − t)
   half_spread_ask = γ × σ² × (T − t) / 2 + (1/γ) × ln(1 + γ/k)
   target_ask = reservation + half_spread_ask
   target_bid = reservation - half_spread_bid
   ```
   where `q` is current inventory in contract-deltas, `γ` is risk
   aversion, `σ` is realized underlying vol, `k` is fill-intensity
   parameter. This is theory-grounded and gives free skew direction.
3. **The brief's `A-both` variant is the right primary backtest** —
   when short, raise BOTH bid (close-aggressive) AND ask (don't add
   shorts). This matches the A-S form above.

### Literature anchors

- **Avellaneda & Stoikov (2008) "High-Frequency Trading in a Limit
  Order Book"** — the canonical reference.
- **Cartea, Jaimungal & Penalva (2015)** Chapter 10, especially
  10.2.3 "Optimal posting strategies with inventory."
- **Guéant, Lehalle & Fernandez-Tapia (2013) "Dealing with the
  inventory risk"** — explicit closed-form for the high-volume
  asymptotic.

### Expected P&L impact

The brief's §1 estimates margin-relief economics; per current data,
expected case is +$50–150K/session in the imbalanced regimes (where
inventory skew would have prevented adverse-direction accumulation),
neutral in the calm regimes.

### Spec deviation implications

This adds a new section to v1.4. Per the brief §6, requires
crowsnest research validation (4-month panel sweep). Not a one-off
operator decision.

### Validation plan

Per the brief §7. Highlight: the brief specifies a 4-month panel.
Add to the brief: include the BUY:SELL imbalance metric in
the panel summary; today's 2.77× is the regime we're trying to
defend against, and the variant must demonstrably reduce it
(target: <1.5× imbalance, ratio of |edge_buy_total| / |edge_sell_total|
toward 1.0).

### Implementation sketch

Per the brief §6. The key new logic in `decide_on_tick` is:
```
let pos = portfolio.position_for_strike(strike, expiry, right);
let skew_bid = if pos < 0 { abs(pos) * k_skew * tick_size } else { 0 };
let skew_ask = if pos < 0 { abs(pos) * k_skew * tick_size } else { 0 };
target_bid = theo - edge + skew_bid;
target_ask = theo + edge + skew_ask;
```
Trader doesn't currently see per-strike position. Need to extend the
`risk_state` IPC event with a per-(strike, expiry, right) position
map, OR have the trader infer from its own fill stream (less reliable
across restarts).

### Open questions

- Granularity (per-strike vs per-greek, brief §4.5) — the brief
  recommends P-strike for simplicity. Suggest backtesting both:
  P-strike will work locally; P-greek (skew toward net-delta-zero or
  net-vega-zero) handles the cross-strike gamma stress.

---

## Recommendation 6 — Lightweight VPIN-style flow toxicity gate

**Lane:** Order flow / signal processing.
**Status:** **needs backtesting** for thresholds; mechanism is well-known
and ship-ready in form.

### Problem

`trader_events-2026-05-05.jsonl`: 36% of option fills happen within
0.5s of another option fill. 75% of fills land within 1s of place_ack.
This is a **Hawkes self-excitation pattern** — fills are clustered in
time, not Poisson. Such clustering is the standard signature of an
informed-trader sweep. We have no detector and no response.

### Current implementation

None. `decide_on_tick` has no flow-state input.

### Why sub-optimal

Glosten-Milgrom equilibrium says spread = adverse-selection cost +
operating cost. Without a toxicity estimator, we're quoting at a fixed
spread regardless of the inferred information asymmetry of the
counterparty pool.

### Proposed alternative — VPIN with simplifications

**VPIN (Easley-Lopez-de-Prado-O'Hara 2012):**
```
VPIN_t = (1 / nV) × sum over last N volume-buckets of |buy_vol - sell_vol|
       = expected probability that the next bucket is informed
```
Adapted for our use:
```
toxicity_60s = sum over last 60s of (|fills_BUY - fills_SELL| × vol_imbalance_weight)
              / total_fills_60s
```
Trip thresholds:
- `toxicity_60s > 0.3`: widen `min_edge` by +1 tick on both sides for
  the next 30 s (ride out the burst)
- `toxicity_60s > 0.5`: cancel all resting quotes and re-quote at
  +2 tick edge (full pull)
- `toxicity_60s > 0.7`: 5-minute cooling-off, no quoting

These thresholds need calibration on backtest data; what's described is
the structure. **The 36% within-0.5s clustering observed today corresponds
to roughly 0.6 toxicity** (half the clusters single-sided per
2026-05-05 data) — the structure is clear, the right cutoffs need to
be tuned.

### Literature

- **Easley, Lopez de Prado & O'Hara (2012) "Flow Toxicity and Liquidity
  in a High-Frequency World"** — the canonical VPIN paper. Caveats
  (Andersen-Bondarenko 2014) apply but VPIN remains the right cheap
  baseline.
- **Bacry, Mastromatteo & Muzy (2015) "Hawkes Processes in Finance"** —
  the multivariate Hawkes self-excitation model. Probably overkill
  for v1; VPIN approximates the marginal effect.
- **Cartea-Jaimungal (2018) "Algorithmic and HF Trading"** chapter 6
  applies these to MM directly.

### Expected P&L impact

Best: cuts the 36% bursty-fill share of edge by 50% by quote-pulling
during informed sweeps. Roughly $100K saved on a worst-day-like
2026-05-01 (CLAUDE.md §16's 17-fills-in-11s burst).
Expected: 5–15% improvement on per-fill edge, modest on session level.
Worst: false-positive widening shrinks fill count without commensurate
edge gain. Falsifiable: A/B with VPIN active vs not, paper, 1 week.
If realized edge per session not statistically different, kill it.

### Spec deviation implications

New section in v1.4 (§7 operational kill switches has the right slot —
"abnormal trade-rate" is already specified at >10× rolling baseline,
which is a coarse cousin). Tighten that gate from ">10× → halt" to
"VPIN-bucketed → graduated response."

### Validation plan

1. Backtest: replay logs-paper data through the simulator with VPIN
   applied. Use 2024 panels to calibrate thresholds, OOS-2026 to
   validate. Pass: realized edge per session ≥ baseline + 0%; tail
   loss ≤ baseline − 25%.
2. Shadow mode: 1 week paper, log VPIN value alongside actions but
   don't act. Confirm the threshold buckets capture the expected
   share of session time.
3. A/B paper: 1 week with treatment hours vs control hours.

### Implementation sketch

In `corsair_trader/src/state.rs`: add `flow_toxicity: AtomicU64` (×1000
for fixed-point). Background task ticking every 5 s computes VPIN
from a ring buffer of (fill_ts, fill_side, fill_size). `decide_on_tick`
reads via `Ordering::Relaxed`. ~80 LoC. Fits the latency budget.

### Open questions

- Should toxicity be per-strike or book-wide? Per-strike is more
  precise but suffers from sample sparsity (most strikes have
  <5 fills/hour). Book-wide is robust but loses signal localization.
  Likely book-wide for v1, per-(call/put) refinement in v2.
- Volume bucketing vs time bucketing: paper with thin volume,
  time-bucketing (60 s window) is more reliable than fixed-volume
  buckets.

---

## Recommendation 7 — Hedge: vol-adaptive tolerance band (impulse control)

**Lane:** Hedging (control theory).
**Status:** **needs backtesting** for the calibration; mechanism is
canonical (Davis-Norman / Dumas-Luciano).

### Problem

`runtime_v3.yaml:79`: `tolerance_deltas: 0.5`. Fixed band, regardless
of regime. Hedge cadence 30 s + on-fill, IOC ±4 ticks (bumped from 2
on 2026-05-04 because thin HG futures + ~140 ms RTT killed half the
±2-tick IOCs).

The spec says ±0.5 deltas. Whether that's optimal depends on:
- HG futures bid-ask cost (~1 tick = $12.50 per contract)
- Underlying vol (drives delta drift between rebalances)
- Option fill rate (drives delta jumps from new positions)

A fixed band can't be optimal across the regime distribution.

### Current implementation

`corsair_hedge/src/manager.rs::maybe_rebalance`: if
`|effective_delta| ≤ tolerance_deltas`, return Idle.

### Why sub-optimal

Davis-Norman (1990) and Constantinides (1986) showed in a portfolio
context that the optimal no-trade band scales like:
```
band ≈ (γ × σ² × cost / wealth)^(1/3)
```
where γ is risk aversion, σ is underlying vol, cost is the ratio
transaction-cost-per-unit / mark-value-per-unit. As σ goes up, the
band SHOULDN'T scale linearly (you'd hedge too rarely); it scales as
σ^(2/3). Same for cost.

Real-time impact: when HG vol spikes from 25% to 40% (e.g., FOMC week),
delta drifts 60% faster but our band is unchanged → we under-hedge
by ~40%. Same direction in the opposite regime: when HG is calm (15%
vol), band is too tight → we over-hedge, paying transaction costs
on noise.

### Proposed alternative

Replace fixed `tolerance_deltas` with:
```
tolerance_deltas_t = base_band × (σ_realized_60s / σ_base)^(2/3)
                                × (cost_realized / cost_base)^(1/3)
```
- `σ_base` = HG long-term vol ≈ 25%
- `cost_base` = 1 tick at $12.50, mark-value at F × 25000 ≈ $150k →
  cost ratio 8e-5
- `base_band` = 0.5 (the current value, calibrated for the base regime)
- realized cost = current bid-ask spread on the hedge contract

This gives band ∈ [0.3, 1.5] roughly across observed HG vol regimes.
Tighter in calm markets (we hedge precisely), looser in volatile ones
(we hedge less often, accept more delta drift).

### Literature

- **Davis & Norman (1990) "Portfolio Selection with Transaction
  Costs"** — original cube-root law.
- **Whalley & Wilmott (1993)** "Optimal Hedging with Transaction
  Costs" — applied to options-MM exactly.
- **Cartea-Jaimungal-Penalva chapter 10** has the simplified form.

### Expected P&L impact

Best: cuts hedge slippage by 30% in calm regimes (40+ minutes/day in
HG paper), saves ~$30/contract × 40 hedges/day = ~$1200/day
overlooked.
Expected: +$50K/year on hedging cost line. Modest absolute, large
relative to the $40-60/option fill estimate in spec §5.
Worst: poor σ_realized estimate at session boundaries widens band
when it shouldn't, drift exceeds delta_kill, halts session. Mitigation:
clamp band to [0.3, 2.0] — never wider than the current delta_kill
backstop.

### Spec deviation implications

v1.4 §5 specifies "Hedge tolerance band: ±0.5 contract-deltas." This
is a deviation. Document with backtest evidence and clamp.

### Validation plan

1. Backtest: replay 2024 + 2026 panels with the dynamic band,
   measure: (a) hedge transaction cost / day, (b) max
   |effective_delta| achieved, (c) delta_kill firing rate,
   (d) MTM drag from delta drift. Pass: (a) drops by ≥20%, (b)
   does not exceed 4.5 (i.e., we never get within 0.5 of the kill),
   (c) no increase, (d) no increase.
2. Live shadow: 1 session, log dynamic band and what would-have-been
   the static-band trades, compare.

### Implementation sketch

`corsair_hedge/src/manager.rs::maybe_rebalance`: replace
`self.cfg.tolerance_deltas` with a `dynamic_tolerance` getter that
reads (a) realized 60s vol from a new field on the runtime, (b)
hedge spread cost from the hedge contract's bid-ask in market_data.
Smooth via EWMA. ~50 LoC.

### Open questions

- σ_realized estimator: which window? 1 minute is too noisy, 1 hour
  is too slow. Suggest 5-minute EWMA with half-life 90 s.
- Interaction with §10 reconciliation: if hedge_qty drifts from
  IBKR view, the dynamic band might fire spurious trades. Recommend
  *only enabling dynamic band after a recent (within 30 s) reconcile
  match*. Otherwise, static fallback.

---

## Recommendation 8 — Vega throttle (NOT halt) replacing the disabled vega_kill

**Lane:** Risk systems.
**Status:** **needs backtesting**. Aligns with §13's evidence on why
the current disable was correct.

### Problem

CLAUDE.md §13 disabled vega_kill (set to 0) on 2026-04-23 because the
$500 threshold bound on 67–88% of fills — operationally a choke, not
a tail backstop. Rescaling to $3000 fixed OOS but broke 2024 Q3.
Disabled is the only choice that's clean across panels, BUT this
leaves us with NO vega defense between "fine" and "−5% daily P&L
halt." That gap is wider than ideal.

### Current implementation

`runtime_v3.yaml:38`: `vega_kill: 0` (disabled). Falls to daily-PnL
halt at $25K loss.

### Why sub-optimal

Tier-1 kills are binary (fire / don't fire). Vega exposure is
continuous. A continuous throttle would let us SCALE QUOTING INTENSITY
as a function of vega — wider spreads when vega is high, narrower
when low — without ever firing a halt.

This matches the §16 design philosophy: graduated defense, not
binary. The dark-book guard, dead-band, and VPIN proposals all share
this shape.

### Proposed alternative

Vega-conditional spread widener:
```
half_spread_t = max(min_edge,
                    base_half_spread × (1 + λ_vega × max(0, |net_vega| - vega_target)))
```
- `vega_target` = $200 (well below the $500 chokepoint)
- `λ_vega` = 0.001 ticks per dollar of vega over target
- At vega = $500: spread is +0.3 tick
- At vega = $1000: spread is +0.8 tick
- At vega = $2000: spread is +1.8 tick — comparable to wide-spread
  skip threshold; effectively pull the more-extreme side

Continuous, no halt, no choke, naturally tightens at extremes.

### Literature

- **Avellaneda-Stoikov** equation 14 — half-spread is increasing in
  inventory (delta proxy) AND in time-to-expiry-weighted volatility
  (vega proxy in our case).
- **Olivares-Nguyen (2017)** — proportional vega throttling in MM.

### Expected P&L impact

Best: prevents the §13 incident class (false halt) while still
defending against vega tails. Saves session productivity equivalent
to ~$20K/year (one false vega halt = ~3 hours of lost session;
historically 4-5 false halts / year).
Expected: neutral to slightly positive on edge.
Worst case: noisier spreads, lower fill volume — kill-feature.

### Spec deviation implications

v1.4 §6.2 specifies vega_kill at $500. Currently disabled (deviation).
This proposal replaces both (the spec value AND the deviation) with
a continuous throttle. Document as new §6.2.A.

### Validation plan

1. Backtest the same Alabaster panels (HG 2024 Q1, 2024 Q3, OOS
   2026) with the throttle active. Pass: max session vega bounded
   ≤ $1500 (where it tops out implicitly via spread widening), no
   throttle-induced halt firing, edge per fill ≥ baseline.
2. Compare: number of `kill_switch` events vs baseline + sum of
   `risk_block_(buy|sell)` counters.

### Implementation sketch

In `compute_risk_gates`: instead of `if vega.abs() >= snap.vega_kill { all = true; }`,
compute and return `vega_extra_edge_ticks`. In `decide_on_tick`,
use `edge = (snap.min_edge_ticks + vega_extra_edge_ticks) * tick_size`.

### Open questions

- Should throttle apply per-side (only widen the side that worsens
  vega) or symmetric? Per-side is theoretically right; symmetric is
  simpler. Backtest to find out.

---

## Recommendation 9 — Replace amend-modify with cancel-replace when adverse

**Lane:** Order flow / Hedging.
**Status:** **ship-ready** (deterministic improvement, low risk).

### Problem

`decision.rs:543-560`: when modifying a quote, the trader uses
`Decision::Modify` (single modify_order) — preserves orderId and queue
position. CLAUDE.md §17 calls this out as a feature ("cuts wire RTT in
half vs cancel+place and leaves no empty-quote window where opposite-
side traders can pick the lone surviving leg").

But: 75% of fills land within 1 s of place_ack. If the market is
sweeping our side, we WANT to cancel and re-place — re-placing at the
back of the queue at a wider price is safer than letting the modify
ride the queue at the existing aggressive level. The modify-vs-cancel
choice should depend on whether we're moving in the same direction as
flow or against it.

### Current implementation

Modify always (when orderId known). Cancel-place only when orderId is
None (place_ack pending).

### Why sub-optimal

When flow is informed and unidirectional (the toxicity-window case),
holding queue position at the current price is exactly the wrong move —
queue position is a liability when you want to NOT get filled. The
modify preserves it; cancel-place loses it; in this case, losing it is
the goal.

### Proposed alternative

Conditional choice:
- Default: modify (current behavior, queue-preserving).
- When VPIN > 0.3 OR realized_60s_imbalance > 2× baseline: cancel-replace.
- When the new price is *farther* from the BBO than the old (we're
  being defensive, widening): cancel-replace, since queue position at
  the more aggressive old price is bad.
- When the new price is *closer* to the BBO than the old (we're being
  aggressive, narrowing): modify, preserving queue.

### Literature

- **Foucault-Kadan-Kandel (2005) "Limit Order Book as a Market for
  Liquidity"** — queue position has positive value when trading on
  noise traders, NEGATIVE value when trading on informed traders.

### Expected P&L impact

Modest direct effect — save the share of "got picked off because the
modify kept us at the front of an adverse queue" cases. Probably 5%
of pickoff incidents.
Synergy with VPIN proposal: this is the natural action to take when
VPIN trips.

### Spec deviation implications

None at v1.4 level. Internal trader heuristic. Documented in
CLAUDE.md §17.

### Validation plan

Replay current logs with the new heuristic. Count of cancel-replaces
that would have fired; compare implied savings to vol of cancel-
replace traffic (which IS the cost — more wire frames on the broker
queue).

### Implementation sketch

`decide_on_tick` line 543: replace the unconditional Modify with:
```
let want_cancel_replace =
    snap.flow_toxicity > 0.3
    || (target_q.farther_from_bbo_than(ex.price));
if want_cancel_replace {
    out.push(Decision::Place {
        side, price: target_q,
        cancel_old_oid: Some(ex.order_id),
    });
} else {
    out.push(Decision::Modify { side, order_id: ex.order_id, price: target_q });
}
```

### Open questions

- Trader doesn't currently emit cancel_order with a place_order in
  one decision; needs the `cancel_old_oid: Some(...)` path that's
  already wired in `Decision::Place`. Verify caller respects it
  (broker IPC code).

---

## Diagnostics — what to extract from existing JSONL logs

The recommendations above can be validated/refuted against current
data. Here's the extraction plan to enable that — most should run as
one-off Python scripts against `logs-paper/*.jsonl`.

### D1. Per-fill realized edge (Rec 1, 4, 5, 8)

For each fill, compute:
- Theo at fill using the most recent `vol_surface` event for that expiry
- Theo at place_ack (T-0)
- Theo at fill (T+latency)
- Realized edge USD = side-signed (fill_price − theo_at_fill) × multiplier
- Latency from place_ack to fill (this is a feature)

Cross-tabulate by (right, side, |log-moneyness|, fit_age_at_fill,
latency_at_fill, place_ack_RTT). The 6-tuple gives the partition
needed for every per-fill analysis below.

Tested on 2026-05-05 in this session: clean implementation in
~70 LoC of Python. Should be production-ize-able as
`scripts/edge_decomposition.py` for daily summary use.

### D2. SABR fit instability (Rec 2)

For consecutive `vol_surface` events at the same expiry:
- |Δα|, |Δρ|, |Δν|, |ΔF| — distributions and time series
- Overlay with realized 60s vol of underlying — does instability
  correlate with vol regime, or is it endogenous to the fitter?

Already computed for 2026-05-05 in this session — see Executive
Summary §2.

### D3. Order-flow clustering (Rec 6)

For each option fill, the inter-arrival time to the next fill on the
same side. Histogram. The empirical Hawkes branching ratio is
1 − P(idle). Today's value: 0.36 (within-0.5s) — this is high, suggests
1−γ ≈ 0.36, γ ≈ 0.64. For a Hawkes process, γ in [0.5, 0.7] is
"highly self-excited" — characteristic of informed-trader sweeps.

### D4. Place-to-fill latency distribution (Rec 1, 9)

`wire_timing-*.jsonl` has place_ack RTT. `trader_events-*.jsonl` has
fill timestamps. Cross-reference by orderId. Distribution: is most
of our fill mass at <100 ms (sub-RTT pickoff) or at >5 s (queue
ride)? The two populations should be modeled differently.

Today's data: 66% < 100 ms, 75% < 1 s, 90% < 15 s. Bimodal.

### D5. Inventory accumulation pattern (Rec 5)

For each strike-expiry-right book line: cumulative position over
time. Identify the "runaway" cases (positions that built up to >2
contracts and stayed). Cross-reference with realized P&L — were
they profitable (in the simulator's no-skew baseline) or money-
losing?

### D6. Forward anchor disagreement (Rec 3)

For every `vol_surface` event: emit (ts, fit_F, spot_at_ts).
Plot fit_F − spot_at_ts. The empirical drift is the carry signal
(should be a smooth time-varying curve). Today's data shows
median 100.5 ticks ($0.05) — that's the HGM6-vs-HGK6 carry,
roughly stable.

### D7. Hedge slippage attribution (Rec 7)

For each hedge fill: (price_at_decide, fill_price, F_at_decide,
F_at_fill). Decompose slippage into "decide-to-place latency"
(broker compute), "place-to-fill latency" (IBKR matching),
"limit-vs-fill" (we placed at F+4ticks, filled at F+Xticks).
Distribution by hedge volume bucket.

---

## Backtesting punch list

For the crowsnest research handoff. Each item is a bounded backtest
job with a clear pass/fail criterion. **Order matches priority by
expected P&L impact.**

### B1. (Rec 1) Δα fast-path ATM IV update — BACKTEST

- **Data**: 2024 Q1, 2024 Q3, OOS 2026 (existing panels), HG only
- **Variants**: {static SABR, Δα with [no clamp, ±10%, ±20% clamp]}
- **Pass criteria**:
  - ATM adverse-edge fills (|lm|<0.01, edge<0) drop ≥30% in count
  - Total edge ≥ baseline + 0%
  - Place_total_per_session ≤ baseline × 1.5 (no spurious spam)
- **Stretch**: ablate the ATM trigger threshold (Δα cutoff for
  triggering a slow refit).

### B2. (Rec 2) SABR Tikhonov regularization — BACKTEST

- **Data**: same 3 panels, replay surface-fitting only (no execution)
- **Variants**: λ ∈ {0, 0.001, 0.005, 0.01, 0.05}, two anchors
  (β=0.5, β=0.7)
- **Pass criteria**:
  - Median |Δρ|, |Δν| between consecutive fits drops ≥50%
  - Median rmse increases ≤25%
  - Surface-overlap-with-future-fit-realized-IV improves (look-ahead
    metric)

### B3. (Rec 5) Inventory-skew pricing — BACKTEST (existing brief)

- **Data**: per
  `docs/briefs/crowsnest_inventory_skew_research_2026-04-22.md` §6
- **Add to brief's grid**: include latency-from-ack feature in
  per-fill simulation; report metrics conditional on latency bucket
  (sub-100ms vs >1s)
- **Pass criteria** per the brief

### B4. (Rec 8) Vega throttle (continuous) — BACKTEST

- **Data**: same Alabaster panels (HG 2024 Q1, 2024 Q3, OOS 2026)
- **Variants**: throttle off (current); λ_vega ∈ {0.0005, 0.001,
  0.002}; vega_target ∈ {$100, $200, $400}
- **Pass criteria**:
  - Number of vega-induced halts = 0 across all panels
  - Max session |vega| bounded ≤ $2000 implicitly
  - Edge per fill ≥ baseline × 0.95 (we're allowed to give up 5% for
    the safety upgrade)

### B5. (Rec 6) VPIN-style toxicity gate — BACKTEST

- **Data**: same panels, with fill stream time-stamped
- **Variants**:
  - VPIN window ∈ {30s, 60s, 120s, 300s}
  - Action thresholds (low/mid/high) per window
- **Pass criteria**:
  - Captures ≥50% of the 2026-05-01 burst (CLAUDE.md §16) in the
    panel
  - Edge-per-fill ≥ baseline
  - Tail loss reduced

### B6. (Rec 7) Vol-adaptive hedge tolerance — BACKTEST

- **Data**: same panels
- **Variants**: dynamic exponent ∈ {0 (static), 0.5, 0.667, 1.0};
  base band 0.5 (current)
- **Pass criteria**:
  - Hedge transaction cost / day drops ≥20%
  - Max |effective_delta| within session bounded ≤ 4.5 (no kill flirt)

### B7. (Rec 3) fit_F-anchor strike windows — REPLAY (no full backtest)

- This is a bug-fix per §3 above. The validation is replay-and-diff:
  for each tick in 2026-05-04, 2026-05-05, identify the (strike, side)
  triples that would decide differently with `forward = vp_msg.forward`
  vs `forward = snap.underlying_price`. Operator manual review; ship
  if behavior is correct on inspection.

### B8. (Rec 4) wide-spread skip restoration — REPLAY

- Same kind of replay-and-diff. Confirm the 8 ATM losing fills on
  2026-05-05 would have been skipped at `mul = 4.0`. If so, ship.

### B9. (Rec 9) Conditional cancel-replace vs amend — REPLAY

- Replay 2026-05-04, 2026-05-05 with the heuristic active in shadow
  mode. Count "cancel-replace would have happened" events and
  cross-reference with subsequent adverse fill share. Ship if the
  shadow signal is non-trivial (>10% of pickoff incidents would have
  been avoided).

---

## Spec deviation summary (post-recommendations)

If all recommendations land:

| # | v1.4 spec | Today live | Post-rec live |
|---|---|---|---|
| Rec 1 | "theo recomputes on every param update or forward tick" §4 | Param-update only | Both (Δα fast path) |
| Rec 2 | n/a | Independent fits | Tikhonov-regularized |
| Rec 3 | "ATM ± 5 nickels" §3.2 (anchor unspecified) | Anchored on spot | Anchored on fit_F |
| Rec 4 | `skip_if_spread_over_edge_mul: 4.0` §3.4 | 0.0 (TEMP) | 4.0 (or dynamic) |
| Rec 5 | n/a (no inventory response) | None | A-S inventory skew |
| Rec 6 | "Abnormal trade-rate >10× baseline" §7 | Not checked | VPIN-graduated |
| Rec 7 | "Hedge tolerance band: ±0.5" §5 | 0.5 fixed | Vol-adaptive 0.3–2.0 |
| Rec 8 | `vega_halt: 500` §6.2 | 0 (disabled) | Continuous throttle |
| Rec 9 | n/a | Always modify | Conditional cancel-replace |

The biggest spec drift is Rec 5 (inventory skew) and Rec 7 (vol-adaptive
hedge band) — both warrant explicit v1.5 spec delta memos before live.
The rest are bug fixes (Rec 3, 4) or graduated-defense additions
consistent with §16's design philosophy.

---

## Caveats

1. The 2026-05-05 session is **one day of paper data with 430 option
   fills**. All single-session statistics here are directional, not
   conclusive. Mass-confirming each finding needs the cross-session
   panel scan in the backtesting punch list.
2. Paper-IBKR matching has the well-known one-sided BBO ghost-flow
   issue (CLAUDE.md §16 Layer 4). The 75% sub-1s fill statistic is
   inflated in paper relative to live. The mechanism (fast pickoff)
   is real; the magnitude is uncertain. Live data after Stage 2
   launch will be the true test.
3. SABR is a Hagan-2002 expansion approximation. Wing extrapolation
   beyond the calibrated strike envelope (now gated at
   `calibrated_min_k`/`max_k`) is not theoretically supported. Our
   active OTM-quoting at ±5 nickels = $0.25 from F is at or beyond
   the edge of useful Hagan validity, especially for short-dated
   contracts where the asymptotic approximation breaks down. SVI
   (currently calibrated but unused — broker publishes SABR per
   `vol_surface.rs:145`) would be a more principled wing model.
   The "should we be using SVI" question is a follow-up to Rec 2,
   not in scope of this brief.
4. The hedge subsystem's `apply_broker_fill` reconciliation
   (CLAUDE.md §10) is post-Phase-6.10 wired but the failure modes
   described in §14 (IOC didn't fill / Error 201 / disconnect)
   remain real. Rec 7's vol-adaptive band assumes the reconciliation
   is sound; if it isn't, the band can drift further before
   correction.
5. None of these recommendations affect v1.4's PRIMARY defense
   (daily P&L halt at −5%, §6.1). That stays.

---

## Where to look in code (touchpoint quick-reference)

| Recommendation | Crate | File | Approx LOC |
|---|---|---|---|
| Rec 1 (Δα) | corsair_broker, corsair_trader | vol_surface.rs, decision.rs | +120 |
| Rec 2 (Tikhonov) | corsair_pricing | calibrate.rs | +40 |
| Rec 3 (fit_F anchor) | corsair_trader | decision.rs:137-181 | ±10 |
| Rec 4 (wide-spread) | config | runtime_v3.yaml:74 | 1 |
| Rec 5 (inventory skew) | corsair_trader, corsair_position | decision.rs, IPC schema | +200 |
| Rec 6 (VPIN) | corsair_trader | state.rs, decision.rs | +120 |
| Rec 7 (vol-adaptive hedge) | corsair_hedge | manager.rs | +80 |
| Rec 8 (vega throttle) | corsair_trader | decision.rs:compute_risk_gates | +40 |
| Rec 9 (conditional CR) | corsair_trader | decision.rs:543 | +30 |

Total estimated LoC for all 9: ~640. Estimated implementation effort:
2–4 weeks for one engineer with the operator owning prioritization.
Backtesting effort (crowsnest side): 4–6 weeks for B1–B6 in parallel.

---

End of spec.
