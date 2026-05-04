//! CPU pinning helpers for tokio worker threads.
//!
//! Both broker and trader use a multi-thread tokio runtime; without
//! pinning, the kernel scheduler is free to migrate worker threads
//! between cores, which costs L1/L2 cache reload + TLB flush on each
//! migration. At our latency budget (TTT p50 ~50 µs, target colo p99
//! ~100 µs), one migration adds 10-50 µs of jitter — meaningful tail.
//!
//! Pinning is done at thread spawn via Builder::on_thread_start.
//! We round-robin worker threads across the cores allowed by the
//! current cpuset (sched_getaffinity), so containers with cpuset
//! constraints get respected automatically — no need to hardcode
//! cores per host.

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
/// Picks the first `desired_workers` CPUs from the allowed set
/// (clamped to the actual size of the allowed set so we never pin
/// to a CPU outside cpuset). Returns a tuple of:
///   - the worker count to pass to `worker_threads(...)`
///   - a closure to pass to `on_thread_start(...)` that round-robin
///     assigns each spawned worker thread to one of the picked CPUs.
///
/// Logs at info level so operators can confirm the pinning at boot.
pub fn build_pinner(
    role: &'static str,
    desired_workers: usize,
) -> (usize, impl Fn() + Send + Sync + 'static + Clone) {
    let allowed = allowed_cpus();
    let workers = desired_workers.min(allowed.len().max(1));
    // Pick a contiguous slice of the allowed set; gives stable
    // pinning across restarts and matches docker cpuset ordering.
    let pins: Vec<usize> = if allowed.is_empty() {
        // Pathological — sched_getaffinity failed. Don't pin.
        Vec::new()
    } else {
        allowed.into_iter().take(workers).collect()
    };
    log::warn!(
        "{}: tokio runtime — {} worker(s), pin cpus = {:?}",
        role,
        workers,
        pins
    );
    let counter = Arc::new(AtomicUsize::new(0));
    let pins_arc = Arc::new(pins);
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
    (workers, closure)
}
