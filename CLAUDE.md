# Corsair v3 — Operator Notes

Hard-earned lessons. Read before debugging anything weird around connectivity,
order lifecycle, or margin display. Each entry took hours to find live; the
goal of this doc is to never re-discover them.

## v3 cutover (2026-05-02) — what changed since the v2 notes were first written

Phase 6.7 retired the Python broker. The hot path is now fully Rust:

```
PRODUCTION STACK (post-2026-05-02)
  corsair-broker-rs    Rust daemon, native IBKR wire client, clientId=0,
                       owns position/risk/hedge/snapshot/IPC server
  trader               Rust binary, SHM IPC consumer, decides + places orders
  ib-gateway           Unchanged
  dashboard            Streamlit, reads hg_chain_snapshot.json
```

Binaries are baked into the corsair image at:
- `/usr/local/bin/corsair_broker_rust`
- `/usr/local/bin/corsair_trader_rust`

**ib_insync is gone from the runtime hot path.** It's no longer in
requirements.txt. The PyO3 bridge crate (`corsair_broker_ibkr`) was
deleted in Phase 6.11. The legacy Python broker entry point
(`src.main`) was deleted in Phase 6.8a.

Sections written for the Python broker era are marked
**[HISTORIC]** below where the underlying behavior no longer
applies (e.g. §2 ib_insync openTrades quirks, §5 lean connect
bypass). The lessons are preserved as context for postmortems
but should not guide live debugging.

Active config for the Rust runtime: `config/runtime_v3.yaml`.
The legacy `config/hg_v1_4_paper.yaml` and `config/corsair_v2_config.yaml`
were deleted in the 2026-05-05 cleanup pass — they had been dead since
the cutover. The v1.4 spec mapping lives in `docs/hg_spec_v1.4.md`.

**Operational scripts**: `scripts/flatten_persistent.py` and
`scripts/flatten.py` were both broken (ib_insync) and have been
deleted. The replacement is the Rust-IPC `corsair_flatten` binary
(baked into the corsair-corsair image at `/usr/local/bin/corsair_flatten`,
source: `rust/corsair_ipc/examples/flatten.rs`). It reads
`hg_chain_snapshot.json`, computes one closing limit order per
non-zero position, and writes them to the broker's IPC commands ring
— bypassing the trader's risk gates so it works while the trader is
killed. Default behavior is aggressive (cross opposite-side BBO);
`--passive` rests at same-side BBO instead. See its `--help` for
flags. Single-shot, not a retry loop — re-run after partial fills.

## Current spec: v1.4 (HG front-only)

Active strategy spec: `docs/hg_spec_v1.4.md` (APPROVED 2026-04-19). Active
config: `config/runtime_v3.yaml` (the legacy `hg_v1_4_paper.yaml` and
`corsair_v2_config.yaml` were retired 2026-05-05 — they were no longer
loaded by any running service).

Key v1.4 differences from v1.3:
- Strike scope per spec: **symmetric ATM ± 5 nickels** (sym_5). Active
  config deviates — **asymmetric OTM-only**: calls ATM→ATM+5, puts ATM−5→ATM.
  See §12 for rationale and measurement. v1.3 was asymmetric NEAR-OTM+ATM.
- **Daily P&L halt at −5% capital** is the PRIMARY defense — kills
  cancel quotes + session-level halt (operator override 2026-04-22 keeps
  positions in place; see §8).
- Capital: $200K → $75K (Stage 1) → $500K live (operator override; §7).
- Hedging subsystem in Rust (`corsair_hedge`); mode **execute** as of
  2026-04-26 (skips near-expiry contracts via runtime contract resolver).
  See §10.
- **Operational kills (SABR RMSE / quote latency / abnormal fill rate)
  are NOT implemented in the Rust runtime.** Spec §7 lists them; the
  Python runtime had `src/operational_kills.py`; the cutover dropped
  the module and never re-implemented in Rust. Tracked in §15 deviations.
- 8 JSONL streams in `logs-paper/` per spec §9.5.

## Spec deviations summary (current state vs v1.4 spec)

This is the cumulative drift table. Each is documented elsewhere; this is
the index. **Stage 1 acceptance evaluation** must use the spec, not the
current operational config.

| Item | Spec | Live | Section |
|---|---|---|---|
| Capital | $75K (Stage 1) | $500K | §7 |
| Strike scope | sym_5 (11 strikes × 2) | asymmetric OTM-only | §12 |
| Daily P&L halt action | flatten + halt | halt only (positions preserved) | §8 |
| Margin kill action | flatten | halt only | §7 |
| vega_kill | $500 | 0 (disabled) | §13 |
| Effective-delta gating | unspecified | combined options+hedge w/ §14 fail-closed staleness | §14 |
| Architecture | single Python process | Rust broker + Rust trader (Phase 6.7 cutover 2026-05-02) | §15 |
| Hedge near-expiry lockout | 7 days (shared with options engine) | 30 days (hedge-specific knob added 2026-05-01) | §10 |
| Operational kills (RMSE/latency/fill-rate) | required (§7) | not implemented in Rust runtime | §15 |
| `quoting.dead_band_ticks` | unspecified | 2 ticks ($0.001 on HG); 20× tighter than legacy paper config | §16 |
| `hedging.ioc_tick_offset` | unspecified | 4 (bumped 2→4 on 2026-05-04) | §10 |
| Tiered improving-fill exceptions | tier-1 margin escape + tier-2 per-constraint improving (margin/delta/theta), carried over from `src/constraint_checker.py` | margin = hard halt (no improving exception); theta = improving-only via trader (§25); delta_kill removed (hedge owns it) | §22, §25 |
| Strategy-kill controller | broker-side risk_monitor fires delta/theta/vega kills | trader-centric: improving-only gates in `compute_risk_gates`, broker keeps only operational + margin + daily_pnl + new `trader_silent` watchdog | §25 |

ETH is tabled (config products list is HG-only) but the multi-product
architecture is preserved — re-enabling ETH is a matter of un-commenting
its product block and wiring market data.

## 0. Source is BAKED INTO the corsair-corsair image — `restart` ≠ `up --build`

Both Rust binaries (`corsair_broker_rust`, `corsair_trader_rust`) and
the corsair_pricing wheel are compiled into the `corsair-corsair`
image at build time. Both `corsair-broker-rs` and `trader` services
reference that image. The compose services do NOT volume-mount Rust
source. Code edits do NOT take effect on `docker compose restart` —
that just bounces the existing container running the cached image.

The `build:` directive lives on `corsair-broker-rs` (canonical build
site since the legacy Python `corsair` service was removed
2026-05-05). To deploy code changes:

```bash
docker compose up -d --build corsair-broker-rs
docker compose up -d --force-recreate trader
```

The first line rebuilds the image and recreates the broker; the
second recreates the trader so it picks up the new image. **`docker
compose build corsair-broker-rs` alone will NOT redeploy the
trader** — its running container continues using the old image until
explicitly recreated.

Burned ~20 minutes on 2026-04-09 testing "fixes" against an old image
because `restart` reported success and the new log lines never appeared.
Verify your edits are live by grepping the new log line in the first
boot output before drawing any conclusions about whether a fix worked.

Gateway and dashboard images are similarly built locally; same rule.

## 1. clientId=0 is REQUIRED on FA paper accounts

Our paper login (`DUP553657` under master `DFP553653`; set via
`IBKR_ACCOUNT` env var in `.env` — the `account_id` in yaml configs is a
placeholder) is a Financial
Advisor / Friends-and-Family account structure: one DFP master, several DUP
sub-accounts. On FA logins, **IBKR routes order status messages through the
FA master**. ib_insync's wrapper looks up trades by `(clientId, orderId)` to
dispatch status updates — when the routing rewrites the clientId on the way
back, the lookup misses and the status update is **silently dropped**. No
warning, no error, just nothing.

**Symptoms** when this is wrong:
- `placeOrder` returns a Trade, but `trade.orderStatus.status` stays at
  `PendingSubmit` forever even though IBKR has already advanced it to
  Submitted/Filled/Cancelled
- Fills still arrive correctly via `execDetailsEvent` (that path uses
  `execId`, not `(clientId, orderId)`), so positions accumulate while the
  dashboard shows everything as "pending" — this is exactly how the morning
  of 2026-04-08 silently built up a 24-short-put position we couldn't see
- Quote engine modify cycle hits "Cannot modify a filled order" because it's
  reading stale local state and trying to amend orders IBKR knows are gone

**Fix:** set `client_id: 0` in `config/corsair_v2_config.yaml`. clientId=0 is
the canonical "master" client that receives order status messages for orders
placed by ANY client on the connection. Our `connection.py` also calls
`ib.reqAutoOpenOrders(True)` when clientId is 0, which is required for the
master-client mode to function.

**Also REQUIRED on FA accounts:** every order must specify `account=` (we set
`account=self._account` in `quote_engine._send_or_update`). Without it, IBKR
returns Error 436 "You must specify an allocation."

## 2. `openTrades()` returns multiple Trade objects per orderId  **[HISTORIC — ib_insync only]**

This was an ib_insync wrapper bug. The Rust native client doesn't
have a Trade-object abstraction — orderId → state mapping is direct.
Preserved for postmortem context.

ib_insync sometimes constructs a NEW Trade object when an `openOrder`
callback fires (notably after `reqAutoOpenOrders` adopts an order on
clientId=0). The Trade returned by `placeOrder` becomes an **orphan** that
nobody updates. Meanwhile, the canonical Trade — the one that ib_insync
mutates in place on every status event — is a separate instance with the
same `orderId`.

**Result:** `ib.openTrades()` can return BOTH the orphan AND the canonical
Trade for the same orderId. Iterating it naively and returning early on the
first match will hand back the orphan, which stays at PendingSubmit forever.

**Rule:** in `quote_engine._canonical_trade(order_id)`, walk the entire
openTrades list and return the **last** match. The dict-comprehension idiom
`{t.order.orderId: t for t in ib.openTrades()}` works the same way (last
write wins) and is what `_build_our_prices_index` uses.

**Don't** cache the placeOrder return value in a local dict — that's the
orphan. Always re-resolve from `_canonical_idx` (which `_build_our_prices_index`
populates) or fall back to walking openTrades.

## 3. Synthetic SPAN runs ~25-30% high vs IBKR for short strangles

Our `synthetic_span.py` is calibrated against single-leg naked shorts and
gets within ±10% per leg. But for multi-leg portfolios — especially short
strangles straddling F — synthetic systematically overstates margin by
~25-30% because the model can't replicate IBKR's inter-strike SPAN offsets.

