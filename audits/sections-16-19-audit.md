# Audit: CLAUDE.md §16 (Spot vs Fit-Forward) and §19 (Taylor Anchor)

**Audit date:** 2026-05-06
**Scope:** Phase 1 audit only — no code changes. Identifies current state of fixes, test coverage, related call sites, and class-of-error analysis. Phase 2 (fixes / regression tests / architectural changes) is a separate workstream pending review.

## Executive summary

Both fixes are **fully applied** in current production code:

- **§16 (Fit-time forward to SVI)**: All 8 production call sites of `svi_implied_vol` / `sabr_implied_vol` / `compute_theo` pass the fit-time forward. The original Python source where the bug lived has been deleted (Phase 6.7..6.11 cutover, CLAUDE.md §15).
- **§19 (Taylor anchor on `spot_at_fit`)**: Both Taylor reprice sites in production (`decision.rs::decide_on_tick` and `main.rs::staleness_check`) anchor on `vp.spot_at_fit`, not `vp.forward`. The broker correctly captures spot-at-fit and ships it on every `vol_surface` event.

**Test coverage is partial**:
- Numerical-correctness tests exist (`svi_intel_check`, `black76_taylor_first_order_check`) and assert correct output given correct input.
- **Regression tests for the bug class do not exist** — there is no test that verifies "passing current spot instead of fit-forward produces a detectably-wrong number." The fixes rely on documentation (comments in code, CLAUDE.md §16/§19) plus careful reviewer attention.
- Recommended additions in §3.3.

**Class-of-error analysis (§4) identifies one architectural gap**: both bugs are "wrong f64 passed to positional float arg" — `forward: f64` and `spot: f64` accept any float, with no compile-time distinction between fit-time and current values. Newtype wrappers would eliminate the class of bug. This is a non-trivial refactor (~6 call sites + several struct fields) and is recommended for Phase 2 review, but is not required to consider §16/§19 closed.

**Cross-reference with the fp32 precision study (§5)**: confirmed orthogonal. The Taylor anchor DOES affect which targets land on tick-grid half-boundaries (because Taylor shifts theo by `δ × (spot − spot_at_fit)`, ~$0.02 in HG), but it does not cause the precision-driven half-tick artifact. The precision study's GREEN verdict is independent of §16/§19 fix status.

---

## 1. §16 — Spot vs fit-forward bug

### 1.1 Background (CLAUDE.md §16, 2026-05-01 paper losses)

Original bug: the Python `compute_theo` in `src/trader/quote_decision.py` was called with **current spot** as the `forward` argument. SVI's `m` parameter is anchored on `log(K / F_fit)`; passing a different F at evaluation re-anchors the wing flex point and produces silently-wrong IVs at deep wings. HXEK6 P560 reproduction: `F_fit=6.021` returned theo=$0.0275 (correct); same params with `F=5.96` (current spot) returned theo=$0.0337 — a 23% gap on a deep-wing put. Drove BUYs at theo−1tick into ask=theo, picked-off-every-time. Net paper loss attributed to the cleanup-passes-3-6 set: ~$25K.

Fix per CLAUDE.md §16 Layer 1 (commit `e6486f6`): `compute_theo` requires the fit-time forward, which the broker emits in every `vol_surface` IPC event.

### 1.2 Call site audit

All current invocations of pricing functions that take a `forward` argument:

