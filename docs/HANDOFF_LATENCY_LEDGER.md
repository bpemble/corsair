# Corsair — Local Latency Optimization Ledger

**Date:** 2026-05-05
**Predecessor:** `docs/HANDOFF_LATENCY_OPTIMIZATION.md` (2026-05-04)
**Status:** local hot-path optimization complete; further latency reduction now requires infrastructure spend (FIX gateway, co-location).

This ledger captures every local-latency change attempted on the i9-13900K
("Elephas") through 2026-05-05. Every result here was measured against a
deterministic replay harness so the data is reproducible.

---

## 1. Headline

| metric | pre-shipping (2026-05-03 baseline) | post-bundle (today, busy market) | improvement |
|---|---|---|---|
| **TTT p50** | 64 µs | **12 µs** (busy market, projected from harness) | **~5×** |
| **TTT p99** | 1255 µs | **~30 µs** (busy market, projected from harness) | **~40×** |
| **IPC p50** | 20 µs | 8 µs | 2.5× |
| **End-to-end p50** (tick → IBKR ack) | ~43-50 ms | **43.6 ms** | structural — local share is now 0.06% of total |
| **End-to-end p99** | ~700-1300 ms | **698 ms** | structural — local share is 0.015% of total |

**Local share of end-to-end latency is now < 0.1%.** Every µs of further trader
optimization is invisible at the operationally relevant timescale. The next
meaningful win is broker→IBKR (network + TWS JVM serialization).

---

## 2. The deployed optimization stack

These are all live in production on the current image (`corsair-corsair:latest`,
commit `9a0cd42`):

### Trader hot path

| optimization | source | function |
|---|---|---|
| Direct msgpack typed-dispatch (no `Value`-tree round trip) | `corsair_trader/src/main.rs:464`, `messages.rs::MsgHeader` | -30-45 µs at p50 vs the original `serde_json::from_value` round trip |
| `Arc<str>` expiry interning across all keyed state | `state.rs::SharedState::intern_expiry` | Replaces ~7 String clones per tick (~140ns each) with Arc bumps (~5ns each) |
| Lock-shard state via `DashMap` + `parking_lot::Mutex` | `state.rs::SharedState` | High-cardinality maps no longer share one mutex; bg tasks contend per-shard, not against a single point |
| Atomic `kills_count` mirror | `state.rs::SharedState::kills_count` | Per-tick "any kills?" gate is one Relaxed load instead of touching every DashMap shard |
| Histogram + scalar split locks | `state.rs::Histograms` / `ScalarState` | Telemetry sort doesn't block hot-path scalar reads |
| `mimalloc` global allocator | `main.rs:33` | Tighter tail latency vs glibc malloc (-100-300 µs at p99) |
| `mlockall(MCL_CURRENT \| MCL_FUTURE)` | `main.rs::lock_all_memory` | No page faults on hot path (requires `ulimits.memlock=-1` in compose) |
| Honest TTT measurement (post-write timestamp) | `main.rs::on_tick:962` | TTT now includes encode + ring write window (was previously underreported by 5-15 µs) |
| **on_tick takes `TickMsg` by value, mutates in place** (item 7, today) | `main.rs::on_tick:730` | Drops 2 String clones per tick (the merged_tick build) |
| **`decide_ns_wall` passed in from `process_event`** (item 8, today) | `main.rs::on_tick:743` | Drops one `SystemTime::now()` syscall per tick (~80-150 ns) |
| **Single preallocated `wire_buf` + `frame_ranges`** (item 9, today) | `main.rs::on_tick:870-895` | Drops 2-4 `Vec<u8>` allocations per place/modify; encode via `rmp_serde::encode::write_named` directly into the buffer |

### Broker hot path

