# Directional Model Feasibility Study — Technical Spec

**Status:** Draft, ready to hand to a Claude Code session on the 13900K box.
**Author context:** Spec written 2026-04-25 from a Corsair v1.4 operator
session. The receiving Claude has not seen that conversation — this doc
must stand alone.

## 1. Context

Corsair (HG copper futures-options market maker, currently at v1.4 Stage 1,
pure symmetric/asymmetric MM with skew off) is bottlenecked on gateway JVM
serialization and IBKR network RTT, not local compute. A separate beefy
host (Intel 13900K, 64 GB RAM, RTX 4090, 8 TB NVMe) is available adjacent
to the live quoter. The operator wants to know whether a short-horizon
directional / adverse-selection model has enough theoretical edge for HG
MM to justify a 3–6 month full build.

This document specifies a **1–2 week feasibility study** whose deliverable
is a single markdown report containing a kill/pass verdict, not a
deployable model.

## 2. Goals

1. Quantify *adverse selection magnitude* on Corsair fills at horizons
   {5 s, 30 s, 5 min}. This is the prize ceiling.
2. Establish a *cheater* (perfect-foresight) economic ceiling via a
   conservative MM simulator. This is the theoretical upper bound.
3. Establish realistic floor via logistic regression and LightGBM on
   engineered intra-HG features.
4. Validate via walk-forward, not k-fold. Report AUC **and** simulated
   economic lift after costs.
5. Produce one markdown report whose executive summary tells a reader
   kill/pass in under 60 seconds.

## 3. Non-goals

- No live integration with Corsair. Output is a decision artifact.
- No cross-asset features (DXY, metals, crude). Gated for v2 — would
  require new market data subs.
- No sequence models (LSTM/transformer). Tabular GBM is the v1 ceiling.
- No skew-mode simulation. Gating-mode only (see §7.5).
- No hyperparameter tuning beyond library defaults. If defaults show no
  signal, tuning won't save it.
- No GPU. LightGBM on the 13900K beats GPU at this sample size; the 4090
  is unused in v1.

## 4. Hardware target

13900K / 64 GB / 4090 / 8 TB NVMe / Linux. Plan for CPU + disk; ignore the
GPU. Sample sizes will be 10⁷–10⁸ rows; use **polars** for replay/feature
work and **pandas** only at the modeling boundary.

## 5. Data contracts

Source: 8 JSONL streams in `logs-paper/` on the live host (`Alabaster`).
Schemas documented in Corsair's `docs/hg_spec_v1.4.md` §9.5. Streams used:

| Stream                          | Use                                         |
|---------------------------------|---------------------------------------------|
| `market_data-YYYY-MM-DD.jsonl`  | NBBO snapshots, sizes, trade ticks          |
| `quotes-YYYY-MM-DD.jsonl`       | Our submitted quotes (decision-time grid)   |
| `fills-YYYY-MM-DD.jsonl`        | Our fills (price, side, contract, ts)       |
| `sabr_fits-YYYY-MM-DD.jsonl`    | SABR params + residuals per cycle           |
| `peer_consensus-YYYY-MM-DD.jsonl` | Incumbent MM behavior snapshots           |
| `burst_detector-YYYY-MM-DD.jsonl` | Burst detector output                     |
| `risk-YYYY-MM-DD.jsonl`         | Greeks, margin                              |
| `hedge_trades-YYYY-MM-DD.jsonl` | Hedge intent (observe-mode currently)       |

**Point-in-time correctness is non-negotiable.** A feature computed at
time *t* may only depend on data with timestamp ≤ *t*. Verify with a
deliberate-leak test (§9).

## 6. Project layout

```
~/feasibility/
├── config/
│   └── study.yaml              # all knobs: horizons, kill/pass thresholds
├── data/
│   ├── raw/                    # rsync mirror of Alabaster logs-paper/
│   ├── parsed/                 # parquet, schema-validated
│   ├── features/               # feature panels, parquet
│   └── labels/                 # label panels, parquet
├── src/
│   ├── parse.py                # JSONL → parquet
│   ├── features.py             # feature engineering on decision grid
│   ├── labels.py               # forward-return labels at multiple horizons
│   ├── adverse_selection.py    # the prize measurement
│   ├── simulator.py            # gating-mode MM simulator
│   ├── models/
│   │   ├── baseline.py         # logistic regression
│   │   ├── gbm.py              # lightgbm
│   │   └── cheater.py          # perfect-foresight upper bound
│   ├── walkforward.py          # walk-forward CV harness
│   ├── economic.py             # signal → simulated P&L
│   └── report.py               # kill/pass dashboard generator
├── reports/
│   └── feasibility_<date>.md   # the deliverable
├── tests/                      # pytest, focused on label/sim correctness
└── pyproject.toml
```

