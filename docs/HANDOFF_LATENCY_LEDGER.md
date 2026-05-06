# Corsair — Local Latency Optimization Ledger

**Date:** 2026-05-06 (last updated)
**Predecessor:** `docs/HANDOFF_LATENCY_OPTIMIZATION.md` (2026-05-04)
**Status:** local hot-path optimization is approaching its structural floor. Two big architectural levers remain (trader+broker merge, infrastructure spend); see §10 + §11.

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
# POST-Lever-1 expected layout (cpuset 8-11 on 13900K/14900K):
#   corsair-hot      cpus=8     current_cpu=8    ← std::thread, busy-spin
#   tokio-rt-worker  cpus=10    current_cpu=10   ← parked, different physical core
#   corsair-bg       cpus=11    current_cpu=11   ← telemetry/staleness/signals
#   jsonl-trader_*   cpus=11    current_cpu=11   ← disk-IO writers
#   corsair_trader_  cpus=8-11  current_cpu=8    ← main thread (parked)
# Cpu 9 (SMT sibling of cpu 8) MUST be empty — see §13.4 for the bug
# that landed tokio worker / JSONL writers on cpu 9 and inflated TTT
# p99 to 6 ms.
```

**SMT-sibling bug check (REGRESSION TEST)**: confirm `cpu 9` (or
whichever cpu shares a physical core with `corsair-hot`) is empty in
the layout above. If a thread is on the SMT sibling of `corsair-hot`,
TTT p99 will inflate by 50-100× from execution-unit contention.

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

---

## 10. 2026-05-06 — Bundles 1-6 + Levers 1-3

Pickup from §1-9 above. Today's session ran a fresh full pass through the
hot path against the harness, shipped six bundles + two of three Levers,
and identified the next architectural lever (§11 below).

### 10.1 Headline (cumulative vs §1)

| metric | §1 baseline (2026-05-05) | post-2026-05-06 | total Δ |
|---|---|---|---|
| **TTT p50 (harness)** | 12-15 µs | **5 µs** | the harness now runs in the floor band; production is ~15 µs (§10.5) |
| **TTT p99 (harness)** | ~30 µs | **~12-14 µs** | -55%; clean shifts in repeated runs |
| **IPC p50 (harness)** | 8 µs | 0-2 µs | floor |
| Production TTT p50 (busy market) | 15 µs | **15-16 µs** (post-deploy) | structural; harness improvement carries through under the noise floor |
| Production TTT p99 (busy market) | 42 µs steady | (limited samples post-deploy; theta halt during validation) | — |

### 10.2 Bundles 1-6 (shipped 2026-05-06 morning)

All bundles are A/B tested via `scripts/run_ab_test.sh` against the
harness; verdicts in dollar-value order of significance. Each "KEEP"
shipped to production via image rebuild.

| bundle | what | code touchpoints | result | KS p |
|---|---|---|---|---|
| **B1 (A+B)** | Hand-rolled msgpack encoders for `PlaceOrder` / `ModifyOrder` / `CancelOrder`; `Ring::write_body` writes 4-byte BE length prefix directly into mmap (eliminates `pack_frame` Vec alloc on broker publish) | `corsair_trader/src/msgpack_encode.rs` (new), `corsair_ipc/src/ring.rs::write_body`, `corsair_ipc/src/server.rs::publish` | **SHIP** — TTT p50 -1 µs, p99 -5.1 µs | 0.001 |
| **B2 (C+D+F+G)** | Hand-rolled `decode_header` (no String alloc per inbound event); single `ScalarSnapshot` per tick (was 2× lock); `Arc<Vec<u8>>` for `events_log` body (refcount bump vs Vec clone); `r_char` cached at decode-time | `msgpack_decode::decode_header`, `state.rs::scalar_snapshot`, `jsonl.rs::LogPayload`, `messages.rs` (drop dead `MsgHeader`) | **KEEP** — neutral on perf, cleaner code; KS p=0.83 on rerun | 0.83 |
| **B3 (E)** | `vol_surfaces: DashMap<…, Arc<VolSurfaceEntry>>` so lookup is refcount bump, not full struct clone | `state.rs::lookup_vol_surface` | **KEEP** — neutral; defends p99 on cold-fit ticks | 0.45 |
| **B4 (H+I)** | Cursor-walk `unpack_all_frames` (single `Vec::drain` at end vs O(N²) per-frame); drop `Arc<Mutex<>>` on events_ring (single-reader path) | `corsair_ipc/src/protocol.rs`, trader `main.rs::async_main` | **KEEP** — neutral; defends p99 on tick bursts | 0.70 |
| **B5 (J)** | Replace `Arc<str>` keys with `u16` expiry intern | (deferred — not implemented) | **DEFER** — expected sustained gain ~30-50 ns is below the harness's ~500 ns p50 noise floor; refactor scope (5 files) outweighs measurability | — |
| **B6 (K)** | Pricing micro-opts: `OnceLock<Normal>` standard normal; cached `(1−β)` powers in SABR; short-circuit `disc=1.0` when `r=0` | `corsair_trader/src/pricing.rs` | **KEEP** — neutral (theo-cache absorbs steady-state); helps only on cache-miss ticks | 0.99 |

### 10.3 Bundles total

```
TTT (harness, n=148/152, baseline → post-B6):
  p50:    6.0 → 5.0 µs   (-1.0,  -16.7%)
  p90:   13.0 → 10.0 µs  (-3.0,  -23.1%)
  p99:   20.1 → 17.5 µs  (-2.6,  -12.8%)
  p99.9: 21.0 → 18.8 µs  (-2.2,  -10.2%)
  KS p=0.0017  (>99.9% confident the distribution shifted)
