# Concurrency Sweep Probe — 2026-04-20 Results

Paper run, clientId=0, DUP553657, corsair stopped, 15 HXEK6 deep-OTM
contracts pre-flight-passed (ask ≥ $0.002). JSONL:
`logs-paper/concurrency_sweep-20260420-235915.jsonl` (630 samples).

## Curve

| N  | place p50 | place p99 | amend p50 | amend p99 | n_amend |
|----|-----------|-----------|-----------|-----------|---------|
| 1  | 1382.9ms* | 1382.9ms* |   40.8ms  |   51.1ms  |   20    |
| 2  |  159.2ms  |  159.2ms  |  136.7ms  |  148.1ms  |   40    |
| 4  |  259.1ms  |  356.2ms  |  236.3ms  |  345.4ms  |   80    |
| 8  |  456.1ms  |  753.4ms  |  435.2ms  |  743.9ms  |  160    |
| 15 |  741.4ms  | 1180.2ms  |  737.8ms  | 1207.3ms  |  300    |

*N=1 place p50 is a cold-start warm-up artifact (first wire write after
connect). N=1 amend p50 = 40.8ms matches the 2026-04-19 idle baseline of
42ms exactly.

## Slope analysis

| Segment | Δp50 | per-extra-N |
|---------|------|-------------|
| N=2 → N=4   | +100ms | **50ms/N** |
| N=4 → N=8   | +199ms | **50ms/N** |
| N=8 → N=15  | +303ms | **43ms/N** |

Reported end-to-end slope (N=1→N=15): 49.8 ms/N.

The curve is strikingly linear across N=2-15: every additional concurrent
order adds ~45-50ms of amend queue time, with no plateau.

## Interpretation

**This is the backend-serialization signature, per the plan's decision rule**
(≥30ms/N AND linear). Every added in-flight order pays a full queue slot.

But a caveat: the multi-clientId probe already established the bottleneck
is account-scoped and post-TCP-split. That narrows it to either (a) the
gateway JVM's per-account OMS queue or (b) IBKR's backend per-account
router. **This sweep alone cannot disambiguate (a) from (b)** — both
produce the same linear-in-N curve. Only a test through a different path
(separate gateway container, or actual FIX) would disambiguate.

## FIX implication

If serialization is in (a) gateway JVM → FIX bypasses it → expect
amend p50 at N=15 drops from 738ms to maybe 100-200ms (memory's
expectation).

If serialization is in (b) IBKR backend → FIX inherits the same queue →
expect amend p50 at N=15 stays around 600-700ms (FIX saves only
per-message TWS API overhead, maybe 20-40ms).

**We do not have evidence to pick (a) vs (b).** The memory's "100-200ms
regardless of concurrency" claim for FIX is from vendor docs / published
benchmarks, not from measurements on this account.

## Item 2 (refresh reduction) — now strongly motivated

Whether FIX turns out to be (a) or (b), Item 2 is a direct win. It
attacks the problem at the source — reducing our own N-in-flight:

- priority_v1 caps per-tick amends at 4 (from current ~15)
- At N=4, this probe measures amend p50 = 236.3ms
- vs. today's 737.8ms at N=15
- **3.1× improvement, no infra spend**

## Recommended next action

Before committing $500/mo × N months to FIX:

1. **Implement Item 2 (priority_v1).** 3× win is locked in regardless of
   what FIX turns out to do.
2. **Run a dedicated-gateway-per-role test** to disambiguate (a) vs (b).
   Cost: ~2h. If separate gateway shards the queue → FIX wins big.
   If not → FIX buys only ~40ms and we stay on TWS.
3. Only then commit to FIX (or not).