## 7. Component specs

### 7.1 parse.py

- Reads `data/raw/*.jsonl`, validates schema, writes
  `data/parsed/<stream>_<date>.parquet`.
- Each row gets `ts_ns` (int64 ns since epoch, UTC) for deterministic joins.
- Idempotent: re-runs on existing files are a no-op via mtime check.
- Schema mismatch aborts with clear error. Do not silently coerce.

### 7.2 features.py

Compute features on a **decision-time grid** — timestamps from
`quotes-*.jsonl` (when Corsair would actually have decided to quote).

Feature families (start small; expand only if baseline shows signal):

- **NBBO microstructure**: bid-ask spread (ticks), top-of-book size
  imbalance, NBBO mid changes over {1 s, 5 s, 30 s} lookbacks
- **Order flow**: tick-rule signed trade volume over {1 s, 5 s, 30 s}
- **SABR-derived**: ATM IV level, skew slope (25Δ call IV − 25Δ put IV),
  residual magnitude
- **Peer behavior**: incumbent MM presence flag, behind-incumbent distance
  stats from `peer_consensus`
- **Our fill imbalance**: count(call fills − put fills) over {30 s, 5 min}
- **Time-of-day**: minute-of-session, session phase (Asia / EU / US)
- **Burst state**: `burst_detector` output

Drop features with > 20 % missingness. Document each feature with a
one-line comment on what it represents and why it might predict.

### 7.3 labels.py

For each decision-time row, compute forward F-return at horizons
h ∈ {5 s, 30 s, 5 min}.

Three label flavors:

1. **Sign**: `sign(F(t+h) − F(t))` ∈ {−1, 0, +1}; encode 0 as missing.
2. **Magnitude**: `|F(t+h) − F(t)| > k_ticks` for k ∈ {1, 2, 3}.
3. **Triple-barrier** (Lopez de Prado): label = +1 if upper TP hit before
   lower SL or t+h, −1 if SL first, 0 if neither.

Strict no-lookahead: F(t+h) read by absolute timestamp, not row index.

### 7.4 adverse_selection.py — the prize measurement

**Most important measurement in the study.** Build this first.

For every fill in `fills-*.jsonl`, compute the F-move *against us* at
{5 s, 30 s, 5 min}. "Against us" = sold call → F up is bad, etc.

Output:
- Distribution (mean, median, p90, p99) of adverse F-move per side per horizon
- Total $ realized adverse selection over the study period
- Estimated $ recoverable if a perfect adverse-selection predictor had
  gated those fills

This is the **prize**. If small after costs, no model is worth building.

### 7.5 simulator.py — gating-mode only

Conservative rule:

- Baseline P&L = actual Corsair P&L over the period (from fills + hedge
  stream).
- For each fill, if the model signal at that fill's decision time would
  have gated that side, remove the fill from the P&L ledger.
- Counterfactual P&L = baseline P&L − removed-fill MtM at horizon h.
- Counterfactual − baseline = signal-driven economic lift.

**Skew mode is out of scope for v1** — it requires queue-position modeling
we don't have data for. Gating mode is the conservative lower bound.

Costs: exchange fees, IBKR commissions, per-fill slippage estimate
(default 0.5 ticks; configurable in `config/study.yaml`).

### 7.6 models/baseline.py

LogisticRegression with class balancing and L2. Required as the floor —
if GBM doesn't beat this, it's noise. Standardize features. Fit per
horizon, per label flavor.

### 7.7 models/gbm.py

LightGBM, library defaults, monotone constraints off. Fit per horizon,
per label flavor. Output: predicted probability per row.

### 7.8 models/cheater.py

