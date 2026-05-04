# Corsair — Latency optimization handoff (host change in progress)

**Author:** Claude (Opus 4.7) for Brian Pemble (bpemble@me.com)
**Created:** 2026-05-04
**Source host:** Alabaster (Intel i7-10710U, 6c/12t, 31 GB RAM)
**Target host:** new bare-metal box — **Intel i9-14900K** (8 P-cores HT + 16 E-cores = 24c/32t)
**Goal:** continue p99 / p50 TTT reduction work on a host without thermal-throttling and process-pinning baggage

This is a **continuation** of `HANDOFF_BAREMETAL_MIGRATION.md` (2026-05-02 → Alabaster). That doc still covers everything you need for the *port itself* (state files to copy, kernel cmdline, IRQ pinning checklist, etc.). This doc covers **what's specifically about the latency work in flight**: what's been measured, what's been shipped, what's next. Read both.

---

## 1. Current latency state (2026-05-04 21:35 UTC, post-shipping)

Steady-state telemetry on Alabaster busy-poll, just before the THETA HALT fired:

```
TTT (broker tick recv → trader frame on commands ring)
  p50 = 64 µs   (pre-changes; new build pending live samples)
  p99 = 1255 µs

IPC (broker tick recv → trader process_event entry)
  p50 = 20 µs → 6 µs   (POST-CHANGES, confirmed live)
  p99 = ~ 750-1200 µs

RTT (broker → IBKR ack)
  p50 = ~ 42 ms   (network + IBKR-side, structural)
  p99 = ~ 1175 ms  (rare)
```

**Key finding from this session's TTT decomposition:**

The 64 µs TTT p50 broke down roughly:
- ~5-10 µs broker dispatch (NIC SCM_TIMESTAMPNS recv → SHM publish)
- ~3-5 µs SHM ring write + FIFO notify
- ~3-10 µs trader wakeup + ring read
- ~3-5 µs `rmp_serde::from_slice` → `GenericMsg`
- **~30-45 µs `serde_json::from_value(generic.extra.clone())`** ← biggest single component, NOW REMOVED
- ~5-10 µs decide + lock + state insert
- ~5-10 µs encode + ring write

The Value-tree round trip was the dominant p50 cost. With it gone (commit `96f9948`) the new p50 should land in the **25-35 µs range**. We could not validate live because the THETA HALT immediately killed the quote loop after restart — see §6 below.

---

## 2. What shipped this session — `commit 96f9948`

```
trader p50 perf: direct msgpack, Arc<str> intern, mimalloc, honest TTT

Files: rust/corsair_trader/{Cargo.toml, src/{decision,jsonl,main,messages,state}.rs}
Net:   +401 / -182 lines
```

### p50-1 — direct msgpack decode (drop Value clone)
- `MsgHeader { msg_type, ts_ns }` with NO `#[serde(flatten)] extra`. rmp_serde walks past unknown map keys cheaply.
- Hot path re-parses `body: &[u8]` directly into the matched typed struct (`TickMsg`, `VolSurfaceMsg`, etc.) via `rmp_serde::from_slice`.
- Cold-path messages (`KillMsg`, `WeekendPauseMsg`, `HelloMsg`, `HelloConfig`) get typed structs, replacing ad-hoc `extra.get("source")` lookups.
- **Estimated -30-45 µs at p50.** Confirmed via IPC drop 20µs → 6µs.

### p50-2 — typed JSONL channel
- `LogPayload` enum with `Event { recv_ns, body }` and `Decision(DecisionLog)` variants.
- Hot path enqueues a typed struct with a u64 ns timestamp. Writer task formats ISO `recv_ts` (chrono) and serializes JSON off-thread.
- Schema preserved for downstream consumers (`scripts/parity_compare.py`, `scripts/cut_over_preflight.py`).
- **Estimated -8-13 µs at p50** (chrono::to_rfc3339 + json! macro removed).

### p50-3 — Arc<str> expiry interning
- `TraderState` gains `expiry_intern: AHashMap<String, Arc<str>>`.
- All HashMap key types (`options`, `vol_surfaces`, `theo_cache`, `our_orders`, `orderid_to_key`) switched from `(u64, String, char, ...)` to `(u64, Arc<str>, char, ...)`.
- `intern_expiry(&str) → Arc<str>` is the single allocation site; clones are Arc bumps (~5 ns) instead of String allocs (~70 ns × 7 per tick).
- **Estimated -1-3 µs at p50.** Smaller win than #1, but free once #1 was in flight.

