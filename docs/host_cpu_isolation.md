# Host CPU isolation for trader / broker hot threads

**Status:** drafted 2026-05-07. NOT YET APPLIED — requires host reboot.
**Owner action:** review pre-flight, run apply script, reboot.

## Problem

The trader's hot loop pins to cpu 8 (`busy_poll=1` mode) and busy-spins on
the events SHM ring. Despite the pin, `/proc/$PID/task/<corsair-hot>/status`
showed **19,598 involuntary context switches in 40 minutes** of uptime
(~8/sec). Each one is the kernel scheduler taking cpu 8 away from the
busy-spin to run something else: a kworker, an IPI handler, a TLB
shootdown response, an HZ=1000 timer tick, etc.

Most preemptions are short (µs-scale timer ticks) and fold into normal
jitter. Rare ones run for tens of ms — those are the 80ms p99 outliers
visible in `wire_timing-*.jsonl::triggering_tick→trader_decide`.

Direct evidence (2026-05-07 13:02:57 incident): 88 ticks with `broker_recv_ns`
between `:57.289` and `:57.369` ALL show `recv_ts = :57.369` in
`trader_events-*.jsonl`. The broker emitted them in real time; the trader
stopped draining for 80ms. The hot thread was preempted as a single block.

## Why the current setup doesn't isolate cpu 8

`/proc/cmdline` currently has:
```
quiet intel_idle.max_cstate=1 processor.max_cstate=1
```

`intel_idle.max_cstate=1` keeps c-state transitions shallow (good — prevents
wake-from-idle latency). But there is no `isolcpus`, `nohz_full`, or
`rcu_nocbs`. The kernel sees cpus 8-15 as ordinary scheduler targets:

- **HZ=1000 timer ticks** fire on every cpu including 8 (cpu 8: 26M LOC interrupts).
- **TLB shootdown IPIs** broadcast to every cpu when ANY process modifies
  its page table (cpu 8: 1.5M TLB).
- **kworker / kthread / RCU callbacks** can be scheduled on cpu 8.
- The `cpuset` directive on the docker container (cpus 8-11) is
  permission-only; it doesn't reserve those cpus from the kernel.

The container's `cpuset 8-11` plus the trader's per-thread `pin_thread_to_cpu`
ensures *the trader's threads* land on the right cpus, but does nothing to
keep *other* kernel/user work off them.

## The change

Add to `GRUB_CMDLINE_LINUX_DEFAULT` in `/etc/default/grub`:

```
isolcpus=8-15 nohz_full=8-15 rcu_nocbs=8-15
```

What each flag does:

| flag | effect |
|---|---|
| `isolcpus=8-15` | Removes cpus 8-15 from the kernel scheduler's load balancer. Threads can still run there if pinned (via `sched_setaffinity` or systemd `CPUAffinity`), but the kernel will NOT migrate other work onto them. |
| `nohz_full=8-15` | Eliminates the periodic timer tick on cpus 8-15 when they're running a single thread. Saves the ~1000 LOC IRQs/sec/cpu and the µs of jitter each one introduces. Requires at least one "housekeeping" cpu (cpu 0 by default) for timekeeping, RCU grace periods, etc. |
| `rcu_nocbs=8-15` | Routes RCU callbacks off cpus 8-15 to "rcuop/X" kthreads on the housekeeping cpu. Without this, RCU callbacks can run on the isolated cpus and cause µs-ms stalls. |

After the reboot, cpu 8 should see only:
- Hard-pinned trader threads (corsair-hot)
- The occasional unavoidable IPI (TLB invalidation on shared mappings)
- Power-management interrupts (PMI/NMI ~30/min)

LOC interrupts should drop from ~26M over 40 min to near zero. Involuntary
context switches on `corsair-hot` should drop from ~8/sec to <0.1/sec.

## Pre-flight (read before applying)

### What else uses cpus 8-15 today?

```bash
# Check container affinities
docker inspect corsair-broker-rs corsair-trader-1 ib-gateway dashboard 2>/dev/null \
    | python3 -c "import json, sys; d=json.load(sys.stdin); [print(c['Name'], c['HostConfig'].get('CpusetCpus')) for c in d]"

# Check systemd services (e.g. the §23 crowsnest pinning)
for svc in crowsnest.slice; do
    systemctl --user show $svc -p AllowedCPUs 2>/dev/null
done

# Snapshot of what's running on cpus 8-15 right now
ps -eLo psr,comm | awk '$1 >= 8 && $1 <= 15' | sort | uniq -c | sort -rn | head
```

Anything currently relying on default-affinity that lands on cpus 8-15
will need explicit pinning AFTER the change. With `isolcpus`, default
affinity becomes 0-7,16-31; processes that need cpus 8-15 must request
them via `taskset` / `CPUAffinity` / `cpuset.cpus`.

The corsair containers already set `cpuset_cpus` explicitly in
docker-compose.yml — those continue to work because Docker calls
`sched_setaffinity` directly. Verify with:

```bash
PID=$(pgrep -f corsair_trader_rust | head -1)
grep Cpus_allowed_list /proc/$PID/task/*/status | sort -u
```

If you see anything OTHER than the explicit pins (8, 9, 10, 11), document
why before proceeding.

### Housekeeping cpu