Perfect-foresight: returns true forward label as prediction. Run through
`economic.py` for theoretical economic ceiling. **The most important
number for the kill/pass decision.** If ceiling < 2× costs, the project
is dead regardless of what real models do — stop here.

### 7.9 walkforward.py

- Time-ordered, no shuffling, no leakage.
- Train window: 4 weeks. Test: 1 week. Slide forward 1 week.
- Refit every fold. No test-fold information in training.
- Output: per-fold AUC, calibration curve, economic lift via `economic.py`.

### 7.10 economic.py

Wraps `simulator.py`. Takes a model's predictions over a fold's test set,
applies them as gating decisions, returns counterfactual P&L. Accumulates
across folds. Reports:
- Total economic lift ($)
- Lift per fill
- Lift per day
- Fold-wise bootstrap CI

### 7.11 report.py

Generates `reports/feasibility_<date>.md` with:

1. Executive summary (verdict + 3 numbers + 1 paragraph). 60-second read.
2. Adverse selection summary (the prize).
3. Cheater-model ceiling (the upper bound).
4. Logistic baseline AUC + economic lift per horizon/flavor (the floor).
5. LightGBM AUC + economic lift per horizon/flavor (the realistic case).
6. Per-fold stability — does signal persist across regimes?
7. Kill/pass verdict against `config/study.yaml` thresholds.

Plots in appendix.

## 8. Kill / pass thresholds (defaults; tune in `config/study.yaml`)

- **Kill** if cheater lift < 2× total transaction costs over study period.
- **Kill** if best non-cheater walk-forward AUC < 0.53.
- **Defer** if cheater > 5× costs but no-cheat lift < costs — signal
  exists but unreachable with intra-HG features. Revisit after
  cross-asset data subscription.
- **Pass to full build** if cheater > 5× costs **and** no-cheat lift > 2×
  costs in walk-forward.
- **Marginal / restrict** if signal works in one regime and dies in
  others. Report the regime explicitly.

## 9. Validation / testing

- Unit tests for label correctness (synthetic price series with known
  forward returns).
- Unit tests for simulator conservatism (gating a known-toxic fill must
  reduce P&L; gating a profitable fill must lower P&L by exactly that
  fill's MtM).
- Snapshot tests for parse.py schema validation.
- **Deliberate-leak test**: shuffle labels forward by one row, confirm
  AUC collapses to 0.5. Catches lookahead bugs.
- End-to-end integration test: synthetic 1-day JSONL → full report.
  Required for every PR.

## 10. Build order (do not skip)

1. `parse.py` — replay JSONL → parquet.
2. `adverse_selection.py` — compute the prize. **Stop here and inspect
   before going further.** If the prize is < 2× costs, the project is
   dead and you've spent ~2 days, not 2 weeks.
3. `labels.py` — multi-horizon forward returns.
4. `simulator.py` — gating-mode P&L counterfactual.
5. `cheater.py` + `economic.py` — compute the theoretical ceiling.
   **Second checkpoint.** If cheater lift < 2× costs, kill the project.
6. `features.py` — engineered intra-HG features.
7. `baseline.py` — logistic regression.
8. `gbm.py` — LightGBM.
9. `walkforward.py` — wrap baseline + GBM in walk-forward CV.
10. `report.py` — produce the deliverable.

## 11. Getting started

```bash
# Project setup
uv init feasibility && cd feasibility
uv add polars pandas scikit-learn lightgbm pyarrow pytest pyyaml

# Data mirror (read-only; do not write back to Alabaster)
rsync -av alabaster:~/corsair/logs-paper/ data/raw/

# Sanity check that the rsync covers a long-enough study window
ls data/raw/ | wc -l   # want at least 30 trading days
```

Then build in the order in §10.

## 12. Reporting back

The deliverable is `reports/feasibility_<date>.md`. Executive summary
must include:

- **Verdict**: KILL / PASS / DEFER / MARGINAL
- **Adverse selection prize** ($/day)
- **Cheater ceiling** ($/day)
- **Best realistic model lift** ($/day, with model name)
- One paragraph of context — what worked, what didn't, what's the
  bottleneck.

Plots, fold tables, feature importance, and per-regime breakdowns go in
the appendix. The executive summary stands alone.