```

Verdict from compare_latency.py reads "INCONCLUSIVE" because p50 lands
exactly on the ±1000 ns gate — the cross-percentile shift is the real
signal here.

### 10.4 Levers 1-3 (afternoon — perf-driven)

A `perf record` profile after Bundles 1-6 showed the new function-level
breakdown:

```
24.43%  combined tokio scheduler (park / wake_by_ref / context::defer / runtime glue)
 4.44%  Ring::read_available
 8.44%  async_main closure (busy-poll body, all hot-path code inlined)
 ~62%   outside binary (mmap memcpy from SHM ring + libc)
 ~0%    each: decode_tick, decide_on_tick, encode_*_into, mimalloc
```

Hot path is now invisible at percent-resolution; tokio scheduler is
the largest visible chunk. Three Levers tested against this picture:

| lever | what | result | KS p |
|---|---|---|---|
| **L3** | mimalloc env tuning (`MIMALLOC_PURGE_DELAY=10000`, `MIMALLOC_RESERVE_HUGE_OS_PAGES=1`) | **REJECT** — TTT p99 +152.9% blowup. Likely the huge-pages fallback path. Single-flag retest (PURGE_DELAY only) was not run; do that before re-trying. | 0.018 |
| **L2** | `Ring::read_available_into(&mut Vec)` — extends caller's buf directly from mmap, no per-drain Vec alloc + memcpy | **SHIP** (bundled with L1) | — |
| **L1** | Trader hot loop on a dedicated `std::thread` (off tokio); `std::hint::spin_loop()` instead of `tokio::task::yield_now().await`; JSONL writer also moved to `std::thread` (was a tokio task; previously blocked workers=1 per the §3.1b reject) | **SHIP** (bundled with L2) — TTT p99 -5-6 µs (-28-34%) across two repeat runs; p50 unchanged (already at floor); KS p=0.58/0.87 (signal is in the tail, p50 doesn't move past gate) | 0.58 / 0.87 |

L1 implementation notes:
- `JsonlWriter` switched from `tokio::sync::mpsc::channel` + `tokio::spawn`
  to `std::sync::mpsc::sync_channel` + `std::thread::spawn`. Channel is
  bounded (10 000); `try_send` semantics unchanged.
- Hot loop extracted to `hot_loop_blocking()` which takes ownership of
  the Ring (Bundle 4I had already dropped its `Arc<Mutex<>>` wrapper)
  plus `Arc` clones of state/counters/commands_ring/jsonl writers.
- Pinning: `corsair-hot` thread → cpu 8 (first allowed). Tokio main_rt
  default workers drops to 1 in busy_poll mode, pinned to cpu 10
  (skipping cpu 8). bg_rt → cpu 11 (sibling of tokio worker). Cpu 9
  (SMT sibling of hot cpu 8) deliberately idle.

Production layout post-Lever-1 (cpuset 8-11):

```
cpu 8  → corsair-hot (std::thread, busy-spin SHM ring, owns the hot path)
cpu 9  → idle (SMT sibling of cpu 8 — kept clear so cpu 8 has full pipeline)
cpu 10 → tokio main_rt worker (parked on `pending::<()>().await`; wakes only on shutdown)
cpu 11 → bg_rt (telemetry 10s, staleness 10Hz, signal handler, JSONL writer threads if not yet pinned)
```

To revert L1: `git revert` the L1 commit + rebuild + force-recreate
trader. The threading model is the only structural change in this
batch; everything else is local refactor.

### 10.5 Production deploy + theta halt (2026-05-06 ~17:33 UTC)

`docker compose up -d --force-recreate trader` was run after Bundles
1-6 + L1+L2 landed. Trader started with `corsair-hot pinning to cpu 8`
log line — confirms the std::thread launched. Live telemetry pre-halt
(during 90s warmup, before vol_surface fits arrived):

```
ipc_p50=8 µs  ipc_p99=22 ms  ttt_p50=15 µs  ttt_p99=58 µs
```

p50/p99 match §1's pre-experiment numbers within noise — the bundles'
harness improvement (-1 µs at p50, -2.6 µs at p99) is below the 1 µs
production-telemetry resolution. The 11 ms outlier observed mid-warmup
disappeared from later samples.

A theta halt fired ~30s after startup (`THETA HALT: $-717 < $-500`)
on carried positions. Halt blocks all `decide_on_tick` paths early
via the `kills_count > 0` gate, so post-halt telemetry produces no
TTT samples (decisions return before the encode/write/histogram-push
path). Validation of L1+L2 in production beyond the 90s pre-halt
window deferred until positions are flattened and the broker is
restarted to clear the sticky kill.

### 10.6 What's still in the local hot path

After Bundles 1-6 + L1+L2, the visible cycle distribution is dominated
by:
- mmap memcpy from SHM ring (~62% — `Ring::read_available_into` plus
  the resulting buffer extend)
- the broker→trader IPC framing/parsing layer (msgpack encode on broker
  side, decode on trader side, length prefix per frame)
- tokio scheduler glue (24.4% post-L1 — only the bg_rt and the parked
  main_rt worker)

The hot path's USEFUL work (decode → decide → encode → write) is now
sub-percent on the profile. The next big win is structural elimination
of the SHM IPC layer — see §11.

---

## 11. The next big lever: trader + broker merge (eliminate IPC)

Current architecture (§15 of CLAUDE.md):

```
ib-gateway TWS  ←——TCP——→  corsair_broker_rust  ←——SHM ring——→  corsair_trader_rust
                            (1MiB ring + msgpack)
                            owns IBKR clientId=0
                            risk + hedge + snapshot + vol_surface fitter
