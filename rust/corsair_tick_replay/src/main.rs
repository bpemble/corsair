//! Synthetic tick replay harness.
//!
//! Stands in for `corsair_broker_rs` so the trader can be benchmarked
//! against a deterministic input. Reads a `trader_events-*.jsonl`
//! recording (the same file the trader writes during live runs) and
//! replays each event through the broker→trader IPC events ring at
//! configurable cadence. The trader runs unmodified; its normal
//! telemetry (TTT/IPC/decision counters) reports the response.
//!
//! Why: paper-market liquidity is wildly variable, so live-run
//! latency numbers carry a lot of noise. Replaying a fixed recording
//! gives reproducible measurements — necessary for A/B-testing
//! latency changes (RT kernel, lock-shard variants, etc.) without
//! the input-distribution shift confounding the result.
//!
//! Usage:
//!   corsair_tick_replay <events.jsonl> [--rate <hz>] [--loop]
//!                                       [--ipc-base <path>]
//!
//! Defaults:
//!   --rate    : original recording timing (use ts_ns deltas)
//!   --loop    : single pass, then exit
//!   --ipc-base: /app/data/corsair_ipc  (matches broker default)
//!
//! Operator workflow:
//!   1. Stop the real broker so the ring path is free:
//!        docker compose stop corsair-broker-rs
//!   2. Run the harness against an existing JSONL:
//!        corsair_tick_replay logs-paper/trader_events-2026-05-04.jsonl
//!   3. Restart the trader pointed at the same ring path
//!      (default works; otherwise CORSAIR_IPC_BASE env).
//!   4. Read trader telemetry as usual to see TTT/IPC under
//!      controlled input.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use corsair_ipc::{ServerConfig, SHMServer};
use serde_json::Value;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Current epoch nanoseconds (matches trader's `now_ns_wall`).
fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[derive(Default)]
struct Args {
    jsonl_path: Option<PathBuf>,
    rate_hz: Option<f64>,
    loop_replay: bool,
    ipc_base: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut out = Args {
        ipc_base: PathBuf::from(
            std::env::var("CORSAIR_IPC_BASE")
                .unwrap_or_else(|_| "/app/data/corsair_ipc".into()),
        ),
        ..Default::default()
    };
    let mut argv = std::env::args().skip(1);
    while let Some(a) = argv.next() {
        match a.as_str() {
            "--rate" => {
                let v = argv.next().ok_or("--rate <hz>")?;
                out.rate_hz = Some(v.parse().map_err(|e| format!("--rate: {e}"))?);
            }
            "--loop" => out.loop_replay = true,
            "--ipc-base" => {
                out.ipc_base =
                    PathBuf::from(argv.next().ok_or("--ipc-base <path>")?);
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: corsair_tick_replay <events.jsonl> [--rate hz] [--loop] [--ipc-base path]"
                );
                std::process::exit(0);
            }
            s if s.starts_with("--") => return Err(format!("unknown flag: {s}")),
            s => {
                if out.jsonl_path.is_some() {
                    return Err(format!("unexpected positional: {s}"));
                }
                out.jsonl_path = Some(PathBuf::from(s));
            }
        }
    }
    if out.jsonl_path.is_none() {
        return Err("usage: corsair_tick_replay <events.jsonl> [...]".into());
    }
    Ok(out)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = parse_args()?;
    let path = args.jsonl_path.as_ref().unwrap();

    log::warn!(
        "corsair_tick_replay: reading {} → ipc_base={} rate={} loop={}",
        path.display(),
        args.ipc_base.display(),
        args.rate_hz
            .map(|r| format!("{:.0}/s", r))
            .unwrap_or_else(|| "original".into()),
        args.loop_replay
    );

    // Stand in for the broker — create the SHM rings + FIFOs.
    let cfg = ServerConfig {
        base_path: args.ipc_base.clone(),
        capacity: 1 << 20,
    };
    let server = SHMServer::create(cfg)?;
    log::warn!("rings created; trader can connect now");

    // Drain the commands ring continuously so the trader can place
    // orders for the duration of the replay without back-pressuring on
    // a full ring. We discard every command — the harness measures the
    // trader's emit cost, not the broker-side dispatch cost.
    // Without this, the trader's TTT push fires only until the 1 MiB
    // commands ring fills (~100s at 200 ev/s), then write_frame starts
    // returning false and TTT samples stop being captured.
    let (mut cmd_rx, _drop_rx) = server.start();
    tokio::spawn(async move {
        while let Some(_cmd) = cmd_rx.recv().await {
            // discard
        }
    });

    // Replay loop. Reads JSONL line by line. Each line has shape:
    //   {"event": {...msgpack-decoded body...}, "recv_ts": "..."}
    // We re-encode the inner `event` object as msgpack and write to
    // the events ring.
    loop {
        let f = File::open(path).await?;
        let mut reader = BufReader::new(f).lines();
        let start = Instant::now();
        let mut count: u64 = 0;
        let mut first_ts_ns: Option<u64> = None;
        let interval_ns = args.rate_hz.map(|hz| (1e9 / hz) as u64);

        while let Some(line) = reader.next_line().await? {
            let outer: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    log::debug!("skip malformed JSONL line: {e}");
                    continue;
                }
            };
            let mut event = match outer.get("event") {
                Some(e) => e.clone(),
                None => continue,
            };

            // Capture the recording's original ts_ns BEFORE overwriting
            // it — pacing reads this to reproduce the inter-arrival
            // shape of the original session. If we rewrite first, the
            // pacing arithmetic collapses (every event's "delta" from
            // first_ts_ns becomes 0) and the harness blasts at full
            // speed, backing up the ring and inflating measured IPC
            // by 100s of ms.
            let original_ts_ns = event
                .get("ts_ns")
                .and_then(|v| v.as_u64());

            // Pace the replay. Two modes:
            //   - rate_hz: fixed cadence; sleep until N/rate_hz seconds
            //     have elapsed since start.
            //   - original: use the recording's `ts_ns` deltas; replays
            //     in real time (paper recording at ~5-30 events/sec).
            if let Some(iv) = interval_ns {
                let target = start + Duration::from_nanos(iv * count);
                let now_inst = Instant::now();
                if target > now_inst {
                    tokio::time::sleep(target - now_inst).await;
                }
            } else if let Some(ts_ns) = original_ts_ns {
                let first = *first_ts_ns.get_or_insert(ts_ns);
                let elapsed_recording_ns = ts_ns.saturating_sub(first);
                let target = start + Duration::from_nanos(elapsed_recording_ns);
                let now_inst = Instant::now();
                if target > now_inst {
                    tokio::time::sleep(target - now_inst).await;
                }
            }

            // Rewrite ts_ns / broker_recv_ns to "now" right before
            // publish so the trader's IPC histogram reflects the
            // replay→trader latency, not the age of the recording.
            // Done AFTER pacing so the original timestamp drove the
            // sleep above.
            let now = now_ns();
            if let Some(obj) = event.as_object_mut() {
                if obj.contains_key("ts_ns") {
                    obj.insert("ts_ns".to_string(), Value::Number(now.into()));
                }
                if obj.contains_key("broker_recv_ns") {
                    obj.insert(
                        "broker_recv_ns".to_string(),
                        Value::Number(now.into()),
                    );
                }
            }
            let body = match rmp_serde::to_vec_named(&event) {
                Ok(b) => b,
                Err(e) => {
                    log::debug!("skip — re-encode failed: {e}");
                    continue;
                }
            };
            // SHMServer::publish does the 4-byte length framing
            // internally; pass the raw msgpack body.
            server.publish(&body);
            count += 1;
        }

        log::warn!(
            "replay pass done: {} events in {:.1}s ({:.0}/s avg)",
            count,
            start.elapsed().as_secs_f64(),
            count as f64 / start.elapsed().as_secs_f64()
        );

        if !args.loop_replay {
            break;
        }
    }

    Ok(())
}
