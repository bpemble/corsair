//! CPU pinning helpers for tokio worker threads.
//!
//! Both broker and trader use a multi-thread tokio runtime; without
//! pinning, the kernel scheduler is free to migrate worker threads
//! between cores, which costs L1/L2 cache reload + TLB flush on each
//! migration. At our latency budget (TTT p50 ~50 µs, target colo p99
//! ~100 µs), one migration adds 10-50 µs of jitter — meaningful tail.
//!
//! Pinning is done at thread spawn via Builder::on_thread_start.
//! Workers are assigned to CPUs by `build_pinner`, which prefers to
//! spread the first N workers across distinct physical cores before
//! filling SMT siblings. On hybrid Intel parts (Alder/Raptor Lake)
//! SMT siblings are interleaved (CPU 0,1 on the same physical core),
//! so naive sequential picks would put both tokio workers on the
//! same core and they'd fight for execution units. The SMT-aware
//! algorithm below avoids that on hybrid layouts and is a no-op on
//! traditional Intel layouts where SMT siblings are far apart.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Read the current process's CPU affinity mask. Returns the list of
/// logical CPU IDs the process is allowed to run on. Honors docker
/// cpuset and Linux cpusets.
pub fn allowed_cpus() -> Vec<usize> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        let pid = 0; // current process
        let r = libc::sched_getaffinity(
            pid,
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut set,
        );
        if r != 0 {
            return Vec::new();
        }
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&i| libc::CPU_ISSET(i, &set))
            .collect()
    }
}

/// Read the SMT thread sibling of `cpu` from sysfs. Returns None if
/// sysfs isn't readable or the topology is degenerate (single-thread
/// physical core has only itself as sibling). The lowest-numbered
/// CPU in the sibling pair is the "physical core id" we use for
/// uniqueness checks in build_pinner.
pub fn physical_core_id(cpu: usize) -> usize {
    let path = format!(
        "/sys/devices/system/cpu/cpu{}/topology/thread_siblings_list",
        cpu
    );
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return cpu;
    };
    // Format is comma-separated CPUs and/or hyphenated ranges,
    // e.g. "0,1" or "0-1". Take the smallest CPU id in the list.
    let mut min_id = cpu;
    for part in contents.trim().split(',') {
        if let Some((lo, _hi)) = part.split_once('-') {
            if let Ok(n) = lo.trim().parse::<usize>() {
                if n < min_id {
                    min_id = n;
                }
            }
        } else if let Ok(n) = part.trim().parse::<usize>() {
            if n < min_id {
                min_id = n;
            }
        }
    }
    min_id
}

/// Pin the calling thread to a single logical CPU. Logs a warning
/// if the syscall fails (caller continues unpinned).
pub fn pin_thread_to_cpu(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        let pid = 0; // current thread
        let r = libc::sched_setaffinity(
            pid,
            std::mem::size_of::<libc::cpu_set_t>(),
            &set,
        );
        if r != 0 {
            log::warn!(
                "sched_setaffinity(cpu={}) failed: {}",
                cpu,
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Build a round-robin pinner closure suitable for
/// `tokio::runtime::Builder::on_thread_start`.
///
/// Returns a tuple of:
///   - the worker count to pass to `worker_threads(...)`
///   - the chosen pin list (CPU id per spawned worker, in order)
///   - a closure to pass to `on_thread_start(...)` that round-robin
///     assigns each spawned worker thread to one of the picked CPUs.
///
/// The pin selection is SMT-aware: pass 1 picks one CPU per distinct
/// physical core in cpuset order; pass 2 fills any remaining slots
/// with leftover SMT siblings. With cpuset `8,9,10,11` on a hybrid
/// 13900K (siblings 8↔9, 10↔11) and 2 workers, this yields pins
/// `[8, 10]` — workers on different physical cores. With cpuset
/// `4,5,10,11` on Comet Lake (siblings 4↔10, 5↔11) and 2 workers
/// the result is `[4, 5]`, identical to the prior naive behavior.
pub fn build_pinner(
    role: &'static str,
    desired_workers: usize,
) -> (usize, Vec<usize>, impl Fn() + Send + Sync + 'static + Clone) {
    let allowed = allowed_cpus();
    let workers = desired_workers.min(allowed.len().max(1));

    let pins: Vec<usize> = if allowed.is_empty() {
        // Pathological — sched_getaffinity failed. Don't pin.
        Vec::new()
    } else {
        let mut pins: Vec<usize> = Vec::with_capacity(workers);
        let mut seen_cores: Vec<usize> = Vec::with_capacity(workers);
        // Pass 1: one CPU per distinct physical core, in cpuset order.
        for &cpu in &allowed {
            if pins.len() >= workers {
                break;
            }
            let core = physical_core_id(cpu);
            if !seen_cores.contains(&core) {
                seen_cores.push(core);
                pins.push(cpu);
            }
        }
        // Pass 2: fill remaining slots with leftover SMT siblings.
        if pins.len() < workers {
            for &cpu in &allowed {
                if pins.len() >= workers {
                    break;
                }
                if !pins.contains(&cpu) {
                    pins.push(cpu);
                }
            }
        }
        pins
    };
    log::warn!(
        "{}: tokio runtime — {} worker(s), pin cpus = {:?} (allowed = {:?})",
        role,
        workers,
        pins,
        allowed
    );
    let counter = Arc::new(AtomicUsize::new(0));
    let pins_arc = Arc::new(pins.clone());
    let closure = move || {
        if pins_arc.is_empty() {
            return;
        }
        let i = counter.fetch_add(1, Ordering::Relaxed);
        let cpu = pins_arc[i % pins_arc.len()];
        pin_thread_to_cpu(cpu);
        log::info!(
            "{}: worker thread #{} pinned to cpu {}",
            role,
            i,
            cpu
        );
    };
    (workers, pins, closure)
}
