# Latency Decomposition Plan

## Context

Corsair v1.4 (HG sym_5, 22 instruments × 2 sides = up to 44 resting orders)
shows amend-RTT that scales roughly linearly with concurrency: idle p50 is
42ms (1 order in flight, corsair stopped), loaded p50 is 369ms with corsair
quoting 11 orders, and a synthetic 15-order burst hits 738ms p50.
`scripts/multi_client_probe.py` established that N clients × 1 order behaves
identically to 1 client × N orders, i.e. the serialization point is
**account-scoped and sits downstream of the TCP socket split** — either in
the IB Gateway JVM OMS or the IBKR backend router. MTR to
`cdc1.ibllc.com` is 20ms; py-spy shows corsair 95%+ idle. Network and Python
are exonerated; per-order queue cost is ~30–45ms. The two workstreams below
(a) characterize the concurrency curve so we can predict post-FIX
throughput, and (b) cut concurrent in-flight orders from the client side so
we stop paying for queue depth we don't need.

---

## Item 1: Concurrency sweep probe

### Goal

Measure amend-RTT (and separately place-RTT) as a function of concurrent
in-flight order count N, for N ∈ {1, 2, 4, 8, 15}, from a single clientId
(clientId=0). Produce a clean p50/p90/p99 vs N curve. Log-log slope is the
diagnostic:

- **Linear** (slope ≈ 1, doubling N doubles latency): backend serialization
  dominates. FIX will NOT help much — same backend path.
- **Sub-linear / plateau**: gateway-JVM-local serialization dominates. FIX
  (which bypasses the JVM OMS entirely) should flatten the curve
  dramatically.

### Files to create/touch

- **Create**: `scripts/concurrency_sweep_probe.py`
- **Touch**: none. Read-only references to `config/hg_v1_4_paper.yaml`, same
  env-var pattern as existing probes (`CORSAIR_GATEWAY_HOST`,
  `CORSAIR_GATEWAY_PORT`, `IBKR_ACCOUNT`, `CORSAIR_CONFIG`).

Reuse patterns from `scripts/amend_latency_probe.py` and
`scripts/multi_amend_probe.py` (both already present — `multi_amend_probe`
is the closest template):
- Lean connect (clientId=0 + `reqAutoOpenOrders(True)` + the four-request
  bootstrap from CLAUDE.md §5).
- `openOrderEvent` keyed by `(orderId, round(lmtPrice, PRICE_DECIMALS))` for
  ack matching — same pattern as both existing probes; do NOT poll
  `canonical().order.lmtPrice` (local mutation means the poll fires
  instantly on local state, not the IBKR round-trip).
- Buy-side deep-OTM contracts from `multi_amend_probe.TARGETS`
  (7 OTM puts + 8 OTM calls on HGJ6, 15 strikes total). Buy-side is live-
  account-safe: premium-only margin at ≤$25/contract × 15 = $375 worst-case.
- Pre-flight filter: drop any contract whose current ask < 2×
  our max bid ($0.0010). Copy from `multi_amend_probe.main` lines 127–149.