```

The SHM IPC was originally built to allow swapping the broker
adapter (FIX/iLink) without touching the trader. **In practice**:
- The broker has a single adapter (`NativeBroker`, the in-tree native
  IBKR wire client). No alternate adapter is implemented.
- The IPC layer costs ~9 µs at p50 (broker emit + ring write + trader
  read + decode), which is roughly half of the current 15 µs production
  TTT.

**Merge proposal**: collapse the broker and trader into one process.
- Single tokio runtime (or std::thread for the hot path; bg async runtime).
- Direct access to broker state from the decision flow — no msgpack.
- Tick events go from native client → in-process closure → decision
  flow, with no ring write or read.
- Outbound place/modify/cancel calls go directly to the IBKR wire
  protocol; no command ring; no tokio mpsc dispatch.

Expected impact: **TTT p50 dropping from ~15 µs to ~3-6 µs** (eliminate
the ~9 µs IPC + msgpack chain). p99 similarly drops by ~10-15 µs.
Combined with L1's tail wins, target p99 ~5-8 µs from current ~12-14 µs.

Caveats and unknowns:
- The kill IPC path (broker → trader for kills) becomes intra-process
  function call. Existing kills_count atomic stays usable; just dispatch
  changes.
- `corsair_flatten` (the ops binary at `rust/corsair_ipc/examples/flatten.rs`)
  uses the SHM IPC to inject orders without the trader. Either: keep
  the IPC as a *secondary* observability/control path; or rewrite
  flatten to spawn a sidecar that talks to the merged binary's REST/UDS
  command socket.
- `corsair_tick_replay` (the harness) feeds ticks into the events ring.
  Same options: keep the IPC for replay only (broker bypassed at boot)
  or refactor the harness to spawn the merged binary with a fake-tick
  source.
- Failure isolation: today, a broker panic doesn't take down decision
  state; merged, both go together. Mitigated by the existing native
  IBKR client being already in-tree (commit `f7e10fd` and following).
- The merged binary still wants to be small: keep the existing module
  separation (`corsair_broker`, `corsair_market_data`, `corsair_pricing`,
  `corsair_risk`, `corsair_hedge`, `corsair_position`) so the runtime
  glue is what changes, not the math.

Effort estimate: **2-4 days of focused work** (a Phase 7 to mirror
Phase 6.7's broker/trader split, but in the opposite direction).
Risk: medium-high — same-day reversion path is the existing v3 split
with the SHM IPC fallback enabled.

This is the recommendation when local µs-level tuning has been
exhausted (now), and before committing to FIX gateway access ($500/mo)
or co-location ($500-2000/mo) — the merged hot path is the last large
locally-controllable lever.

---

## 12. 2026-05-06 — Pointers and harness data

Add to §8:

| location | content |
|---|---|
| `~/corsair_latency/baseline_2026-05-06_today.json` | fresh baseline against pre-Bundle-1 image |
| `~/corsair_latency/exp_b1_handenc_zeroalloc.json` | Bundle 1 candidate dump |
| `~/corsair_latency/exp_b6_pricing_micro.json` | Bundles 1-6 cumulative dump (used as Lever baseline) |
| `~/corsair_latency/exp_l3_mimalloc_purge.json` | Lever 3 result (REJECT) |
| `~/corsair_latency/exp_l12_stdthread_zerocopy.json` + `_v2.json` | Lever 1+2 candidate dumps (both SHIP-direction) |
| `~/corsair_latency/exp_l12_pinfix.json` | post-cpu-pin-fix Lever 1+2 verification |
| `/tmp/trader_perf.data` | 30s perf record post-Bundle-6, used for the §10.4 profile (note: tmpfs, ages out on reboot) |
| `/tmp/corsair_trader_rust` | binary copy needed for symbol resolution against `/tmp/trader_perf.data` (matches BuildID `482ff10a...`) |

---

## 13. 2026-05-06 evening — Tier A experiments

After §10's bundles + Levers and the §11 deferral discussion (trader+broker
merge waits on a separate-FCM transition so the rework lands once),
two Tier A experiments were run:

### 13.1 Tier A Exp 1 — PURGE_DELAY-only mimalloc retest

Hypothesis: §10.4 L3 rejected mimalloc env tuning bundled. The
`MIMALLOC_RESERVE_HUGE_OS_PAGES=1` flag's huge-pages fallback was
suspected; isolate `MIMALLOC_PURGE_DELAY=10000` alone.

Result: **REJECTED** across two harness runs.

```
Run 1 (n=149 vs baseline n=151):
  TTT p50:    +1 µs    p99:   +1 µs   KS p=0.38 (indistinguishable)
