//! Corsair trader binary — Rust port of src/trader/main.py.
//!
//! Connects to broker via SHM IPC (events ring + commands ring +
//! notification FIFOs), processes ticks, makes quote decisions, and
//! sends place_order / cancel_order commands.
//!
//! Hot path is single-threaded; tokio is used for concurrent
//! background tasks (telemetry, staleness loop, FIFO read polling).
//! State sharding (Priority 1, 2026-05-04) puts the heavy maps
//! behind DashMap and the histograms/scalars behind their own
//! parking_lot::Mutex so the bg tasks don't block the hot loop on
//! a single big mutex; see state.rs for the layout rationale.
//!
//! Env vars:
//!   CORSAIR_TRADER_PLACES_ORDERS=1    actually send place_orders
//!   CORSAIR_IPC_TRANSPORT=shm         must be shm (we don't support socket)
//!   CORSAIR_LOG_LEVEL=info|debug|...

mod decision;
mod ipc;
mod jsonl;
mod messages;
mod msgpack_decode;
mod msgpack_encode;
mod pricing;
mod state;
mod tte_cache;
mod types;

// p99-4 (2026-05-04): mimalloc as global allocator. The hot path
// allocates on tick decode (rmp_serde) and on Decision/CancelOrder
// frame construction. mimalloc has tighter tail latency than glibc
// malloc — typical 100-300µs reduction at p99 for allocation-heavy
// hot paths. No code changes elsewhere required; the registration
// below is the entire integration.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// parking_lot::Mutex for the hot-path ring locks. Faster uncontended
// path than std::sync::Mutex, infallible lock() (no Result), and the
// same poisoning-free semantics we already use for histograms/scalars.
use parking_lot::Mutex;

use crate::decision::{decide_on_tick, Decision};
use crate::ipc::shm::{wait_for_rings, Ring, DEFAULT_RING_CAPACITY};
use crate::jsonl::{DecisionInner, DecisionLog, LogPayload};
use crate::messages::*;
use crate::state::{DecisionCounters, OurOrder, OurOrderKey, SharedState};

const VERSION: &str = "rust-v1";

fn now_ns_wall() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn now_ns_monotonic() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    Instant::now().duration_since(start).as_nanos() as u64
}