The kernel needs at least one cpu OUTSIDE the `nohz_full` set for
timekeeping, watchdogs, and migration kthreads. With `nohz_full=8-15`,
cpus 0-7 and 16-31 are housekeeping. Do not extend `nohz_full` to cover
cpu 0.

### Hyperthreading sibling pairing

i9-14900K topology (P-cores 0-7 are SMT, E-cores 16-31 are not):
- P-core 4 = cpus 8 + 9 (cpu 9 is SMT sibling of 8)
- P-core 5 = cpus 10 + 11
- P-core 6 = cpus 12 + 13
- P-core 7 = cpus 14 + 15

Isolating 8-15 covers four full P-cores. The trader uses cpus 8, 10, 11
(hot, tokio, bg+jsonl) and reserves cpu 9 empty (SMT sibling of hot).
The broker uses cpus 12-15 (4 tokio workers + bg). Both fit cleanly
within the isolation set.

### What to reboot

Just the host. The corsair stack will come back up via `docker compose up`
on systemd boot.

## Apply

```bash
# 1. Back up the existing config
sudo cp /etc/default/grub /etc/default/grub.backup-$(date +%Y%m%d)

# 2. Edit GRUB_CMDLINE_LINUX_DEFAULT
sudo sed -i \
    's|^GRUB_CMDLINE_LINUX_DEFAULT=.*|GRUB_CMDLINE_LINUX_DEFAULT="quiet intel_idle.max_cstate=1 processor.max_cstate=1 isolcpus=8-15 nohz_full=8-15 rcu_nocbs=8-15"|' \
    /etc/default/grub

# 3. Verify the edit BEFORE regenerating grub.cfg
grep GRUB_CMDLINE_LINUX_DEFAULT /etc/default/grub

# 4. Regenerate grub.cfg
sudo update-grub

# 5. Reboot
sudo reboot
```

## Verify post-reboot

```bash
# 1. Cmdline reflects the change
cat /proc/cmdline
# Expect: ...isolcpus=8-15 nohz_full=8-15 rcu_nocbs=8-15

# 2. Sysfs confirms
cat /sys/devices/system/cpu/isolated         # expect: 8-15
cat /sys/devices/system/cpu/nohz_full        # expect: 8-15

# 3. Wait for the corsair stack to come back up (or `docker compose up -d`)
# 4. Trader thread placement still correct
PID=$(pgrep -f corsair_trader_rust | head -1)
for tid in $(ls /proc/$PID/task/); do
    name=$(cat /proc/$PID/task/$tid/comm 2>/dev/null)
    cpus=$(grep Cpus_allowed_list /proc/$PID/task/$tid/status | awk '{print $2}')
    echo "  $name  cpus=$cpus"
done

# 5. Run for 10+ minutes under live market conditions, then check
PID=$(pgrep -f corsair_trader_rust | head -1)
HOT_TID=$(grep -l '^corsair-hot$' /proc/$PID/task/*/comm | sed 's|.*/task/\([0-9]*\)/.*|\1|')
grep ctxt_switches /proc/$PID/task/$HOT_TID/status
# Expect involuntary < 1 per minute (was ~8/sec pre-fix)

# 6. LOC interrupts on cpu 8 should be near-static
awk 'NR==1{for(i=1;i<=NF;i++)if($i=="CPU8")c=i+1} /^LOC:/{print "cpu8 LOC=" $c}' /proc/interrupts
sleep 60
awk 'NR==1{for(i=1;i<=NF;i++)if($i=="CPU8")c=i+1} /^LOC:/{print "cpu8 LOC=" $c}' /proc/interrupts
# Expect delta near 0 (was ~60k/min pre-fix)

# 7. p99 measurement under live load
docker compose logs --tail=200 trader 2>&1 | grep ttt_p99_ns | tail -10
# Expect p99 in tens-of-µs range (was ms-range pre-fix)
```

## Rollback

If anything misbehaves (a workload couldn't pin to 8-15, a service is
unhappy, etc.):

```bash
sudo cp /etc/default/grub.backup-* /etc/default/grub
sudo update-grub
sudo reboot
```

Or temporarily without reboot, you can disable `nohz_full` for a single
boot by editing the kernel cmdline at the GRUB menu. `isolcpus` cannot
be disabled at runtime.

## Expected outcome

| metric | pre-isolation | post-isolation (projected) |
|---|---|---|
| corsair-hot involuntary ctx-switches | ~8/sec | <0.1/sec |
| cpu 8 LOC interrupts | ~1000/sec | ~0/sec when busy |
| `tick→trader_decide` p99 | 3.8ms | ~50-100µs |
| `tick→trader_decide` max | 80ms | a few ms (TLB IPI worst case) |
| `tick→trader_decide` p50 | 5.5µs | unchanged (5-6µs) |

p50 doesn't move because the steady-state hot path was never the issue.
The win is entirely in the tail.

## Why this is now a credible win and not just §21 redux

§21 (CLAUDE.md) concluded "stop chasing local-path code optimization at the
current scale" because sub-µs Rust changes don't extract from measurement
noise. That verdict still holds. THIS change is at a different layer:
the kernel scheduler. The 80ms tail isn't a code-path latency; it's the
kernel context-switching the hot thread out for tens of milliseconds. No
amount of Rust micro-optimization fixes it. The fix is at the kernel
scheduler boundary, and `isolcpus + nohz_full + rcu_nocbs` is the
canonical lever for that.