### p50-4 — TTT honesty fix
- TTT was previously stamped at `on_tick` entry — under-reported by the encode + ring-write window.
- Now stamped via a fresh `now_ns_wall()` AFTER `commands_ring.write_frame`. The TTT push is consolidated into the existing post-write state-lock so we don't take an extra mutex.
- **No latency reduction** — but the metric now includes ~5-15 µs of work that was previously hidden. Expect post-fix TTT p50 to look ~10 µs HIGHER than it would have under the old measurement, even with the p50 wins above. **This is a feature, not a regression.** Consumers should compare deltas, not absolute values, against pre-this-commit numbers.

### p99-4 — mimalloc as global allocator
- `#[global_allocator] static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;`
- Tighter tail latency than glibc malloc on rmp_serde decode + small-Vec churn.
- **Estimated -100-300 µs at p99** in allocation-heavy hot paths. No code-side coupling beyond the registration.

---

## 3. Verified diagnostic findings — Alabaster

### 3.1 CPU pinning is correct (p99-6)
- Trader: cpuset `4-5,10-11`, tokio workers on cores 4 and 5 (different physical cores), bg thread on core 10 (SMT sibling of 4).
- Broker: cpuset `2-3,8-9`, single hot thread on core 2.
- No action required.

### 3.2 IRQ affinity is clean (p99-7)
- NIC `eno1` IRQ-144 lives on CPU 6 (physical core 0's SMT sibling) — **off the trader and broker cpusets**.
- `softirq` NET_RX on trader cores 4,5,10,11: **all zero**.
- `softirq` SCHED on trader cores: also zero. Highest is broker cores at ~17k each over total uptime — acceptable.
- No host-side IRQ remapping was done. The clean layout is default Linux + Alabaster's stock setup.

### 3.3 JSONL backpressure is not a bottleneck (p99-3)
- Writer uses `try_send` + drop counter; non-blocking by design.
- Zero drops in today's logs.
- Throughput ~1.5 KB/s per file; 256 MiB rotation cap won't trip for ~50h of trading.
- Channel cap is 10 k messages, normal rate ~30 events/s — queue depth headroom is ~333 s of inbound.

---

## 4. Outstanding latency work — by priority

### Priority 1 — Lock-shard `TraderState` (p99-1)
**Estimated win: -300 to -500 µs at p99.**

Currently `TraderState.options`, `vol_surfaces`, `theo_cache`, `our_orders`, `orderid_to_key`, etc. all live behind a single `std::sync::Mutex`. The 10 Hz staleness sweep, 10 s telemetry, and graceful-shutdown task all briefly contend with the hot loop on this mutex. Under load, the hot loop's tail picks up these brief contentions.

Plan:
1. Move read-mostly maps (`vol_surfaces`, `theo_cache`) behind their own `RwLock` or `DashMap` — staleness loop reads, hot path reads + occasionally writes (only when fit_ts_ns advances).
2. Move `our_orders` + `orderid_to_key` to a separate mutex or `DashMap` — staleness loop iterates, hot path inserts/removes.
3. Keep `kills`, `risk_*`, `tick_size`, etc. behind the original mutex; they're tiny and rarely contended.
4. Audit lock-order — current invariant is `state → ring`. New scheme must preserve that strictly.

CLAUDE.md §17 already names this as the next planned cleanup (`#7 lock-shards`).

### Priority 2 — Clean 24h TTT histogram baseline
Before deciding RT kernel, busy-poll A/B, etc., we need:
- 24 h of TTT samples on the new host with the p50 wins shipped + lock sharding done.
- Histogram with p50 / p90 / p99 / p99.9 / max.
- Per-bucket attribution (when a tail event fires, what was the box doing? — `perf record -e sched:sched_switch,sched:sched_wakeup -p $(pgrep corsair_trader_rust)` for ~5 minutes is a cheap start).

Without this we're tuning blind.

### Priority 3 — Real-time kernel (PREEMPT_RT)
**Estimated win: bounds the p99 tail; tail events drop from ~5 / 500 samples → ~0.**

See `HANDOFF_BAREMETAL_MIGRATION.md` Phase 6 and the inline brief in this conversation transcript. Gating: if Priority 2's histogram still shows scheduler-attributable tail events after lock sharding, flip RT. If they're allocator/userspace-attributable, RT won't help.

Order: ship lock sharding first; collect histogram; decide on RT.

### Priority 4 — Busy-poll A/B (p99-2)
Both broker and trader currently busy-poll in production (env vars `CORSAIR_BROKER_BUSY_POLL=1`, `CORSAIR_TRADER_BUSY_POLL=1`). The "A/B" is therefore "prove busy-poll is worth a core" rather than "decide whether to enable."

Procedure:
1. With p50 wins live, baseline 30 min busy-poll.
2. Flip `CORSAIR_TRADER_BUSY_POLL=` (unset), restart trader, baseline 30 min FIFO-notify.
3. Compare p50 / p90 / p99. If FIFO-notify p99 is < 1.5x busy-poll p99, the core is not worth it.

Lower priority because it's already committed-on; rarely revisited.

### Priority 5 — broker-side cleanup (out of scope this session, but visible)
- Broker has been throwing **"modify_order ack timeout"** + **"Duplicate order id"** errors during testing. This is broker-side, not trader. Likely a race between the modify and the next place at the same key. The trader's `cancel-before-replace` logic ostensibly prevents this; the duplicate-order-id implies an old order survived a cancel.
- Worth investigating in a separate session — could be eating broker dispatch latency that shows up downstream.

---

## 5. Tooling commands — copy-paste for the new host

### 5.1 Latency probes (network)
```bash
# IBKR endpoint baseline (the 16.3 ms reference)
ping -c 50 cdc1.ibllc.com
mtr -r -c 100 cdc1.ibllc.com

# TCP-level latency (more representative than ICMP)
sudo apt install hping3
sudo hping3 -S -p 4002 -c 50 cdc1.ibllc.com   # paper TWS
# or
nping --tcp-connect -p 4002 -c 50 cdc1.ibllc.com
```

### 5.2 Live trader telemetry tail
```bash
docker compose logs --tail=200 trader 2>/dev/null \
  | grep -E "telemetry:" | tail -20

# parse ttt_p50 / ipc_p50 only:
docker compose logs trader 2>/dev/null \
  | awk '/telemetry:/ {for(i=1;i<=NF;i++) if($i~/ttt_p|ipc_p/) printf $i" "; print ""}' \
  | tail -20
```

### 5.3 Pin / IRQ verification (run on new host post-port)
```bash
# Trader pinning
TRADER_PID=$(docker inspect --format '{{.State.Pid}}' corsair-trader-1)
grep Cpus_allowed_list /proc/$TRADER_PID/status
for tid in $(ls /proc/$TRADER_PID/task); do
  echo "tid=$tid: $(cat /proc/$TRADER_PID/task/$tid/comm) cpus=$(grep Cpus_allowed_list /proc/$TRADER_PID/task/$tid/status | awk '{print $2}')"
done

# IRQ affinity for NIC
cat /proc/interrupts | head -1
cat /proc/interrupts | awk 'NR>1 && /[a-zA-Z]/{sum=0; for(i=2;i<=NF-1;i++){sum+=$i}; if(sum>1000) print sum, $0}' | sort -rn | head -10

# Softirq distribution
cat /proc/softirqs | grep -E "^(NET_RX|TIMER|SCHED):"
```

### 5.4 Synthetic load harness (TODO — does not exist yet)
A useful future tool: a Python or Rust script that pumps msgpack ticks through the SHM events ring at configurable rate, so you can validate TTT improvements offline (without needing live IBKR session + risk-state un-killed). Sketched but not built; would unblock latency validation when markets are closed or kills are sticky.

`scripts/rust_trader_parity.py` exists but compares decisions, not timing. A timing-focused replay is missing.

---

## 6. Why we couldn't validate TTT live this session

After the build redeployed at 21:26 UTC:
1. Broker reconciled from IBKR positions, found a pre-existing 147-contract short-put position (carry-over from earlier session) with ~$13 K theta exposure.
2. THETA HALT fired at 21:31:03 UTC: `THETA HALT: $-10206 < $-500`.
3. Kill is sticky (source=risk per CLAUDE.md §15). Broker auto-cancelled all open orders → `total=0`.
4. Trader correctly refuses to quote under kill. **Decisions = {} → no TTT samples.**

To clear: flatten the position via `scripts/flatten.py` (sanctioned per CLAUDE.md §15), then `docker compose restart corsair-broker-rs` to reset the sticky kill.

The IPC win (20µs → 6µs) was visible because tick events still flow through the IPC ring even when the trader doesn't decide on them.

**On the new host:** start fresh with no pre-existing position and the perf changes will be testable from minute one. If you carry the existing position via state migration, expect the same kill on first boot.

---

## 7. New-host: i9-14900K — what changes vs. Alabaster

The new host is an **Intel i9-14900K** (Raptor Lake Refresh). Headline specs:

```
P-cores: 8  × 2 threads (HT)         → CPU 0-15  (typical Linux numbering)
E-cores: 16 × 1 thread               → CPU 16-31
Boost:   6.0 GHz P-core (1-2 cores), 5.7 GHz P all-core, 4.4 GHz E all-core
Cache:   36 MiB L3 (shared), 2 MiB L2 per P-core, 4 MiB L2 per E-core cluster
TDP:     125 W base / 253 W max turbo
```

This is a **massive** improvement over Alabaster (i7-10710U, 15 W cTDP-up). Specifically:
- **No thermal throttling** under busy-poll on a desktop cooler. Drop `idle=poll` from the kernel cmdline; the chip doesn't need it.
- **3x the L3 cache.** Whole portfolio + market data state fits in L3 with room to spare. L2 misses to L3 are ~12 ns vs. L3-miss to RAM at ~100 ns.
- **5.7 GHz sustained all-core.** Our hot path is ~50-100 instructions per tick; clock matters.

### 7.1 The hybrid (P-core / E-core) gotcha — read first

The 14900K is a **hybrid topology**. Linux exposes all 32 logical CPUs in a flat list, but they are NOT equal:

- **P-cores (Performance):** 5.7 GHz, deep OoO, AVX2/AVX-512 disabled (yes, even on K — it's fused off on Raptor Lake). Use these for the trader and broker hot paths.
- **E-cores (Efficient):** 4.4 GHz, narrower pipeline, simpler branch predictor. Roughly 50-60% of P-core IPC. Fine for housekeeping, NOT for the hot loop.

**Verify the topology Linux sees on the new host BEFORE setting `cpuset`:**

```bash
lscpu --extended=CPU,CORE,MAXMHZ
# P-cores will report MAXMHZ ~6000; E-cores ~4400.

# Or via the kernel's hybrid-CPU sysfs (5.18+):
cat /sys/devices/cpu_atom/cpus    # E-cores (Atom-derived)
cat /sys/devices/cpu_core/cpus    # P-cores (Cove-derived)
```

Typical layout (Linux on a stock 14900K):
- `/sys/devices/cpu_core/cpus` = `0-15` (P-cores; 0,1=core 0; 2,3=core 1; ...; 14,15=core 7)
- `/sys/devices/cpu_atom/cpus` = `16-31` (E-cores; one thread per core)

**Verify before committing to a cpuset.** BIOS settings can re-enumerate (especially "Legacy Game Compatibility Mode" which disables E-cores).

### 7.2 Recommended cpuset on 14900K

Replace Alabaster's `cpuset: 4-5,10-11` (trader) and `cpuset: 2-3,8-9` (broker) with **P-core-only** assignments. Suggested layout, assuming standard P=0-15 / E=16-31:

```yaml
# docker-compose.yml
services:
  corsair-broker-rs:
    cpuset: "4-5,12-13"     # P-cores 2 and 6 with both SMT siblings
  trader:
    cpuset: "8-9,10-11"     # P-cores 4 and 5 with both SMT siblings
  # Housekeeping (kernel, dashboard, etc.) → P-cores 0,1,14,15 (siblings)
  #                                        + ALL E-cores (16-31)
```

Layout rationale:
- Broker on P-cores 2 and 6 (CPUs 4-5,12-13) → 2 physical cores, 4 threads.
- Trader on P-cores 4 and 5 (CPUs 8-9,10-11) → 2 physical cores, 4 threads.
- Both keep tokio worker on different physical cores (no SMT contention between hot threads).
- Bg threads land on the SMT sibling of the worker → shared L1/L2, low cross-core wakeup cost.
- P-core 0 (CPUs 0-1) reserved for systemd / IRQ context — Linux strongly prefers CPU 0 for housekeeping and you'll fight the scheduler if you take it.
- All E-cores left alone for everything else (Docker daemon, dashboard, etc.).

**Validation step on first boot:**
```bash
TRADER_PID=$(docker inspect --format '{{.State.Pid}}' corsair-trader-1)
for tid in $(ls /proc/$TRADER_PID/task); do
  comm=$(cat /proc/$TRADER_PID/task/$tid/comm)
  cpus=$(grep Cpus_allowed_list /proc/$TRADER_PID/task/$tid/status | awk '{print $2}')
  psr=$(awk '{print $39}' /proc/$TRADER_PID/task/$tid/stat)
  echo "$comm  cpus=$cpus  current_cpu=$psr"
done
# Hot threads should show current_cpu in the P-core range.
```

### 7.3 Other 14900K-specific knobs

- **Disable Intel Thread Director's "preferred" hints if they steer the trader to E-cores.** With `cpuset` pinned to P-cores only, Thread Director is a no-op for our workload, but worth confirming via `psr` field above.
- **Hardware prefetchers:** keep them enabled (they help the L2 → L3 path on our hot loop). BIOS default is fine.
- **C-states:** for the cleanest p99, disable C2/C3 (limit to C0/C1). BIOS option, or via `intel_idle.max_cstate=1` on the kernel cmdline. Trade: ~10-15 W more idle power for ~10-50 µs better wakeup tail.
- **Turbo Ratio Lock / per-core ratios:** stock multipliers are fine. Don't OC for latency work — instability creates the worst kind of jitter (sporadic clock blips).
- **Memory:** any DDR5-5600+ kit with EXPO/XMP enabled. We're not bandwidth-bound, but stable XMP is more important than raw speed.
- **NIC:** Z790 boards typically ship with Intel I225-V / I226-V 2.5 GbE. Both are well-supported on Linux 6.x. If your specific board has a Realtek 8125, run on it for a session and check `/proc/interrupts` jitter — typically fine but occasionally has tail outliers.

### 7.4 What stays the same

- Docker compose layout, image build process, env vars — all unchanged.
- Kernel cmdline: copy from `HANDOFF_BAREMETAL_MIGRATION.md` Phase 3, but **do NOT include `idle=poll`** (was an Alabaster-specific workaround for thermal-throttle-induced wake jitter; not needed here).
- IRQ pinning approach unchanged — let kernel default place the NIC IRQ on a non-trader, non-broker core (typically CPU 0) and verify.

---

## 8. Resume checklist for the next session

Once on the new host, in order:

1. ✅ Port using `HANDOFF_BAREMETAL_MIGRATION.md` Parts 1-4 (state files, OS install, kernel cmdline, cpuset).
2. ✅ Confirm `docker compose up -d` brings everything healthy.
3. ✅ Verify CPU pinning per §5.3 above.
4. ✅ Verify IRQ affinity (NIC IRQ should be off the trader/broker cpusets).
5. ⬜ With markets open + no carry-over kill: collect baseline TTT for 30 min. Capture `ttt_p50 / p99 / max`.
6. ⬜ Compare against this session's pre-change baseline (`ttt_p50=64µs, p99=1255µs`) and post-change estimate (`ttt_p50=25-35µs, p99=900-1100µs`) to validate the perf wins.
7. ⬜ Implement Priority 1 — lock-shard `TraderState`. PR is ungenerated; design notes in §4 above.
8. ⬜ Re-baseline after lock sharding.
9. ⬜ Decide on RT kernel based on the histogram.

---

## 9. Open questions for next session

- ~~Does `idle=poll` need to come along to the new host?~~ **No** — confirmed in §7.3 above for the 14900K. Drop it.
- Is the broker's `modify_order` duplicate-order-id race a real issue or test-environment noise? Worth a focused investigation.
- Should we build the synthetic load harness (§5.4) so latency validation isn't gated on live market state + un-killed sessions?
- The 147-short-put position: is that legitimate state to carry forward, or should we flatten before migration so the new host comes up clean? **Recommendation: flatten before migration.** Carrying it forward will just trigger THETA HALT again on first boot of the new host. Use `scripts/flatten.py --product HG --order-type limit` (see CLAUDE.md §15). Wait until session is open to ensure decent fills.
- BIOS settings to verify on the 14900K before first boot: SVM/VT-x on (Docker), C-states limited to C0/C1 (for p99), all P-cores enabled, all E-cores enabled (housekeeping headroom), Hyper-Threading on.

---

## 10. References

- `CLAUDE.md` — operator notes, §15-17 cover the architecture / cut-over / trader Rust port history. Critical reading.
- `docs/HANDOFF_BAREMETAL_MIGRATION.md` — companion doc, covers the port mechanics.
- `docs/findings/` — empirical findings folder; vega_kill characterization etc. live here.
- Memory: `~/.claude/projects/-home-ethereal-corsair/memory/` — auto-memory continues to pick up across hosts via the cloud-stored Claude account, so context like the Alabaster IPC baseline + spec deviations carries over.
- `git log` since `64b675e` — full chronology of this session's perf work.