fn places_orders() -> bool {
    std::env::var("CORSAIR_TRADER_PLACES_ORDERS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Lock all current and future memory pages so the hot path never
/// page-faults. mimalloc allocations after this point are guaranteed
/// resident (RLIMIT_MEMLOCK permitting). Logs and continues on failure
/// — non-fatal so we still run on hosts where MEMLOCK is too small
/// (the page-fault risk just remains as a tail-latency outlier).
///
/// Audit Phase 1 #6 (2026-05-05). Container needs `ulimits.memlock`
/// raised in docker-compose.yml or this errors with EPERM/ENOMEM.
fn lock_all_memory() {
    unsafe {
        let r = libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE);
        if r != 0 {
            let e = std::io::Error::last_os_error();
            eprintln!(
                "mlockall failed (errno={:?}); continuing without page-fault \
                 protection. Raise docker-compose ulimits.memlock to fix.",
                e.raw_os_error()
            );
        } else {
            eprintln!("mlockall MCL_CURRENT|MCL_FUTURE succeeded");
        }
    }
}

fn main() -> std::io::Result<()> {
    // Lock memory before logger init / runtime build so future
    // allocations from those subsystems are also resident. Pre-logger
    // print uses raw eprintln since env_logger isn't up yet.
    lock_all_memory();

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // Lever 1 (2026-05-06): in busy_poll mode the hot loop runs on a
    // dedicated `std::thread` pinned to cpu 8 (first allowed). Tokio
    // workers are pinned to the REMAINING cpus so they don't share a
    // physical core with the hot thread. Default workers drops to 1
    // since main_rt's only future is `pending::<()>().await`; bg work
    // (telemetry, staleness, signal handlers) lives on bg_rt.
    //
    // FIFO mode (busy_poll=0) keeps the legacy 2-worker layout —
    // worker 0 owns the FIFO event loop on cpu 8, worker 1 helps on
    // cpu 10.
    let busy_poll_main = std::env::var("CORSAIR_TRADER_BUSY_POLL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let allowed_cpus_main = corsair_ipc::cpu_affinity::allowed_cpus();
    let main_rt = if busy_poll_main {
        let hot_cpu = allowed_cpus_main.first().copied().unwrap_or(0);
        // Tokio worker MUST land on a different physical core than the
        // hot std::thread — otherwise it shares execution units with
        // the SMT sibling. Bug 2026-05-06: previously picked "first cpu
        // != hot_cpu" which selected cpu 9 (SMT sibling of cpu 8) on
        // hybrid Intel parts, causing 6 ms TTT p99 outliers from
        // pipeline contention. Skip cpus that share a physical core
        // with hot_cpu.
        let hot_core = corsair_ipc::cpu_affinity::physical_core_id(hot_cpu);
        let tokio_cpu = allowed_cpus_main
            .iter()
            .find(|&&c| {
                c != hot_cpu
                    && corsair_ipc::cpu_affinity::physical_core_id(c) != hot_core
            })
            .copied()
            .unwrap_or_else(|| {
                allowed_cpus_main
                    .iter()
                    .find(|&&c| c != hot_cpu)
                    .copied()
                    .unwrap_or(hot_cpu)
            });
        let workers = std::env::var("CORSAIR_TRADER_WORKERS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1);
        log::warn!(
            "busy_poll: tokio main_rt — {} worker(s) pinned to cpu {} \
             (hot std::thread reserves cpu {} on physical core {})",
            workers,
            tokio_cpu,
            hot_cpu,
            hot_core
        );
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .on_thread_start(move || {
                corsair_ipc::cpu_affinity::pin_thread_to_cpu(tokio_cpu);
                log::info!("busy_poll: tokio worker pinned to cpu {}", tokio_cpu);
            })
            .build()?
    } else {
        // FIFO-mode: 2 workers via build_pinner.
        let desired = std::env::var("CORSAIR_TRADER_WORKERS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(2);
        let (workers, _pins, pinner) =
            corsair_ipc::cpu_affinity::build_pinner("corsair_trader", desired);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .on_thread_start(pinner)
            .build()?
    };

    // Background runtime — staleness sweep (10Hz) + telemetry (10s)
    // live here, off the hot loop's cores. Single-thread current-
    // thread runtime on a dedicated std thread, pinned to the first
    // CPU in the cpuset that ISN'T already a worker pin. On hybrid
    // Intel parts the worker pins are SMT-spread across distinct
    // physical cores (see build_pinner), so the bg thread typically
    // lands on the SMT sibling of one of the worker cores — shared
    // L1/L2, low cross-core wakeup cost. With map locks now sharded
    // (Priority 1, 2026-05-04) the bg task's brief contention with
    // the hot loop is per-shard rather than against a single big
    // mutex.
    let allowed = corsair_ipc::cpu_affinity::allowed_cpus();
    // Bg thread placement: reverse-iter so the first eligible cpu is
    // the LAST allowed (e.g. cpu 11 with cpuset 8,9,10,11 + hot on 8,
    // tokio worker on 10). Lever 1 placement (busy_poll=1):
    //   cpu 8: hot std::thread (active, busy-spinning)
    //   cpu 9: idle (SMT sibling of cpu 8 — kept clear of bg work)
    //   cpu 10: tokio worker (parked on pending future)
    //   cpu 11: bg_rt (telemetry/staleness/signal handlers)
    // FIFO mode preserves the legacy `pins` exclusion so bg lands on
    // a cpu not used by tokio workers.
    let bg_cpu = if busy_poll_main {
        // bg → cpu 11 typical (LAST allowed, SMT sibling of tokio cpu 10)
        let hot_cpu = allowed_cpus_main.first().copied();
        let tokio_cpu = allowed_cpus_main
            .iter()
            .find(|&&c| Some(c) != hot_cpu)
            .copied();
        allowed
            .iter()
            .rev()
            .find(|c| Some(**c) != hot_cpu && Some(**c) != tokio_cpu)
            .copied()
    } else {
        // FIFO mode: pick last cpu not used by tokio workers. Recompute
        // pins via build_pinner (cheap; allocs a small Vec).
        let desired = std::env::var("CORSAIR_TRADER_WORKERS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(2);
        let (_w, pins, _p) = corsair_ipc::cpu_affinity::build_pinner("corsair_trader_bg_pin", desired);
        allowed.iter().rev().find(|c| !pins.contains(c)).copied()
    };
    let bg_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let bg_handle = bg_rt.handle().clone();
    let _bg_thread = std::thread::Builder::new()
        .name("corsair-bg".into())
        .spawn(move || {
            if let Some(cpu) = bg_cpu {
                log::warn!("corsair_trader: bg runtime pinning to cpu {}", cpu);
                corsair_ipc::cpu_affinity::pin_thread_to_cpu(cpu);
            } else {
                log::warn!(
                    "corsair_trader: bg runtime — no spare cpu past workers, sharing"
                );
            }
            bg_rt.block_on(std::future::pending::<()>());
        })?;

    main_rt.block_on(async_main(bg_handle))
}

async fn async_main(bg_handle: tokio::runtime::Handle) -> std::io::Result<()> {
    log::warn!("corsair_trader (Rust) {} starting", VERSION);

    let transport = std::env::var("CORSAIR_IPC_TRANSPORT")
        .unwrap_or_else(|_| "shm".into());
    if transport != "shm" {
        log::error!(
            "corsair_trader (Rust) only supports CORSAIR_IPC_TRANSPORT=shm, got {}",
            transport
        );
        std::process::exit(2);
    }

    let base = std::env::var("CORSAIR_IPC_BASE")
        .unwrap_or_else(|_| "/app/data/corsair_ipc".into());
    let events_path = PathBuf::from(format!("{}.events", base));
    let commands_path = PathBuf::from(format!("{}.commands", base));
    let events_notify_path = PathBuf::from(format!("{}.events.notify", base));
    let commands_notify_path = PathBuf::from(format!("{}.commands.notify", base));

    log::info!("Waiting for broker SHM rings at {}...", base);
    wait_for_rings(&base).await?;
    log::warn!("SHM rings present; opening");

    let mut events_ring = Ring::open(&events_path, DEFAULT_RING_CAPACITY)?;
    let mut commands_ring = Ring::open(&commands_path, DEFAULT_RING_CAPACITY)?;
    // Wait for FIFO files (broker creates them when it opens rings).
    for _ in 0..50 {
        if events_notify_path.exists() && commands_notify_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    events_ring.open_notify(&events_notify_path, /* as_writer */ false)?;
    commands_ring.open_notify(&commands_notify_path, /* as_writer */ true)?;
    log::warn!("SHM client connected to {} (notify-fifo enabled)", base);

    // JSONL writers — background threads, hot path enqueues only.
    // Pin them to the bg cpu (cpu 11 on production) so they share a
    // physical core with the bg runtime rather than drifting onto
    // cpu 9 (SMT sibling of hot cpu 8). Disk-IO + JSON serialization
    // have rare bursts of activity that would steal pipeline cycles
    // from the hot thread if co-located on the same physical core.
    let log_dir = std::env::var("CORSAIR_LOGS_DIR")
        .unwrap_or_else(|_| "/app/logs-paper".into());
    let jsonl_pin = corsair_ipc::cpu_affinity::allowed_cpus()
        .iter()
        .rev()
        .next()
        .copied();
    let events_log = jsonl::JsonlWriter::start(
        std::path::PathBuf::from(&log_dir),
        "trader_events",
        jsonl_pin,
    );
    let decisions_log = jsonl::JsonlWriter::start(
        std::path::PathBuf::from(&log_dir),
        "trader_decisions",
        jsonl_pin,
    );
    let events_log = Arc::new(events_log);
    let decisions_log = Arc::new(decisions_log);

    // Send welcome.
    let welcome = Welcome {
        msg_type: "welcome",
        ts_ns: now_ns_wall(),
        trader_version: VERSION,
    };
    let body = rmp_serde::to_vec_named(&welcome).expect("welcome encode");
    let frame = ipc::protocol::pack_frame(&body);
    if !commands_ring.write_frame(&frame) {
        log::error!("welcome write_frame failed (commands ring full?)");
    }

    // Lock-sharded shared state. SharedState fields use interior
    // mutability (DashMap, parking_lot::Mutex) so callers can take
    // shared `&SharedState` references; no top-level mutex needed.
    // Counters use AtomicU64 fields — wait-free fetch_add per gate
    // increment, no mutex acquire.
    //
    // Bundle 4I (2026-05-06): events_ring stays unwrapped (no
    // Arc<Mutex<>>) — only the hot loop reads it; bg tasks never
    // touch it. Saves one parking_lot::Mutex acquire per inbound
    // tick. commands_ring keeps the Arc<Mutex<>> wrapper because it
    // has multiple writers (hot loop, staleness, telemetry, shutdown).
    let state = Arc::new(SharedState::new());
    let counters = Arc::new(DecisionCounters::default());
    let commands_ring = Arc::new(Mutex::new(commands_ring));

    // Graceful shutdown: SIGTERM / SIGINT → cancel every resting
    // order at IBKR, then exit. Without this, `docker compose stop
    // trader` killed the container while orders sat at IBKR for up
    // to gtd_lifetime_s seconds — any market cross during that window
    // would fill orphans and rebuild the cascade pattern we just
    // spent the day fixing. Spawned on the bg runtime so the hot
    // path stays untouched.
    {
        let state_sd = Arc::clone(&state);
        let commands_ring_sd = Arc::clone(&commands_ring);
        bg_handle.spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("graceful_shutdown: SIGTERM handler failed: {e}");
                    return;
                }
            };
            let mut sigint = match signal(SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("graceful_shutdown: SIGINT handler failed: {e}");
                    return;
                }
            };
            tokio::select! {
                _ = sigterm.recv() => log::warn!("graceful_shutdown: SIGTERM"),
                _ = sigint.recv() => log::warn!("graceful_shutdown: SIGINT"),
            }
            // Latency-harness hook: if `CORSAIR_TRADER_HIST_DUMP_PATH`
            // is set, dump the in-memory ipc_us / ttt_us samples to a
            // JSON file before doing any cancel work. The
            // corsair_tick_replay orchestrator reads this file as the
            // raw sample input for KS/bootstrap A/B comparison.
            if let Ok(path) = std::env::var("CORSAIR_TRADER_HIST_DUMP_PATH") {
                let h = state_sd.histograms.lock();
                let dump = serde_json::json!({
                    "ipc_us": h.ipc_us.iter().copied().collect::<Vec<_>>(),
                    "ttt_us": h.ttt_us.iter().copied().collect::<Vec<_>>(),
                });
                drop(h);
                match std::fs::write(&path, dump.to_string()) {
                    Ok(()) => log::warn!(
                        "graceful_shutdown: histogram dumped to {}",
                        path
                    ),
                    Err(e) => log::error!(
                        "graceful_shutdown: histogram dump to {} failed: {}",
                        path,
                        e
                    ),
                }
            }
            // Snapshot all live order_ids and emit cancel frames.
            // DashMap iter holds shard read guards; we drop them by
            // collecting eagerly so any concurrent place_ack flood
            // can't keep us alive.
            let order_ids: Vec<i64> = state_sd
                .orderid_to_key
                .iter()
                .map(|e| *e.key())
                .collect();
            log::warn!(
                "graceful_shutdown: cancelling {} resting orders before exit",
                order_ids.len()
            );
            let send_ts = now_ns_wall();
            let frames: Vec<Vec<u8>> = order_ids
                .into_iter()
                .filter_map(|oid| {
                    let cancel = CancelOrder {
                        msg_type: "cancel_order",
                        ts_ns: send_ts,
                        order_id: oid,
                    };
                    rmp_serde::to_vec_named(&cancel)
                        .ok()
                        .map(|body| ipc::protocol::pack_frame(&body))
                })
                .collect();
            {
                let mut ring = commands_ring_sd.lock();
                for frame in &frames {
                    ring.write_frame(frame);
                }
            }
            // Brief delay so the cancels actually clear our SHM ring
            // and reach the broker before we exit. Broker forwards to
            // IBKR; once on the wire they're authoritative regardless
            // of whether we're alive.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            log::warn!("graceful_shutdown: cancels flushed, exiting");
            std::process::exit(0);
        });
    }

    // Staleness loop — cancels resting orders whose price has drifted
    // too far from current theo. Mirrors src/trader/main.py's
    // staleness_loop. Without it, an order placed at theo-edge can sit
    // through a theo move and become uncompetitive (or worse, become
    // adverse). Runs at 10Hz (matches Python's STALENESS_INTERVAL_S).
    // Spawned on the bg runtime so its 10Hz wake-up doesn't preempt
    // the hot loop on the main runtime's pinned cores.
    {
        let state = Arc::clone(&state);
        let counters = Arc::clone(&counters);
        let commands_ring = Arc::clone(&commands_ring);
        bg_handle.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if !places_orders() {
                    continue;
                }
                staleness_check(&state, &counters, &commands_ring);
            }
        });
    }

    // Spawn telemetry loop (10s cadence). Also on bg runtime.
    {
        let state = Arc::clone(&state);
        let counters = Arc::clone(&counters);
        let commands_ring = Arc::clone(&commands_ring);
        bg_handle.spawn(async move {
            let mut total_events: u64 = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let (tel, n_events) = build_telemetry(&state, &counters, &commands_ring, &mut total_events);
                let body = match rmp_serde::to_vec_named(&tel) {
                    Ok(b) => b,
                    Err(e) => {
                        log::warn!("telemetry encode failed: {}", e);
                        continue;
                    }
                };
                let frame = ipc::protocol::pack_frame(&body);
                let mut ring = commands_ring.lock();
                ring.write_frame(&frame);
                drop(ring);
                log::info!(
                    "telemetry: events={} ipc_p50={:?} ipc_p99={:?} ttt_p50={:?} ttt_p99={:?} \
                     opts={} orders={} hedge={:?} cmd_drops={} decisions={}",
                    n_events,
                    tel.ipc_p50_us,
                    tel.ipc_p99_us,
                    tel.ttt_p50_us,
                    tel.ttt_p99_us,
                    tel.n_options,
                    tel.n_active_orders,
                    tel.risk_hedge_delta,
                    tel.commands_frames_dropped,
                    tel.decisions,
                );
            }
        });
    }

    // Mode select: busy-poll (CORSAIR_TRADER_BUSY_POLL=1) trades 1
    // CPU core for ~50-100µs latency reduction by skipping the FIFO
    // wakeup path entirely. Default OFF — operators opt in.
    let busy_poll = std::env::var("CORSAIR_TRADER_BUSY_POLL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if busy_poll {
        // Lever 1 (2026-05-06): hot loop runs on a dedicated
        // `std::thread` outside the tokio scheduler. Eliminates the
        // ~24% of cycles previously spent in tokio park/wake/defer
        // visible in perf — the hot path no longer hits the scheduler
        // even on empty-ring iterations (replaced `yield_now` with
        // `std::hint::spin_loop()`).
        let allowed = corsair_ipc::cpu_affinity::allowed_cpus();
        let hot_cpu = allowed.first().copied();
        let state_h = Arc::clone(&state);
        let counters_h = Arc::clone(&counters);
        let commands_ring_h = Arc::clone(&commands_ring);
        let events_log_h = Arc::clone(&events_log);
        let decisions_log_h = Arc::clone(&decisions_log);
        std::thread::Builder::new()
            .name("corsair-hot".into())
            .spawn(move || {
                hot_loop_blocking(
                    events_ring,
                    state_h,
                    counters_h,
                    commands_ring_h,
                    events_log_h,
                    decisions_log_h,
                    hot_cpu,
                );
            })
            .expect("spawn corsair-hot thread");
        log::warn!(
            "CORSAIR_TRADER_BUSY_POLL=1 — hot loop on dedicated \
             std::thread, pinned to cpu {:?} (Lever 1)",
            hot_cpu
        );
        // Block the tokio runtime so bg tasks (telemetry, staleness,
        // graceful_shutdown signal handler) keep running. Hot thread
        // is detached and runs until process exit. Graceful_shutdown
        // calls std::process::exit on signal, which kills the hot
        // thread along with the rest of the process.
        std::future::pending::<()>().await;
        Ok(())
    } else {
        // FIFO-notify mode: block on the events FIFO; wake when broker
        // writes a notification byte. Lower CPU; ~50-100µs worst-case
        // wakeup latency from FIFO + scheduler. Stays on tokio for
        // back-compat — Lever 1 didn't refactor this path because
        // production runs busy_poll=1 and the FIFO path is dev-only.
        use tokio::io::unix::AsyncFd;
        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        let evt_fifo_fd = events_ring.notify_r_fd().expect("events fifo");
        let evt_async_fd = AsyncFd::new(EvtFd(evt_fifo_fd))?;

        loop {
            // Drain ring (zero-copy via Lever 2).
            let n = events_ring.read_available_into(&mut buf);
            if n > 0 {
                let frames = ipc::protocol::unpack_all_frames(&mut buf)?;
                for body in frames {
                    // body is owned Vec<u8> — moved into process_event.
                    process_event(
                        &state, &counters, &commands_ring,
                        body, &events_log, &decisions_log,
                    );
                }
                continue;
            }
            events_ring.drain_notify();
            match tokio::time::timeout(
                Duration::from_millis(100),
                evt_async_fd.readable(),
            )
            .await
            {
                Ok(Ok(mut guard)) => {
                    guard.clear_ready();
                }
                _ => {
                    // timeout or error — fall through to re-read
                }
            }
        }
    }
}

