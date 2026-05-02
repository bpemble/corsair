//! End-to-end SHM ring + FIFO round-trip latency benchmark.
//!
//! Spawns a trader-impersonator thread that reads frames off the events
//! ring and immediately echoes them onto the commands ring. The main
//! thread acts as the broker: writes a frame, waits for the echo,
//! records the wall-clock RTT.
//!
//! RTT here = "broker → trader IPC" + "trader → broker IPC" — the two
//! IPC hops in the wire-to-wire path. Compare against the harness's
//! hardcoded estimates: 80 µs (FIFO) / 15 µs (busy-poll) per hop.
//!
//! Build:
//!   cargo build --release --package corsair_ipc --example rt_bench
//! Run (pinned to mimic broker+trader cpusets):
//!   taskset -c 2,3,4,5,8,9,10,11 ./target/release/examples/rt_bench
use corsair_ipc::ring::Ring;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

/// Pin the calling thread to a single logical CPU. Required because
/// `taskset` only sets the process-wide affinity mask — with multiple
/// threads on a multi-CPU mask, the scheduler can co-schedule both onto
/// one core, defeating the cross-core IPC measurement we want.
fn pin_to_cpu(cpu: usize) {
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
            eprintln!("warn: sched_setaffinity({cpu}) failed: {}", std::io::Error::last_os_error());
        }
    }
}

const PAYLOAD_BYTES: usize = 64;
const N_WARMUP: usize = 2_000;
const N_ITERS: usize = 50_000;

fn percentiles(samples: &mut [u64]) -> (u64, u64, u64, u64, u64, u64, u64) {
    samples.sort_unstable();
    let p = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    (
        samples[0],
        p(0.50),
        p(0.90),
        p(0.95),
        p(0.99),
        p(0.999),
        *samples.last().unwrap(),
    )
}

fn run_bench(mode: &str, broker_cpu: usize, trader_cpu: usize) {
    pin_to_cpu(broker_cpu);
    let tmp = PathBuf::from("/tmp/corsair_rt_bench");
    let _ = std::fs::create_dir_all(&tmp);
    let evt_path = tmp.join("events");
    let cmd_path = tmp.join("commands");
    let evt_notify = tmp.join("events.notify");
    let cmd_notify = tmp.join("commands.notify");
    for p in [&evt_path, &cmd_path] {
        let _ = std::fs::remove_file(p);
    }

    // Owner side (broker impersonator).
    let mut evt_w = Ring::create_owner(&evt_path, 1 << 20).unwrap();
    let mut cmd_r = Ring::create_owner(&cmd_path, 1 << 20).unwrap();
    Ring::create_notify_fifo(&evt_notify).unwrap();
    Ring::create_notify_fifo(&cmd_notify).unwrap();
    evt_w.open_notify(&evt_notify, true).unwrap();
    cmd_r.open_notify(&cmd_notify, false).unwrap();

    // Client side (trader impersonator).
    let mut evt_r = Ring::open_client(&evt_path, 1 << 20).unwrap();
    let mut cmd_w = Ring::open_client(&cmd_path, 1 << 20).unwrap();
    evt_r.open_notify(&evt_notify, false).unwrap();
    cmd_w.open_notify(&cmd_notify, true).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let mode_t = mode.to_string();

    // Trader thread: echo events → commands.
    let handle = thread::spawn(move || {
        pin_to_cpu(trader_cpu);
        let evt_fd = evt_r.notify_r_fd().expect("evt fifo");
        loop {
            if stop_t.load(Ordering::Relaxed) {
                break;
            }
            let chunk = evt_r.read_available();
            if !chunk.is_empty() {
                evt_r.drain_notify();
                cmd_w.write_frame(&chunk);
                continue;
            }
            if mode_t == "fifo" {
                let mut buf = [0u8; 64];
                unsafe {
                    libc::read(evt_fd, buf.as_mut_ptr() as *mut _, buf.len());
                }
            } else {
                std::hint::spin_loop();
            }
        }
    });

    let payload = vec![0xABu8; PAYLOAD_BYTES];
    let cmd_fd = cmd_r.notify_r_fd().unwrap();

    let wait_for_echo = |cmd_r: &mut Ring, mode: &str| {
        loop {
            let chunk = cmd_r.read_available();
            if !chunk.is_empty() {
                cmd_r.drain_notify();
                return;
            }
            if mode == "busy" {
                std::hint::spin_loop();
            } else {
                let mut buf = [0u8; 64];
                unsafe {
                    libc::read(cmd_fd, buf.as_mut_ptr() as *mut _, buf.len());
                }
            }
        }
    };

    // Warmup.
    for _ in 0..N_WARMUP {
        evt_w.write_frame(&payload);
        wait_for_echo(&mut cmd_r, mode);
    }

    // Measure.
    let mut samples_ns = Vec::with_capacity(N_ITERS);
    for _ in 0..N_ITERS {
        let t0 = Instant::now();
        evt_w.write_frame(&payload);
        wait_for_echo(&mut cmd_r, mode);
        samples_ns.push(t0.elapsed().as_nanos() as u64);
    }

    stop.store(true, Ordering::Relaxed);
    evt_w.write_frame(&payload); // wake trader thread
    handle.join().unwrap();

    let (mn, p50, p90, p95, p99, p999, mx) = percentiles(&mut samples_ns);
    println!(
        "mode={:5}  iters={}  payload={}B  broker_cpu={} trader_cpu={}",
        mode, N_ITERS, PAYLOAD_BYTES, broker_cpu, trader_cpu
    );
    println!(
        "  RTT (µs): min={:.2}  p50={:.2}  p90={:.2}  p95={:.2}  p99={:.2}  p99.9={:.2}  max={:.2}",
        mn as f64 / 1000.0,
        p50 as f64 / 1000.0,
        p90 as f64 / 1000.0,
        p95 as f64 / 1000.0,
        p99 as f64 / 1000.0,
        p999 as f64 / 1000.0,
        mx as f64 / 1000.0,
    );
    println!(
        "  per-hop p50 (RTT/2): {:.2} µs",
        (p50 as f64 / 1000.0) / 2.0
    );
}

fn main() {
    println!("=== corsair_ipc round-trip latency bench ===");
    println!("RTT = broker → trader IPC + trader → broker IPC (two SHM hops)");
    println!();
    // Mimic production layout: broker on phys core 2, trader on phys
    // core 4 (different SMT pairs, both on isolated cores).
    let broker_cpu = std::env::var("RT_BROKER_CPU").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(2);
    let trader_cpu = std::env::var("RT_TRADER_CPU").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(4);
    run_bench("fifo", broker_cpu, trader_cpu);
    println!();
    run_bench("busy", broker_cpu, trader_cpu);
}