**Calibrate at runtime, not at re-fit time.** In
`constraint_checker.IBKRMarginChecker`:
- `update_cached_margin()` reads `MaintMarginReq` from IBKR account values
  every ~5 min and computes `ibkr_scale = ibkr_actual / raw_synthetic`
  (bounded to `[0.5, 1.25]` for safety; falls back to 1.0 outside that range)
- `get_current_margin()` and `check_fill_margin()` apply `ibkr_scale` to
  every output so the constraint checker (and dashboard) compare against
  IBKR-equivalent values, not raw synthetic
- The bound prevents the scaling from masking a real risk model failure: if
  ratio drifts outside the band, we'd rather be conservative than blow the cap

**Don't** try to re-calibrate the scan ranges in `synthetic_span.py` to fix
this. The model is structurally limited; the scale-factor approach is the
right answer for runtime.

## 4. ETH options have a daily ~1-hour close (16:00–17:00 CT)

ETH **futures** trade nearly 24/5, but ETH **options on futures** (the
ETHUSDRR product we quote) have a daily settlement break: roughly **16:00 CT
to 17:00 CT** (21:00–22:00 UTC). During this window:

- IBKR returns `bid=-1.0 ask=-1.0 last=nan` for every option contract (the
  "no data available" sentinel)
- Our SABR fitter logs `WARNING: SABR calibration skipped: only 0 valid quotes`
- `find_incumbent` returns `empty_side` for everything, so `_process_side`
  never places quotes
- The watchdog is fine because the *underlying futures* tick stream is still
  alive — only the options product is dark

**This is normal.** Don't restart, don't recreate the gateway, don't panic.
The system will resume quoting automatically when options reopen at 17:00 CT.

If you see `bid=-1` from a probe, just wait for the reopen. If the futures
tick stream ALSO died, that's a real problem and the watchdog should be
flapping.

## 5. Lean connect bypass (don't let ib_insync's stock connectAsync run)  **[HISTORIC — ib_insync only]**

The native Rust client's connect path doesn't go through ib_insync,
so the per-FA-account bootstrap timeout is structurally avoided.
NativeBroker boot ~50ms (vs 30-90s for the lean Python bypass).
Preserved for postmortem context — if we ever revert to PyO3+ib_insync
for any reason, this is the gotcha.

ib_insync's `IB.connectAsync` issues a long list of initializing requests
in parallel after the API handshake: positions, open orders, **completed
orders**, **executions**, account updates, **and per-sub-account multi-account
updates** for every account on the FA login. On a paper login with 6
sub-accounts and a heavy overnight order history, this bootstrap consistently
times out — completed orders alone can take 60+ seconds.

**Our `connection.py` replaces it with a hand-rolled bootstrap** that issues
only the four requests we actually need:
1. `client.connectAsync` — TCP/API handshake
2. `reqPositionsAsync` — to seed our position book
3. `reqOpenOrdersAsync` — to know what's resting from prior runs
4. `reqAccountUpdatesAsync` — for cash/margin/balance state

Plus `reqAutoOpenOrders(True)` if `client_id == 0`.

This brings the connect from ~33-90s down to ~0.3-1s. **Do not switch back
to `ib.connectAsync`** unless you've also fixed all the bloat-request
timeouts upstream.

## 6. ETH option contract multiplier is 50, not 100

`config.product.multiplier: 50`. Most equity options use 100; ETH futures
options on CME use 50. If you see P&L or margin numbers that look 2× too
big or 2× too small, check the multiplier first.

## 7. Capital + cap

Currently configured at $500K capital (bumped 2026-04-22 from $75K paper
after the kill-switch behavior change below made manual margin monitoring
the primary control):
- `capital: 500000` ($500K)
- `margin_ceiling_pct: 0.50` (soft ceiling = $250K, blocks new opens)
- `margin_kill_pct: 0.70` (kill at $350K, **halt not flatten** — see below)
- `daily_pnl_halt_pct: 0.05` (daily halt at −$25K, **halt not flatten** — §8)
- `delta_ceiling: 3.0`, `theta_floor: -500`, `theta_kill: -500`
- `delta_kill: 5.0`, `vega_kill: 0` (disabled 2026-04-23 — see §13)

**Greek kills did NOT scale with capital** when we moved to $500K. 5
contract-deltas is the same hedge capacity regardless of book size, so
under a bigger book it's now easier to bind on `delta_kill` before
`margin_kill`. Worth revisiting if the strategy consistently binds on
delta before margin.

**Margin kill behavior (operator override 2026-04-22)**: on breach, the
kill fires with `kill_type="halt"` — cancel all resting quotes, session
lockout (source=risk, sticky, requires `docker compose restart corsair`
to clear), but **positions are NOT flattened**. Deviates from v1.4 §6.2
which specified flatten. Rationale: on 2026-04-22 the margin kill flattened
an 8-position book into wide spreads via IOC limits and erased $1.6K of
morning P&L in 700ms (realized net −$1,325 after starting the hour at
+$1,600 MtM). Operator now monitors margin manually and unwinds
discretionarily. To reinstate flatten, flip `kill_type` back in
`risk_monitor.py` at the margin kill call site and update
`INDUCE_SENTINELS["margin"]`.

## 8. Daily P&L halt: HALT (cancel + session-level lockout, not flatten)

**Operator override 2026-04-22**: deviates from v1.4 §6.1 which specified
**flatten all positions + flatten hedge**. Now uses `kill_type="halt"`:
cancel all resting quotes, session-level lockout, **positions preserved**.
Same rationale as margin kill (§7) — flatten slippage into wide option
spreads is too costly.

The path:

1. `RiskMonitor.kill(..., kill_type="halt")` fires (threshold:
   `daily_pnl_halt_pct × capital`, default −5%)
2. `quotes.cancel_all_quotes()` runs
3. Paper-stream `kill_switch-YYYY-MM-DD.jsonl` row emitted with full book
   snapshot (positions, margin, pnl, greeks). `book_state.kill_type`
   field now reads "halt" instead of "flatten".
4. `risk.killed = True`, source="daily_halt"

**Session rollover at 17:00 CT** still auto-clears the daily_halt source
(see `risk.clear_daily_halt()`), so quoting resumes automatically at next
session with whatever position inventory we had at halt time. Every other
kill source is sticky — requires manual restart + investigation before
quoting resumes.

**Per-fill halt check**: the fill handler calls
`risk.check_daily_pnl_only()` on every live fill so the halt fires
between the 5-minute full risk checks. Daily P&L includes hedge MTM
via `RiskMonitor._hedge_mtm_usd()`. Same `kill_type="halt"` applies.

**The `flatten_callback` is still wired in main.py** — no live call path
invokes it today, but the plumbing is preserved for future re-enablement
and induced-test parity.

## 9. v1.4 induced kill-switch tests (Gate 0)

Every v1.4 Tier-1 kill must pass an induced-breach test before Stage 1
launch (§9.4). Induce via sentinel file:

```bash
docker compose exec corsair python scripts/induce_kill_switch.py --switch daily_pnl
docker compose exec corsair python scripts/induce_kill_switch.py --switch margin
docker compose exec corsair python scripts/induce_kill_switch.py --switch delta
docker compose exec corsair python scripts/induce_kill_switch.py --switch theta
docker compose exec corsair python scripts/induce_kill_switch.py --switch vega
```

The script writes `/tmp/corsair_induce_<switch>`; `RiskMonitor.check()`
picks it up on the next cycle, fires the matching kill through its real
path (cancel + flatten/halt + paper log), and deletes the sentinel.
Induced kills carry `source="induced_<source>"` in
`kill_switch-YYYY-MM-DD.jsonl` so reconciliation can distinguish them
from genuine breaches.

Non-daily_halt induced kills are sticky — `docker compose restart corsair`
to clear. daily_halt induced kills auto-clear at the next CME session
rollover (17:00 CT).

## 10. Hedge mode: execute (re-enabled 2026-04-26 after Phase 0 fix)

`src/hedge_manager.py` runs in one of two modes via
`config.hedging.mode`:
- **observe**: computes required hedge trades and logs intent to
  `logs-paper/hedge_trades-YYYY-MM-DD.jsonl`; does NOT place futures
  orders. Local `hedge_qty` is updated optimistically so daily P&L
  halt still sees hedge MTM as if trades had filled.
- **execute** (CURRENT): places aggressive IOC limit orders at market
  ± 1 tick on the first tradeable HG futures contract past the
  near-expiry cutoff.

**Current state 2026-04-26**: config is `mode: "execute"`.
`hedge_manager.resolve_hedge_contract()` now skips contracts within
`near_expiry_lockout_days` (default 7), routing trades to e.g. HGK6
instead of HGJ6 during HGJ6's lockout window — that was the Phase 0
fix (commit `b598c7b`). First execute fill landed 2026-04-26 19:30
UTC (order id `141`, BUY 3 at 6.0755, periodic rebalance, net_delta
−3.08 → −0.08); zero Error 201 rejections in the gateway log for the
session.

**Phase 0 gate** (Thread 3 deployment runbook): ≥1 trading session
with execute firing AND no Error 201 rejections. 2026-04-26 session
result: 4 execute fills (BUY 3@6.0755, BUY 1@6.0248, SELL 1@6.0080,
SELL 1@5.9983), every fill at F (zero tick slippage), position
cycled to flat, peak hedge MTM drawdown −$412.50, zero Error 201s.
**Gate satisfied** — but treat the flip as conditional until further
sessions confirm; revert to observe if Error 201s reappear.

History: execute was previously flipped 2026-04-22 (commit `2fc3858`)
and reverted same day (commit `470d09f`) because IBKR rejected orders
on front-month HG futures (HGJ6) during the near-expiry lockout
window. The 2026-04-26 fix is the contract resolver skipping
near-expiry, not a config-only revert.

**RESOLVED 2026-04-27 evening** (was elevated to live-deployment hard
prerequisite earlier same day when §14 effective-delta gating shipped).
Three-layer reconciliation now in place:

- **Boot reconcile** (`hedge_manager.reconcile_from_ibkr`): on every
  startup, reads `ib.positions()`, finds the FUT position matching the
  resolved hedge contract by conId/localSymbol, sets `hedge_qty` and
  `avg_entry_F` to match. Wired in `main.py` after `resolve_hedge_contract`,
  before the first `risk_monitor.check()`. Closes the boot-state-loss
  failure mode (in-memory `hedge_qty` reset → spurious delta_kill on
  every restart with options_delta > 5.0).
- **Periodic reconcile** (`hedge_manager.rebalance_periodic` calls
  `reconcile_from_ibkr(silent=True)` at the top of every 30s tick).
  Catches divergences from non-filling IOCs that previously left
  optimistic `hedge_qty` out of sync. Silent when local matches IBKR;
  loud on actual divergence so operator sees auto-corrections.