/// Lever 1 (2026-05-06): trader hot loop on a dedicated `std::thread`.
/// Pinned to `hot_cpu` (typically cpu 8 on production cpuset 8-11).
/// Idles via `std::hint::spin_loop()` — no kernel transition, no
/// scheduler hop. Uses Lever 2's `read_available_into` for zero-alloc
/// drain. Runs forever until process exit.
fn hot_loop_blocking(
    mut events_ring: Ring,
    state: Arc<SharedState>,
    counters: Arc<DecisionCounters>,
    commands_ring: Arc<Mutex<Ring>>,
    events_log: Arc<jsonl::JsonlWriter>,
    decisions_log: Arc<jsonl::JsonlWriter>,
    hot_cpu: Option<usize>,
) {
    if let Some(cpu) = hot_cpu {
        log::warn!("corsair-hot: pinning to cpu {}", cpu);
        corsair_ipc::cpu_affinity::pin_thread_to_cpu(cpu);
    }
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    loop {
        let n = events_ring.read_available_into(&mut buf);
        if n > 0 {
            // Errors here are schema drift; we log + skip the whole
            // drain rather than killing the process.
            let frames = match ipc::protocol::unpack_all_frames(&mut buf) {
                Ok(f) => f,
                Err(e) => {
                    log::error!("corsair-hot: unpack error, skipping drain: {}", e);
                    continue;
                }
            };
            for body in frames {
                process_event(
                    &state,
                    &counters,
                    &commands_ring,
                    body,
                    &events_log,
                    &decisions_log,
                );
            }
            continue;
        }
        // Empty ring — pause hint. Single PAUSE instruction (Intel) /
        // YIELD (ARM); sub-nanosecond, no kernel transition. Safe
        // because this thread owns its CPU exclusively.
        std::hint::spin_loop();
    }
}