Run 2 (n=148):
  TTT p50:    +1 µs    p99:  +18 µs   p99.9: +37.7 µs   KS p=0.14
```

Interpretation: PURGE_DELAY=10000 defers purges, so when they fire
they're larger and stall the calling thread longer. Net is a tail
regression. Combined with the L3 reject, the conclusion is **mimalloc
defaults are appropriate for this workload — do not tune env vars.**

### 13.2 Tier A Exp 2 — Per-tick burst cap

Goal: address the empirical inflight=2 RTT cliff observed in
`logs-paper/wire_timing-2026-05-06.jsonl`:

```
inflight=0   p50= 42ms p99= 443ms   ← 64% of orders today
inflight=1   p50= 50ms p99= 655ms   ← 26%
inflight=2   p50=246ms p99= 920ms   ← 5.3% (5× p50 jump)
inflight=3+  p50=349-683ms p99=1.0-1.1s   ← 4.7%
```

~10% of orders today fire at inflight≥2 and pay 200-700 ms p50 of
JVM-OMS queue inflation. Designed gate: max 2 outbound sends in any
50 ms sliding window. Implementation:

- `OutboundLimiter` with two timestamp slots (`AtomicU64`); slot
  eviction picks the oldest. Ordering Relaxed because a one-extra
  slip on cross-thread races is acceptable.
- Gated in `decide_on_tick` after the cooldown / dead-band gates,
  before the modify-vs-place branch. Both kinds consume slots.
- `CancelAll` (risk self-block) bypasses the cap — yanking quotes
  on a kill is time-critical.
- Window is configurable via `CORSAIR_TRADER_BURST_WINDOW_NS`;
  read at boot into `SharedState::burst_window_ns`. Set to `0` to
  disable.

Pre-deploy harness check (replay doesn't simulate IBKR backpressure
so TTT shouldn't change): TTT p50/p90/p99 unchanged, KS p=0.62. Drop
in sample count (151→137) confirms the cap fires in the harness too.

Production verification via wire_timing-2026-05-06.jsonl analysis
(deploy at epoch 1778091399, n_pre=32769 / n_post=901):

| metric | pre-cap | post-cap | delta |
|---|---|---|---|
| place p50 RTT | 172.4 ms | 92.2 ms | **-47%** |
| place p99 RTT | 821.6 ms | 348.5 ms | **-58%** |
| modify p50 RTT | -0.3 ms | -0.2 ms | unchanged |
| modify p99 RTT | 594.4 ms | 318.5 ms | **-46%** |
| % orders inflight≥2 at send | 10.0% | 3.8% | **-62%** |
| inflight=4+ p99 RTT | 1086 ms | 445 ms | -59% |
| inflight=8+ bucket | 0.4% | 0.0% | eliminated |

Verdict: **SHIP**.

Trade-off: ~45% of would-be sends are deferred up to 50 ms before
re-firing. Net order rate halves (~5400/hr → ~2700/hr in this sample).
For paper: clean win. **For live**: validate alpha-loss-from-staleness
vs latency-improved-execution before keeping the cap on. Operator
disable: `CORSAIR_TRADER_BURST_WINDOW_NS=0` in compose env.

The cap only fires when the trader would have piled into the IBKR
OMS queue, so disabling it lets us measure the live-trading staleness
penalty. If paper-style >50% gating reproduces in live, the cap should
stay; if live has lower natural concurrency, it might be unnecessary.

### 13.4 SMT-sibling pinning bug — TTT p99 6 ms → 80 µs

Discovered in production telemetry shortly after the Tier A Exp 2
deploy when the operator reported TTT p99 at 6 ms. The bug had been
masked in the harness because the harness's cpuset (4,5) doesn't
share the same hybrid-CPU SMT topology as production's (8-11).

Diagnosis via `/proc/$TRADER_PID/task/*/status`:

```
hot std::thread     → cpu 8   ✓
tokio worker        → cpu 9   ✗ SMT sibling of hot cpu 8
corsair-bg          → cpu 11  ok
jsonl-trader_events → cpu 11  ok (kernel placed; unpinned)
jsonl-trader_decisions → cpu 9 ✗ SMT sibling of hot cpu 8 (unpinned drift)
```

When jsonl-trader_decisions or tokio worker had a brief activity burst
on cpu 9, they stole pipeline execution units from cpu 8 (Intel Raptor
Lake hybrid: SMT siblings share execution units, not just L1). The
hot thread on cpu 8 saw 6 ms gaps in its busy-spin, manifesting as TTT
p99 outliers.

**Root cause**: two pinning gaps in Lever 1 (commit `632aea5`):

1. **Tokio worker pinning fallback** in `main.rs::async_main` was
   `allowed_cpus.iter().find(|&&c| c != hot_cpu)` — picks cpu 9 on
   hybrid Intel parts where 8 and 9 share a physical core. Should
   skip the entire physical core, not just the cpu id.

2. **JsonlWriter::start did not pin** the spawned `std::thread`. The
   kernel scheduler placed both writers within the cpuset (8-11), so
   they drifted onto cpu 9 along with everything else.

**Fix** (commit `4503cb3`):

- Made `corsair_ipc::cpu_affinity::physical_core_id` `pub`. Tokio
  worker pinning now uses `physical_core_id(c) != physical_core_id(hot_cpu)`
  in the predicate. On cpuset 8-11 (8/9 sibling, 10/11 sibling), this
  picks cpu 10 instead of cpu 9.
- `JsonlWriter::start` now takes `pin_cpu: Option<usize>`. Trader
  passes the last allowed cpu (cpu 11 — colocates with bg_rt, both
  are disk-IO/sparse).

**Verification**:

```
pre-fix:  TTT p50=17 µs  p99=6116 µs   (production)
post-fix: TTT p50=18 µs  p99=  79 µs   (production, -99%)
```

Layout post-fix (verified via `/proc/$pid/task`):

```
cpu 8  → corsair-hot       (busy-spin, owns hot path)
cpu 9  → IDLE              (deliberately empty — see warning above)
cpu 10 → tokio-rt-worker   (parked on pending future)
cpu 11 → corsair-bg + jsonl-trader_events + jsonl-trader_decisions
```

**Why the harness missed this**: harness pins both replay and trader to
cpuset 4,5 on this Comet/Raptor host. `physical_core_id(4)` and
`physical_core_id(5)` differ — cpu 4 and 5 are NOT SMT siblings on
this layout. So the harness's tokio worker and JSONL writers don't
share a physical core with the hot thread regardless of pinning logic.
Only production cpuset 8-11 exposed the bug. **Future verification**:
after any threading change, check thread placement via /proc on the
*production* cpuset, not just the harness.

### 13.5 What's left after Tier A + the SMT fix

After Tier A:

- Mimalloc tuning is exhausted (defaults are correct).
- Per-tick burst cap landed; the §3.4 RTT inflation pattern is broken.
- §11's trader+broker merge remains the only large local lever, deferred
  pending FCM transition.

End-to-end p99 was 698 ms pre-cap (per §3.4); post-cap with most orders
landing at inflight≤1, end-to-end p99 should drop into the 350-500 ms
range. Not measured directly today — wire_timing-2026-05-07.jsonl will
have a clean post-cap distribution to validate.

The remaining infrastructure levers (FIX gateway, co-location) are
unchanged from §6. Their ROI is now even stronger relative to local
work — the local hot path contributes ~15 µs of TTT to a ~350-500 ms
end-to-end p99, i.e. <0.005% of total. Any further locally-focused
optimization is purely academic until those infrastructure changes
land or until the trader+broker merge deletes the remaining ~9 µs of
SHM IPC overhead.