- **execDetailsEvent listener** (`hedge_manager._on_exec_details`):
  subscribed at construction; filters for FUT fills matching the
  resolved hedge contract; updates `hedge_qty` + `avg_entry_F` +
  `realized_pnl_usd` from IBKR's actual execution data. Deduped by
  execId. **Replaces the optimistic-on-placement update path** in
  `_place_or_log` execute branch — observe mode still uses optimistic
  since no real order is sent.

Plus a related fix: `_subscribe_hedge_market_data` subscribes to the
resolved hedge contract's live ticker so IOC limits are anchored on
the **actual hedge contract** (HGK6), not the options-engine
underlying (HGJ6). Calendar spread of 7 ticks observed 2026-04-27
made every BUY IOC die against the wrong-contract anchor.

Original limitation note (preserved for context): no
execDetailsEvent callback for hedge fills, no periodic
reconciliation against `ib.positions()`. Implications now that
execute is live:
- If an IOC doesn't fill (market moved past ±1-tick limit in RTT
  window), local `hedge_qty` says we hedged but IBKR says we didn't.
- If the IOC fills at the limit (not F), `avg_entry_F` is off by 1
  tick — small MTM error.

**To revert to observe**: flip `hedging.mode: execute → observe` in
`config/hg_v1_4_paper.yaml` and `docker compose up -d --build corsair`.

Bounds note: hedge_manager has no explicit cap on `hedge_qty`. Target
is always `-round(net_delta)` to flatten the options delta. Effective
bound comes from `delta_kill: 5.0` — if portfolio |net_delta| > 5 the
delta kill fires (`kill_type="hedge_flat"`) and force-flats hedge_qty
to 0. Tolerance band is `tolerance_deltas: 0.5` (no trade if
|effective_delta| ≤ 0.5). Rebalance cadence 30s + on every option fill.

## 11. The dashboard polls; it doesn't stream

Streamlit is request/response. Currently:
- corsair writes `data/chain_snapshot.json` every **250ms** (4Hz)
- Streamlit page reruns every **500ms**
- Chain table fragment runs every **250ms** (matches snapshot rate)

If you need true push, you have to escape Streamlit (FastAPI + SSE / WebSocket
sidecar). Several hours of work; deferred indefinitely.

## 12. Strike scope deviation: asymmetric ATM+OTM, not sym_5

**Operator override 2026-04-20** (commit `bde574b`): deviates from v1.4
§3.2 which specified symmetric sym_5 (11 strikes × 2 rights = 22
instruments, 44 resting orders). Effective behavior is asymmetric
ATM+OTM only — calls quoted ATM and OTM (above F), puts quoted ATM and
OTM (below F). Result: ~12 instruments, ~24 max resting orders (ATM
strike shared C+P).

**Where this is enforced (post v3 cutover)**: in the Rust trader, NOT
the YAML. `runtime_v3.yaml` sets `quote_range_low/high: -5..5` per
product, but that knob is no longer load-bearing for quoting — it's
only consumed as a fallback for `strike_range_low/high` (the
market-data subscription window) at `corsair_broker/src/subscriptions.rs:337-338`.
The asymmetric ATM+OTM filter actually lives in
`corsair_trader/src/decision.rs:155-164`:

- ATM-window: skip if `|strike - forward| > MAX_STRIKE_OFFSET_USD` (0.30)
- OTM-only: skip ITM calls (K < F − 0.025) and ITM puts (K > F + 0.025)

The Python-era `quote_range_low/high` per-right split is gone with the
Python broker. The trader's hardcoded constants (`MAX_STRIKE_OFFSET_USD`,
`ATM_TOL_USD`) are the canonical knobs now.

**Measurement that justified the deviation**: per-refresh gateway JVM
serialization latency was the dominant bottleneck (see
`project_fix_colo_evidence`). Halving order count halved per-refresh
serialization time. ITM strikes had thin flow and wide spreads — quoting
them consumed the order budget without generating fill volume.

**Risk being monitored**: short-C vs short-P inventory imbalance on
persistent underlying drift. If skew sustains >30%, re-enable an
`inventory_balance` gate. Delta kill at 5.0 is the backstop.

**Subscription window**: `strike_range_low/high` governs market-data
subscription (±7 strikes = $0.35 window for SABR wing stability). The
subscription window is rolled on ATM drift by `corsair_market_data/src/atm.rs`
so the boot-ATM anchor no longer goes stale.

**To revert to sym_5**: change `MAX_STRIKE_OFFSET_USD` to 0.25 and
delete the OTM-only branches in `decision.rs:155-164`. Expect ~2×
per-refresh order traffic unless FIX/colo has shipped by then.

## 13. vega_kill disabled 2026-04-23 (Alabaster characterization)