| Location | Caller | `forward` arg passed | Source of value | Status |
|---|---|---|---|---|
| `corsair_trader/src/decision.rs:300` | `decide_on_tick` | `pricing_forward` | `vp_msg.forward` (= broker's fit-time forward, set in `state.rs::VolSurfaceEntry::forward` from `VolSurfaceMsg::forward`) | ✅ fit-time |
| `corsair_trader/src/decision.rs:726` | `compute_theo` (SVI branch) | `forward` | passed through from caller | ✅ fit-time (transitively, via L300) |
| `corsair_trader/src/decision.rs:736` | `compute_theo` (SABR branch) | `forward` | same | ✅ fit-time |
| `corsair_trader/src/main.rs:1441` | `staleness_check` | `vp.forward` | `state.lookup_vol_surface(...)` → broker fit-time | ✅ fit-time |
| `corsair_pricing/src/lib.rs:359` | `decide_quote` (Python entry, SABR) | `forward` | passed by Python caller | ✅ — Python call site is via PyO3 FFI; Python path is deprecated post-cutover but the API contract is preserved |
| `corsair_pricing/src/lib.rs:369` | `decide_quote` (Python entry, SVI) | `forward` | same | ✅ |
| `corsair_broker/src/tasks.rs:1194` | broker per-leg theo | `e.forward` | `vol_surface_cache` entry, populated from `calibrate_sabr` result (so by definition fit-time) | ✅ fit-time |
| `corsair_broker/src/ipc.rs:1161` | broker fill edge calculation | `f.forward` | same `VolSurfaceCacheEntry` | ✅ fit-time |

**Verification method:** read each call site, traced the `forward` argument back to its origin. All paths terminate at either `VolSurfaceMsg.forward` (trader side) or `VolSurfaceCacheEntry.forward` (broker side), both of which are populated by the calibrator's fit-time forward. No call site uses `state.scalars.underlying_price` (current spot) or any other non-fit value.

**Fix status: FULLY APPLIED.** No remaining incorrect call sites identified.

### 1.3 Where the original bug lived

The original bug was in Python (`src/trader/quote_decision.py:compute_theo`). That file no longer exists — the entire Python trader was deleted in Phase 6.7..6.11 (CLAUDE.md §15). Current pricing entry points are exclusively Rust:

- `corsair_trader/src/pricing.rs` (binary's hot path; pure Rust, no Python deps)
- `corsair_pricing/src/lib.rs` (PyO3 wrapper crate; mostly used by Python tests and any remaining Python tooling, not the live trader)

Risk that the bug recurs in Python is therefore moot — the Python is gone. Risk that it recurs in Rust is the live concern, addressed in §3 and §4.

### 1.4 Adjacent risk: forward-drift guard at `decision.rs:274`

```rust
let drift = (spot - pricing_forward).abs();
let max_drift = MAX_FORWARD_DRIFT_TICKS as f64 * snap.tick_size;
if drift > max_drift { ... skip_forward_drift ... }
```

This compares **current spot** against **fit-time forward** intentionally — the gate measures how far current spot has drifted from the fit's view of the world (200 ticks ≈ $0.10, sized to cover the K6→M6 calendar carry plus a margin). The use of `pricing_forward` here is **not** an instance of the §16 bug; conflating the two values in this specific line would actually disable the gate.

This is highlighted as a place where a future maintainer might "fix" the comparison to use `spot_at_fit` thinking they're aligning with §19 — that would be a regression. Documenting this in the audit so the §16/§19 fix story stays coherent.

---

## 2. §19 — Taylor anchor bug

### 2.1 Background (CLAUDE.md §19, 2026-05-06)

Initial Rust port of the Taylor reprice used `(spot − pricing_forward)` as the anchor. On HG, `spot` (front-month K6) and `pricing_forward` (option's parity F = M6) differ by ~$0.04–0.05 of structural calendar carry (80–100 ticks). The Rust trader interpreted that carry as drift and shifted every theo by `δ × $0.05 ≈ $0.025` per option in a directionally consistent way:
- Calls (δ > 0, spot < forward → drift < 0): theo dropped → SELL targets fell below the bid → cross-protect skipped every SELL.
- Puts: mirror image — no two-sided BUY-side puts.

Live symptom 2026-05-06 12:35–12:45 UTC: dashboard showed call BIDs only / put ASKs only. `skip_would_cross_*` counters spiked.

Fix: Taylor anchor must be `vp.spot_at_fit` (broker's spot at fit time), not `vp.forward`. Broker captures `und_spot` at fit time and ships it as `spot_at_fit` on every `vol_surface` event.

### 2.2 Taylor reprice site audit

Two production locations in Rust apply the Taylor reprice:

| Location | Caller | Formula in code | Anchor used | Status |
|---|---|---|---|---|
| `corsair_trader/src/decision.rs:341` | `decide_on_tick` (hot path) | `(theo_at_fit + delta_at_fit * (spot - vp_msg.spot_at_fit)).max(0.01)` | `vp_msg.spot_at_fit` | ✅ correct |
| `corsair_trader/src/main.rs:1445` | `staleness_check` (10 Hz loop) | `(theo_at_fit + delta_at_fit * (snap.underlying_price - vp.spot_at_fit)).max(0.01)` | `vp.spot_at_fit` | ✅ correct |

Both formulas use identical anchor (`vp.spot_at_fit` for the entry, populated from `VolSurfaceMsg.spot_at_fit` for the IPC event). Both are commented in-line with explicit warnings against re-introducing the carry-confusion bug.

### 2.3 spot_at_fit propagation chain

The fix requires the broker to emit `spot_at_fit` and the trader to use it. Verified end-to-end:

| Step | Location | Status |
|---|---|---|
| Broker captures spot at fit time | `corsair_broker/src/vol_surface.rs:145` (`let (forward, spot_at_fit, expiry_groups) = match snapshot_chain(runtime, &product)`) | ✅ |
| Broker emits in `vol_surface` event | `corsair_broker/src/vol_surface.rs:235` (`spot_at_fit,` in `VolSurfaceEvent` literal) | ✅ |
| Wire schema declares the field | `corsair_trader/src/messages.rs:154` (`pub spot_at_fit: Option<f64>`) | ✅ — Option for back-compat with older brokers |
| Trader stores on entry | `corsair_trader/src/state.rs:66` (`pub spot_at_fit: f64`) | ✅ |
| Trader populates entry | `corsair_trader/src/main.rs:760-769` (`let spot_at_fit = vs.spot_at_fit.unwrap_or_else(...)`) | ✅ |
| Hot-path Taylor uses `vp_msg.spot_at_fit` | `decision.rs:341` | ✅ |
| Staleness Taylor uses `vp.spot_at_fit` | `main.rs:1445` | ✅ |

### 2.4 Back-compat fallback at `main.rs:760`

```rust
let spot_at_fit = vs.spot_at_fit.unwrap_or_else(|| state.scalars.lock().underlying_price);
```

When the broker is older and doesn't emit `spot_at_fit` (None on the wire), the trader falls back to **current spot at message-arrival time**. The in-line comment notes:
> "Falling back to vs.forward would re-introduce the carry-confusion bug."

This fallback is operationally safe in homogeneous deployments (broker and trader from the same image; `vs.spot_at_fit` is never None in practice), but it's a place where a partial regression could go undetected — if a future broker change inadvertently dropped `spot_at_fit` from the wire, the fallback would silently use current spot, which differs from broker's fit-time spot only by IPC latency (a few ms; well below tick precision in normal operation but not catastrophe-bounded if IPC stalls).

**Recommendation (§3):** add a startup-time sanity log that asserts `vs.spot_at_fit.is_some()` for the first N events and warns if not. Cheap diagnostic.

### 2.5 Adjacent: theo cache key

```rust
let theo_key = (
    SharedState::strike_key(strike),
    Arc::clone(expiry_arc),
    r_char,
    vp_msg.fit_ts_ns,
);
```

The cache invalidates by `fit_ts_ns` — when a new fit lands, all old cached `(theo_at_fit, delta_at_fit)` pairs become stale and are evicted at the next 200-entry capacity sweep. This is correct for §19: the cached values are anchored at the OLD `spot_at_fit` and would shift incorrectly if applied with the new fit's `spot_at_fit`. The `fit_ts_ns` keying handles this implicitly. Verified at `decision.rs:286-320`.

---

## 3. Test coverage

### 3.1 Existing tests directly relevant to §16

| Test | File | What it asserts | What it does NOT assert |
|---|---|---|---|
| `svi_intel_check` | `corsair_trader/src/pricing.rs:217-234` | At F=6.021 (fit-time), K=5.6, T=25.5/365, with the §16 reproduction params, `svi_implied_vol` returns iv ≈ 0.253 ± 0.005 | Does NOT exercise the bug — never calls with `F=5.96` (current spot) and asserts the result is wrong |
| `lm_recovers_synthetic_sabr_params` | `corsair_pricing/src/calibrate.rs:563-578` | Calibrator recovers SABR params from synthetic chain | Indirect; doesn't address §16 |
| `svi_fits_synthetic_chain` | `corsair_pricing/src/calibrate.rs:597-614` | Calibrator recovers SVI params | Indirect |

**Gap:** there is no negative test that asserts "passing the wrong forward produces a detectably-wrong IV/theo." A regression test that calls `svi_implied_vol(5.96, 5.60, T, ...)` with §16 params and asserts iv ≠ 0.253 would catch a re-introduction (or, more usefully, a test that calls compute_theo with both forwards and asserts a >5% theo difference at the §16 reproduction strike).

### 3.2 Existing tests directly relevant to §19

| Test | File | What it asserts | What it does NOT assert |
|---|---|---|---|
| `black76_taylor_first_order_check` | `corsair_trader/src/pricing.rs:181-197` | For small `dF`, `theo(F+dF) ≈ theo(F) + δ(F) × dF` | Tests Taylor numerical accuracy, NOT the anchor choice — doesn't distinguish between using `spot_at_fit` and using `forward` |
| `taylor_cache_stores_theo_and_delta_pair` | `corsair_trader/src/decision.rs:1118-1158` | The theo cache stores `(theo_at_fit, delta_at_fit)` as a tuple (not just theo) | Tests cache shape, not anchor correctness |

**Gap:** there is no test that asserts "Taylor reprice with `vp.forward` as anchor produces operationally-wrong quotes on a calendar-carrying product." A regression test that constructs a `VolSurfaceEntry` with `forward = 6.05`, `spot_at_fit = 6.00` (mimicking $0.05 carry), then runs `decide_on_tick` with `spot = 6.00` and asserts that BOTH BUY and SELL sides emit a Place (i.e., neither is filtered by cross-protect due to a wrong-direction Taylor shift) — this would catch a regression.

### 3.3 Recommended new tests (Phase 2)

For §16:
1. **Wrong-forward regression**: `svi_implied_vol(F_current, K, T, params_fit_at_F_fit)` should produce iv that differs from `svi_implied_vol(F_fit, K, T, ...)` by >5% on the §16 wing reproduction. Codifies the magnitude of the bug as a numerical assertion. If anyone re-introduces the bug in a refactor, the test fails.
2. **All-call-sites property test**: a doc-test or integration test that grep-asserts every call to `compute_theo` / `svi_implied_vol` / `sabr_implied_vol` in the codebase passes the fit-time forward (verified by reading the variable name or struct field at the call site). This is more of a lint than a test, but is the most direct regression guard against the class of error.

For §19:
1. **Carry-product Taylor test**: construct a `VolSurfaceEntry` with `forward = 6.05`, `spot_at_fit = 6.00`, `delta_at_fit = 0.5`. Apply Taylor at `current_spot = 6.00`. Assert `theo == theo_at_fit` (no shift, because spot equals spot_at_fit). Then with the WRONG anchor (`forward` substituted), `theo` would be `theo_at_fit + 0.5 × (6.00 − 6.05) = theo_at_fit − 0.025`, a $0.025 shift. The test asserts the actual implementation produces 0 shift, not −0.025. Catches the §19 regression.
2. **End-to-end placement test**: with the same surface, run `decide_on_tick` and assert that the BUY and SELL quotes are within 1 tick of `theo_at_fit ± edge`, NOT shifted by `δ × carry`. The original CLAUDE.md §19 symptom (one-sided quoting) would re-emerge if this regressed.

For both:
3. **Architectural hardening** (recommended, see §4): convert `forward: f64` and `spot/spot_at_fit: f64` to newtypes (`FitForward(f64)`, `SpotAtFit(f64)`, `CurrentSpot(f64)`). Compile-time prevents the class of bug entirely. Higher upfront cost; eliminates the need for the negative tests above (the type system enforces the constraint).

### 3.4 Test infrastructure note

Current Rust test layout is `#[cfg(test)] mod tests` inline within source files. The corsair workspace has no integration-test directory at the workspace level (`rust/tests/`). The Python test layout (`tests/test_pricing_parity.py`) referenced in `corsair_pricing/src/lib.rs:5` is no longer wired (the only Python tests remaining are `tests/test_ipc_protocol.py` plus `tests/__init__.py` and `conftest.py`). Either layout works for the recommended tests above; inline module tests are the lower-friction option.

---

## 4. Class-of-error analysis

### 4.1 The shared bug pattern

Both bugs are instances of: **passing the wrong f64 to a positional float argument that has no type-level distinction from another semantically-different f64**.

**§16**: `compute_theo(forward: f64, ...)` accepts any f64. The CALLER must know to pass `vp_msg.forward` (fit-time) and not `state.scalars.underlying_price` (current spot). Both are `f64`. The compiler does not help.

**§19**: `theo_at_fit + delta_at_fit * (spot - spot_at_fit)` is a formula over four f64s. Substituting `vp.forward` for `vp.spot_at_fit` is a one-character change that compiles cleanly and produces wrong behavior.

### 4.2 Other potential sites of the same class

Exhaustive search performed on `f64` parameters with semantic ambiguity:

| Site | Parameters that could be confused | Current state |
|---|---|---|
| `compute_theo` (decision.rs:715) | `forward: f64` (fit-time) vs current spot | Correct (§1.2) |
| Taylor reprice (decision.rs:341, main.rs:1445) | `spot` vs `spot_at_fit` vs `forward` | Correct (§2.2) |
| Forward-drift guard (decision.rs:274) | `spot` vs `pricing_forward` | Correct USE (intentional comparison; §1.4) |
| Risk gates (decision.rs:641) | `risk_effective_delta`, `risk_margin_pct`, `risk_theta`, `risk_vega` — all f64 | These are typed by name, not by newtype; risk of swap exists but is mitigated by structural `ScalarSnapshot` field access (no positional args) |
| Black-76 inputs (`black76_price_and_delta(f, k, t, sigma, r, right)`) | `f` (forward) vs `k` (strike) | Both f64, positional. Risk of swap exists but is high-noise (would produce wildly wrong prices; would fail the existing `svi_intel_check` regression). |
| `iv_f64.iv_*` calls in compare module (fp32 spike) | `forward, strike, tte` | Out of scope — that's the spike code, throwaway. |

**Highest-risk sites, ranked by likelihood × consequence:**

1. **Taylor reprice anchor** (already bit us — §19). Mitigation: §3.3 regression test + newtype wrappers.
2. **compute_theo forward** (already bit us — §16). Mitigation: §3.3 regression test + newtype wrappers.
3. Risk gate field swaps (e.g., reading `risk_theta` where the code expects `risk_vega`). Lower risk because most accesses are structural (`snap.risk_theta`), but a refactor could re-introduce positional confusion.

### 4.3 Architectural mitigations

**Option A — Newtype wrappers (recommended for Phase 2)**

```rust
#[derive(Copy, Clone)] pub struct FitForward(pub f64);
#[derive(Copy, Clone)] pub struct CurrentSpot(pub f64);
#[derive(Copy, Clone)] pub struct SpotAtFit(pub f64);
```

Update `compute_theo` signature: `fn compute_theo(forward: FitForward, ...)`. Update Taylor: `fn taylor_reprice(theo_at_fit: f64, delta: f64, current: CurrentSpot, anchor: SpotAtFit) -> f64`. The compiler now rejects passing a `CurrentSpot` where a `FitForward` is expected.

- Cost: ~6 call sites + several struct fields (~1-2 days of careful work, including PR review).
- Benefit: eliminates the entire class of bug at compile time. Future maintainers cannot regress without explicitly transmuting between newtypes (which is a clear code smell visible in review).
- Trade-off: moderate boilerplate; needs `.0` access for arithmetic in places.

**Option B — Named-field struct args**

Replace `compute_theo(forward, strike, tte, ...)` with `compute_theo(PricingInputs { forward, strike, tte, ... })`. Reduces positional confusion. **Does not eliminate** the class of bug — still possible to assign `current_spot` to the `forward` field by typo.

- Cost: smaller refactor. Mostly cosmetic.
- Benefit: partial; helps reviewers more than compilers.

**Option C — Defensive runtime invariant**

Add `assert!((current_spot - fit_forward).abs() < SOME_THRESHOLD)` at compute_theo entry. Already partially exists as the `MAX_FORWARD_DRIFT_TICKS` gate (§1.4). 

- Limitation: would NOT have caught the original §16 bug because the bug was passing `current_spot` AS the `forward` argument, in which case the assertion `|current_spot - current_spot| < threshold` always passes. This option is the wrong shape for the class of bug.

**Option D — Property-based tests (universal, lower-cost)**

Write the regression tests in §3.3 even without architectural change. Catches re-introduction at CI time rather than compile time. Lower bar to clear; should be done regardless of A/B/C.

**Recommendation for Phase 2 review:**

Combination: **Option D (definitely, low cost) + Option A (recommended, moderate cost)**. Option D alone is sufficient to consider §16/§19 closed regressively. Option A is the architecturally durable answer that prevents recurrence by construction.

---

## 5. Cross-reference with fp32 precision study

### 5.1 Confirmation of orthogonality

The fp32 precision study (`/home/ethereal/fp32-precision-spike/results/analysis_report.md`, §6 "Cross-reference: §16/§19 incident scenarios") explicitly notes both incidents are **algorithmic input bugs, not arithmetic precision bugs**. Restated here:

- §16: passing current spot to SVI's compute_theo. Both fp64 and fp32 would produce the same wrong IV given the same wrong input. The fp32-vs-fp64 deviation is microcents; the §16 mispricing was 23% on the wing. Magnitude difference: ~6 orders of magnitude. fp32 cannot create §16, cannot prevent §16, and cannot make §16 worse.
- §19: Taylor anchor. Same analysis — the carry-as-drift shift is `δ × $0.05 ≈ $0.025` per option, three orders of magnitude above fp32's microcent noise. fp32 is invisible to this bug.

**Confirmed: §16 and §19 are precision-irrelevant.**

### 5.2 One indirect interaction

The Taylor anchor choice does affect WHERE on the tick grid `target = theo - edge` lands, which in turn affects the half-tick rounding boundary identified in the precision study (§11). Specifically:

- With **correct anchor** (`spot_at_fit`): Taylor shifts theo by `δ × (spot − spot_at_fit)`. For HG with `δ ≈ 0.5` and post-fit drift of ~$0.01, the Taylor shift is ~$0.005 (10 ticks).
- With **wrong anchor** (`forward`): Taylor shifts theo by `δ × (spot − forward)`. The K6→M6 carry of $0.04–0.05 dominates; shift is ~$0.02–0.025 (40–50 ticks).

Both numbers are large enough to move `target = theo − edge` across multiple tick-grid lines. So the anchor choice CHANGES which specific ticks land on the half-tick rounding boundary. But it does not CAUSE the half-tick rounding artifact (which is fp32's representation of bid/ask, not the Taylor anchor) and does not affect the precision study's headline rates.

The fp32 study's GREEN verdict (0.25% meaningful disagreement) is **robust** to §16/§19 fix status — the precision concern is concentrated at SABR `|ρ| ≥ 0.999` and is independent of the Taylor anchor. If §19 had been left wrong, the precision study would have produced different specific row-by-row outcomes but the same aggregate verdict.

### 5.3 Confirmation note for the precision study report

The precision study's §6 ("Cross-reference: §16/§19 incident scenarios") states: "Honest answer to 'would the recent paper losses have been worse under fp32?': no". This audit confirms that statement — neither §16 nor §19 has any fp32-specific mechanism, and the fp32 prototype's viability does not depend on §16/§19 fix status.

---

## 6. Phase 2 input — open questions for the reviewer

These are decisions for the reviewer (operator + researcher), not action items I will execute without confirmation:

1. **Regression-test scope**: are the §3.3 tests sufficient, or do you want broader test coverage (e.g., end-to-end cross-product test on representative recorded vol_surface events)?
2. **Architectural change scope**: do we adopt Option A (newtype wrappers), and if so, in which crates? `corsair_trader` only? Or push down to `corsair_pricing` and `corsair_broker` too? (Pushing down maximizes safety but increases blast radius of the refactor.)
3. **Documentation**: is CLAUDE.md §16/§19 the right place for the recurrence-prevention notes, or should there be a dedicated `docs/anti-patterns.md`?
4. **Backstop**: do we want a CI lint that grep-asserts no call site uses `state.scalars.underlying_price` as the second argument to `svi_implied_vol` / `sabr_implied_vol` / `compute_theo` (i.e., a syntactic ban)? Cheap to add, narrow blast radius, codifies the rule.

---

## Appendix A — Files read for this audit

- `/home/ethereal/corsair/CLAUDE.md` (§16, §19, §15 for Python deletion context)
- `/home/ethereal/corsair/rust/corsair_trader/src/decision.rs` (compute_theo, decide_on_tick, Taylor at L341, forward-drift gate at L274, tests)
- `/home/ethereal/corsair/rust/corsair_trader/src/main.rs` (process_event for `vol_surface` at L745, staleness_check + Taylor at L1380-1480)
- `/home/ethereal/corsair/rust/corsair_trader/src/messages.rs` (VolSurfaceMsg shape, spot_at_fit Option<f64>)
- `/home/ethereal/corsair/rust/corsair_trader/src/state.rs` (VolSurfaceEntry shape)
- `/home/ethereal/corsair/rust/corsair_trader/src/pricing.rs` (svi_implied_vol, sabr_implied_vol, black76_taylor test)
- `/home/ethereal/corsair/rust/corsair_pricing/src/lib.rs` (PyO3 entry points; decide_quote, calibrators)
- `/home/ethereal/corsair/rust/corsair_pricing/src/calibrate.rs` (sabr_implied_vol callers in calibration loop, test cases)
- `/home/ethereal/corsair/rust/corsair_broker/src/vol_surface.rs` (broker-side spot_at_fit capture and emission)
- `/home/ethereal/corsair/rust/corsair_broker/src/tasks.rs` (broker per-leg theo at L1194)
- `/home/ethereal/corsair/rust/corsair_broker/src/ipc.rs` (broker fill-edge SABR call at L1161)
- `/home/ethereal/corsair/tests/` (workspace tests — only `test_ipc_protocol.py` remains; no pricing-parity or decision-parity tests in current tree)
- `/home/ethereal/fp32-precision-spike/results/analysis_report.md` (precision study cross-reference)

## Appendix B — Verification queries used

```bash
grep -rn "compute_theo" rust/ --include="*.rs"
grep -rn "svi_implied_vol\|sabr_implied_vol" rust/ --include="*.rs"
grep -rn "spot_at_fit" rust/ --include="*.rs"
grep -rn "spot - .*forward\|spot - .*pricing_forward" rust/ --include="*.rs"
grep -rln "fit_forward\|carry_bug\|HXEK6.*P560" rust/ src/ tests/
```

All call-site claims in §1.2 and §2.2 are verifiable by re-running these queries.
