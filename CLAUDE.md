# Corsair v2 — Operator Notes

Hard-earned lessons. Read before debugging anything weird around connectivity,
order lifecycle, or margin display. Each entry took hours to find live; the
goal of this doc is to never re-discover them.

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

Our paper login (`DUP553656` under master `DFP553653`) is a Financial
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

## 7. Stage 1 capital + cap

Currently configured for Stage 1 of the deployment ramp:
- `capital: 200000` ($200K)
- `margin_ceiling_pct: 0.50` (synthetic cap = $100K)
- `margin_kill_pct: 0.70` (kill switch at $140K)
- `delta_ceiling: 3.0`, `theta_floor: -200`, `vega_kill: 1000`
- `delta_kill: 5.0`

These numbers come from the deployment ramp doc. Stage 2 (post-validation)
goes to $500K margin. Don't loosen them ad-hoc — they're the safety net.

## 8. The dashboard polls; it doesn't stream

Streamlit is request/response. Currently:
- corsair writes `data/chain_snapshot.json` every **250ms** (4Hz)
- Streamlit page reruns every **500ms**
- Chain table fragment runs every **250ms** (matches snapshot rate)

If you need true push, you have to escape Streamlit (FastAPI + SSE / WebSocket
sidecar). Several hours of work; deferred indefinitely.