`vega_kill: 0` in `config/hg_v1_4_paper.yaml`. Deviates from v1.4 §6.2
which specified $500. Alabaster backtest (HG, 2024 Q1 / 2024 Q3 / OOS
2026 panels, $250K tier, production per-key cap=5) found the $500
threshold binds on **67-88% of fills** — choke, not tail backstop — and
in the OOS panel only **3.7% of $500-breaches coincide with margin > 80%
of cap**, so vega isn't leading margin stress. Rescaling to $3000 fixes
OOS (0.0% co-occurrence) but breaks 2024 Q3 (97.4%). Disable is the only
choice clean across all three panels; consistent with §87 ("defend tail
with daily P&L halt") and SPAN already pricing a 25% vol scan.

Precipitating incident: 2026-04-23 14:30 CT a real VEGA HALT fired at
net_vega=−624 on a $103K-margin book (21% of cap) — not a tail event,
just a strangle that drifted slightly short vega on a normal fill mix.
The sticky halt left us out of the market for the rest of the US
session until restart.

Guard: `risk_monitor.py:244` is `if vega_kill > 0 and abs(worst_vega) > vega_kill`,
so 0 is a no-op — no code change needed.

Evidence: `docs/findings/vega_kill_characterization_2026-04-23.md`.
HG-specific; re-characterize before any ETH capital bump.

## 14. Effective-delta gating (2026-04-27 paper, 2026-05-02 live-ready, fail-closed shipped 2026-05-05)

**Status update 2026-05-05**: hedge-state staleness now fails closed in
both `RiskMonitor::check` / `check_per_fill_delta` (corsair_risk) and
`ConstraintChecker::check` (corsair_constraint). When any HedgeManager's
last reconcile/fill is older than `HEDGE_FRESHNESS_MAX_AGE_NS` (5 min,
2.5× the periodic reconcile cadence), the gate strips `hedge_qty` back
out of effective delta and evaluates options-only, with an operator-
visible WARN. Wired in `corsair_broker/src/tasks.rs:213-242` (per-fill)
and `~530` (periodic) — the broker pre-fetches `hedge_fresh = ALL fresh`
and the per-product `hedge_qty_in_worst_delta`, then passes through.
Closes the §14 "live-blocked" gap.

**Status update 2026-05-02**: the §10 reconciliation prerequisite is now
satisfied in the Rust runtime — boot reconcile, periodic reconcile
(every 4 hedge ticks ~ 2 min), and `execDetails`-fed `apply_broker_fill`
are all wired (Phase 6.9 + 6.10). Live-deployment block is lifted.

The original 2026-04-27 notes below describe the Python-broker
implementation and its constraints; the Rust implementation
preserves the same gate semantics but reads `hedge_qty` from the
periodically-reconciled `HedgeManager` state.


`delta_ceiling` (constraint checker) and `delta_kill` (risk monitor) now
gate against **effective_delta = options_delta + hedge_qty** instead of
options-only. The hedge subsystem unlocks quoting capacity rather than
just flattening risk — the design intent of v1.4 §5 ("hedge target: net
book delta ≈ 0 at all times"), now realized at the gate level.

**What changed**:
- `constraint_checker.py:546`: `delta_for_product(prod) + hedge.hedge_qty`
- `risk_monitor.py:182, 177`: same, applied to per-product worst_delta
  loop AND the `__all__` cross-product fallback
- `hedge_manager.HedgeFanout.hedge_qty_for_product(product)` exposes
  per-product hedge state to RiskMonitor
- `main.py`: HedgeManager constructed before ConstraintChecker; passed
  in via `hedge_manager=hedge` kwarg (per-product); `RiskMonitor` gets
  the multi-product `hedge_fanout` (already wired)
- `snapshot.py`: portfolio block emits `options_delta` + `hedge_delta`
  + `effective_delta` triple; `net_delta` aliased to effective for
  dashboard back-compat
- `dashboard.py`: Net Delta tile shows `opt +X.XX | hdg ±N` breakdown
  beneath the headline so hedge masking is visible at a glance

**Toggle**: `constraints.effective_delta_gating: true` (default). Flip
false to fall back to options-only — wiring stays unconditional, the
flag is read inside both gate paths. Use for rollback during testing.

**Why this is paper-acceptable but live-blocked**: gates now depend on
`hedge_manager.hedge_qty` — local intent, not IBKR-confirmed state. Per
§10 there is no `execDetailsEvent` callback for hedge fills and no
periodic reconciliation against `ib.positions()`. Failure modes that
were tolerable when gates used options-only become safety regressions
under effective gating:
- IOC didn't fill (market moved past ±1-tick limit in RTT) — local
  says hedged, IBKR says no, gates think effective is flat, options
  drift unbounded
- Error 201 rejection at IBKR — silently rejected, local already
  credited, same as above
- Gateway disconnect during hedge order — order never reached the
  wire, local still credits

In paper today these are research concerns (paper hedge fills are
reliable). For LIVE, **§10 reconciliation is now a HARD prerequisite**:
- `execDetailsEvent` callback for hedge fills (or equivalent
  verification mechanism)
- Periodic `ib.positions()` reconciliation against `hedge_qty`
- Alert / kill on drift between intended and confirmed hedge state

Do not deploy live with effective-delta gating until the above is in
place. To run live before §10 lands, flip `effective_delta_gating: false`
to revert to options-only gates (capacity loss, but safety bound is
no longer hedge-trust-dependent).

**Spec status**: v1.4 spec (§5 + §6.2) doesn't explicitly mandate either
options-only or effective gating. §5 says "hedge target: net book delta
≈ 0 at all times" implying the relevant exposure metric is combined;
§6.2 says "±5.0 contract-deltas" without specifying. This change reads
the spec consistently as "all directional gates use the same combined
metric the hedge subsystem already targets." Per
`feedback_spec_deviation`, the change is documented at each call site
and here.

## 15. Broker/trader split — fully shipped (Phase 6.7, 2026-05-02)

The architectural split is now production. Two Rust processes share a
single IBKR clientId via a SHM-ring IPC layer (msgpack frames). The
trader is a self-contained binary; the broker can in principle be
swapped (FIX/iLink) without touching the trader.

### Production topology

```
   corsair-broker-rs                       trader (corsair_trader_rust)
   ─────────────────────────               ──────────────────────────
   NativeBroker (Rust IBKR client)         No gateway connection
   clientId=0 (FA master)                  SHM ring consumer
   risk monitor, hedge, snapshot           tick → decide_quote → place
   vol_surface fitter (60s)                6-layer safety stack (§16)
   IPC server (SHM + FIFO notify)          tokio multi-thread runtime
   forwards: ticks, fills, status,         sends: place_order,
   risk_state, vol_surface →               cancel_order, modify_order,
   ← receives commands from trader         telemetry → broker
```

### Phase history

| Phase | What | Status |
|---|---|---|
| 1-5  | Python broker + Python/Rust trader split | superseded |
| 5B   | Rust broker daemon (shadow mode) | shipped |
| 6.1-6.5 | Native Rust IBKR wire client | shipped |
| 6.6  | NativeBroker — Broker trait impl | shipped |
| 6.7  | Production cutover; Python broker retired | shipped 2026-05-02 |
| 6.8-6.11 | Cleanup, P0/P1 fixes, dead-code removal | shipped 2026-05-02 |

### Env flags (default-on; opt-out with explicit unset)

- `CORSAIR_BROKER_IPC_ENABLED` — defaults to `1` post-cutover. Broker
  hosts the SHM IPC server.
- `CORSAIR_TRADER_PLACES_ORDERS` — defaults to `1` post-cutover. Trader
  emits place/cancel/modify commands.
- `CORSAIR_IPC_TRANSPORT` — defaults to `shm`. The legacy `socket`
  transport is no longer wired in the Rust trader binary; it would
  require a Python-trader rebuild.
- `CORSAIR_TRADER_LANG` — defaults to `rust`. The Python trader was
  removed in Phase 6.8a.
- `CORSAIR_BROKER_KIND` — defaults to `ibkr`, resolves to NativeBroker.
  No alternate adapter is currently implemented.

### Operational gotchas (still applicable)

- **Compose env propagation**: each `docker compose stop` + `up` must
  include any non-default env vars inline. Each Bash session is a
  fresh shell — exports don't persist.
- **Sticky risk kills survive restarts** for `source=risk` (delta,
  margin, etc.). To clear: `docker compose restart corsair-broker-rs`
  AND verify `kill_switch-*.jsonl` shows no recent risk-source kills.
- **`scripts/flatten.py`** is the sanctioned way to clear positions
  when the broker can't auto-recover. Stop the broker first, then run
  with `--product HG --order-type limit` (HG is thin, market orders
  give bad fills).
- **TTT histogram**: trader's per-event TTT is logged every 10s in
  the trader's container logs as `ttt_p50_us`/`ttt_p99_us`. Empty
  (`None`) means no events processed (markets closed or no ticks).
- **Legacy `corsair` service removed 2026-05-05.** The pre-cutover
  Python broker service was kept as `--profile legacy-python-broker`
  for a few days post-cutover, but its CMD was already running the
  Rust broker (with the wrong config path) so the "rollback profile"
  never actually rolled back. It was deleted from docker-compose.yml.
  Real rollback path: `git revert` the Phase 6.7+ commits and rebuild
  the image; no in-place runtime fallback exists.

### Standard command-line workflow

```bash
# Default — brings up the live Rust stack (4 services:
# ib-gateway, corsair-broker-rs, trader, dashboard)
docker compose up -d

# Rebuild + redeploy after code changes
# (corsair-broker-rs owns the build for the corsair-corsair image
#  shared with trader)
docker compose up -d --build corsair-broker-rs
docker compose up -d --force-recreate trader

# Stop everything (gateway + dashboard stay up)
docker compose stop corsair-broker-rs trader

# Emergency rollback to Python broker — requires git revert of
# Phase 6.7..6.11 + image rebuild. There is no runtime-only path
# (Python source was deleted in Phase 6.8a).
git revert --no-commit <Phase 6.7..6.11 SHAs>
docker compose up -d --build corsair-broker-rs
```

## 16. Cut-over safety stack (cleanup passes 3-6, 2026-05-01)

The cut-over path produced ~$25K of paper losses on 2026-05-01 across
multiple incidents. Root cause was the trader passing **current spot**
to SVI's compute_theo when the surface is anchored on **fit-time
forward** (commit `e6486f6`). Six layers of defense now sit on top of
that fix to prevent recurrence:

### Layer 1 — Fit-time forward (THE root-cause fix, e6486f6)

`compute_theo` in `src/trader/quote_decision.py` requires the fit-time
forward (which broker emits in every `vol_surface` IPC event), NOT the
current underlying spot. SVI's `m` parameter is anchored on
log-moneyness relative to that specific F; using a different F at
evaluation silently re-anchors the wing flex point. Reproduction:
HXEK6 P560 with F_fit=6.021 returned theo=0.0275 (matches broker);
same params with F=5.96 (current spot) returned theo=0.0337 — a 23%
gap on a deep wing put. That gap drove BUY at theo-1tick=0.033
into ask=0.033 = picked-off-every-time.

### Layer 2 — Risk-feedback gate (#8)

Broker publishes `risk_state` event at 1Hz with margin/Greeks/effective
delta. Trader self-gates new placements:
- `effective_delta + 1.0 >= delta_ceiling` → block BUYs
- `effective_delta - 1.0 <= -delta_ceiling` → block SELLs
- `|effective_delta| >= delta_kill - 1.0` → block ALL
- `margin_pct >= margin_ceiling_pct` → block ALL
- risk_state stale >5s OR not yet seen → block ALL (fail-closed)

### Layer 3 — Strike-window restrictions

ATM-window: skip strikes more than `MAX_STRIKE_OFFSET_USD` ($0.30) from
current spot. OTM-only: skip ITM calls (K < F − $0.025) and ITM puts
(K > F + $0.025). Spec §3.3 / CLAUDE.md §12.

### Layer 4 — Dark-book and thin-book guards (decision time + on-rest)

Decide-time check: refuse if bid <= 0, ask <= 0, bid_size < 1, or
ask_size < 1. ON-REST check (10Hz staleness loop): cancel any
resting order whose latest tick shows the market has gone dark (any
of the same conditions). 2026-05-01 burst: 17 fills in 11s at
bid=0/ask=0 — root cause is paper-IBKR matching against ghost flow
when displayed BBO is one-sided. ON-REST cancellation closes that
attack vector.

### Layer 5 — Forward-drift guard

Refuse to quote when `|current_spot - fit_forward| > 200 ticks`
($0.10). SVI extrapolation becomes unreliable when underlying moves
far from the anchor; broker's fit cadence (~60s) usually catches
this, but the guard covers the gap.

### Layer 6 — Throttling: dead-band + GTD-refresh + cancel-before-replace

Mirrors broker's `_send_or_update`. Three-rule chain:

1. **Dead-band**: skip if |new_price - rest_price| < `DEAD_BAND_TICKS`
   (1 tick) AND age < `GTD_LIFETIME_S - GTD_REFRESH_LEAD_S` (3.5s).
2. **GTD-refresh override**: bypass dead-band when ≥3.5s has elapsed
   since the last place — keeps the quote alive before GTD-5s expiry.
3. **Cooldown floor**: 250ms hard minimum between sends per key
   (defensive backstop).

Cancel-before-replace: when re-placing at a key with a known orderId,
send `cancel_order` for the old orderId immediately before sending
the new `place_order`. Without this, every re-place leaves the OLD
order at IBKR pending GTD-expiry (5s), bloating IBKR's order book and
pushing place_rtt_us above 1s. Skip cancel if old orderId unknown
(place_ack hasn't arrived yet) — falls back to GTD-expiry.

### Pre-flip checklist + monitoring

Before flipping `CORSAIR_TRADER_PLACES_ORDERS=1`, run the preflight:

```bash
docker run --rm --network host \
    -v $PWD/scripts:/app/scripts:ro \
    -v $PWD/data:/app/data:ro \
    -v $PWD/logs-paper:/app/logs-paper:ro \
    corsair-corsair python3 /app/scripts/cut_over_preflight.py
```

Returns nonzero if any of 9 checks fail (containers up, position flat,
no kills, daily P&L healthy, SABR fits fresh, risk_state flowing, hedge
resolved, trader decisions active, no recent adverse fills). Treat
green as the gate to enabling cut-over.

While running cut-over, leave `scripts/live_monitor.py` open in a
shell — it polls snapshot every 5s and alerts on adverse fills,
position drift, concentration, or P&L approaching halt.

### Decision counters in trader telemetry

The trader emits a counter dict in its 10s telemetry log line. Useful
for tuning:

| Counter | Means |
|---|---|
| `place` | decide_quote returned "place" |
| `skip` (with reason) | decide_quote returned "skip" + reason |
| `skip_off_atm` | trader-level ATM-window block |
| `skip_itm` | OTM-only restriction |
| `skip_in_band` | dead-band gate (price hasn't moved enough) |
| `skip_cooldown` | per-key cooldown floor |
| `staleness_cancel` | resting order cancelled — too far from theo |
| `staleness_cancel_dark` | resting order cancelled — market went dark |
| `replace_cancel` | cancel-before-replace fired |
| `risk_block` / `_buy` / `_sell` | risk gate engaged |

`skip_in_band` should dominate skip reasons in calm markets. High
`risk_block` rate means margin or delta is near a limit — operator
should investigate. High `staleness_cancel_dark` rate means liquidity
is thin; consider tightening `MIN_BBO_SIZE`.

## 17. Rust trader binary (cleanup pass 7-onwards, 2026-05-01)

Full Rust port of `src/trader/main.py` lives at
`rust/corsair_trader/`. The Python trader still works; the Rust
binary is selected at runtime via env var.

### What it is

A 9.8 MB native binary baked into the corsair docker image at
`/usr/local/bin/corsair_trader_rust`. Same protocol on the wire
(msgpack frames over SHM rings + FIFO notify), same env-var
conventions, same JSONL output format. Drop-in replacement for the
Python trader.

Feature parity: full 6-layer safety stack, dead-band, GTD-refresh,
cancel-before-replace + skip-near-GTD, tick-jumping with edge
constraint, staleness loop with dark-book on-rest guard, all 6
decision counters, place_ack handling, kill/resume, JSONL streams
(trader_events + trader_decisions, 256 MB rotation), uvloop-
equivalent (tokio multi-thread). SVI + SABR pricing via the
existing `corsair_pricing` Rust crate (Hagan SABR + SVI implied
vol + Black76).

### How to run

Selection is at runtime via `CORSAIR_TRADER_LANG`:

```bash
# Python trader (default — backward compat)
CORSAIR_BROKER_MODE=1 CORSAIR_TRADER_PLACES_ORDERS=1 \
    CORSAIR_IPC_TRANSPORT=shm \
    docker compose --profile broker-split up -d --force-recreate trader

# Rust trader (recommended — faster hot path)
CORSAIR_BROKER_MODE=1 CORSAIR_TRADER_PLACES_ORDERS=1 \
    CORSAIR_IPC_TRANSPORT=shm CORSAIR_TRADER_LANG=rust \
    docker compose --profile broker-split up -d --force-recreate trader
```

The Rust trader **requires** `CORSAIR_IPC_TRANSPORT=shm` — it does
not implement the legacy Unix-socket transport. Hard exit otherwise.

### Latency improvement vs Python trader

Measured 2026-05-01 in steady-state cut-over:

| Metric | Python trader | Rust trader | Δ |
|---|---|---|---|
| TTT p50 | ~380 μs | ~50 μs | 7.6× |
| TTT p99 | ~5.6 ms | ~1-3 ms | 2-5× |
| IPC p50 | ~110 μs | ~80 μs | 1.4× |
| Compute p50 | ~270 μs | ~10 μs | 27× |

The compute portion is sub-microsecond in Rust — the entire 6-gate
stack + decide_quote + cancel-before-replace logic runs faster than
1 μs end-to-end. Tail latency (TTT p99) is now dominated by tokio
scheduler interactions rather than Python orchestration.

### Architecture notes

- Single tokio multi-thread runtime (2 workers). Hot loop, staleness
  task, telemetry task, JSONL writers all share the runtime. No
  uvloop on the trader despite uvloop being faster on the broker
  (tested 2026-05-01: uvloop INCREASED trader's TTT p50 because
  libuv's scheduler gives more time to non-hot-path tasks; default
  asyncio's tighter `sleep(0)` semantics happens to be optimal here).
- `state::SharedState` is lock-sharded (Priority 1, 2026-05-04).
  Heavy maps (`options`, `our_orders`, `orderid_to_key`,
  `vol_surfaces`, `theo_cache`, `expiry_intern`, `kills`) live in
  `dashmap::DashMap` so the 10 Hz staleness sweep iterates one
  shard while the hot loop inserts to others without blocking.
  Histograms (`ipc_us`, `ttt_us`) and scalars (config + risk +
  underlying + weekend_paused) each live behind their own
  `parking_lot::Mutex`; the hot path snapshots scalars once per
  tick into a stack-local `ScalarSnapshot`. `DecisionCounters`
  fields are `AtomicU64` — counter bumps are wait-free. `kills`
  also has a parallel `kills_count: AtomicUsize` so the per-tick
  "any kills?" gate is one Relaxed load instead of an
  `is_empty()` that touches every DashMap shard. No deadlocks —
  the hot path holds at most one state lock at a time.
- `tte_cache` module memoizes parsed expiry datetimes per-thread
  (production has ~4 unique expiries; cache is essentially immortal
  after warmup).
- HashMap keys use `char` for `right` and `side` instead of
  `String`. Saves ~80% of hot-path heap allocations.
- `OptionState` is a slim Copy-only struct stored in the `options`
  dict instead of full `TickMsg` clones (which would carry redundant
  expiry/right strings).

### Where to look when debugging

- Container logs: `docker compose logs -f trader`
- Telemetry every 10s with full counter dict (search for
  `[corsair_trader] telemetry:`)
- JSONL streams in `logs-paper/`:
  `trader_events-YYYY-MM-DD.jsonl` (one line per inbound IPC event)
  `trader_decisions-YYYY-MM-DD.jsonl` (one line per place outcome)
- SHM ring drop monitor warns every 10s if `frames_dropped` grew
  on either ring (events or commands). Critical safety signal —
  if trader is too slow to drain, broker drops events including
  `kill` messages.

### Rollback

There is no in-place runtime fallback. The Python trader was deleted
in Phase 6.8a (no `CORSAIR_TRADER_LANG=python` path remains) and the
Python-vs-Rust parity scripts were removed in the 2026-05-05 cleanup.
To roll back: `git revert` the Phase 6.7+ commits and rebuild the
image. To temporarily stop quoting: stop the trader container — the
broker stays connected and snapshot/risk continue.

### Known limitations

- No partial-fill handling. Production places `qty=1` orders so
  partial fills are impossible; if that ever changes, the
  order_ack handler needs updating.

## 18. Code-quality cleanup pass (2026-05-05)

A full audit + cleanup pass landed 2026-05-05 (one commit per group).
Highlights worth knowing about when debugging:

### Correctness fixes
- **IPC ring memory ordering** (`corsair_ipc/src/ring.rs`): producer/
  consumer offsets now use `AtomicU64` with explicit `Acquire`/`Release`
  pairs. The prior plain `from_le_bytes`/`copy_from_slice` allowed the
  compiler to reorder data writes past the offset publish — silent
  cross-process data race under load.
- **Hedge-staleness fail-closed** in RiskMonitor + ConstraintChecker
  per §14 above. This was the long-standing live-deployment hard
  prerequisite from the 2026-04-27 effective-delta-gating change.
- **Realized P&L VWAP** (`corsair_position/src/portfolio.rs`):
  `add_fill` now blends same-direction fills via running absolute-qty
  VWAP. Previously each open overwrote `avg_fill_price` with the most
  recent fill, miscalculating realized P&L between IBKR reconciles.
- **`update_ask` updates `hedge_underlying`** (`corsair_market_data`):
  was bid/last only, leaving hedge mark unset on ask-first sequences.
- **Improving-delta sign-flip** (`corsair_constraint`): the "improving"
  exception used to fire when post-fill delta crossed zero with smaller
  abs(); now requires `sign(post) == sign(cur)` for the bypass.
- **NaN guards in pricing**: SABR `(1−2ρz+z²).max(0).sqrt()` clamp,
  finite-result fallback to alpha; SVI surfaces a counter on negative-
  variance floor; Brent returns `None` on non-convergence (was masking
  with last `b`).
- **Trader risk-gate margin None**: previously fail-closed only on
  `effective_delta = None`; now also on `margin_pct = None`.
- **`KillMsg.source` defaulting to `"?"`**: removed; unparseable kill/
  resume messages are now dropped + counted (was masking errors via a
  self-cancelling `"?"` kill/resume cycle).
- **MissedTickBehavior::Skip** on every broker `tokio::time::interval`
  (was default Burst — could fire several risk checks back-to-back on
  worker recovery).
- **`cancel_all_resting` join_all**: previously sequential await per
  cancel — O(N) RTT on the kill path; now concurrent.

### Performance
- **SVI fit `spawn_blocking`** (`vol_surface.rs`): the 5-50ms LM solve
  no longer blocks the runtime worker.
- **Vol-surface cache Arc-swap**: `vol_surface_cache` is now
  `Mutex<Arc<HashMap>>` — readers Arc-clone instead of cloning the full
  map every 250 ms snapshot tick.
- **Trader `PlaceOrder<'a>`**: borrows `expiry`/`right`; `side` is
  `&'static str`. Eliminates 3-4 String allocations per place.
- **Trader rings → `parking_lot::Mutex`** (was `std::sync::Mutex`).
- **`numerical_jacobian` pre-alloc** in calibrate.rs (was allocating
  `vec![vec![0.0; n]; m]` per LM iter).
- **`reqwest::Client` reused via `OnceLock`** in notify.rs.
- **`open_orders` snapshot cache** with 1s TTL on the snapshot path.

### Dead code purged
- Python: `src/sabr.py`, `src/pricing.py`, `src/trader/` deleted (broken
  imports — `backmonth_surface`/`utils` modules didn't exist).
- Scripts: `scripts/flatten.py`, `reconcile_positions.py`,
  `capture_place_order_bytes.py`, `parity_compare.py`,
  `rust_trader_parity.py` deleted (broken `ib_insync` imports).
  `scripts/flatten_persistent.py` was also broken (ib_insync) and
  was deleted on 2026-05-06; replaced by the `corsair_flatten` Rust
  binary (`rust/corsair_ipc/examples/flatten.rs`).
- Tests: `test_sabr.py`, `test_calibrate_parity.py`, `test_pricing_parity.py`,
  `test_decide_quote.py` — all unimportable. Only `test_ipc_protocol.py`
  remains.
- Configs: `config/hg_v1_4_paper.yaml`, `config/corsair_v2_config.yaml` —
  no longer loaded by any service.
- `requirements.txt`: `docker>=7.0` dropped (no Python imports it).
- Dockerfile: `COPY src/ tests/` removed (Rust services don't need them).
- Rust: `corsair_oms` shrunk from ~330 LOC of orphaned abstractions to
  ~70 (only `OrderBook::new` and `apply_status` are used). Trader
  `Decision::Skip`, `replace_cancel` counter, vestigial `OurOrder.send_ns`
  / `OptionState.{strike,ts_ns,broker_recv_ns}` all removed. Broker
  `pub type AsyncMutex<T>`, `with_market_view`, dead `ioc_tick_offset` /
  `hedge_tick_size` config knobs deleted.
- Cargo deps dropped: `smallvec`, `dashmap` from corsair_broker_ibkr_native;
  `log` and `chrono` from corsair_oms; `sync` feature from corsair_tick_replay.
- IBKR client `recvmsg_with_kernel_ts` (and the `SO_TIMESTAMPNS`
  enable) deleted. `requests::place_order` slow path marked
  `#[deprecated]` (production goes through `place_template::place_order_fast`).

### Cleanups
- `now_ns()` hoisted to `corsair_broker/src/time.rs` (was duplicated
  in 3 modules).
- Trader vol-surface lookup chain extracted to `state.lookup_vol_surface()`.
- Trader msgpack-decode parse-error pattern collapsed into a `decode_msg<T>`
  helper (was 9 sites).
- Trader `strike_key` switched from `f64::to_bits()` → quantized `i64`
  (`(s*10_000).round() as i64`) — fixes a latent bug where
  `6.025` and `6.0250000001` would hash to different bins.
- Default broker mode is now `Live` (was `Shadow`); flip
  `CORSAIR_BROKER_SHADOW=1` to opt-in.
- Stale Phase / ib_insync / PyO3 / "Mirrors X.py" comments removed
  throughout the broker and broker_api crates.

## 19. Taylor reprice — anchor on `spot_at_fit`, NOT `forward` (2026-05-06)

The trader applies a first-order Taylor adjustment to keep theos
tracking spot between SABR fits (~60s cadence):

```
theo ≈ theo_at_fit + delta_at_fit × (current_spot − spot_at_fit)
```

Both `theo_at_fit` and `delta_at_fit` are cached at fit time
(`theo_cache: DashMap<TheoCacheKey, (f64, f64)>`). The same formula
runs in both `decision::decide_on_tick` and `main::staleness_check`
so the staleness loop's drift comparison is against current-spot
theo, not fit-frozen theo.

### THE BUG WE LIVED THROUGH

Initial Rust port (2026-05-06 morning) used `(spot − pricing_forward)`
as the anchor. **This is wrong on HG.**

`spot` = HGK6 front-month tick stream we subscribe to.
`pricing_forward` = HGM6 parity-implied F (the option's actual
underlying month). They differ by structural calendar carry of
~$0.04–0.05 (80–100 ticks).

Treating that carry as "drift" shifted every theo by `delta × carry`
≈ `delta × $0.05` ≈ $0.025 per option. Direction:
- **Calls** (delta > 0, spot < forward → drift < 0) → theo dropped
  → BUYs landed below market mid (good if you want passive bids)
  → **SELLs would have been below the bid → cross-protect skipped
    every SELL → no two-sided quotes anywhere on the call side.**
- **Puts** (delta < 0) → mirror image, no two-sided BUY-side puts.

Live symptom: dashboard shows call BIDs only (no asks), put ASKs only
(no bids). `skip_would_cross_bid` and `skip_would_cross_ask` counters
spike. Verified live on 2026-05-06 between 12:35 and 12:45 UTC.

### THE FIX (current state)

Broker captures `und_spot` at fit time and ships it as `spot_at_fit`
in every `vol_surface` event:
- `corsair_broker/src/vol_surface.rs::snapshot_chain` returns
  `(forward, und_spot, …)` instead of `(forward, …)`.
- `VolSurfaceEvent` adds `spot_at_fit: f64`.

Trader stores it on `VolSurfaceEntry` and uses it as the Taylor
anchor:
- `corsair_trader/src/messages.rs::VolSurfaceMsg::spot_at_fit:
   Option<f64>` (Option for back-compat with older brokers).
- `corsair_trader/src/state.rs::VolSurfaceEntry::spot_at_fit: f64`.
- Decision flow: `(spot − vp_msg.spot_at_fit)`. Staleness loop:
  same.

### How to verify it's working

1. Pull a recent `vol_surface` event from `trader_events-*.jsonl` —
   it must contain BOTH `forward` AND `spot_at_fit`. They typically
   differ by $0.04–0.05 (the K6→M6 carry).
2. Check `decision.rs` — the formula MUST read `vp_msg.spot_at_fit`,
   never `vp_msg.forward` (or `pricing_forward`) for the Taylor term.
3. Live chain check: on a calm market with the trader running,
   most ATM-window strikes should be **two-sided** (`bid_live=True
   AND ask_live=True`). One-sided coverage is the canary for the
   carry bug recurring.

### Why this is independent of §16's e6486f6 fix

§16's root-cause was passing CURRENT spot to SVI's `compute_theo` —
breaking the SVI `m` log-moneyness anchor. That fix locks SVI to
`fit_forward`. Taylor reprice is a SEPARATE first-order correction
on top — it doesn't touch SVI at all, just shifts the post-Black76
theo. Both must coexist:
- SVI surface anchored on `fit_forward` (don't change this).
- Black76 evaluated at `fit_forward` (gives `theo_at_fit`,
  `delta_at_fit`).
- Taylor adjustment uses `(current_spot − spot_at_fit)`.

If you ever see a regression to `spot − forward` in the Taylor
formula, revert it immediately and replace with `spot − spot_at_fit`.
It is NOT just a small numerical difference — it kills two-sided
quoting entirely on any product with calendar carry.

## 20. Round-half-away tick quantizer — verified bias, deferred to live telemetry (2026-05-06)

The fp32 precision study (`/home/ethereal/fp32-precision-spike/`)
flagged a directional bias at half-tick targets and recommended
switching the tick quantizer in `corsair_trader/src/decision.rs:516-517`
from round-half-away-from-zero to round-toward-`mkt_mid`. Followup
verification + backtest concluded **the bias is real but the change
is deferred to Path C (live `effective_delta` telemetry observation)**
rather than shipped via PR. Recording the lineage here so the
finding doesn't get lost.

### What was found

`verification_fp64_directional_bias.md` (fp32-precision-spike repo)
confirmed that the round-half-away rule produces a deterministic
UP-bias on tick-aligned mid-capped targets in production fp64 — same
rule, opposite direction from the fp32 spike's original finding. On
Day 1's trace (2026-05-06 10-11 CDT, 244k priced ticks):
- BUY mid-capped: 32% UP (more aggressive) / 16% DOWN / 52% EXACT.
- SELL mid-capped: 51% UP (less aggressive) / 0.3% DOWN / 49% EXACT.
- Net inventory direction predicted: small LONG-drift (~4 contracts
  per 6h-equivalent session, well within `delta_kill = 5.0`).

### What was tested

`quantizer_backtest_results.md` (fp32-precision-spike repo) compared
round-half-away vs round-toward-mkt_mid on 3 days (Day 1 = 2026-05-06,
Day 2 = 2026-05-05, Day 3 = 2026-05-04). Pre-committed thresholds
required ≥2/3 days to pass both magnitude (>50 changes per 6h-equiv)
and symmetry (|BUY-SELL effect|/max < 0.30). 0/3 days passed both —
Day 1 had the magnitude (47k BUY corrections) but ~1.0 asymmetry
(SELL had 300 corrections, not 47k). Days 2 and 3 had zero
mid-capped placements at all (different microstructure regimes).

### Why deferred rather than shipped

The asymmetry is intrinsic to the fix structure: Rule A's UP-bias
maps to MORE aggressive on BUY but LESS aggressive on SELL (because
"UP" means different operational things on the two sides). Rule B
("round AWAY from spread") corrects BUY (47k cases per Day 1) but
not SELL (matches Rule A's existing UP direction). The fix is a
PARTIAL correction — and the binding pre-committed threshold caught
that.

The hedge runs continuously and corrects directional drift; if the
rule's bias is operationally invisible at the `effective_delta`
telemetry level, the change isn't worth a behavior shift. Path C
is: observe live telemetry post-deploy, and only revisit the
quantizer if the predicted LONG-drift shows up in `risk_effective_delta`
time series over 1-2 weeks of clean live data.

### How to apply

- **Don't ship a quantizer change without new evidence.** The
  pre-committed test said skip; ship only if (a) live telemetry
  shows the predicted drift, or (b) a new analysis with fill-level
  data tightens the magnitude estimate enough to override the
  pre-committed verdict.
- **Don't ship the change just because the FPGA prototype wants it.**
  The FPGA portability concern was secondary motivation — the rule's
  behavior is the same in fp32 and fp64 (just different magnitudes
  of representation noise). FPGA implementation can adopt
  round-toward-mkt_mid independently if the FPGA team prefers it
  for predictability; that's an FPGA-side decision, not a production
  trader change.
- **The §16/§19 lineage is closed at this point.** Audit (PR #1),
  calibrator constraint (PR #2), and this verification + backtest are
  the complete §16/§19 followup record. Newtype refactor (audit
  Option A) is the next downstream item; tracked separately as task
  #16 in the spike's task list.

## 21. Trader latency micro-opt audit — noise floor + ship/defer (2026-05-06)

Followup to §17/§18. A four-candidate audit looked for sub-µs hot-path
wins on top of the already-shipped Rust trader (TTT p50 ~50µs prior).
Scoreboard:

| Candidate | Predicted gain | Single-run Δ | Multi-run verdict |
|---|---|---|---|
| `wire_buf` thread-local | 50ns p50 / 200ns p99 | −272ns p50 | NOISE |
| `arc_dedup` (hoist `strike_key`, share `sk`) | 5–10ns | −741ns p50 | NOT SIGNIFICANT (p≈0.36); shipped as refactor |
| `ringbuf` histogram (replace VecDeque) | 100–200ns | −373ns p50 | NOISE |
| `price_eq` (drop abs+epsilon, use exact f64==) | 2ns × N | −288ns p50 | NOISE |

### What we learned about measurement

The original `compare_latency.py` thresholds (1000ns p50 noise floor)
were sized for the µs-precision histogram era, when integer-truncated µs
gave ±200% per-arm variance. Neither precision nor threshold survives
under modern N=100k IPC + N=2.7k TTT samples per 600s run.

After switching the trader histogram to **nanosecond storage**
(`state.rs::Histograms::ipc_ns` / `ttt_ns`, dump JSON keys `ipc_ns` /
`ttt_ns`, telemetry print line `_ns` suffix), single-run noise floor at
TTT p50 is ~280ns. Looked like a credible threshold to gate against.

**It wasn't.** The 280ns "noise" was almost entirely the FIRST run after
a fresh container — cold-start: cold mimalloc, cold page cache, JIT-
equivalent warmup in the busy-poll path. After 5 baseline runs:

```
warm baselines:  [4529, 4537, 4550, 4559]   sd = 13ns,  spread = 30ns
baseline_1 (cold): 5392ns                                              ← +860ns outlier
arc_dedup all:   [4302, 4492, 4532, 4599, 4718]   sd = 153ns,  spread = 416ns
```

Warm-state TTT p50 reproducibility is **±15ns** — order of magnitude
tighter than I assumed before measuring it. arc_dedup vs warm baselines
mean Δ = −15ns. arc_dedup's intrinsic spread is 12× warm-baseline's
spread; the previously-observed single-run wins (−272 to −741ns) were
the cold-start in baseline drawing into the wrong arm.

### Ship rules going forward

- **Trader histogram is in nanoseconds.** Don't revert to µs without
  also bumping `compare_latency.py --noise-floor-p50-ns` back to its
  µs-era default (which is wrong for ns inputs by a factor of 1000).
  The dashboard reads broker-side `ttt_us` from snapshot — that's a
  separate measurement and stays in µs.
- **A single-run A/B at this latency scale is dominated by cold-start
  noise.** Always do at least 5+5 interleaved runs (use
  `--image <tag>` flag with two pre-built tags) before claiming a win.
  Drop the first run of each arm if it's >300ns slower than the rest
  of the arm — that's the cold-start tell.
- **Effect sizes need to be ≥45ns p50 (3σ on the warm noise floor)
  with N=5 each to be detectable.** The four candidates' true effects
  are below this threshold; conclusions about whether they "work"
  require either much larger N or microbenchmarks (criterion in
  `rust/perf_bench/`) that bypass the IPC path.
- **`arc_dedup` was shipped anyway** as a code-quality refactor: a DRY
  hoist with no behavior change and zero risk. It does NOT claim a
  measurable latency win. Future readers should not treat the §17 /
  §18 "list of optimizations" lineage as having a §21 entry.

### Followup if anyone wants to revisit

Microbench in `rust/perf_bench/` for `decide_on_tick`'s key construction
loop. That bypasses the IPC + tokio scheduler noise that dominates the
600s harness. Predicted 5–10ns gain on `arc_dedup` should be cleanly
visible in a tight criterion benchmark even if it's invisible end-to-end.

### 2026-05-07 broker-side followup — three more null results

After the trader-audit lessons above, a follow-up pass examined the
broker post-trader leg (Broker TTT p50 27µs vs Trader TTT p50 15.5µs;
the 12µs delta is broker-side work). Two audits + three multi-run A/Bs:

1. **Broker hand-rolled msgpack decoder** for PlaceOrder/Modify/Cancel
   (mirroring the trader's INBOUND tick decoder). Predicted gain: 1-2µs.
   Multi-run (3+3 ABABAB at 60s): TTT p50 Δ=−218ns, t=−0.87, INCONCLUSIVE.
2. **`pump_ticks` batch-drain + parking_lot::Mutex on `market_data`**.
   N=3 showed apparent t=−2.94 p50 win — but bumping to N=8 revealed
   that was sample-size artifact (Δ shrank to −63ns, t=−1.28). Tail
   variance ~doubled (p99 SD 528→1171, p999 SD 956→2090) — the
   predicted "mid-batch tick arrival waits for entire batch" effect
   is real, even if mean shift wasn't statistically significant.
3. **`parking_lot::Mutex` on `market_data` ALONE** (without batch-drain).
   Multi-run (5+5): t-stats all between −0.27 and +1.26, ALL
   INCONCLUSIVE. parking_lot's theoretical 30-50ns uncontended-path
   advantage doesn't materialize because the lock is rarely contended
   (snapshot at 250ms vs ticks at ~100Hz means a few % contention rate
   at most). The 30-50ns × low-contention doesn't accumulate.

The broader broker audit before #2 surveyed the entire post-trader
leg (msgpack decode, validation, IBKR wire encode, socket write,
async overhead, lock contention with snapshot/risk_state) and found
the 12µs is **well-distributed across already-optimized small things**
— TCP_NODELAY ✅, SO_BUSY_POLL=50µs ✅, hand-rolled `place_template`
encoder ✅, contract HashMap ✅, async wire_timing serialization ✅,
canonical lock order ✅. No 1-3µs lumps remain.

**Combined record across both audits**: 7 multi-run validations, 6
INCONCLUSIVE, 1 shipped-as-refactor (arc_dedup). The pattern is
unambiguous at the current architecture scale: **sub-µs broker/trader
optimizations don't extract from measurement noise even with disciplined
N=8 multi-run protocol**.

### Where the bottleneck actually lives

User-visible latency is broker RTT 84ms = 99.97% IBKR network, 0.03%
broker code. The "broker is the bottleneck" framing is partially
right — broker has more local time than trader (27µs vs 15.5µs) — but
the bottleneck *within our code* has been hammered, and the bottleneck
*overall* is the IBKR connection. Real wins:

- **FIX migration** (~10-30ms via smarter ack semantics, no IBKR API
  reflection, leaner wire format). Bake the no-reflection-decode
  pattern into the FIX-side decoder from day one (this audit's
  msgpack hand-roll work is the proven template; saved to stash).
- **Colo near IBKR** (~80ms via network distance — order of magnitude
  on user-visible latency).
- **FPGA / kernel-bypass** per `docs/fpga_arty_a7_feasibility.md`
  (~order of magnitude on broker-leg + tighter tail).

Stop chasing local-path code optimization at the current scale. The
remaining items in `rust/corsair_broker/` are research-grade
observations, not actionable optimizations. Moving the actual
bottleneck requires architectural changes.

## 22. Tiered improving-fill exceptions — preserved in code, not wired (deviation noted 2026-05-07)

The Python predecessor (`src/constraint_checker.py`) had a tiered
constraint-gating system that allowed quotes/fills to proceed even at
risk-threshold breach, provided the fill *reduced* the breach:

- **Tier 1 (margin priority escape)**: at `cur_margin > margin_ceiling`,
  accept any fill where `post_margin < cur_margin` — even if it
  incidentally drifts delta/theta. Rationale: margin is solvency-
  critical; small delta/theta drift is operationally recoverable, but
  a margin breach risks broker-forced liquidation. Margin always
  takes priority over delta/theta hygiene.
- **Tier 2 (per-constraint improving)**: at any individual ceiling
  breach (margin, delta, theta), accept fills that reduce that
  specific constraint, with sign-flip protection on delta to prevent
  rotation through zero into the opposite extreme.
- **Tier 0 (always block)**: hard kills (`margin_kill`, `delta_kill`,
  `theta_kill`) reject unconditionally, no exceptions.

The Rust port faithfully preserves this in
`corsair_constraint::ConstraintChecker::check()`
(`rust/corsair_constraint/src/checker.rs:183-352`), including the
sign-flip fix from §18. **However, `check()` is never called on the
live order path.** The trader's `decision::compute_risk_gates`
(`rust/corsair_trader/src/decision.rs:645`) implements only per-side
delta blocking at `delta_ceiling`, and ALL-blocks at `margin_ceiling`,
`theta_kill`, `vega_kill`, `delta_kill`. The constraint instance is
held in `runtime.constraint` but only used for `ibkr_scale()`
(snapshot rendering) and `update_cached_margin()` (calibration).

### Operational impact

When the trader hits margin or theta breach, all quoting halts until
the operator manually unwinds. The original tier-1/tier-2 logic would
have allowed the bot to self-unwind by accepting only improving fills.
24h live `risk_block` counter ≈115 events (≈1% of decisions). Real
gap during pickoff recovery scenarios where margin drifts up; not
catastrophic.

### Bug noted while documenting (2026-05-07)

`compute_risk_gates` assumes BUY adds delta and SELL subtracts. True
for CALLS, **false for PUTS** (buying a put subtracts delta, selling
a put adds delta). At `delta_ceiling` breach, the gate currently
blocks the wrong side for puts. Latent under the OTM-only short-
strangle config (puts contribute negative delta on either book
direction); a future product config that quotes long puts would
expose it.

### Re-implementation plan (when prioritized)

Two options, increasing scope:

1. **Trader-side per-(right, side, strike) gates with improving
   exceptions for delta and theta**. Data is already in `theo_cache`
   modulo exposing theta. Margin improving requires position state
   from the broker (new IPC field or per-strike publish). ~150 LOC,
   no broker latency cost.
2. **Broker-side `ConstraintChecker.check()` wired into
   `handle_place`** — most faithful to the Python lineage, reuses the
   existing tested checker. Adds ~1µs to broker TTT for the SPAN math
   per place (the hot path is already at 27µs broker TTT; this is a
   ~4% increase). ~50 LOC at the call site plus a small data-flow
   piece to feed `cur_long_premium` and friends.

Deferred 2026-05-07 because:
- Latency cost of (2) is unwelcome on the broker hot path; (1)
  requires non-trivial scoping (per-strike position IPC)
- Operational impact (~1% block rate) is real but not blocking
- The §18 history of improving-fill bugs (sign-flip) suggests the
  feature deserves careful design, not a quick reimplementation

Pre-FIX migration is the natural moment to tackle this — the broker
hot path gets reorganized then anyway, and the constraint-check call
can be folded into the new structure cleanly.

### What NOT to do

- **Don't delete `corsair_constraint::ConstraintChecker::check()` as
  dead code.** It captures the Python algorithm including the
  sign-flip fix; deleting forfeits years of operational learning.
  Keep it as the reference implementation pending the wire-up.
- **Don't "fix" the puts-direction bug in `compute_risk_gates`
  without re-thinking the whole gate.** A partial fix that makes
  puts right but doesn't add improving-fill exceptions ships
  inconsistent semantics. Either do the full §22 work or leave it
  documented as latent.

## 23. Host CPU isolation — keep crowsnest off the corsair cpuset (2026-05-07)

Trader pinning is necessary but not sufficient. The trader's hot
loop busy-spins on cpu 8; cpu 9 is the SMT sibling (same physical
P-core), so any work scheduled on cpu 9 steals pipeline execution
units from `corsair-hot` and produces multi-millisecond TTT p99
outliers that don't show up in p50 or p99 of the broker-side
histogram (which only sees the broker's own work).

The first incarnation of this bug was the trader's OWN background
threads (jsonl writers, fallback tokio worker) landing on cpu 9 —
fixed in commit `4503cb3` (May 6) by pinning each thread to a
specific cpu and preferring cross-physical-core fallbacks.

**The second incarnation is host-side**: the `crowsnest.slice`
(bitstamp/coinbase/dydx/etc. data collectors, ~14 services) ran
with affinity `0xffffffff` and the kernel scheduler placed dozens
of Python threads on cpus 8 and 9 (and 12-15 where the broker's
tokio workers live). Symptom on 2026-05-07: trader `ttt_p99_ns`
clustered at 5.4ms / 6.9ms / 8ms across 21+ telemetry windows
even though steady-state p99 is ~90µs.

### Topology (i9-14900K, SMT enabled)

```
cpus  0-15  → 8 P-cores HT (each P-core = 2 logical cpus)
              (cpus 8,9 = P-core 4; cpus 10,11 = P-core 5; etc.)
cpus 16-31  → 16 E-cores (1 logical cpu each)

corsair containers:
  trader → cpus 8-11   (P-cores 4 and 5)
  broker → cpus 12-15  (P-cores 6 and 7)

trader thread layout:
  cpu 8   → corsair-hot std::thread (busy-spin, hot path)
  cpu 9   → SMT sibling of cpu 8 — DELIBERATELY EMPTY
  cpu 10  → tokio-rt-worker (parked)
  cpu 11  → corsair-bg + jsonl-trader_events + jsonl-trader_decisions
```

### The fix

`AllowedCPUs=0-7,16-31` on the slice would be cleanest, but
`user@1000.service` does not have the `cpuset` controller
delegated (only `cpu memory pids`), so cgroup-level cpuset
constraints silently no-op. Two paths:

**Option A (cleanest, requires sudo)** — delegate cpuset:

```ini
# /etc/systemd/system/user@.service.d/delegate.conf
[Service]
Delegate=cpu cpuset memory pids
```

Then `sudo systemctl daemon-reload && sudo systemctl restart user@1000.service`
(this kills the user session). The existing
`crowsnest.slice.d/cpus.conf` would then take effect.

**Option B (no sudo, what we shipped)** — per-service
`CPUAffinity` drop-ins. CPUAffinity uses `sched_setaffinity`
directly and bypasses cpuset delegation. Drop-ins live at
`~/.config/systemd/user/<svc>.service.d/cpus.conf`:

```ini
[Service]
CPUAffinity=0-7 16-31
```

Applied to all 14 crowsnest services on 2026-05-07. The
slice-level `AllowedCPUs` override is also in place so that
delegation, if enabled later, will pin the slice without a
second pass.

### Verification

```bash
# 1. Slice config visible to systemd
systemctl --user show crowsnest.slice | grep AllowedCPUs

# 2. Per-service kernel-level affinity (the load-bearing one)
PID=$(systemctl --user show bitstamp-collector -P MainPID)
grep Cpus_allowed_list /proc/$PID/status
# expected: 0-7,16-31

# 3. Live thread placement — no crowsnest threads on 8-15
ps -eLo psr,cgroup | awk '$1 >= 8 && $1 <= 15' | grep -c crowsnest
# expected: 0
```

### Why this matters

The trader's own pinning gets you to "no contention from your own
threads". This change gets you to "no contention from anything
else on the host". On Alabaster (former host) the kernel cmdline
included `isolcpus=8-11` which solved the same problem at boot
time; on the current i9-14900K we don't `isolcpus` because we
want the corsair cpuset to participate in housekeeping (kworkers
etc.). Per-cgroup affinity is the substitute.

### Don't

- **Don't pin everything to a single cpu** ("everyone goes to
  cpu 0"). The kernel uses cpu 0 for many default-affinity
  housekeeping paths (timer ticks, default IRQ handling) — moving
  ALL userspace there creates contention with the kernel itself.
  `0-7` plus E-cores spreads load across multiple physical cores.
- **Don't think `systemctl show ... | grep AllowedCPUs` proves the
  pin works.** That only shows what systemd was TOLD; if cpuset
  isn't delegated, the directive is silently dropped at the
  cgroup level. Always verify at `/proc/<pid>/status`
  Cpus_allowed_list.
- **Don't restrict the slice tighter than CPUQuota allows.**
  `crowsnest.slice` has `CPUQuota=1600%` (16 cores). The
  `0-7,16-31` mask has 24 logical cpus = ample room.

## 24. SABR IV inversion uses mid, not microprice (2026-05-07)

`vol_surface.rs::snapshot_chain` previously computed the input
price for IV inversion via microprice:

```rust
price = (bid * ask_size + ask * bid_size) / (bid_size + ask_size)
```

That formula is informative when displayed sizes track real
liquidity. On HG OTM strikes they don't — bids and asks frequently
display 1-lot resting orders that pull the formula toward
whichever side has the smaller quote. The result was systematic
side-asymmetric bias visible in production:

- OTM puts: bid_size often 1, ask_size 16-33 → microprice tilted
  toward bid → IV inversion underestimated → put theos
  systematically 1-3 ticks BELOW mid
- OTM calls: mirror image → call theos 1 tick ABOVE mid

Put-call parity on the theo side was clean (`theo_C - theo_P + K
= F` consistent across strikes), so the issue wasn't a fit bug —
the inputs were biased. Switched to plain `(bid + ask) / 2.0`.
The two-sided / non-zero-size guards above the price line stay in
place; only the formula changed.

A size-floor compromise (`bs ≥ 3 && as ≥ 3`) was considered and
rejected — it would have cut the put input set from 7 strikes to
2 on the inspected snapshot, leaving the fit underdetermined on
the put side.

### Why microprice was originally chosen

The 2026-05-05 cleanup pass bundled microprice with the
zero-size / one-sided / inverted-book skip checks under "Rec 2
expanded" remediation of the 5.90 P / 6.05 C accumulation
incident. The skip checks did the load-bearing work (excluding
stale strikes during fast spot moves); the microprice tweak rode
along. Removing the formula keeps the load-bearing guards.

### How to revert

If a future regime change makes microprice signal load-bearing
again (e.g., HG flow gets deep enough that displayed sizes
genuinely reflect demand asymmetry), revert to the size-weighted
formula at `snapshot_chain`. Don't half-measure with a size
floor unless you've checked that the post-floor strike set is
big enough to fit (≥4-5 points per side after the OTM-only
filter).

## 25. Trader-centric kill controller — improving-only gates + watchdog (2026-05-07)

Aligned the kill stack with v1.4 spec §6.2 by making the trader
the authoritative strategy-kill controller. Spec language for
delta_kill ("Halt new opens, force-hedge to 0") and theta_kill
("Halt new opens") IS improving-only quoting; the broker's
prior block-all behavior was over-conservative. The Python
predecessor's tier-1 margin-improving exception (CLAUDE.md §22)
is intentionally NOT carried over — margin breach stays as
hard halt per operator preference.

### What changed

| Where | Before | After |
|---|---|---|
| Margin breach | broker block_all + trader block_all | unchanged (margin stays broker hard halt) |
| Theta breach | broker fires `THETA HALT` (block_all) | trader gates per-side: BUYs blocked, SELLs allowed |
| Delta_ceiling breach | trader block per-side, sign-bug latent on puts | trader gates per-(right, side) sign-aware via cached `gx.delta` |
| Delta_kill (5.0 hard) | broker fires `DELTA KILL` + force-hedge | **REMOVED** — hedge engine owns the delta control loop |
| Vega | disabled (vega_kill=0) | unchanged (improving-only is hard to define for vega; if re-enabled it stays as block_all) |
| daily_pnl_halt | broker | unchanged (P&L tracking lives in broker) |
| Operational kills (gateway, calibration, recon) | broker | unchanged |

The §22 latent puts-direction bug is fixed as part of this work
— `improving_passes` reads the signed `gx.delta` (positive for
calls, negative for puts) so all four (right, side) combinations
are gated correctly.

### How "improving-only" works (the load-bearing part)

For a single-contract fill of qty=1:

| Action | Δ portfolio_delta | Δ portfolio_theta |
|---|---|---|
| BUY call  | +call_delta (+0.x) | +call_theta (negative) |
| SELL call | −call_delta (−0.x) | −call_theta (positive) |
| BUY put   | +put_delta (−0.x)  | +put_theta (negative) |
| SELL put  | −put_delta (+0.x)  | −put_theta (positive) |

Read off:
- **Theta breach** (theta < theta_kill) → improve = increase
  theta → allow SELLs (collect decay), block BUYs. Right-agnostic.
- **Delta breach high** (eff > +ceiling) → improve = decrease
  delta → allow SELL call + BUY put. Block BUY call + SELL put.
- **Delta breach low** (eff < −ceiling) → mirror image.

Implementation: `corsair_trader/src/decision.rs::improving_passes`.
Truth table enumerated exhaustively in
`improving_passes_truth_table` test. ~1-2 ns per gate per side
(below the §21 noise floor).

### Greek cache extension

`theo_cache` was previously `(theo, delta)` per-strike per-fit.
Now stores full `TheoGreeks { theo, delta, theta, vega }` (still
at multiplier=1.0). Caching all four together costs ~5 extra
fp64s of memory per strike (44 strikes × 32 bytes = 1.4 KB) and
no extra compute on the hot path — `compute_theo` already calls
Black76 via SVI/SABR; the same call now returns all greeks via
`black76_greeks`. Theta + vega drive `improving_passes`; delta
still drives Taylor reprice.

### Trader watchdog (broker-side)

With strategy kills gone, a trader crash leaves the broker with
no per-strike protection. Mitigated by a 1Hz heartbeat from
trader → broker plus a watchdog task on the broker:

- Trader publishes `heartbeat` msgpack frame every 1s on the
  commands ring.
- Broker's `dispatch_commands` updates `Runtime::last_trader_msg_ns`
  on EVERY received frame (place / cancel / modify / telemetry /
  heartbeat / welcome). Active markets keep the watchdog warm
  via order traffic; calm markets rely on the heartbeat.
- `trader_watchdog` task runs 1Hz, fires `trader_silent` kill if
  the gap exceeds `CORSAIR_TRADER_WATCHDOG_TIMEOUT_S` (default 5s).
- `trader_silent` is a sticky `KillSource` — operator must
  `docker compose restart corsair-broker-rs` after investigating
  the underlying trader fault. Auto-resume on heartbeat is
  deliberately NOT supported — a trader stuck in crash-restart
  would silently mask itself.

### Hello rehydration

`HelloMsg` now carries `active_kills: Vec<String>`. When the
trader reconnects (after crash/restart), the broker emits its
current active kill set in `hello`; the trader populates its
local `kills` map from the list and stays out of the market
until the operator clears via broker restart. Closes a window
where a restarted trader could resume quoting against a broker
still holding `trader_silent` from the prior trader.

### Why margin stays at broker

P&L tracking, position state, margin computation all live in
the broker (broker owns IBKR fills + portfolio state). Moving
margin/daily_pnl to the trader would be a multi-week refactor
(trader needs its own portfolio state). For now: margin and
daily_pnl_halt stay broker-side; everything else moves to
trader. Operational kills (gateway disconnect, calibration
RMSE, fill rate per spec §7) also stay broker-side because
they're infrastructure responses, not strategy policy.

### Verification

- `improving_passes_truth_table` test: enumerates all 4 (right ×
  side) × 4 (breach states) = 16 cases. Sign tables documented
  inline.
- `delta_ceiling_high_blocks_sell_put_allows_buy_put` integration
  test: drives the full `decide_on_tick` flow with a put tick at
  delta breach, confirms BUY allowed + SELL blocked. Pins the §22
  puts-direction fix.
- Live verification (2026-05-07 deploy): trader hit theta breach
  (theta=-523 vs theta_kill=-500), `risk_block_buy` counter
  climbed to 11k+ over a few minutes (BUYs blocked); `place`
  counter still incremented for SELLs. Behavior matches design.

### Don't

- **Don't add delta_kill back to the broker.** Hedge engine
  controls delta; a separate hard halt is redundant and fires
  spuriously when hedge is mid-rebalance.
- **Don't auto-resume on trader heartbeat.** Sticky kill is
  load-bearing — auto-resume would mask crash loops.
- **Don't shorten the watchdog timeout below 3s.** GTD-5s on
  every order is the safety floor; 5s timeout means at most
  one expiry window of unmanaged orders. Tighter than that
  risks false-positive on 1-2s GC pauses.
- **Don't sprinkle `improving_passes` calls anywhere else.** The
  per-side gate runs ONCE in `decide_on_tick` per side per tick,
  exactly where the place/modify decision is being made. Adding
  it elsewhere (e.g. staleness loop) would either re-block
  already-resting orders (operator-confusing) or no-op.
