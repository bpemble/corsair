# Corsair v2 — Operator Notes

Hard-earned lessons. Read before debugging anything weird around connectivity,
order lifecycle, or margin display. Each entry took hours to find live; the
goal of this doc is to never re-discover them.

## Current spec: v1.4 (HG front-only)

Active strategy spec: `docs/hg_spec_v1.4.md` (APPROVED 2026-04-19). Active
config: `config/hg_v1_4_paper.yaml` (selected via `CORSAIR_CONFIG` env var
in docker-compose.yml; defaults to v1.4). v1.3 config is preserved for
rollback at `config/corsair_v2_config.yaml`.

Key v1.4 differences from v1.3 (see §9 below for details):
- Strike scope per spec: **symmetric ATM ± 5 nickels** (sym_5). Active
  config deviates — **asymmetric OTM-only**: calls ATM→ATM+5, puts ATM−5→ATM.
  See §12 for rationale and measurement. v1.3 was asymmetric NEAR-OTM+ATM.
- **Daily P&L halt at −5% capital (−$3,750)** is the PRIMARY defense — not
  just cancels quotes, FLATTENS options + hedge, session-level halt.
- Capital: $200K → $75K (Stage 1). Kill switches rescaled accordingly.
- NEW hedging subsystem via HG futures (`src/hedge_manager.py`).
  Mode **execute** as of 2026-04-26 (Phase 0 fix `b598c7b` skips
  near-expiry contracts). See §10 for current state.
- NEW operational kills (`src/operational_kills.py`): SABR RMSE sustained
  >0.05, quote latency median >2s for 60s, abnormal fill rate >10× baseline.
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
| Effective-delta gating | unspecified | combined options+hedge | §14 |
| Architecture | single process | broker/trader split (cut-over default ON post-2026-05-01) | §15 |
| Hedge near-expiry lockout | 7 days (shared with options engine) | 30 days (hedge-specific knob added 2026-05-01) | §10 |

ETH is tabled (config products list is HG-only) but the multi-product
architecture is preserved — re-enabling ETH is a matter of un-commenting
its product block and wiring market data.

## 0. Source is BAKED INTO the corsair image — `restart` ≠ `up --build`

The `corsair` service in `docker-compose.yml` builds from the local
Dockerfile and **does not volume-mount `src/`**. Code edits do NOT take
effect on `docker compose restart corsair` — that just bounces the
existing container running the cached image.

To deploy code changes:

```bash
docker compose up -d --build corsair
```

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

## 2. `openTrades()` returns multiple Trade objects per orderId

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

## 5. Lean connect bypass (don't let ib_insync's stock connectAsync run)

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

## 12. Strike scope deviation: asymmetric, not sym_5

**Operator override 2026-04-20** (commit `bde574b`): deviates from v1.4
§3.2 which specified symmetric sym_5 (11 strikes × 2 rights = 22
instruments, 44 resting orders). Active config is asymmetric OTM-only:

- Calls: `quote_range_low: 0, high: 5` → ATM to ATM+$0.25 (OTM + ATM)
- Puts:  `quote_range_low: -5, high: 0` → ATM−$0.25 to ATM (OTM + ATM)

Result: ~12 instruments, ~24 max resting orders (ATM strike shared C+P).

**Measurement that justified the deviation**: per-refresh gateway JVM
serialization latency was the dominant bottleneck (see
`project_fix_colo_evidence`). Halving order count halved per-refresh
serialization time. ITM strikes had thin flow and wide spreads — quoting
them consumed the order budget without generating fill volume.

**Risk being monitored**: short-C vs short-P inventory imbalance on
persistent underlying drift. If skew sustains >30%, re-enable the
`inventory_balance` gate (removed when v1.4 symmetric scope made it
redundant; re-needs code in quote_engine). Delta kill at 5.0 is the
backstop.

**Subscription vs quote scope are separate**: `strike_range_low/high`
governs market-data subscription (±7 strikes = $0.35 window for SABR
wing stability), while `quote_range_low/high` governs where we rest
orders. The subscription window is rolled on ATM drift by
`market_data.recenter_strike_window` (added 2026-04-22) so the boot-ATM
anchor no longer goes stale — prior behavior froze subs at boot ATM,
leaving the window lopsided after a few strikes of drift.

**To revert to sym_5**: set `quote_range_low: -5, high: 5` on both calls
and puts. Expect ~2× per-refresh serialization cost unless FIX/colo has
shipped by then.

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

## 14. Effective-delta gating (2026-04-27 — paper-acceptable, live-blocked)

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

## 15. Broker/trader split (mm_service_split, 2026-05-01 — colo prep)

Phased architectural split designed in
`docs/architecture/mm_service_split.md`. Two processes share a single
IBKR clientId via a Unix-socket IPC layer (msgpack frames). Built as
prep for an eventual FIX-direct colo system; the trader process is
intended to migrate with minimal change while the broker becomes a
FIX adapter.

### Topology

```
   broker (current corsair)              trader (new MM service)
   ─────────────────────────              ──────────────────────
   ib_insync IB(clientId=0)               (no gateway connection)
   risk monitor, hedge, snapshot          price book, decision logic
   slow workloads in worker threads       SABR theo via Rust
   forwards events → IPC →                receives, decides, places
   dispatches commands ← IPC ←            sends place_order back
```