/// Newtype wrapper so AsyncFd can take a RawFd.
struct EvtFd(std::os::fd::RawFd);
impl std::os::fd::AsRawFd for EvtFd {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0
    }
}

/// Decode a typed msgpack body, bumping `dropped_parse_errors` on
/// failure. Returns None on error so the caller can early-`return`
/// without further matching. Centralises the
/// `match rmp_serde::from_slice { Ok(_)=>... Err(_)=>{counter; return;} }`
/// boilerplate that previously appeared at every typed dispatch arm.
#[inline]
fn decode_msg<T: serde::de::DeserializeOwned>(
    body: &[u8],
    counters: &DecisionCounters,
) -> Option<T> {
    match rmp_serde::from_slice::<T>(body) {
        Ok(v) => Some(v),
        Err(_) => {
            counters.dropped_parse_errors.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

fn process_event(
    state: &Arc<SharedState>,
    counters: &Arc<DecisionCounters>,
    commands_ring: &Arc<Mutex<Ring>>,
    body: Vec<u8>,
    events_log: &Arc<jsonl::JsonlWriter>,
    decisions_log: &Arc<jsonl::JsonlWriter>,
) {
    let recv_wall_ns = now_ns_wall();
    // Bundle 2F (2026-05-06): wrap body in Arc<Vec<u8>> upfront so the
    // JSONL enqueue is a refcount bump (~5 ns) instead of a fresh Vec
    // clone (~100 ns for 100-200B msgpack bodies). Subsequent borrows
    // (decode_header, decode_msg, decode_tick) read through the Arc.
    let body_arc = Arc::new(body);
    let body: &[u8] = &body_arc;

    // Bundle 2C (2026-05-06): replaced `rmp_serde::from_slice::<MsgHeader>`
    // with a hand-rolled walker that returns borrowed bytes for the
    // type field — no String allocation per event. Symmetric to the
    // tick decoder shipped earlier.
    let (type_bytes, header_ts_ns) = match msgpack_decode::decode_header(body) {
        Some(v) => v,
        None => {
            log::debug!("malformed msg, ignoring");
            counters.dropped_parse_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    // p50-2 (2026-05-04): defer JSONL line construction to the writer
    // task.
    events_log.write(LogPayload::Event {
        recv_ns: recv_wall_ns,
        body: Arc::clone(&body_arc),
    });

    if let Some(emit_ns) = header_ts_ns {
        let lat = recv_wall_ns.saturating_sub(emit_ns) / 1000;
        if lat < 5_000_000 {
            let mut h = state.histograms.lock();
            let cap = h.ipc_cap;
            h.ipc_us.push_back(lat);
            if h.ipc_us.len() > cap {
                h.ipc_us.pop_front();
            }
        }
    }

    match type_bytes {
        b"tick" => {
            // Hand-rolled msgpack decoder (msgpack_decode::decode_tick)
            // replaces rmp_serde::from_slice for the hot path. Saves
            // ~1-2 µs at p50 by avoiding the Value-tree round trip and
            // by reusing this stack-local TickMsg's String allocations
            // across consecutive ticks within process_event scope.
            let mut tick = TickMsg::default();
            if !msgpack_decode::decode_tick(body, &mut tick) {
                counters.dropped_parse_errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
            // ts_ns lives at the outer-message level in the wire format
            // (broker_ipc.py emits it there); copy it down so on_tick's
            // TTT computation has the broker's emit timestamp.
            if tick.ts_ns.is_none() {
                tick.ts_ns = header_ts_ns;
            }
            on_tick(
                state,
                counters,
                commands_ring,
                tick,
                decisions_log,
                recv_wall_ns,
            );
        }
        b"underlying_tick" => {
            let ut: UnderlyingTickMsg = match decode_msg(body, counters) {
                Some(v) => v,
                None => return,
            };
            state.scalars.lock().underlying_price = ut.price;
        }
        b"vol_surface" => {
            let vs: VolSurfaceMsg = match decode_msg(body, counters) {
                Some(v) => v,
                None => return,
            };
            // Side comes as "C", "P", or "BOTH" (current broker fits a
            // combined surface, see audit T4-10). The lookup tries
            // (expiry, right_char) → 'C' → 'P'; it never tries 'B'.
            // So when broker emits "BOTH", populate both 'C' and 'P'.
            // Taylor reprice anchor: prefer broker-provided
            // `spot_at_fit` (the spot the broker actually saw when
            // fitting). When absent (older broker), fall back to our
            // current spot — a few ms of IPC latency, well below
            // tick precision. Falling back to `vs.forward` would
            // re-introduce the carry-confusion bug.
            //
            // §19 audit follow-up: throttled warning when the fallback
            // fires, so a future broker change that silently drops the
            // field gets noticed in telemetry. Logs at counts 1, 2, 4,
            // 8, 16, 32 then falls silent — once is informative, the
            // fallback is correct so we don't need to spam. See
            // audits/sections-16-19-audit.md §2.4.
            let spot_at_fit = match vs.spot_at_fit {
                Some(v) => v,
                None => {
                    static MISSING_COUNT: std::sync::atomic::AtomicU32 =
                        std::sync::atomic::AtomicU32::new(0);
                    let n = MISSING_COUNT.fetch_add(1, Ordering::Relaxed);
                    if n < 50 && (n + 1).is_power_of_two() {
                        log::warn!(
                            "vol_surface from broker missing spot_at_fit \
                             (event #{}); falling back to current spot. \
                             §19 Taylor anchor protection is degraded; \
                             investigate broker version.",
                            n + 1
                        );
                    }
                    state.scalars.lock().underlying_price
                }
            };
            // Wrap the wire f64 values into typed newtypes here, at
            // the IPC → trader boundary. From this point onward the
            // type system rejects cross-substitution between forward
            // and spot. See crate::types module docs.
            let entry = Arc::new(crate::state::VolSurfaceEntry {
                forward: crate::types::FitForward(vs.forward),
                params: vs.params,
                fit_ts_ns: vs.ts_ns.unwrap_or(0),
                calibrated_min_k: vs.calibrated_min_k,
                calibrated_max_k: vs.calibrated_max_k,
                spot_at_fit: crate::types::SpotAtFit(spot_at_fit),
            });
            let expiry_arc = state.intern_expiry(&vs.expiry);
            let side_upper = vs.side.to_ascii_uppercase();
            if side_upper == "BOTH" {
                state
                    .vol_surfaces
                    .insert((Arc::clone(&expiry_arc), 'C'), Arc::clone(&entry));
                state.vol_surfaces.insert((expiry_arc, 'P'), entry);
            } else {
                let side_char = side_upper.chars().next().unwrap_or('C');
                state.vol_surfaces.insert((expiry_arc, side_char), entry);
            }
        }
        b"risk_state" => {
            let r: RiskStateMsg = match decode_msg(body, counters) {
                Some(v) => v,
                None => return,
            };
            let mut sc = state.scalars.lock();
            sc.risk_effective_delta = Some(r.effective_delta);
            sc.risk_margin_pct = Some(r.margin_pct);
            sc.risk_hedge_delta = Some(r.hedge_delta);
            sc.risk_theta = Some(r.theta);
            sc.risk_vega = Some(r.vega);
            sc.risk_state_age_monotonic_ns = now_ns_monotonic();
        }
        b"place_ack" => {
            let p: PlaceAckMsg = match decode_msg(body, counters) {
                Some(v) => v,
                None => return,
            };
            // place_ack arrives from the broker after IBKR ack; an
            // empty or malformed `right` means we can't reliably build
            // the (strike, expiry, right, side) key, and silently
            // bucketing under 'C' would mis-route puts. Drop the
            // message and bump dropped_parse_errors so operators see
            // the schema drift signal.
            let r_char = match p.right.chars().next() {
                Some(c) => c.to_ascii_uppercase(),
                None => {
                    counters.dropped_parse_errors.fetch_add(1, Ordering::Relaxed);
                    log::warn!("place_ack with empty `right`; dropping");
                    return;
                }
            };
            let s_char = match p.side.as_str() {
                "BUY" => 'B',
                "SELL" => 'S',
                _ => return,
            };
            let expiry_arc = state.intern_expiry(&p.expiry);
            let key: OurOrderKey = (
                SharedState::strike_key(p.strike),
                expiry_arc,
                r_char,
                s_char,
            );
            // Update existing OR insert. DashMap's entry API gives us
            // both branches in one op without a TOCTOU window.
            state
                .our_orders
                .entry(key.clone())
                .and_modify(|o| o.order_id = Some(p.order_id))
                .or_insert_with(|| OurOrder {
                    price: p.price,
                    place_monotonic_ns: now_ns_monotonic(),
                    order_id: Some(p.order_id),
                });
            state.orderid_to_key.insert(p.order_id, key);
        }
        // Broker emits "order_status" (the canonical name in Rust runtime);
        // legacy adapters may use "order_ack". Both routed to the same
        // terminal-state cleanup path.
        b"order_status" | b"order_ack" => {
            let a: OrderAckMsg = match decode_msg(body, counters) {
                Some(v) => v,
                None => return,
            };
            let oid = match a.order_id {
                Some(o) => o,
                None => return,
            };
            let status = a.status.unwrap_or_default();
            // Audit T1-4: Rejected and ApiRejected are terminal too;
            // FA accounts emit them when IBKR rejects a place at the
            // pre-trade risk check. Without these, our_orders leaks
            // the entry and staleness loop tries to cancel a
            // non-existent orderId forever.
            let terminal = matches!(
                status.as_str(),
                "Filled" | "Cancelled" | "ApiCancelled" | "Inactive" | "Rejected" | "ApiRejected"
            );
            if terminal {
                if let Some((_, key)) = state.orderid_to_key.remove(&oid) {
                    state.our_orders.remove(&key);
                }
            }
        }
        b"kill" => {
            let k: KillMsg = match decode_msg(body, counters) {
                Some(v) => v,
                None => return,
            };
            // Reject kill messages without a usable `source` rather than
            // bucketing them under "?"; an unparseable source means we
            // can't reliably resume later (a `resume` for the real
            // source would never match the "?" key, leaving the kill
            // sticky). Bump dropped_parse_errors so operators see the
            // signal rather than a silent indefinite halt.
            let src = match k.source.as_deref() {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => {
                    counters.dropped_parse_errors.fetch_add(1, Ordering::Relaxed);
                    log::warn!("kill message missing/empty `source`; dropping");
                    return;
                }
            };
            let reason = k.reason.unwrap_or_else(|| "?".to_string());
            // Increment kills_count only on a NEW source — duplicate
            // kill messages for an already-active source must not
            // double-count, or kills_count drifts above kills.len()
            // and the hot path's gate fires forever.
            if state.kills.insert(src, reason).is_none() {
                state.kills_count.fetch_add(1, Ordering::Relaxed);
            }
        }
        b"resume" => {
            let k: KillMsg = match decode_msg(body, counters) {
                Some(v) => v,
                None => return,
            };
            // Same rule as `kill`: drop on missing/empty source. A
            // resume with no source can't match any kill key — silently
            // bucketing under "?" would never clear the sticky kill.
            let src = match k.source.as_deref() {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => {
                    counters.dropped_parse_errors.fetch_add(1, Ordering::Relaxed);
                    log::warn!("resume message missing/empty `source`; dropping");
                    return;
                }
            };
            if state.kills.remove(&src).is_some() {
                state.kills_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
        b"weekend_pause" => {
            let wp: WeekendPauseMsg = match decode_msg(body, counters) {
                Some(v) => v,
                None => return,
            };
            state.scalars.lock().weekend_paused = wp.paused;
        }
        b"hello" => {
            let h: HelloMsg = match decode_msg(body, counters) {
                Some(v) => v,
                None => return,
            };
            log::warn!("broker hello received");
            if let Some(cfg) = h.config {
                let mut sc = state.scalars.lock();
                if let Some(v) = cfg.min_edge_ticks {
                    sc.min_edge_ticks = v as i32;
                }
                if let Some(v) = cfg.tick_size {
                    sc.tick_size = v;
                }
                if let Some(v) = cfg.delta_ceiling {
                    sc.delta_ceiling = v;
                }
                if let Some(v) = cfg.delta_kill {
                    sc.delta_kill = v;
                }
                if let Some(v) = cfg.margin_ceiling_pct {
                    sc.margin_ceiling_pct = v;
                }
                if let Some(v) = cfg.gtd_lifetime_s {
                    sc.gtd_lifetime_s = v;
                }
                if let Some(v) = cfg.gtd_refresh_lead_s {
                    sc.gtd_refresh_lead_s = v;
                }
                if let Some(v) = cfg.dead_band_ticks {
                    sc.dead_band_ticks = v as i32;
                }
                if let Some(v) = cfg.skip_if_spread_over_edge_mul {
                    sc.skip_if_spread_over_edge_mul = v;
                }
                if let Some(v) = cfg.theta_kill {
                    sc.theta_kill = v;
                }
                if let Some(v) = cfg.vega_kill {
                    sc.vega_kill = v;
                }
            }
        }
        _ => {
            // Unknown / not-yet-handled type — ignore.
        }
    }
}

fn on_tick(
    state: &Arc<SharedState>,
    counters: &Arc<DecisionCounters>,
    commands_ring: &Arc<Mutex<Ring>>,
    mut tick: TickMsg,
    decisions_log: &Arc<jsonl::JsonlWriter>,
    decide_ns_wall: u64,
) {
    let now_mono = now_ns_monotonic();
    // Item 8 (2026-05-05): `decide_ns_wall` is now passed in from
    // `process_event` rather than re-sampled here. Eliminates one
    // `SystemTime::now()` syscall per tick (~80-150ns each on x86_64
    // vDSO + Rust wrapping). The few-µs gap between `recv_wall_ns`
    // and the previous `decide_ns_wall` is absorbed into the place
    // order's `ts_ns` field — broker-side wire timing inflates
    // marginally but stays within the noise envelope.

    // Hot-path layout (post p50-1..4 rewrite, lock-shard 2026-05-04):
    //   1. Intern expiry; insert option (DashMap insert, no big lock);
    //      decide. decide_on_tick takes one scalar lock for its
    //      snapshot, and DashMap reads on theo_cache / vol_surfaces /
    //      our_orders.
    //   2. Enqueue typed Decision payloads to JSONL (writer formats
    //      ISO + serializes off-thread; see p50-2).
    //   3. Encode all outbound msgpack frames.
    //   4. Take ring lock, write all frames, unlock. Capture
    //      `send_ns_wall` AFTER write — that's the honest TTT
    //      end-point now (p50-4 — was previously stamped at function
    //      entry, which under-reported TTT by the encode + write
    //      window).
    //   5. Apply incumbency updates (DashMap inserts/removes).
    //      Histogram TTT push uses its own parking_lot lock.
    // Drop ticks with empty/malformed `right` instead of silently
    // bucketing under 'C' — that would mis-route puts to call state
    // and corrupt the OptionState cache. Counted as a parse error so
    // schema drift surfaces in telemetry.
    let r_char = match tick.right.chars().next() {
        Some(c) => c.to_ascii_uppercase(),
        None => {
            counters.dropped_parse_errors.fetch_add(1, Ordering::Relaxed);
            log::warn!("tick with empty `right`; dropping");
            return;
        }
    };
    let expiry_arc = state.intern_expiry(&tick.expiry);
    // IBKR's L1 market data emits bid_changed and ask_changed as
    // separate updates, so an incoming tick often has only one of
    // (bid, ask) populated. Merge the new tick with the cached
    // OptionState for this strike/right so the dark-book gate
    // (raw_bid<=0 || raw_ask<=0) sees the latest known values for
    // BOTH sides rather than dropping the tick because the missing
    // side is None. Without this merge every one-sided tick fires
    // skip_one_sided_or_dark — at off-hours paper, that's basically
    // every tick.
    let opt_key = (
        SharedState::strike_key(tick.strike),
        Arc::clone(&expiry_arc),
        r_char,
    );
    let cached = state.options.get(&opt_key).map(|r| *r.value());
    let merged_bid = tick.bid.or(cached.and_then(|c| c.bid));
    let merged_ask = tick.ask.or(cached.and_then(|c| c.ask));
    let merged_bid_size = tick.bid_size.or(cached.and_then(|c| c.bid_size));
    let merged_ask_size = tick.ask_size.or(cached.and_then(|c| c.ask_size));
    let opt_state = crate::state::OptionState {
        bid: merged_bid,
        ask: merged_ask,
        bid_size: merged_bid_size,
        ask_size: merged_ask_size,
    };
    state.options.insert(opt_key, opt_state);
    // Bundle 2D (2026-05-06): one scalar snapshot per tick. Previously
    // on_tick locked scalars for `(forward, gtd_lifetime_s)` AND
    // decide_on_tick locked separately for its own snapshot — two
    // mutex acquires for the same data. Now we snapshot once here and
    // hand the snapshot to decide_on_tick.
    let snap = state.scalar_snapshot();
    let forward = snap.underlying_price;
    let gtd_lifetime_s = snap.gtd_lifetime_s;
    // Item 7 (2026-05-05): mutate the owned `tick` in place with the
    // merged L1 view rather than building a fresh `TickMsg`. The
    // previous approach allocated two Strings per tick (expiry, right
    // clones) — small but frequent. Taking the tick by-value at the
    // call site lets us reuse its allocations.
    tick.bid = merged_bid;
    tick.ask = merged_ask;
    tick.bid_size = merged_bid_size;
    tick.ask_size = merged_ask_size;
    // `decide_ns_wall` was sampled at process_event entry; pass it
    // through so decide_on_tick can compare vol_surface fit_ts_ns
    // (CLOCK_REALTIME ns) against it without an extra SystemTime call.
    let decisions = decide_on_tick(
        state,
        counters,
        &tick,
        &expiry_arc,
        r_char,
        now_mono,
        decide_ns_wall,
        &snap,
    );

    // p50-2: log decisions to JSONL via typed payload.
    // The writer task formats ISO + serializes the JSON line.
    for d in &decisions {
        if let Decision::Place {
            side,
            price,
            cancel_old_oid,
        } = d
        {
            decisions_log.write(LogPayload::Decision(DecisionLog {
                recv_ns: decide_ns_wall,
                trigger_ts_ns: tick.ts_ns,
                forward,
                decision: DecisionInner {
                    action: "place",
                    side: side.as_str(),
                    strike: tick.strike,
                    expiry: Arc::clone(&expiry_arc),
                    right: r_char,
                    price: *price,
                    cancel_old_oid: *cancel_old_oid,
                },
            }));
        }
    }

    if decisions.is_empty() || !places_orders() {
        return;
    }

    // Item 9 (2026-05-05): single preallocated wire buffer reused
    // for every frame this tick. Previous design did 2 Vec
    // allocations per frame (rmp_serde::to_vec_named + pack_frame);
    // typical Place produced 4 fresh Vecs, Modify 2. Now we encode
    // each frame's body directly into `wire_buf` after a 4-byte
    // length placeholder, then backfill the placeholder with the
    // actual body length. `frame_ranges` records each frame's slice
    // so the ring-write loop below feeds them out one at a time.
    //
    // Bundle 1A (2026-05-06): the encoders below are now hand-rolled
    // (msgpack_encode::encode_*_into), reflection-free, replacing the
    // prior `rmp_serde::encode::write_named` path. Saves ~1 µs per
    // place/modify frame at p50 by skipping serde's reflection over
    // the typed structs.
    let mut wire_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut frame_ranges: Vec<std::ops::Range<usize>> =
        Vec::with_capacity(decisions.len() * 2);
    // `_to_track` tuples carry the index of the "load-bearing" frame
    // for that decision — the place_order frame for a Place, the
    // modify_order frame for a Modify. After the ring-write loop,
    // `frame_ok[idx] == true` iff that frame actually made it onto
    // the SHM ring. If false, the broker never sees the command, so
    // we MUST NOT advance our local our_orders state — otherwise the
    // trader thinks an order is in-flight that doesn't exist, sits in
    // skip_in_band on every subsequent tick, and the strike goes
    // permanently idle (the bug we hunted for several hours on
    // 2026-05-05). Cancel-before-place's cancel frame index is not
    // tracked: a failed cancel just leaves the old order at IBKR
    // until GTD-expiry; that's recoverable. A failed place is the
    // poisoning case.
    let mut places_to_track: Vec<(OurOrderKey, OurOrder, Option<i64>, usize)> =
        Vec::with_capacity(decisions.len());
    let mut modifies_to_track: Vec<(OurOrderKey, f64, i64, usize)> =
        Vec::with_capacity(decisions.len());
    // For CancelAll: (order_id, frame_idx) pairs. After the ring write,
    // only entries whose frame succeeded are removed from local
    // our_orders/orderid_to_key — failed cancels mean the order is
    // still resting at IBKR and our local view must continue to
    // reflect that, so subsequent ticks can retry the cancel rather
    // than leaving an unknown phantom there until GTD-expiry.
    let mut cancels_to_track: Vec<(i64, usize)> = Vec::new();
    let mut cancel_all_fired = false;

    /// Reserve a 4-byte length placeholder, run the body encoder, then
    /// backfill the placeholder with the body length. Returns the
    /// frame's range in `wire_buf` (length prefix + body).
    fn frame_with<F: FnOnce(&mut Vec<u8>)>(
        buf: &mut Vec<u8>,
        encode_body: F,
    ) -> std::ops::Range<usize> {
        let frame_start = buf.len();
        buf.extend_from_slice(&[0u8; 4]);
        let body_start = buf.len();
        encode_body(buf);
        let body_len = (buf.len() - body_start) as u32;
        buf[frame_start..frame_start + 4]
            .copy_from_slice(&body_len.to_be_bytes());
        frame_start..buf.len()
    }

    for d in decisions {
        match d {
            Decision::Place {
                side,
                price,
                cancel_old_oid,
            } => {
                if let Some(oid) = cancel_old_oid {
                    let cancel = CancelOrder {
                        msg_type: "cancel_order",
                        ts_ns: decide_ns_wall,
                        order_id: oid,
                    };
                    frame_ranges.push(frame_with(&mut wire_buf, |b| {
                        msgpack_encode::encode_cancel_into(b, &cancel)
                    }));
                }
                // Borrow expiry/right from `tick`; `side` is &'static
                // from `Side::as_str()`. No String allocations on the
                // hot path for any of these — saves ~3 heap ops per
                // Place. expiry_arc.as_ref() would also work but
                // tick.expiry is already a String we own and is
                // identical content; using it keeps the borrow tied
                // to the outer `tick` whose lifetime spans the
                // encode_frame call.
                let p = PlaceOrder {
                    msg_type: "place_order",
                    ts_ns: decide_ns_wall,
                    strike: tick.strike,
                    expiry: tick.expiry.as_str(),
                    right: tick.right.as_str(),
                    side: side.as_str(),
                    qty: 1,
                    price,
                    order_ref: "corsair_trader_rust",
                    triggering_tick_broker_recv_ns: tick.broker_recv_ns,
                };
                let place_frame_idx = frame_ranges.len();
                frame_ranges.push(frame_with(&mut wire_buf, |b| {
                    msgpack_encode::encode_place_into(b, &p)
                }));
                let okey: OurOrderKey = (
                    SharedState::strike_key(tick.strike),
                    Arc::clone(&expiry_arc),
                    r_char,
                    side.as_char(),
                );
                places_to_track.push((
                    okey,
                    OurOrder {
                        price,
                        place_monotonic_ns: now_mono,
                        order_id: None,
                    },
                    cancel_old_oid,
                    place_frame_idx,
                ));
            }
            Decision::Modify {
                side,
                order_id,
                price,
            } => {
                // Single-message amend. Refresh GTD on every modify so
                // the order doesn't expire mid-update.
                let m = ModifyOrder {
                    msg_type: "modify_order",
                    ts_ns: decide_ns_wall,
                    order_id,
                    price,
                    gtd_seconds: gtd_lifetime_s as u32,
                    triggering_tick_broker_recv_ns: tick.broker_recv_ns,
                };
                let modify_frame_idx = frame_ranges.len();
                frame_ranges.push(frame_with(&mut wire_buf, |b| {
                    msgpack_encode::encode_modify_into(b, &m)
                }));
                let okey: OurOrderKey = (
                    SharedState::strike_key(tick.strike),
                    Arc::clone(&expiry_arc),
                    r_char,
                    side.as_char(),
                );
                modifies_to_track.push((okey, price, order_id, modify_frame_idx));
            }
            Decision::CancelAll { order_ids } => {
                let n = order_ids.len();
                for oid in &order_ids {
                    let cancel = CancelOrder {
                        msg_type: "cancel_order",
                        ts_ns: decide_ns_wall,
                        order_id: *oid,
                    };
                    let cancel_frame_idx = frame_ranges.len();
                    frame_ranges.push(frame_with(&mut wire_buf, |b| {
                        msgpack_encode::encode_cancel_into(b, &cancel)
                    }));
                    cancels_to_track.push((*oid, cancel_frame_idx));
                }
                log::warn!(
                    "trader: CancelAll fired ({} orders) — risk self-block",
                    n
                );
                modifies_to_track.clear();
                places_to_track.clear();
                cancel_all_fired = true;
            }
        }
    }

    // Single ring lock for ALL outbound frames in this tick.
    // Each rejected frame increments place_dropped — surfaces as
    // `place_dropped` in the 10s telemetry decisions dict and as the
    // monotonic `cmd_drops=` field in the telemetry print line.
    // `frame_ok[i]` records whether frame i made it onto the ring;
    // the post-write tracking loops use this to decide whether to
    // advance our_orders state (only on success — otherwise the
    // trader poisons its own view by claiming an order is in-flight
    // that the broker never received).
    let mut frame_ok: Vec<bool> = vec![true; frame_ranges.len()];
    {
        let mut ring = commands_ring.lock();
        for (i, r) in frame_ranges.iter().enumerate() {
            if !ring.write_frame(&wire_buf[r.clone()]) {
                counters.place_dropped.fetch_add(1, Ordering::Relaxed);
                frame_ok[i] = false;
            }
        }
    }

    // p50-4 (2026-05-04): TTT honesty fix. `send_ns_wall_after_write`
    // captured AFTER ring write — covers msgpack encode + ring lock
    // + write_frame. Previously the TTT was stamped at on_tick entry
    // (decide_ns_wall), so it under-reported by ~5-15µs. The metric
    // is now an honest "broker tick recv → trader frame in commands
    // ring" wire-to-wire-internal latency.
    let send_ns_wall_after_write = now_ns_wall();

    // TTT histogram push — separate parking_lot mutex so the bg
    // telemetry sort doesn't block this tiny (~50ns) push.
    if let Some(emit_ns) = tick.ts_ns {
        let lat = send_ns_wall_after_write.saturating_sub(emit_ns) / 1000;
        if lat < 5_000_000 {
            let mut h = state.histograms.lock();
            let cap = h.ttt_cap;
            h.ttt_us.push_back(lat);
            if h.ttt_us.len() > cap {
                h.ttt_us.pop_front();
            }
        }
    }

    if cancel_all_fired {
        // Per-frame cleanup: remove only the orders whose cancel frame
        // actually made it onto the ring. A dropped cancel frame means
        // the broker never received it; the order is still resting at
        // IBKR, so we must keep our local view consistent (otherwise
        // we'd think the cancel succeeded, fall through to placing new
        // orders, and end up with double inventory).
        for (oid, frame_idx) in &cancels_to_track {
            if !frame_ok[*frame_idx] {
                continue;
            }
            if let Some((_, key)) = state.orderid_to_key.remove(oid) {
                state.our_orders.remove(&key);
            }
        }
    }
    for (key, order, cancel_old_oid, place_idx) in places_to_track {
        // If the place frame was rejected by the SHM ring, the broker
        // never sees this order — don't poison our_orders by claiming
        // it's in-flight. A subsequent tick will re-evaluate from a
        // clean state and try again. (The optional cancel for the old
        // oid may or may not have made it; if it did, the old order
        // is gone at the broker but we leave orderid_to_key alone so
        // it can clear naturally on order_status. If not, GTD-expiry
        // sweeps it within ~5s.)
        if !frame_ok[place_idx] {
            continue;
        }
        if let Some(oid) = cancel_old_oid {
            state.orderid_to_key.remove(&oid);
        }
        state.our_orders.insert(key, order);
    }
    for (key, new_price, order_id, modify_idx) in modifies_to_track {
        // Same reasoning: a dropped modify means the broker still
        // holds the prior price. Leaving our_orders unchanged keeps
        // the trader's view consistent with what's actually resting.
        if !frame_ok[modify_idx] {
            continue;
        }
        if let Some(mut o) = state.our_orders.get_mut(&key) {
            o.price = new_price;
            o.place_monotonic_ns = now_mono;
            debug_assert_eq!(o.order_id, Some(order_id));
        }
    }
}

/// Periodic staleness check — refresh / cancel resting orders whose
/// current theo has drifted, or whose market has gone dark. Runs at
/// 10Hz from a bg-runtime tokio task.
///
/// Amend bias (2026-05-05): drift-stale orders are now refreshed via
/// `modify_order` instead of cancel-then-next-tick-place. Modify RTT
/// is ~4.5x faster than place (41ms vs 186ms p50 per wire_timing
/// analysis), so the same quote update reaches IBKR faster. The new
/// price targets `theo ± min_edge_ticks * tick_size` — the same
/// edge the placement logic would target on the next tick. Drift
/// always adjusts the order in the SAFE direction (BUY stays at or
/// below ask, SELL stays at or above bid) because the staleness
/// trigger condition (`drift > threshold`) implies the order's
/// price is already on the resting side of theo.
///
/// Dark-book staleness still uses `cancel_order` — we don't want to
/// be in a dark market at any refreshed price.
fn staleness_check(
    state: &Arc<SharedState>,
    counters: &Arc<DecisionCounters>,
    commands_ring: &Arc<Mutex<Ring>>,
) {
    use crate::decision::{compute_theo, time_to_expiry_years, STALENESS_TICKS};

    enum Action {
        Modify {
            order_id: i64,
            key: OurOrderKey,
            new_price: f64,
        },
        Cancel {
            order_id: i64,
            key: OurOrderKey,
            reason_dark: bool,
        },
    }
    let mut actions: Vec<Action> = Vec::new();

    let snap = state.scalar_snapshot();
    let tick_size = snap.tick_size;
    let threshold = STALENESS_TICKS as f64 * tick_size;
    let min_edge = snap.min_edge_ticks as f64 * tick_size;
    let gtd_lifetime_s = snap.gtd_lifetime_s;

    for entry in state.our_orders.iter() {
        let key = entry.key();
        let order = entry.value();
        let order_id = match order.order_id {
            Some(o) => o,
            None => continue, // unack'd; can't cancel yet
        };
        // Inverse of `SharedState::strike_key`: i64 quantum / 10_000.
        let strike = (key.0 as f64) / 10_000.0;
        let expiry = &key.1;
        let r_char = key.2;
        let s_char = key.3;

        // Look up vol surface for this option (right + 'C' / 'P'
        // fallback chain — see SharedState::lookup_vol_surface).
        let vp = state.lookup_vol_surface(expiry, r_char);
        let vp = match vp {
            Some(v) => v,
            None => continue,
        };
        let tte = match time_to_expiry_years(expiry) {
            Some(t) if t > 0.0 => t,
            _ => continue,
        };
        // Use fit-time forward (anchored point for SVI), then apply
        // the same Taylor reprice the hot-path decision flow applies:
        //   theo ≈ theo_at_fit + delta_at_fit × (spot − spot_at_fit)
        // Anchored on `vp.spot_at_fit` (broker's spot at fit time),
        // NOT `vp.forward` — the latter conflates carry with drift.
        // Without this in the staleness loop, the loop's drift check
        // compares order.price vs fit-frozen theo, can't see when our
        // quote is stale relative to current market mid.
        let (_iv, theo_at_fit, delta_at_fit) =
            match compute_theo(vp.forward, strike, tte, r_char, &vp.params) {
                Some(v) => v,
                None => continue,
            };
        let theo = (theo_at_fit + delta_at_fit * (snap.underlying_price - vp.spot_at_fit.raw())).max(0.01);

        // Stale if our price is too unfavorable vs current theo.
        // Drift > threshold → modify to fresh edge (amend bias).
        let drift = if s_char == 'B' {
            order.price - theo
        } else {
            theo - order.price
        };
        if drift > threshold {
            // Target: same edge the placement logic uses on a fresh
            // tick. BUY ↓ to (theo - min_edge), SELL ↑ to (theo +
            // min_edge), snapped to the tick grid. Both directions
            // stay on the safe side of the spread (drift > threshold
            // implies order was already non-crossing).
            let raw_new = if s_char == 'B' {
                theo - min_edge
            } else {
                theo + min_edge
            };
            let new_price = (raw_new / tick_size).round() * tick_size;
            // Skip degenerate "no movement" modify — shouldn't happen
            // given drift > threshold but defensive.
            if (new_price - order.price).abs() > tick_size * 0.01 {
                actions.push(Action::Modify {
                    order_id,
                    key: key.clone(),
                    new_price,
                });
            }
            continue;
        }

        // Dark-book ON-REST guard. Cancel (not modify) — we don't
        // want to be in a dark market at any refreshed price.
        let opt_key = (key.0, Arc::clone(expiry), r_char);
        if let Some(latest_ref) = state.options.get(&opt_key) {
            let latest = latest_ref.value();
            let bid_alive = matches!(latest.bid, Some(b) if b > 0.0);
            let ask_alive = matches!(latest.ask, Some(a) if a > 0.0);
            let bsz = latest.bid_size.unwrap_or(0);
            let asz = latest.ask_size.unwrap_or(0);
            let market_dark = !bid_alive || !ask_alive || bsz <= 0 || asz <= 0;
            if market_dark {
                actions.push(Action::Cancel {
                    order_id,
                    key: key.clone(),
                    reason_dark: true,
                });
            }
        }
    }

    if actions.is_empty() {
        return;
    }

    // Process actions. Each modify keeps the order live with a fresh
    // price; each cancel removes it from our_orders. No bulk lock.
    let now_w = now_ns_wall();
    let now_m = now_ns_monotonic();
    for action in actions {
        match action {
            Action::Modify {
                order_id,
                key,
                new_price,
            } => {
                let modify = ModifyOrder {
                    msg_type: "modify_order",
                    ts_ns: now_w,
                    order_id,
                    price: new_price,
                    gtd_seconds: gtd_lifetime_s as u32,
                    triggering_tick_broker_recv_ns: None,
                };
                if let Ok(body) = rmp_serde::to_vec_named(&modify) {
                    let frame = ipc::protocol::pack_frame(&body);
                    if !commands_ring.lock().write_frame(&frame) {
                        counters.place_dropped.fetch_add(1, Ordering::Relaxed);
                        // Frame dropped — leave our_orders entry as-is.
                        // Next staleness pass will retry.
                        continue;
                    }
                }
                if let Some(mut o) = state.our_orders.get_mut(&key) {
                    o.price = new_price;
                    o.place_monotonic_ns = now_m;
                }
                counters.staleness_modify.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Action::Cancel {
                order_id,
                key,
                reason_dark,
            } => {
                let cancel = CancelOrder {
                    msg_type: "cancel_order",
                    ts_ns: now_w,
                    order_id,
                };
                if let Ok(body) = rmp_serde::to_vec_named(&cancel) {
                    let frame = ipc::protocol::pack_frame(&body);
                    if !commands_ring.lock().write_frame(&frame) {
                        counters.place_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                state.our_orders.remove(&key);
                state.orderid_to_key.remove(&order_id);
                if reason_dark {
                    counters.staleness_cancel_dark.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters.staleness_cancel.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

fn build_telemetry(
    state: &Arc<SharedState>,
    counters: &Arc<DecisionCounters>,
    commands_ring: &Arc<Mutex<Ring>>,
    total_events_running: &mut u64,
) -> (Telemetry, u64) {
    // Snapshot histograms under the histogram lock; drop before the
    // sort so the hot path can keep pushing while we sort the copy.
    let (mut ipc_sorted, mut ttt_sorted) = {
        let h = state.histograms.lock();
        let ipc: Vec<u64> = h.ipc_us.iter().copied().collect();
        let ttt: Vec<u64> = h.ttt_us.iter().copied().collect();
        (ipc, ttt)
    };
    ipc_sorted.sort_unstable();
    ttt_sorted.sort_unstable();
    let pct = |v: &[u64], q: f64| -> Option<u64> {
        if v.is_empty() {
            None
        } else {
            let idx = ((v.len() as f64) * q) as usize;
            Some(v[idx.min(v.len() - 1)])
        }
    };
    let n_events = state.options.len() as u64; // approx; real count in process loop
    *total_events_running += n_events;
    let killed: Vec<String> = state.kills.iter().map(|e| e.key().clone()).collect();
    let (weekend_paused, risk_hedge_delta) = {
        let sc = state.scalars.lock();
        (sc.weekend_paused, sc.risk_hedge_delta)
    };
    let commands_frames_dropped = commands_ring.lock().frames_dropped;
    (
        Telemetry {
            msg_type: "telemetry",
            ts_ns: now_ns_wall(),
            events: serde_json::json!({}),
            decisions: counters.to_json(),
            ipc_p50_us: pct(&ipc_sorted, 0.50),
            ipc_p99_us: pct(&ipc_sorted, 0.99),
            ipc_n: ipc_sorted.len(),
            ttt_p50_us: pct(&ttt_sorted, 0.50),
            ttt_p99_us: pct(&ttt_sorted, 0.99),
            ttt_n: ttt_sorted.len(),
            n_options: state.options.len(),
            n_active_orders: state.our_orders.len(),
            n_vol_expiries: state.vol_surfaces.len(),
            killed,
            weekend_paused,
            risk_hedge_delta,
            commands_frames_dropped,
        },
        *total_events_running,
    )
}
