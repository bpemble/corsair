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
- NEW hedging subsystem via HG futures front-month (`src/hedge_manager.py`).
  Default mode: **observe** (logs intent, no orders). Flip to **execute**
  once Gate 0 induced tests validate the paths.
- NEW operational kills (`src/operational_kills.py`): SABR RMSE sustained
  >0.05, quote latency median >2s for 60s, abnormal fill rate >10× baseline.
- 8 JSONL streams in `logs-paper/` per spec §9.5.

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

## 10. Hedge mode: observe (reverted 2026-04-22; IBKR near-expiry lockout)

`src/hedge_manager.py` runs in one of two modes via
`config.hedging.mode`:
- **observe** (CURRENT): computes required hedge trades and logs
  intent to `logs-paper/hedge_trades-YYYY-MM-DD.jsonl`; does NOT
  place futures orders. Local `hedge_qty` is updated optimistically
  so daily P&L halt still sees hedge MTM as if trades had filled.
- **execute**: places aggressive IOC limit orders at market ± 1 tick
  on the front-month HG futures contract.

**Current state 2026-04-23**: config is `mode: "observe"`. Execute was
flipped on 2026-04-22 (commit `2fc3858`) and reverted the same day
(commit `470d09f`) because **IBKR rejects orders on front-month HG
futures (HGJ6) during the near-expiry lockout window** — so execute
mode can't actually place hedges against the contract our options
expire into. Until the front-month rolls to HGK6/HGM6 or the lockout
window closes, execute is functionally disabled.

Implications:
- Hedge drift cannot be measured against real IBKR positions during
  the lockout — no orders are flowing to IBKR to drift against.
- Daily P&L halt still includes hedge MTM via local `hedge_qty`, but
  that's **intent**, not **executed** — there's no futures exposure
  offsetting options delta. The portfolio is effectively unhedged at
  IBKR's book.
- v1.5 execDetailsEvent wiring (the real reconciliation fix) is
  blocked until execute mode can actually place orders to reconcile.

**KNOWN LIMITATION (v1.5 blocker, do NOT enable in LIVE until
resolved)**: no execDetailsEvent callback for hedge fills, no periodic
reconciliation against `ib.positions()`. If/when execute is re-enabled:
- If an IOC doesn't fill (market moved past ±1-tick limit in RTT
  window), local `hedge_qty` says we hedged but IBKR says we didn't.
- If the IOC fills at the limit (not F), `avg_entry_F` is off by 1
  tick — small MTM error.

**To re-enable execute** (only after front-month rolls / lockout
lifts): flip `hedging.mode: observe → execute` in yaml and restart.

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