- Contamination-field stripping on modify: every amend must call
  `quote_engine._strip_contamination_fields(trade.order)` semantics —
  copy the static-method body from `src/quote_engine.py:973` into the
  probe (it's a self-contained helper; avoid importing the engine).

### Methodology

For each N in [1, 2, 4, 8, 15]:

1. **Place phase**: issue N `placeOrder` calls back-to-back at `START_PRICE
   = 0.0005`, each followed by `await asyncio.sleep(0)` so the event loop
   can interleave wire writes. Record `sent_ns` per (orderId,
   START_PRICE). Wait up to 20s for all N acks via `openOrderEvent`.
   Compute per-place RTT from `(ack_ns − sent_ns)`.
2. **Amend phase (K rounds)**: K=20. Each round alternates price between
   `0.0005` and `0.0010`. Within a round, fire all N `placeOrder`-on-
   modified-trade calls concurrently (same round-robin pattern as
   `multi_amend_probe`), then wait up to 5s for that round's N acks. A
   round that times out is logged and skipped — don't retry, don't abort.
3. **Settle gap**: 300ms `asyncio.sleep` between rounds so token-bucket
   credits replenish (gateway paper bucket is ~125 msg/s nominal).
4. **Teardown for this N**: cancel all N orders via `ib.cancelOrder`, wait
   2s, confirm `ib.positions()` shows zero delta on the probed contracts
   before moving to the next N.
5. Between N levels: `asyncio.sleep(3)` to let any stragglers drain.
6. Repeat for next N.

Output: one JSONL row per individual amend sample and one per place sample.

### What we measure

- Per-sample: `{ts, N, phase: "place"|"amend", order_id, round, rtt_ms,
  lmt_price, contract: "HGJ6_<right><strike>", iteration}`.
- Post-run report printed to stdout per N: `place p50/p90/p99`, `amend
  p50/p90/p99`, sample counts.

Separate place vs amend because IBKR's order entry path differs from the
modify path — place creates server-side state; modify mutates it. They can
have different queue costs and we want to see that decomposition.

### Config knobs (argparse)

- `--n-levels` default `1,2,4,8,15`
- `--rounds-per-n` default `20`
- `--round-gap-ms` default `300`
- `--level-gap-s` default `3`
- `--out` default `logs-paper/concurrency_sweep-<YYYYMMDD-HHMMSS>.jsonl`

### Expected outcome

Table + stdout summary and a JSONL file enumerating every sample:

```
N=1   place p50=42ms   amend p50=41ms   n=20
N=2   place p50=~60    amend p50=~75    n=40
N=4   place p50=~120   amend p50=~150   n=80
N=8   place p50=~250   amend p50=~300   n=160
N=15  place p50=~500   amend p50=~720   n=300
```

Expected slope if linear in N: amend p50 ≈ 42 + 45·(N−1) ms.

Decision rule after the run:
- Slope ≥ 30 ms/N AND curve is linear → backend-serialized → FIX buys
  at most per-message overhead reduction, not parallelism.
- Slope < 15 ms/N OR sub-linear → gateway-JVM-local → FIX should buy a
  large speedup (back to ~40ms p50 regardless of N, plus per-msg savings).

### Risks

- **Contract availability**: `multi_amend_probe` uses 7 puts + 8 calls on a
  fixed expiry (`20260427`). If that expiry has rolled off, update `EXPIRY`
  and the `OTM_PUT_STRIKES` / `OTM_CALL_STRIKES` lists to track current
  HGJ6/HGK6 front. Keep buy-side + deep-OTM + ≥2× ask-over-bid pre-flight.
- **Rejection storms**: if any N round throws Error 103 or 104, bail that
  round and continue; the contamination-field strip mitigates this.
- **Order-lifetime guard**: `_send_or_update` has a `MIN_ORDER_LIFETIME_NS`
  gate (see `src/quote_engine.py:1116-1122`) that the standalone probe
  does NOT inherit. Gateway itself doesn't enforce this — our engine does
  to prevent its own race. The probe can modify-too-soon without
  triggering it, which is fine: we're measuring gateway latency, not our
  engine's throttle.

### Runtime estimate

- 5 N levels × (≈2s place + 20 rounds × ≈1s + 2s teardown) = ≈500s
- Plus connect (~2s) + qualify+preflight (~5s) + 4×3s level gaps = ≈20s
- **Total ≈ 9 min.** Round to 30 min total including corsair stop/start,
  env sanity checks, and a second run if the first looks anomalous.

### Run prerequisites

- `docker compose stop corsair` (clientId=0 collision — CLAUDE.md §1).
- Gateway must be healthy (`docker compose ps ib-gateway` shows Up, port
  4002 open).
- CME regular session for HG options (avoid the 16:00–17:00 CT settlement
  break — HG has a similar quirk to ETHUSDRR per CLAUDE.md §4; check live
  BBO has non-sentinel values in a sanity ticker before starting).
- After the run: `docker compose start corsair` to resume quoting.

### How to verify it worked

1. JSONL has `5 × 20 ≈ 100` amend rows per level (less if some rounds
   timed out) and `N` place rows per level.
2. Sum of all rows should be ≤600 (100 amends × 5 levels + 30 places),
   within a few of that is fine.
3. `ib.positions()` check post-run: zero HGJ6/HGK6 futures-option exposure
   on DUP553657. If non-zero, cancel manually via a fresh clientId=0
   session and investigate.
4. `docker compose logs ib-gateway --since 10m` should show no Error 103
   storms — transient 103s are OK but a cascade means the contamination
   strip isn't firing.
5. Invocation:
   ```bash
   docker compose run --rm --no-deps \
       -v $(pwd)/scripts:/app/scripts \
       -v $(pwd)/logs-paper:/app/logs-paper \
       corsair python3 /app/scripts/concurrency_sweep_probe.py
   ```

---

## Item 2: Refresh reduction (priority_v1 refresh policy)

### Goal

Cut concurrent orders-in-flight per refresh tick from ~15 to ~4–5. The
concurrency sweep in Item 1 shows each additional in-flight order costs
~30–45ms; halving in-flight count roughly halves loaded-amend p50 even
without any backend changes. Achieved client-side by:

1. Prioritising ATM strikes (always refresh on theo change).
2. Widening the dead-band on far strikes (|delta| < 0.1).
3. Capping concurrent amends fired per tick (staggered over multiple ticks).
4. Enforcing a max-age floor so no strike can go stale indefinitely.

### Files to create/touch

- **`src/quote_engine.py`** — primary change site.
  - `update_quotes` (line 436): after the existing sort-by-|delta|-desc at
    line 557–563, add the priority-bucketing + per-tick cap logic. The
    delta is already computed and available on `option.delta` (line 560).
  - `_send_or_update` (line 1029): add a second dead-band branch keyed on
    a new `delta_band` argument (or derive from the option in-method by
    adding `option.delta` to the existing signature; prefer passing the
    delta explicitly to keep the method pure).
  - `_process_side` (line 677): pass the computed `abs_delta` and a
    `priority_bucket` token through to `_send_or_update` so the dead-band
    selection is local to the send path.
  - New instance attribute `self._last_amend_ns: Dict[Tuple[float, str,
    str, str], int]` tracking per-key last amend send time — used for the
    max-age enforcement.
  - New instance attribute `self._tick_amend_count: int` reset at the top
    of `update_quotes` for the per-tick concurrency cap.
- **`config/hg_v1_4_paper.yaml`** — add the new knobs under `quoting:` and
  an HG override block. No change to existing keys.
- **`src/config.py`** — add the new fields to whichever quoting dataclass
  / namespace already exists; default them so rollback to legacy is
  automatic when the flag is absent.

### Algorithm (priority_v1)

Inside `update_quotes`, for each `(strike, exp, right)` in the already-
sorted-by-|delta| iteration:

1. Compute `abs_delta = abs(option.delta or 0)`.
2. Classify:
   - `near` if `abs_delta >= delta_threshold_near` (default 0.30).
   - `far`  if `abs_delta <  delta_threshold_far`  (default 0.10).
   - `mid`  otherwise.
3. Determine dead-band tick width (applied inside `_send_or_update`):
   - near: `dead_band_near_ticks` (default 2 — matches current
     `min_modify_ticks`).
   - mid:  `dead_band_mid_ticks` (default 3).
   - far:  `dead_band_far_ticks` (default 6 — 3× the current dead-band).
4. Max-age override: if
   `(now_ns − self._last_amend_ns.get(key, 0)) / 1e9 > max_age_s`
   (default 5.0 for far, 3.0 for mid, 1.0 for near — configurable as a
   single `max_age_s_far` / `max_age_s_near` pair), set
   `force_refresh = True`, which bypasses the dead-band (but NOT the
   per-tick cap, so a forced refresh still queues behind newer higher-
   priority amends — that's fine because the next tick drains it).
5. Per-tick concurrency cap: maintain `self._tick_amend_count` reset to 0
   at the top of `update_quotes`. Before firing an amend, check
   `self._tick_amend_count >= max_concurrent_amends_per_tick` (default
   4). If yes, skip this amend — it will re-evaluate on the next tick.
   Increment on every successful `placeOrder` modify send from
   `_send_or_update`. New places (not modifies) are exempt from the cap —
   we'd rather send a missing ATM place than defer it, and new places
   are rare once the book is populated.
6. Because the loop is already sorted by `|delta|` descending (line
   557–563), when the cap is reached, the skipped amends are the lowest-
   priority ones. No extra sorting needed.
7. Existing edge-violation bypass (line 1099–1108) stays; it overrides
   the dead-band when the resting price violates min_edge against current
   theo. Priority_v1's wider far-strike dead-band must not override
   edge-violation — keep it `if (not edge_violation and not
   force_refresh and abs(price - cur) < band)`. The edge check is the
   correctness floor.

### Migration plan / feature flag

Add a new config field `quoting.refresh_policy` with values:
- `legacy` (default, behaviour-preserving): existing single
  `min_modify_ticks` dead-band + no per-tick cap + no max-age logic.
- `priority_v1`: all four rules above.

In `_send_or_update` and `update_quotes`, branch on
`self.config.quoting.refresh_policy`. One-line rollback:
flip the yaml key, `docker compose up -d --build corsair`.

### Config knobs (defaults tuned for HG sym_5)

```yaml
quoting:
  refresh_policy: priority_v1     # or "legacy"
  delta_threshold_near: 0.30      # |delta| >= 0.30 → near bucket
  delta_threshold_far: 0.10       # |delta| <  0.10 → far bucket
  dead_band_near_ticks: 2         # same as current min_modify_ticks on HG
  dead_band_mid_ticks: 3
  dead_band_far_ticks: 6
  max_concurrent_amends_per_tick: 4
  max_age_s_near: 1.0
  max_age_s_mid: 3.0
  max_age_s_far: 5.0
```

HG product-block override (under `products[0].quoting` in
`hg_v1_4_paper.yaml`):
- `dead_band_near_ticks: 20` (HG tick = $0.0005, current dead-band is 20
  ticks = $0.01, keep as-is for near)
- `dead_band_mid_ticks: 30`
- `dead_band_far_ticks: 60` (6× on HG = $0.03 — well under `min_edge =
  $0.001`'s concern; the `edge_violation` guard catches any real drift)

Note: the current HG `min_modify_ticks: 20` represents the `near` band in
priority_v1. Keep the legacy knob intact for rollback — priority_v1 reads
the new knobs, not the legacy one.

### Test plan

1. **Unit sanity (no gateway)**: synthetic test in a REPL that calls
   `_send_or_update` with mocked `trade` and `option` objects across the
   three delta buckets, asserting the correct dead-band is applied and
   the per-tick counter increments.
2. **Gateway smoke**: start corsair with `refresh_policy: priority_v1`,
   observe one 5-minute window in `logs-paper/` and verify:
   - No Error 103/104 storms.
   - Fills still happen on ATM strikes at the same rate as legacy (pull
     `fills-YYYY-MM-DD.jsonl` row counts, grouped by |delta| bucket).
   - `skips-YYYY-MM-DD.jsonl` doesn't show a new failure mode.
3. **Quantitative**: after priority_v1 is stable in paper, run Item 1's
   `concurrency_sweep_probe.py` a SECOND time while corsair is RUNNING
   (yes — the whole point is to measure loaded conditions). Expected:
   concurrent in-flight count drops from ~15 to ~5, loaded-amend p50
   drops from ~370ms toward ~130ms (assuming 45ms/N linear). If the curve
   is flatter than predicted, concurrent reduction has diminishing
   returns — likely means we're also hitting a non-queue bottleneck
   (e.g. per-order gateway work) and the remaining win is smaller.
4. **Rollback test**: flip `refresh_policy: legacy`, redeploy, confirm
   behaviour returns to baseline.

### Expected load reduction (rough math)

- sym_5 = 22 instruments × 2 sides = 44 potential orders. Call it 30
  actively resting in steady state (some sides skip on edge / constraint
  gates).
- Underlying tick fans out to all 22; with legacy, a meaningful theo
  move can trigger modifies on ~15 of them (near+mid with 2-tick band).
- With priority_v1: 4 near strikes always refresh (or 8 if we count both
  sides) + maybe 20% of mid + 5% of far per tick under the 4-amend cap.
  Steady-state concurrent-in-flight: ~4 (cap) — the cap is the binding
  constraint by construction.
- Loaded-amend p50 at N=4 per Item 1's curve: ≈120ms vs ≈370ms legacy.
  **Roughly 3× TTT improvement with zero backend/FIX work.**

### Risks

- **Far-strike staleness → adverse selection**. Wider dead-band + amend
  deferral means far strikes quote against staler theo. Mitigated by (a)
  far strikes have |delta| < 0.1 so gamma P&L is small per tick move;
  (b) `max_age_s_far = 5.0s` caps how stale they get; (c) the existing
  edge-violation bypass still catches hard drifts. Net expected cost:
  marginal additional adverse selection on wings, which we already
  accept as the least-profitable tier anyway.
- **Correctness regression if max-age isn't enforced**: the
  `self._last_amend_ns` map must be updated on every successful amend
  send inside `_send_or_update` (at the `_record_send_latency` / post-
  `placeOrder` point, line 1145-ish) AND on every successful new place
  (line 1237 area). Miss either, and a strike can rest unrefreshed
  indefinitely. Unit test the map update on both paths.
- **Dead-band too aggressive → missed legitimate moves**. 6× tick
  (= $0.03 on HG) is still well inside the HG max-BBO-width of $0.05.
  Sanity: log every far-band skip with current theo delta vs resting
  price and watch the distribution for 24h — if it has a fat right tail,
  tune down.
- **Per-tick cap = 4 is binding even during quiet periods**. Not
  actually a risk — when few strikes need amends, all of them fire and
  the cap is unused. It only matters when `theo` moved on many strikes
  at once, which is exactly the case we want to stagger.
- **Interaction with existing `_margin_rejected` / modify-storm guards**:
  the new cap runs BEFORE the existing modify-storm guard (line 1070),
  so the modify-storm check still functions as the inner defence layer.
  No changes needed there.

### Non-goals

- No change to the 3-layer quote model (L1 penny-jump / L2 SABR / L3
  off at Stage 1–2). Priority_v1 only reorders / dead-bands the amend
  sends.
- No change to SABR fitting, calibration cadence, or `maybe_recal_sabr`.
- No change to the underlying tick source or `market_data.tick_queue`
  draining.
- No change to fill handling, hedge manager, or risk monitor.
- No change to the full-refresh cadence or `fallback_interval` — a full
  refresh with priority_v1 still iterates every strike and still obeys
  the per-tick cap; it just takes 3–4 ticks to complete a full sweep
  instead of one.

### Runtime estimate

- Implementation: 3–4h (code), 1h (unit tests on mocked trades), 2h
  (gateway smoke in paper). Total ~1 working day.
- Rollout: flip config, rebuild, observe 1 session in paper before
  letting it ride overnight.

### How to verify it worked

1. **Concurrent-in-flight instrumentation**: add a log line inside
   `update_quotes` per cycle: `logger.info("cycle done in_flight=%d
   amends_sent=%d amends_deferred=%d", ...)`. Grep the production log
   after 10 min — deferred should be non-zero during tick bursts, and
   in-flight should hover near `max_concurrent_amends_per_tick`.
2. **Latency histogram**: existing `_record_send_latency` already logs
   per-send latency. Compare the p50/p90 before vs after flip, same
   session window length, corsair-loaded conditions.
3. **Re-run Item 1's sweep probe with corsair RUNNING** under both
   policies. Compare curves. Expected: `priority_v1` flattens the
   corsair-loaded samples because our own in-flight count is lower.
4. **Fill rate parity on ATM**: `fills-YYYY-MM-DD.jsonl` grouped by
   strike distance from ATM — ATM and ±1 nickel fills should match
   legacy within noise; ±3/±4/±5 fills may drop 10–20% (expected,
   acceptable).
5. **No new kill-switch trips** in `kill_switch-YYYY-MM-DD.jsonl`.
6. **Rollback verification**: flip to `legacy`, redeploy, confirm
   latency and fill metrics return to baseline — proves the flag is
   actually gating behaviour.