### Phase status (as of 2026-05-01)

| Phase | What | Status |
|---|---|---|
| 1 | IPC plumbing (Unix socket, msgpack) | done |
| 2 | Broker forwards events; trader logs decisions | done |
| 3 | Trader places orders via IPC; broker dispatches | **shipped, default DISABLED** |
| 4 | Slow workloads off broker's loop (snapshot build, daily_state.save) | done — TTT p99 5ms → 2.4ms |
| 5 | SHM ring-buffer transport | code exists, untested in production |
| 6 | Rust port of trader's decide_quote + SABR | done |

### Env flags

- `CORSAIR_BROKER_MODE=1` — broker starts the IPC server. Default off → no behavior change.
- `CORSAIR_TRADER_PLACES_ORDERS=1` — broker dispatches trader commands to `ib.placeOrder`; broker's own quote engine is gated off. **DO NOT enable in production until further notice — see "Cut-over disabled" below.**
- `CORSAIR_IPC_TRANSPORT=socket|shm` — default `socket`. SHM is implemented but not yet exercised end-to-end.
- `CORSAIR_TRADER_BACKEND=python` — forces the Python `decide_quote` path on the trader. Default uses Rust for SABR.
- All four are wired through `docker-compose.yml`'s `environment:` blocks for both services. Adding a new flag requires updating the compose file too — host-shell exports alone don't reach containers.

### Cut-over disabled

`CORSAIR_TRADER_PLACES_ORDERS=1` was tested twice on 2026-05-01 (cut-over windows ~01:26 and ~01:43) and produced ~21 adverse fills with negative edge totaling several thousand dollars. Two contributing bugs:

1. **One-sided market quoting** — trader placed orders into thin/dark books at SABR theo, becoming the sole price on a side and getting filled when MMs dumped. **Fixed** in `src/trader/quote_decision.py` and `rust/corsair_pricing/src/lib.rs:decide_quote` — both now skip with reason `one_sided_or_dark` unless BOTH bid AND ask are live.
2. **No risk feedback** — trader had no view of position deltas / margin / Greeks, so it kept placing orders that built up an options_delta of -14.48 and tripped delta_kill. **Fixed** in 712593d-followup: broker publishes a periodic `risk_state` event (1Hz) carrying margin / effective_delta / theta / vega / position counts; trader self-gates new placements against `delta_ceiling` / `delta_kill` / `margin_ceiling_pct` thresholds (delivered in hello config snapshot).

After both fixes are validated end-to-end (next session), cut-over should be re-enabled cautiously — short window first, monitor `trader_decisions-*.jsonl` for risk_block reasons, ensure no buildup of imbalanced positions.

### Operational gotchas

- **Compose env propagation**: each `docker compose stop` + `up` must include the env vars inline (`CORSAIR_BROKER_MODE=1 docker compose up -d --force-recreate corsair`). Each Bash session is a fresh shell — exports don't persist.
- **Sticky risk kills survive restarts** for `source=risk` (delta, margin, etc.). To clear: `docker compose restart corsair` AND verify `kill_switch-*.jsonl` shows no recent risk-source kills. Trader-driven cut-over has caused these — restart corsair WITHOUT the trader flag to recover.
- **`scripts/flatten.py`** is the sanctioned way to clear positions when the broker can't auto-recover (post-cut-over delta blow-out, etc.). Stop corsair first, then run with `--product HG --order-type limit` (HG is thin, market orders give bad fills).
- **TTT histogram**: cap is 500ms in `quote_engine.py:_record_send_latency`. Empty histogram (`n=0`) post-cutover means broker isn't placing orders (either killed, or trader-driven cut-over with the metric not yet rewired through the cut-over path — it's captured in `_try_place_order`, but TTT lookup needs `_decision_tick_ns` populated by the broker's quote engine which is gated off during cut-over).

### Trader command-line workflow

```bash
# Default (no broker mode, no trader): identical to pre-split
docker compose up -d

# Broker mode only — broker quotes, trader logs decisions for parity
CORSAIR_BROKER_MODE=1 docker compose up -d --force-recreate corsair
docker compose --profile broker-split up -d trader

# Full cut-over (CURRENTLY UNSAFE — see above)
CORSAIR_BROKER_MODE=1 CORSAIR_TRADER_PLACES_ORDERS=1 \
    docker compose up -d --force-recreate corsair
CORSAIR_BROKER_MODE=1 CORSAIR_TRADER_PLACES_ORDERS=1 \
    docker compose --profile broker-split up -d --force-recreate trader

# Rollback from cut-over: drop the trader flag, restart corsair
unset CORSAIR_TRADER_PLACES_ORDERS
CORSAIR_BROKER_MODE=1 docker compose up -d --force-recreate corsair
docker compose stop trader
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
| `skip_dark_at_place` | dark-book guard at place time |
| `staleness_cancel` | resting order cancelled — too far from theo |
| `staleness_cancel_dark` | resting order cancelled — market went dark |
| `replace_cancel` | cancel-before-replace fired |
| `risk_block` / `_buy` / `_sell` | risk gate engaged |

`skip_in_band` should dominate skip reasons in calm markets. High
`risk_block` rate means margin or delta is near a limit — operator
should investigate. High `staleness_cancel_dark` rate means liquidity
is thin; consider tightening `MIN_BBO_SIZE`.