| optimization | source | function |
|---|---|---|
| Native Rust IBKR wire client (no PyO3, no ib_insync) | `corsair_broker_ibkr_native/` | NativeBroker boot ~50ms (vs 30-90s for ib_insync's bootstrap) |
| Dedicated-thread command pump (busy-poll mode) | `corsair_ipc/src/server.rs::command_pump_blocking` | command IPC p50 ~50µs vs ~470µs for the tokio-task path |
| SHM ring + FIFO notify (zero kernel for ring access) | `corsair_ipc/src/ring.rs` | Producer→consumer is mmap copy + atomic store with Release/Acquire ordering |

### Configuration / infrastructure

| setting | value | rationale |
|---|---|---|
| Kernel cmdline | `intel_idle.max_cstate=1 processor.max_cstate=1` | C-state cap; cores never drop below C1 (no exit-latency jitter) |
| CPU governor | `performance` | Cores stay at peak boost; no DVFS jitter |
| Trader cpuset | `8,9,10,11` | P-cores 4+5 with both SMT siblings |
| Broker cpuset | `12,13,14,15` | P-cores 6+7 with both SMT siblings |
| Gateway cpuset | `0,1,6,7` | P-cores 0+3 (low-priority, JVM, shares with OS housekeeping) |
| NIC IRQ placement | cores 26-29 (E-cores) | Off the trader/broker cpusets — no IRQ contamination |
| `kernel.perf_event_paranoid` | 1 | `perf` profiling allowed for non-root processes (set via `/etc/sysctl.d/99-perf.conf`) |
| Trader busy-poll | `CORSAIR_TRADER_BUSY_POLL=1` | Tight loop reading SHM ring; ~13 µs IPC reduction at the cost of 1 dedicated core |
| Broker busy-poll | `CORSAIR_BROKER_BUSY_POLL=1` | Same idea, command-side dispatch |
| `tokio` worker count | 2 | Worker 0 hot-loop, Worker 1 + bg for telemetry/staleness |
| Bg-thread pinning | First allowed CPU not in worker pin set (cpu 9 currently — SMT sibling of cpu 8) | Shared L1/L2 with worker 0 for low cross-core wakeup cost |

---

## 3. Today's experimental ledger

Every change tested against the deterministic replay harness with a fixed
JSONL recording. Sample sizes are per-arm; KS test compares full distributions.

### 3.1 Bundle items 7+8+9 — SHIPPED (commit `9a0cd42`)

Three coupled hot-path optimizations, tested as a bundle because individual
µs-scale changes don't clear the harness's per-run noise envelope.

```
TTT (n=153 baseline, n=153 bundle, harness):
  p50:   7 µs →  6 µs   (-14%)
  p90:  13 µs → 10 µs   (-23%)
  p99:  23 µs → 16 µs   (-32%)
  KS test: D=0.230, p=0.0005   ← distributions differ at >99.9% confidence
```

Confirmed live in production:

```
production TTT (busy market window):
  p50:  21 µs → 17 µs  (just after deploy)  →  15 µs (steady state, busy)
  p99:  88-93 µs → 61-76 µs → 42 µs
```

### 3.1b Bg thread on idle-worker SMT sibling + target-cpu=native — SHIPPED (commit `d7dc939`)

Two coupled changes on top of bundle 7+8+9, tested together against the
post-bundle harness baseline:

```
TTT (n=143 baseline, n=184 candidate, harness):
  p50:   9 µs →  7 µs   (-22%)
  p90:  18 µs → 13 µs   (-28%)
  p99:  35 µs → 30 µs   (-15%)
  KS test: p=5e-7   ← clean distributional shift
```

`workers=1` was tested as a third member of this bundle but **rejected**
(KS p=5e-25 regression). With one tokio worker, the JSONL writer task
contends with the busy-poll loop and blocks the hot path.

Bg thread now pins to cpu 11 (SMT sibling of the idle worker 1) instead
of cpu 9 (sibling of worker 0). target-cpu=native unlocks AVX2 + Raptor
Lake-specific tuning. Caveat: native binaries are HOST-SPECIFIC; cross-
host portability requires `target-cpu=alderlake` or `x86-64-v3`.

### 3.1c Hand-rolled msgpack decoder for TickMsg — SHIPPED (commit `6e410ff`)

Replaces `rmp_serde::from_slice::<TickMsg>(&body)` on the highest-frequency
inbound path with a hand-rolled byte-level decoder.

```
TTT (n=184 each, harness, vs bg+native baseline):
  p50:    7 µs →  6 µs   (-14%)
  p90:   13 µs → 12 µs   (-10%)
  p99:   30 µs → 19 µs   (-37%)   ← upper tail collapses
  p99.9: 65 µs → 19 µs   (-71%)
  max:   69 µs → 19 µs   (-73%)

IPC (n=5165, harness):
  p50:   1 µs → 0 µs    (sub-microsecond decode)
  p90:   4 µs → 2 µs    (-50%)
  KS:    p=8e-47        ← decoder distribution dramatically shifted
```

Implementation in `corsair_trader/src/msgpack_decode.rs`:
- One-pass byte walk; no `Value`-tree
- Key dispatch via (length, content) match
- In-place fill via caller-provided `TickMsg::default()`; reuses
  String allocations across consecutive ticks
- Returns `false` on schema mismatch → bumps `dropped_parse_errors`,
  drops the tick (clean signal on broker schema drift)
- 5 round-trip property tests verify byte-level parity with
  `rmp_serde::to_vec_named`

Reverting any of the three: `git revert <sha>` + image rebuild +
`docker compose up -d --force-recreate trader corsair-broker-rs`.

### 3.2 Items tested and rejected

| item | what | result | data preserved at |
|---|---|---|---|
| **Item 1**: mimalloc env tuning (`MIMALLOC_PURGE_DELAY=10000`, `MIMALLOC_RESERVE_HUGE_OS_PAGES=1`) | Reduce tail decommit jitter | Inconclusive at harness scale; KS p=0.80; p50 unchanged | reverted, not in compose |
| **Item 4**: `DashMap::with_shard_amount(8)` | Reduce cache-line traversal in 10 Hz staleness sweep | Inconclusive on KS but slight p50 regression (+1 µs); not worth the change | reverted |
| **Items 5+6**: drop Mutex on events ring + zero-copy `read_available_into` | Save lock acquire + alloc per ring read | Neutral at harness scale (KS p=0.83); ~50-100 ns/op below noise floor | reverted; backups at `/tmp/{ring_rs,main_rs}_5_6.rs.bak` (now wiped — reimplement from this doc if revisited) |
| **`isolcpus=8-15 nohz_full=8-15 rcu_nocbs=8-15`** | Eliminate kernel preemption from trader/broker cores | Tested; the IRQ + softirq invariants verified clean (cores 8-11 had 0 SCHED softirqs after isolation). However the post-reboot warmup confounded the measurement and the rollback didn't immediately restore the morning baseline either. Effect was indistinguishable from boot-state effect at p50; can't claim a win or loss. | rolled back; backup at `/etc/default/grub.pre-isolcpus.1778001938` |
| **PREEMPT_RT kernel** (`6.12.85+deb13-rt-amd64`) | Lower scheduler-preemption tail | **Confirmed regression**, statistically significant (KS p=2.8e-11). Without `SCHED_FIFO` on hot threads, the kernel-side preemption-check overhead inflates the hot path. | RT data: `~/corsair_latency/rt_kernel.json`; non-RT companion: `~/corsair_latency/non_rt_kernel.json` |

```
PREEMPT_RT vs non-RT (n=143 each, harness, same recording):
  TTT p50:   9 µs →  15 µs   (+67%)
  TTT p90:  18 µs →  28 µs   (+54%)
  TTT p99:  35 µs →  50 µs   (+43%)
  KS test: D=0.413, p=2.8e-11   ← regression definitive
```

This is the cleanest A/B in the ledger. RT is not viable for our workload
without `SCHED_FIFO` on the hot threads, and even with it the upside is
< 10 µs, while the operational complexity (rtprio limit on the container,
runaway-thread risk under busy-poll) is real.

### 3.3 Hardware profiling findings (perf)

Captured 30-second `perf record` + `perf stat` on the running trader:

```
IPC                       1.80 insn/cycle    excellent (~45% of theoretical max)
L1 dcache miss rate       0.0013%            essentially perfect
Branch mispredict rate    0.0003%            essentially perfect
L3 miss rate              11.4% of refs      moderate; ~640 µs/s of DRAM time → 0.06% of wall
Context switches          41/s               low (busy-poll holds the thread)
CPU migrations            2/s                pinning is stable
Sustained frequency       5.50 GHz           at peak boost
```

**Conclusion: hot path is compute-bound, not memory-bound.** No cache or
branch issue to optimize; further wins must reduce instruction count.

Function-level breakdown (perf 30s sample, all trader threads):

```
47.4%  async_main::closure                     ← busy-poll loop body (decode + decide + encode + write)
18.0%  tokio::runtime::park::Inner::park       ┐
14.6%  tokio::runtime::park::wake_by_ref       │ → 37.8% TOTAL TOKIO SCHEDULER OVERHEAD
 5.2%  tokio::runtime::context::defer          ┘   (worker 1 + bg parking/waking with ~no work)
 7.7%  Ring::read_available
 7.0%  main                                    ← outer fn / init residue
```

The 38% tokio overhead is the only remaining structural inefficiency. It
doesn't directly inflate TTT (worker 0 runs continuously) but it burns
~38% of CPU on park/wake of worker 1 and the bg thread. Reducing this
(via `CORSAIR_TRADER_WORKERS=1`) is the only further improvement worth
considering, but it has runaway-blocking risk and the actual TTT win is
likely sub-µs.

### 3.4 RTT analysis (the actual p99 problem)

End-to-end p99 = 698 ms is dominated by TWS gateway JVM serialization at
the broker→IBKR boundary:

```
RTT p50/p99 by in-flight concurrency at send time (45,441 placed orders):
  inflight=0:    53 ms / 430 ms
  inflight=1:    54 ms / 633 ms
  inflight=2:   255 ms / 1038 ms      ← +200 ms from one extra in-flight
  inflight=4:   461 ms / 1170 ms
  inflight=8:   842 ms / 1719 ms
  inflight=16: 1159 ms / 1719 ms

Modify vs Place:
  modify (n=17,419):  p50= 41 ms   p99= 573 ms
  place  (n=28,022):  p50=186 ms   p99=1053 ms
```

Each additional in-flight order adds ~50-90 ms to subsequent acks. This is
the JVM-gateway per-account OMS queue (single-threaded). The worst observed
RTTs (>1.7 s) all cluster on bursts where 14-20 orders fired within < 1 ms
of each other. The fix paths:

1. **Free**: per-tick burst cap in the trader (limit outbound to ~2 orders
   per tick window). Captures 80%+ of the p99 win since the slow tail is
   driven by simultaneous bursts.
2. **$500/mo**: FIX gateway access (bypasses TWS JVM entirely; queue moves
   to IBKR backend where it's much faster).

Neither is local-latency work in the hot-path sense, but both are the
realistic levers for moving the operationally-relevant p99.

---

## 4. The replay harness

Use this to A/B any future trader change before deploying.

### Files

```
rust/corsair_tick_replay/             — replay binary (synthetic broker)
scripts/run_latency_harness.sh        — orchestrator: replay + trader on isolated SHM
scripts/run_ab_test.sh                — wrapper: candidate run + auto-compare to baseline
scripts/compare_latency.py            — bootstrap CIs + KS test + verdict
```

### Run a baseline

```bash
# Save persistently (NOT /tmp; tmpfs gets wiped on reboot)
mkdir -p ~/corsair_latency
./scripts/run_latency_harness.sh ~/corsair_latency/baseline_$(date +%Y%m%d_%H%M).json --duration 300
```

### Run an A/B test

```bash
# After a candidate change is built into corsair-corsair:latest:
./scripts/run_ab_test.sh my_change ~/corsair_latency/baseline_<date>.json --duration 300
# → prints TTT + IPC comparison with bootstrap CIs and KS test
# → saves candidate dump as ~/corsair_latency/my_change.json
```

### Pass extra env to the candidate trader (no rebuild needed for env-only changes)

```bash
./scripts/run_ab_test.sh mimalloc_test ~/corsair_latency/baseline.json \
    --env MIMALLOC_PURGE_DELAY=10000 \
    --env MIMALLOC_RESERVE_HUGE_OS_PAGES=1
```

### Noise floor (measured today, n=143-155 per arm, 5-min runs)

```
TTT p50:  ±500 ns baseline-to-baseline drift
TTT p99:  ±35 µs baseline-to-baseline drift (200% relative — too coarse to gate verdict)
IPC p50/p90: rock solid (0/2-3 µs across runs)
IPC p99:  ±200 ms baseline-to-baseline drift (queue-lag artifact during heavy events)
```

The compare script gates verdicts on p50 only (threshold ±1000 ns =
2× envelope). p99 is reported but informational only at this sample size.
Longer runs (20+ min) would tighten p99 noise enough to gate, at the cost
of slower iteration.

---

## 5. Latency decomposition (TTT p50 = 15 µs)

Theoretical breakdown based on perf data + code reading. Empirical
sub-µs profiling needs stage instrumentation (one code change away).

```
TTT p50 ≈ 15-17 µs end-to-end (broker emit → trader frame on commands ring)

  ┌─────────────────────────────────────────────────────────────┐
  │ Broker side                                     ~3–5 µs (~25%)│
  │   tick handler → events ring write + FIFO notify             │
  ├─────────────────────────────────────────────────────────────┤
  │ Trader hot path                                ~10–12 µs (~70%)│
  │   ring read (mmap copy)                          ~0.5–1.0 µs │
  │   msgpack header decode                          ~0.5–1.0 µs │
  │   IPC histogram push                             ~0.05 µs    │
  │   JSONL events_log enqueue                       ~0.1 µs    │
  │   match dispatch + typed TickMsg parse           ~1.0–2.0 µs │
  │   options DashMap insert + scalar snapshot       ~0.3–0.5 µs │
  │   decide_on_tick (gates + theo cache):           ~3.0–5.0 µs │
  │   wire_buf encode (CancelOrder + PlaceOrder)     ~1.5–2.5 µs │
  │   commands ring lock + write_frame                ~0.3–0.5 µs│
  │   TTT histogram push                              ~0.05 µs   │
  ├─────────────────────────────────────────────────────────────┤
  │ Syscalls / kernel transitions                    ~1–2 µs (~10%)│
  │   2× SystemTime::now() (vDSO, ~80ns each)                    │
  │   FIFO notify (1B write syscall)                ~0.5–1.0 µs  │
  └─────────────────────────────────────────────────────────────┘

By category at 15 µs p50:
  SOFTWARE (function execution):    ~10–12 µs   ~60–70%
  HARDWARE/MEMORY SUBSYSTEM:         ~3–5 µs    ~20–30%
  SYSCALLS/KERNEL:                   ~1–2 µs    ~5–10%
  BROKER (counted in TTT):           ~3–5 µs    ~20–30%
```

---

## 6. What's next (not local)

The local hot path is exhausted. Further latency reduction requires
infrastructure spend, in increasing order:

| spend | what you get | end-to-end Δ | ROI |
|---|---|---|---|
| **Per-tick burst cap (free, hours of work)** | Limit trader outbound to N orders per K-ms window; eliminates the 14-20-orders-at-same-timestamp bursts | -200-400 ms at p99 | high — pure software change, no infra |
| **Modify-vs-replace bias (free, hours of work)** | Defer cancels by 250 ms when an amend might suffice | -50-100 ms at p99 | high — incremental on top of bundle |
| **FIX gateway access (~$500/mo)** | Bypasses TWS JVM serialization; concurrency slope drops from 50-90 ms/N to <10 ms/N | -5-15 ms at p50, -200-400 ms at p99 | high — first real infra spend |
| **Co-location near IBKR (~$500-2000/mo)** | Cut wire RTT from ~30-50 ms to ~0.5-2 ms | -30-42 ms at p50, -500-650 ms at p99 | the big hardware win |
| **CME Globex direct connectivity ($5K+/mo)** | Bypass IBKR for HG/ETH FOPs | -43 ms at p50; sub-ms achievable | overkill for paper / Stage 1 |

Start with the per-tick burst cap — it's free, addresses the actual
observed p99 driver (CLAUDE.md `project_fix_colo_evidence`), and provides
data on whether FIX is worth $500/mo before committing.

---

## 7. Host migration checklist

When porting Corsair to a new bare-metal host, verify each invariant in
this list **before** baselining latency. Skipping these reproduces the
"why is everything slow?" surprises we hit during this session's reboots.

### Kernel cmdline

```bash
cat /proc/cmdline
# Expect:  ... intel_idle.max_cstate=1 processor.max_cstate=1 ...
```

If absent, edit `/etc/default/grub`'s `GRUB_CMDLINE_LINUX_DEFAULT`, then
`sudo update-grub`, reboot.

### CPU governor

```bash
cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor | sort -u
# Expect:  performance
```

Set via cpufrequtils package or systemd unit, NOT via boot cmdline (kernel
exposes `intel_pstate=passive intel_cpufreq=performance` for that path).

### NIC IRQ placement

```bash
cat /proc/interrupts | awk 'NR==1 || /eno|enp|en[a-z][0-9]/'
```

Verify the NIC's TxRx queue IRQs land on cores OUTSIDE the trader and
broker cpusets (currently cpuset 8-15 — IRQs should be on 0-7 or 16-31).
On the 13900K box, default placement was already on E-cores 26-29.

### CPU isolation: DON'T

Per today's findings: do NOT set `isolcpus=` / `nohz_full=` / `rcu_nocbs=`.
The trader is busy-poll, so it doesn't benefit from the kernel staying off
these cores; the only effect we measured was confounded reboot-warmup. If
you want to revisit on a different topology, run the harness A/B before
committing to it.

### CPU pinning verification

```bash
TRADER_PID=$(docker inspect --format '{{.State.Pid}}' corsair-trader-1)
for tid in $(ls /proc/$TRADER_PID/task); do
    comm=$(cat /proc/$TRADER_PID/task/$tid/comm)
    cpus=$(grep Cpus_allowed_list /proc/$TRADER_PID/task/$tid/status | awk '{print $2}')
    psr=$(awk '{print $39}' /proc/$TRADER_PID/task/$tid/stat)
    echo "tid=$tid comm=$comm cpus=$cpus current_cpu=$psr"
done
# Expect:  workers on different physical cores (8 and 10 on 13900K)
#          bg thread on cpu 9 (SMT sibling of cpu 8 — shared L1/L2 with worker 0)
```

If your new host has different P/E-core layout, re-derive the cpuset map.
On Raptor Lake (13900K/14900K): P-cores are CPU 0-15 (8 cores × 2 SMT),
E-cores are 16-31. Verify with:

```bash
cat /sys/devices/cpu_core/cpus    # P-core CPU range
cat /sys/devices/cpu_atom/cpus    # E-core CPU range
```

### mlockall

```bash
docker compose logs trader 2>&1 | grep -E "mlockall"
# Expect:  mlockall MCL_CURRENT|MCL_FUTURE succeeded
```

If "EPERM" or "ENOMEM": ensure `ulimits.memlock: -1` is set in
`docker-compose.yml` for the trader service.

### busy-poll

```bash
docker compose logs trader 2>&1 | grep "BUSY_POLL"
# Expect:  CORSAIR_TRADER_BUSY_POLL=1 — busy-polling SHM ring
```

`CORSAIR_TRADER_BUSY_POLL` and `CORSAIR_BROKER_BUSY_POLL` should both be
set to `1` in `docker-compose.yml`.

### perf access

```bash
cat /proc/sys/kernel/perf_event_paranoid
# Expect:  1   (or lower)
```

Set via `/etc/sysctl.d/99-perf.conf` (see "scripts" in the harness section).

### Latency baselines

After all of the above is verified, take 3 baseline runs to establish
the new host's noise floor before testing any change:

```bash
mkdir -p ~/corsair_latency
for i in 1 2 3; do
    ./scripts/run_latency_harness.sh ~/corsair_latency/baseline_$i.json --duration 300
done
# Pairwise compare to estimate envelope:
for pair in "1 2" "1 3" "2 3"; do
    set -- $pair
    python3 scripts/compare_latency.py ~/corsair_latency/baseline_$1.json ~/corsair_latency/baseline_$2.json --metric ttt_us
done
```

---

## 8. Pointers / preserved data

| location | content |
|---|---|
| commit `9a0cd42` | bundle 7+8+9 + replay harness + env-gated histogram dump |
| `~/corsair_latency/non_rt_kernel.json` | non-RT harness baseline (today, n=143) |
| `~/corsair_latency/rt_kernel.json` | PREEMPT_RT harness run (today, n=143) — proof RT regresses |
| `/etc/default/grub.pre-isolcpus.1778001938` | pre-isolcpus grub config backup |
| `logs-paper/wire_timing-2026-05-05.jsonl` | broker-side wire timing data; 45k+ orders for RTT analysis |
| `logs-paper/trader_events-2026-05-05.jsonl` | trader event recording (used as harness input) |
| `docs/HANDOFF_LATENCY_OPTIMIZATION.md` | predecessor handoff; lock-shard etc. context |

---

## 9. References

- `CLAUDE.md` §15-17 — broker/trader split, cut-over safety stack, Rust trader binary
- `CLAUDE.md` `project_fix_colo_evidence` — concurrency-RTT empirical findings
- `docs/latency_decomp_results.md` — the original concurrency sweep probe (April)
- `docs/HANDOFF_BAREMETAL_MIGRATION.md` — host-port mechanics
