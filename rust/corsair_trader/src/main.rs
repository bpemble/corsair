//! Corsair trader binary — Rust port of src/trader/main.py.
//!
//! Connects to broker via SHM IPC (events ring + commands ring +
//! notification FIFOs), processes ticks, makes quote decisions, and
//! sends place_order / cancel_order commands.
//!
//! Hot path is single-threaded; tokio is used for concurrent
//! background tasks (telemetry, staleness loop, FIFO read polling).
//!
//! Env vars:
//!   CORSAIR_TRADER_PLACES_ORDERS=1    actually send place_orders
//!   CORSAIR_IPC_TRANSPORT=shm         must be shm (we don't support socket)
//!   CORSAIR_LOG_LEVEL=info|debug|...

mod decision;
mod ipc;
mod jsonl;
mod messages;
mod pricing;
mod state;
mod tte_cache;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::decision::{decide_on_tick, Decision, GTD_LIFETIME_S};
use crate::ipc::shm::{wait_for_rings, Ring, DEFAULT_RING_CAPACITY};
use crate::messages::*;
use crate::state::{DecisionCounters, OurOrder, TraderState};

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

fn main() -> std::io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // Multi-thread tokio runtime with pinned workers. Default 2
    // workers (per CLAUDE.md §17 — busy-poll hot loop + helpers).
    // CORSAIR_TRADER_WORKERS overrides. Pinning honors docker
    // cpuset; on bare-metal trader gets cpuset 4,5,10,11 and pins
    // workers to first two of those.
    let desired = std::env::var("CORSAIR_TRADER_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(2);
    let (workers, pinner) =
        corsair_ipc::cpu_affinity::build_pinner("corsair_trader", desired);
    let main_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .on_thread_start(pinner)
        .build()?;

    // Background runtime — staleness sweep (10Hz) + telemetry (10s)
    // live here, off the hot loop's cores. Removes scheduler
    // preemption of the hot loop from these periodic tasks (the
    // mutex contention itself remains until #7 lock-shards
    // TraderState.options out of the single mutex). Single-thread
    // current-thread runtime on a dedicated std thread, pinned to
    // the next CPU after the main runtime's workers.
    let allowed = corsair_ipc::cpu_affinity::allowed_cpus();
    let bg_cpu = allowed.get(workers).copied();
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

    // JSONL writers — background tasks, hot path enqueues only.
    let log_dir = std::env::var("CORSAIR_LOGS_DIR")
        .unwrap_or_else(|_| "/app/logs-paper".into());
    let events_log = jsonl::JsonlWriter::start(
        std::path::PathBuf::from(&log_dir),
        "trader_events",
    );
    let decisions_log = jsonl::JsonlWriter::start(
        std::path::PathBuf::from(&log_dir),
        "trader_decisions",
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

    // Shared state behind a mutex; hot path is single-thread but
    // background tasks (telemetry, staleness) need access.
    let state = Arc::new(Mutex::new(TraderState::new()));
    let counters = Arc::new(Mutex::new(DecisionCounters::default()));
    let events_ring = Arc::new(Mutex::new(events_ring));
    let commands_ring = Arc::new(Mutex::new(commands_ring));

    // Graceful shutdown: SIGTERM / SIGINT → cancel every resting
    // order at IBKR, then exit. Without this, `docker compose stop
    // trader` killed the container while orders sat at IBKR for up
    // to GTD_LIFETIME_S seconds — any market cross during that window
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
            // Snapshot all live order_ids and emit cancel frames.
            // Holding state lock briefly only to read order_id list;
            // we cap the cancel set at the moment of signal so an
            // ongoing place_ack flood can't keep us alive.
            let order_ids: Vec<i64> = {
                let s = state_sd.lock().unwrap();
                s.orderid_to_key.keys().copied().collect()
            };
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
                let mut ring = commands_ring_sd.lock().unwrap();
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
                let (tel, n_events) = build_telemetry(&state, &counters, &mut total_events);
                let body = match rmp_serde::to_vec_named(&tel) {
                    Ok(b) => b,
                    Err(e) => {
                        log::warn!("telemetry encode failed: {}", e);
                        continue;
                    }
                };
                let frame = ipc::protocol::pack_frame(&body);
                let mut ring = commands_ring.lock().unwrap();
                ring.write_frame(&frame);
                drop(ring);
                log::info!(
                    "telemetry: events={} ipc_p50={:?} ipc_p99={:?} ttt_p50={:?} ttt_p99={:?} \
                     opts={} orders={} decisions={}",
                    n_events,
                    tel.ipc_p50_us,
                    tel.ipc_p99_us,
                    tel.ttt_p50_us,
                    tel.ttt_p99_us,
                    tel.n_options,
                    tel.n_active_orders,
                    tel.decisions,
                );
            }
        });
    }

    // Hot loop: tight read of events ring + decision dispatch.
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);

    // Mode select: busy-poll (CORSAIR_TRADER_BUSY_POLL=1) trades 1
    // CPU core for ~50-100µs latency reduction by skipping the FIFO
    // wakeup path entirely. Default OFF — operators opt in.
    let busy_poll = std::env::var("CORSAIR_TRADER_BUSY_POLL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if busy_poll {
        log::warn!(
            "CORSAIR_TRADER_BUSY_POLL=1 — busy-polling SHM ring \
             (1 CPU core hot, lower latency)"
        );
        loop {
            let chunk = events_ring.lock().unwrap().read_available();
            if !chunk.is_empty() {
                buf.extend_from_slice(&chunk);
                let frames = ipc::protocol::unpack_all_frames(&mut buf)?;
                for body in frames {
                    // event_count tracked in telemetry; local counter retired
                    process_event(
                        &state, &counters, &commands_ring,
                        &body, &events_log, &decisions_log,
                    );
                }
                continue;
            }
            // No frame ready. Yield to the tokio scheduler so other
            // tasks (telemetry, staleness, JSONL flush) can run, but
            // don't sleep — we want to recheck the ring immediately.
            // tokio::task::yield_now is sub-microsecond.
            tokio::task::yield_now().await;
        }
    } else {
        // FIFO-notify mode (default): block on the events FIFO; wake
        // when broker writes a notification byte. Lower CPU; ~50-100µs
        // worst-case wakeup latency from FIFO + scheduler.
        use tokio::io::unix::AsyncFd;
        let evt_fifo_fd = events_ring.lock().unwrap().notify_r_fd().expect("events fifo");
        let evt_async_fd = AsyncFd::new(EvtFd(evt_fifo_fd))?;

        loop {
            // Drain ring.
            let chunk = events_ring.lock().unwrap().read_available();
            if !chunk.is_empty() {
                buf.extend_from_slice(&chunk);
                let frames = ipc::protocol::unpack_all_frames(&mut buf)?;
                for body in frames {
                    // event_count tracked in telemetry; local counter retired
                    process_event(
                        &state, &counters, &commands_ring,
                        &body, &events_log, &decisions_log,
                    );
                }
                continue;
            }
            events_ring.lock().unwrap().drain_notify();
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

/// Newtype wrapper so AsyncFd can take a RawFd.
struct EvtFd(std::os::fd::RawFd);
impl std::os::fd::AsRawFd for EvtFd {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0
    }
}

fn process_event(
    state: &Arc<Mutex<TraderState>>,
    counters: &Arc<Mutex<DecisionCounters>>,
    commands_ring: &Arc<Mutex<Ring>>,
    body: &[u8],
    events_log: &Arc<jsonl::JsonlWriter>,
    decisions_log: &Arc<jsonl::JsonlWriter>,
) {
    let recv_wall_ns = now_ns_wall();
    // Decode just the type field first.
    let generic: GenericMsg = match rmp_serde::from_slice(body) {
        Ok(g) => g,
        Err(e) => {
            log::debug!("malformed msg, ignoring: {}", e);
            counters.lock().unwrap().dropped_parse_errors += 1;
            return;
        }
    };
    // Mirror Python's trader_events JSONL schema:
    // {"recv_ts": ISO, "event": <full msgpack body as a JSON object>}.
    // Decoded msgpack to serde_json::Value for the log line.
    if let Ok(event_value) = rmp_serde::from_slice::<serde_json::Value>(body) {
        let line = serde_json::json!({
            "recv_ts": chrono::Utc::now().to_rfc3339(),
            "event": event_value,
        });
        events_log.write(line);
    }
    if let Some(emit_ns) = generic.ts_ns {
        let lat = recv_wall_ns.saturating_sub(emit_ns) / 1000;
        if lat < 5_000_000 {
            let mut s = state.lock().unwrap();
            s.ipc_us.push_back(lat);
            if s.ipc_us.len() > 2000 {
                s.ipc_us.pop_front();
            }
        }
    }

    match generic.msg_type.as_str() {
        "tick" => {
            let mut tick: TickMsg = match serde_json::from_value(generic.extra.clone()) {
                Ok(t) => t,
                Err(_) => {
                    counters.lock().unwrap().dropped_parse_errors += 1;
                    return;
                }
            };
            // ts_ns lives at the outer-message level in the wire format
            // (broker_ipc.py emits it there); copy it down so on_tick's
            // TTT computation has the broker's emit timestamp.
            if tick.ts_ns.is_none() {
                tick.ts_ns = generic.ts_ns;
            }
            on_tick(state, counters, commands_ring, &tick, decisions_log);
        }
        "underlying_tick" => {
            let ut: UnderlyingTickMsg = match serde_json::from_value(generic.extra.clone()) {
                Ok(t) => t,
                Err(_) => {
                    counters.lock().unwrap().dropped_parse_errors += 1;
                    return;
                }
            };
            state.lock().unwrap().underlying_price = ut.price;
        }
        "vol_surface" => {
            let vs: VolSurfaceMsg = match serde_json::from_value(generic.extra.clone()) {
                Ok(t) => t,
                Err(_) => {
                    counters.lock().unwrap().dropped_parse_errors += 1;
                    return;
                }
            };
            // Side comes as "C", "P", or "BOTH" (current broker fits a
            // combined surface, see audit T4-10). The lookup at
            // decision.rs:123 tries (expiry, right_char) → 'C' → 'P';
            // it never tries 'B'. So when broker emits "BOTH", populate
            // both 'C' and 'P' entries with the same params.
            let entry = crate::state::VolSurfaceEntry {
                forward: vs.forward,
                params: vs.params,
                fit_ts_ns: vs.ts_ns.unwrap_or(0),
            };
            let mut s = state.lock().unwrap();
            let side_upper = vs.side.to_ascii_uppercase();
            if side_upper == "BOTH" {
                s.vol_surfaces.insert((vs.expiry.clone(), 'C'), entry.clone());
                s.vol_surfaces.insert((vs.expiry, 'P'), entry);
            } else {
                let side_char = side_upper.chars().next().unwrap_or('C');
                s.vol_surfaces.insert((vs.expiry, side_char), entry);
            }
        }
        "risk_state" => {
            let r: RiskStateMsg = match serde_json::from_value(generic.extra.clone()) {
                Ok(t) => t,
                Err(_) => {
                    counters.lock().unwrap().dropped_parse_errors += 1;
                    return;
                }
            };
            let mut s = state.lock().unwrap();
            s.risk_effective_delta = Some(r.effective_delta);
            s.risk_margin_pct = Some(r.margin_pct);
            s.risk_hedge_delta = Some(r.hedge_delta);
            s.risk_state_age_monotonic_ns = now_ns_monotonic();
        }
        "place_ack" => {
            let p: PlaceAckMsg = match serde_json::from_value(generic.extra.clone()) {
                Ok(t) => t,
                Err(_) => {
                    counters.lock().unwrap().dropped_parse_errors += 1;
                    return;
                }
            };
            let r_char = p.right.chars().next().unwrap_or('C').to_ascii_uppercase();
            let s_char = match p.side.as_str() {
                "BUY" => 'B',
                "SELL" => 'S',
                _ => return,
            };
            let key = (
                TraderState::strike_key(p.strike),
                p.expiry,
                r_char,
                s_char,
            );
            let mut s = state.lock().unwrap();
            if let Some(o) = s.our_orders.get_mut(&key) {
                o.order_id = Some(p.order_id);
            } else {
                s.our_orders.insert(
                    key.clone(),
                    OurOrder {
                        price: p.price,
                        send_ns: now_ns_wall(),
                        place_monotonic_ns: now_ns_monotonic(),
                        order_id: Some(p.order_id),
                    },
                );
            }
            s.orderid_to_key.insert(p.order_id, key);
        }
        // Broker emits "order_status" (the canonical name in Rust runtime);
        // legacy adapters may use "order_ack". Both routed to the same
        // terminal-state cleanup path.
        "order_status" | "order_ack" => {
            let a: OrderAckMsg = match serde_json::from_value(generic.extra.clone()) {
                Ok(t) => t,
                Err(_) => {
                    counters.lock().unwrap().dropped_parse_errors += 1;
                    return;
                }
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
                let mut s = state.lock().unwrap();
                if let Some(key) = s.orderid_to_key.remove(&oid) {
                    s.our_orders.remove(&key);
                }
            }
        }
        "kill" => {
            let extra = &generic.extra;
            let src = extra.get("source").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let reason = extra.get("reason").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            state.lock().unwrap().kills.insert(src, reason);
        }
        "resume" => {
            let extra = &generic.extra;
            let src = extra.get("source").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            state.lock().unwrap().kills.remove(&src);
        }
        "weekend_pause" => {
            let extra = &generic.extra;
            let paused = extra.get("paused").and_then(|v| v.as_bool()).unwrap_or(false);
            state.lock().unwrap().weekend_paused = paused;
        }
        "hello" => {
            let extra = &generic.extra;
            log::warn!("broker hello: {}", extra);
            if let Some(cfg) = extra.get("config").and_then(|v| v.as_object()) {
                let mut s = state.lock().unwrap();
                if let Some(v) = cfg.get("min_edge_ticks").and_then(|x| x.as_i64()) {
                    s.min_edge_ticks = v as i32;
                }
                if let Some(v) = cfg.get("tick_size").and_then(|x| x.as_f64()) {
                    s.tick_size = v;
                }
                if let Some(v) = cfg.get("delta_ceiling").and_then(|x| x.as_f64()) {
                    s.delta_ceiling = v;
                }
                if let Some(v) = cfg.get("delta_kill").and_then(|x| x.as_f64()) {
                    s.delta_kill = v;
                }
                if let Some(v) = cfg.get("margin_ceiling_pct").and_then(|x| x.as_f64()) {
                    s.margin_ceiling_pct = v;
                }
            }
        }
        _ => {
            // Unknown / not-yet-handled type — ignore.
        }
    }
}

fn on_tick(
    state: &Arc<Mutex<TraderState>>,
    counters: &Arc<Mutex<DecisionCounters>>,
    commands_ring: &Arc<Mutex<Ring>>,
    tick: &TickMsg,
    decisions_log: &Arc<jsonl::JsonlWriter>,
) {
    let now_mono = now_ns_monotonic();
    let send_ns_wall = now_ns_wall();

    // Hot-path optimization (cleanup pass 14, 2026-05-01): consolidate
    // mutex acquisitions. Previously took the state lock 5-7 times per
    // tick (insert tick, decide, JSONL write, TTT push, cancel, place,
    // our_orders insert). Now: one extended critical section that does
    // tick insert + decide + TTT push, releases, then unlocked work
    // (JSONL write, ring writes), then a final brief lock for our_orders
    // insert. Cuts mutex churn ~70%.
    let (decisions, forward) = {
        let mut s = state.lock().unwrap();
        let mut c = counters.lock().unwrap();
        let r_char = tick.right.chars().next().unwrap_or('C').to_ascii_uppercase();
        // Slim option-state insert. NO TickMsg clone (saves 2 String
        // allocs); key built with right-as-char (saves 1 alloc); value
        // is OptionState (8 fields, all stack/Copy types).
        let opt_state = crate::state::OptionState {
            strike: tick.strike,
            bid: tick.bid,
            ask: tick.ask,
            bid_size: tick.bid_size,
            ask_size: tick.ask_size,
            ts_ns: tick.ts_ns,
            broker_recv_ns: tick.broker_recv_ns,
        };
        s.options.insert(
            (TraderState::strike_key(tick.strike), tick.expiry.clone(), r_char),
            opt_state,
        );
        let forward = s.underlying_price;
        let decisions = decide_on_tick(&mut s, &mut c, tick, now_mono);

        // TTT push if we're going to send (cheap; under same lock).
        if !decisions.is_empty() && places_orders() {
            if let Some(emit_ns) = tick.ts_ns {
                let lat = send_ns_wall.saturating_sub(emit_ns) / 1000;
                if lat < 5_000_000 {
                    s.ttt_us.push_back(lat);
                    if s.ttt_us.len() > 500 {
                        s.ttt_us.pop_front();
                    }
                }
            }
        }
        (decisions, forward)
    };

    // Log decisions to JSONL — UNLOCKED. JSONL writer is mpsc-async;
    // hot path doesn't block on disk.
    let recv_ts = chrono::Utc::now().to_rfc3339();
    for d in &decisions {
        if let Decision::Place { side, price, cancel_old_oid } = d {
            decisions_log.write(serde_json::json!({
                "recv_ts": recv_ts,
                "trigger_ts_ns": tick.ts_ns,
                "forward": forward,
                "decision": {
                    "action": "place",
                    "side": side.as_str(),
                    "strike": tick.strike,
                    "expiry": tick.expiry,
                    "right": tick.right,
                    "price": price,
                    "cancel_old_oid": cancel_old_oid,
                },
            }));
        }
    }

    if decisions.is_empty() || !places_orders() {
        return;
    }

    // Build all outbound frames first (msgpack encoding) — UNLOCKED.
    // Then take the locks just once each at the end. Reduces contention
    // with staleness/telemetry tasks.
    let mut frames: Vec<Vec<u8>> = Vec::with_capacity(decisions.len() * 2);
    // Track outcomes: (key, new_state, optional_old_orderid_to_evict).
    // For Place + cancel_old_oid we evict the old orderid mapping; for
    // Modify the orderid stays valid (same order, new price).
    let mut places_to_track: Vec<((u64, String, char, char), OurOrder, Option<i64>)> =
        Vec::with_capacity(decisions.len());
    let mut modifies_to_track: Vec<((u64, String, char, char), f64, i64)> =
        Vec::with_capacity(decisions.len());
    let mut cancel_all_fired = false;
    let r_char_for_track = tick.right.chars().next().unwrap_or('C').to_ascii_uppercase();
    for d in decisions {
        match d {
            Decision::Place { side, price, cancel_old_oid } => {
                if let Some(oid) = cancel_old_oid {
                    let cancel = CancelOrder {
                        msg_type: "cancel_order",
                        ts_ns: send_ns_wall,
                        order_id: oid,
                    };
                    if let Ok(body) = rmp_serde::to_vec_named(&cancel) {
                        frames.push(ipc::protocol::pack_frame(&body));
                    }
                }
                let p = PlaceOrder {
                    msg_type: "place_order",
                    ts_ns: send_ns_wall,
                    strike: tick.strike,
                    expiry: tick.expiry.clone(),
                    right: tick.right.clone(),
                    side: side.as_str().to_string(),
                    qty: 1,
                    price,
                    order_ref: "corsair_trader_rust".into(),
                    triggering_tick_broker_recv_ns: tick.broker_recv_ns,
                };
                if let Ok(body) = rmp_serde::to_vec_named(&p) {
                    frames.push(ipc::protocol::pack_frame(&body));
                }
                let okey = (TraderState::strike_key(tick.strike),
                            tick.expiry.clone(), r_char_for_track,
                            side.as_char());
                places_to_track.push((
                    okey,
                    OurOrder {
                        price,
                        send_ns: send_ns_wall,
                        place_monotonic_ns: now_mono,
                        order_id: None,
                    },
                    cancel_old_oid,
                ));
            }
            Decision::Modify { side, order_id, price } => {
                // Single-message amend. Refresh GTD on every modify so
                // the order doesn't expire mid-update.
                let m = ModifyOrder {
                    msg_type: "modify_order",
                    ts_ns: send_ns_wall,
                    order_id,
                    price,
                    gtd_seconds: GTD_LIFETIME_S as u32,
                    triggering_tick_broker_recv_ns: tick.broker_recv_ns,
                };
                if let Ok(body) = rmp_serde::to_vec_named(&m) {
                    frames.push(ipc::protocol::pack_frame(&body));
                }
                let okey = (TraderState::strike_key(tick.strike),
                            tick.expiry.clone(), r_char_for_track,
                            side.as_char());
                modifies_to_track.push((okey, price, order_id));
            }
            Decision::CancelAll { order_ids } => {
                // Fire cancel_order for each known orderId, then clear
                // our_orders so subsequent ticks see no incumbents
                // (idempotent — we won't re-fire CancelAll once empty).
                let n = order_ids.len();
                for oid in &order_ids {
                    let cancel = CancelOrder {
                        msg_type: "cancel_order",
                        ts_ns: send_ns_wall,
                        order_id: *oid,
                    };
                    if let Ok(body) = rmp_serde::to_vec_named(&cancel) {
                        frames.push(ipc::protocol::pack_frame(&body));
                    }
                }
                log::warn!(
                    "trader: CancelAll fired ({} orders) — risk self-block",
                    n
                );
                // Schedule local our_orders flush after the ring
                // write below.
                modifies_to_track.clear();
                places_to_track.clear();
                cancel_all_fired = true;
            }
            Decision::Skip => {}
        }
    }

    // Single ring lock for ALL outbound frames in this tick.
    {
        let mut ring = commands_ring.lock().unwrap();
        for frame in &frames {
            ring.write_frame(frame);
        }
    }

    // Single state lock for all incumbency updates.
    {
        let mut s = state.lock().unwrap();
        if cancel_all_fired {
            // Wipe local incumbents — broker-side cancel will land
            // shortly, but our_orders should already reflect "no
            // longer want resting". Avoids re-firing CancelAll on
            // subsequent ticks during the in-flight cancel window
            // and prevents the next tick from trying to amend
            // orders we just cancelled.
            s.our_orders.clear();
            s.orderid_to_key.clear();
        }
        for (key, order, cancel_old_oid) in places_to_track {
            if let Some(oid) = cancel_old_oid {
                s.orderid_to_key.remove(&oid);
            }
            s.our_orders.insert(key, order);
        }
        // Modify path: update price + cooldown timestamp on the
        // existing entry. order_id stays the same; do NOT remove the
        // orderid_to_key mapping (same order at IBKR).
        for (key, new_price, order_id) in modifies_to_track {
            if let Some(o) = s.our_orders.get_mut(&key) {
                o.price = new_price;
                o.send_ns = send_ns_wall;
                o.place_monotonic_ns = now_mono;
                // order_id stays Some(order_id); confirmed for clarity.
                debug_assert_eq!(o.order_id, Some(order_id));
            }
        }
    }
}

/// Periodic staleness check — cancel any resting order whose
/// current theo has drifted more than STALENESS_TICKS from the
/// order's price. Mirrors Python's staleness_loop in
/// src/trader/main.py. Runs at 10Hz from a tokio task.
fn staleness_check(
    state: &Arc<Mutex<TraderState>>,
    counters: &Arc<Mutex<DecisionCounters>>,
    commands_ring: &Arc<Mutex<Ring>>,
) {
    use crate::decision::{compute_theo, time_to_expiry_years, STALENESS_TICKS};

    // Snapshot the orders to check (avoid holding lock during sends).
    // For each (key, OurOrder), compute current theo using fit-time
    // forward + vol_surface. Cancel if order is too far off.
    struct ToCancel {
        order_id: i64,
        key: (u64, String, char, char),
        reason_dark: bool,
    }
    let mut cancels: Vec<ToCancel> = Vec::new();

    {
        let s = state.lock().unwrap();
        let tick_size = s.tick_size;
        let threshold = STALENESS_TICKS as f64 * tick_size;
        for (key, order) in s.our_orders.iter() {
            let order_id = match order.order_id {
                Some(o) => o,
                None => continue, // unack'd; can't cancel yet
            };
            let strike = f64::from_bits(key.0);
            let expiry = &key.1;
            let r_char = key.2;
            let s_char = key.3;

            // Look up vol surface for this option.
            let vp = s
                .vol_surfaces
                .get(&(expiry.clone(), r_char))
                .or_else(|| s.vol_surfaces.get(&(expiry.clone(), 'C')))
                .or_else(|| s.vol_surfaces.get(&(expiry.clone(), 'P')));
            let vp = match vp {
                Some(v) => v,
                None => continue,
            };
            let tte = match time_to_expiry_years(expiry) {
                Some(t) if t > 0.0 => t,
                _ => continue,
            };
            // Use fit-time forward (anchored point for SVI).
            let res = match compute_theo(vp.forward, strike, tte, r_char, &vp.params) {
                Some(v) => v,
                None => continue,
            };
            let theo = res.1;

            // Stale if our price is too unfavorable vs current theo.
            // BUY: bad when we'd pay above theo (price > theo).
            // SELL: bad when we'd sell below theo (price < theo).
            let drift = if s_char == 'B' {
                order.price - theo
            } else {
                theo - order.price
            };
            if drift > threshold {
                cancels.push(ToCancel {
                    order_id,
                    key: key.clone(),
                    reason_dark: false,
                });
                continue;
            }

            // Dark-book ON-REST guard (mirror Python). Cancel if
            // latest tick state for this contract has gone dark.
            let opt_key = (key.0, expiry.clone(), r_char);
            if let Some(latest) = s.options.get(&opt_key) {
                let bid_alive = matches!(latest.bid, Some(b) if b > 0.0);
                let ask_alive = matches!(latest.ask, Some(a) if a > 0.0);
                let bsz = latest.bid_size.unwrap_or(0);
                let asz = latest.ask_size.unwrap_or(0);
                let market_dark = !bid_alive || !ask_alive || bsz <= 0 || asz <= 0;
                if market_dark {
                    cancels.push(ToCancel {
                        order_id,
                        key: key.clone(),
                        reason_dark: true,
                    });
                }
            }
        }
    }

    if cancels.is_empty() {
        return;
    }

    // Send cancels and update local state.
    {
        let mut s = state.lock().unwrap();
        let mut c = counters.lock().unwrap();
        for tc in cancels {
            let cancel = CancelOrder {
                msg_type: "cancel_order",
                ts_ns: now_ns_wall(),
                order_id: tc.order_id,
            };
            if let Ok(body) = rmp_serde::to_vec_named(&cancel) {
                let frame = ipc::protocol::pack_frame(&body);
                commands_ring.lock().unwrap().write_frame(&frame);
            }
            s.our_orders.remove(&tc.key);
            s.orderid_to_key.remove(&tc.order_id);
            if tc.reason_dark {
                c.staleness_cancel_dark += 1;
            } else {
                c.staleness_cancel += 1;
            }
        }
    }
}

fn build_telemetry(
    state: &Arc<Mutex<TraderState>>,
    counters: &Arc<Mutex<DecisionCounters>>,
    total_events_running: &mut u64,
) -> (Telemetry, u64) {
    let s = state.lock().unwrap();
    let c = counters.lock().unwrap();
    let mut ipc_sorted: Vec<u64> = s.ipc_us.iter().copied().collect();
    ipc_sorted.sort_unstable();
    let mut ttt_sorted: Vec<u64> = s.ttt_us.iter().copied().collect();
    ttt_sorted.sort_unstable();
    let pct = |v: &[u64], q: f64| -> Option<u64> {
        if v.is_empty() {
            None
        } else {
            let idx = ((v.len() as f64) * q) as usize;
            Some(v[idx.min(v.len() - 1)])
        }
    };
    let n_events = s.options.len() as u64; // approx; real count in process loop
    *total_events_running += n_events;
    (
        Telemetry {
            msg_type: "telemetry",
            ts_ns: now_ns_wall(),
            events: serde_json::json!({}),
            decisions: c.to_json(),
            ipc_p50_us: pct(&ipc_sorted, 0.50),
            ipc_p99_us: pct(&ipc_sorted, 0.99),
            ipc_n: ipc_sorted.len(),
            ttt_p50_us: pct(&ttt_sorted, 0.50),
            ttt_p99_us: pct(&ttt_sorted, 0.99),
            ttt_n: ttt_sorted.len(),
            n_options: s.options.len(),
            n_active_orders: s.our_orders.len(),
            n_vol_expiries: s.vol_surfaces.len(),
            killed: s.kills.keys().cloned().collect(),
            weekend_paused: s.weekend_paused,
        },
        *total_events_running,
    )
}
