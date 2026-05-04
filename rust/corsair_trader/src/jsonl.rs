//! Append-only JSONL writer with daily + size-based rotation.
//! Mirror of src/trader/main.py:JSONLWriter.
//!
//! Background-thread design: the hot path sends a typed `LogPayload`
//! over an mpsc channel; a tokio task drains, formats the ISO `recv_ts`
//! from a u64 ns timestamp, and serializes JSON to disk. Hot path
//! never blocks on disk I/O nor pays for chrono::to_rfc3339 nor
//! serde_json::Value tree construction.
//!
//! Channel is bounded (10k); on overflow we drop the message and bump
//! a counter (preferred over backpressuring the decision loop).

use chrono::{Datelike, SecondsFormat, TimeZone, Utc};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

const MAX_BYTES_PER_FILE: u64 = 256 * 1024 * 1024; // 256 MiB
const CHANNEL_CAPACITY: usize = 10_000;

/// Hot-path payload variants. Both carry `recv_ns` (u64 epoch
/// nanoseconds — `now_ns_wall()` at enqueue time); the writer formats
/// to RFC3339 ISO so consumers (`parity_compare.py`,
/// `cut_over_preflight.py`) keep their `recv_ts` schema unchanged.
pub enum LogPayload {
    /// Raw event body — writer decodes msgpack → JSON for the line.
    /// Defers msgpack→Value tree allocation off the hot path.
    Event { recv_ns: u64, body: Vec<u8> },
    /// Decision struct — writer wraps with `recv_ts` and serializes.
    Decision(DecisionLog),
}

/// Mirror of the Python `trader_decisions` schema. Plain struct (no
/// `Serialize` derive) — the writer task wraps it in a `json!` macro
/// with `recv_ts` ISO format so consumers see the same line shape.
/// Avoids needing the serde `rc` feature for `Arc<str>` support.
pub struct DecisionLog {
    /// Hot-path stamp; writer formats to ISO before serializing.
    pub recv_ns: u64,
    pub trigger_ts_ns: Option<u64>,
    pub forward: f64,
    pub decision: DecisionInner,
}

pub struct DecisionInner {
    pub action: &'static str,
    pub side: &'static str,
    pub strike: f64,
    /// `Arc<str>` clones cost an Arc bump, not an alloc.
    pub expiry: Arc<str>,
    /// One-char string like "C"/"P". Stored as `char`; serde emits
    /// as a single-char string in JSON (matches the prior schema).
    pub right: char,
    pub price: f64,
    pub cancel_old_oid: Option<i64>,
}

pub struct JsonlWriter {
    sender: mpsc::Sender<LogPayload>,
    pub dropped: Arc<AtomicU64>,
}

impl JsonlWriter {
    /// Spawn the background writer task and return a handle.
    pub fn start(log_dir: PathBuf, prefix: &'static str) -> Self {
        let (tx, rx) = mpsc::channel::<LogPayload>(CHANNEL_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_clone = Arc::clone(&dropped);
        tokio::spawn(async move {
            writer_task(log_dir, prefix, rx, dropped_clone).await;
        });
        Self { sender: tx, dropped }
    }

    /// Hot-path entry. Non-blocking: drops on full channel and bumps
    /// the dropped counter. Returns false if dropped.
    pub fn write(&self, value: LogPayload) -> bool {
        match self.sender.try_send(value) {
            Ok(()) => true,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }
}

/// Format a u64 epoch-ns timestamp as RFC3339 with millisecond
/// precision. Matches the Python writer's chrono ISO output that
/// `parity_compare.py` expects.
fn format_iso(ns: u64) -> String {
    let secs = (ns / 1_000_000_000) as i64;
    let sub = (ns % 1_000_000_000) as u32;
    let dt = match Utc.timestamp_opt(secs, sub).single() {
        Some(d) => d,
        None => return String::new(),
    };
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

async fn writer_task(
    log_dir: PathBuf,
    prefix: &'static str,
    mut rx: mpsc::Receiver<LogPayload>,
    dropped: Arc<AtomicU64>,
) {
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        log::warn!("jsonl: create log_dir {:?} failed: {}", log_dir, e);
        return;
    }

    let mut writer: Option<BufWriter<File>> = None;
    let mut current_day: Option<String> = None;
    let mut current_part: u32 = 0;
    let mut current_size: u64 = 0;
    let mut last_dropped_logged: u64 = 0;
    let mut writes_since_flush: u32 = 0;
    const WRITES_PER_FLUSH: u32 = 100;
    // Reusable serialization buffer — saves one Vec<u8> alloc per line.
    let mut line_buf: Vec<u8> = Vec::with_capacity(4096);

    while let Some(payload) = rx.recv().await {
        let today = Utc::now();
        let day_str = format!(
            "{:04}-{:02}-{:02}",
            today.year(),
            today.month(),
            today.day()
        );

        let need_roll = match &current_day {
            None => true,
            Some(d) if d != &day_str => {
                current_part = 0;
                true
            }
            _ if current_size >= MAX_BYTES_PER_FILE => {
                current_part += 1;
                true
            }
            _ => false,
        };
        if need_roll {
            if let Some(mut w) = writer.take() {
                let _ = w.flush();
            }
            let suffix = if current_part == 0 {
                String::new()
            } else {
                format!(".{}", current_part)
            };
            let path = log_dir.join(format!("{}-{}.jsonl{}", prefix, day_str, suffix));
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => {
                    let metadata_size = f.metadata().map(|m| m.len()).unwrap_or(0);
                    current_size = metadata_size;
                    writer = Some(BufWriter::new(f));
                    log::info!(
                        "jsonl: opened {} (existing {} bytes)",
                        path.display(),
                        metadata_size
                    );
                }
                Err(e) => {
                    log::warn!("jsonl: open {} failed: {}", path.display(), e);
                    writer = None;
                }
            }
            current_day = Some(day_str);
        }

        if let Some(w) = writer.as_mut() {
            line_buf.clear();
            let serialize_res = match payload {
                LogPayload::Event { recv_ns, body } => {
                    // msgpack → serde_json::Value, then wrap with recv_ts.
                    let event_value: serde_json::Value =
                        match rmp_serde::from_slice(&body) {
                            Ok(v) => v,
                            Err(e) => {
                                log::warn!("jsonl: event msgpack decode failed: {}", e);
                                continue;
                            }
                        };
                    let line = serde_json::json!({
                        "recv_ts": format_iso(recv_ns),
                        "event": event_value,
                    });
                    serde_json::to_writer(&mut line_buf, &line)
                }
                LogPayload::Decision(d) => {
                    // Wrap the typed struct with recv_ts ISO so the
                    // schema matches the Python form.
                    let line = serde_json::json!({
                        "recv_ts": format_iso(d.recv_ns),
                        "trigger_ts_ns": d.trigger_ts_ns,
                        "forward": d.forward,
                        "decision": {
                            "action": d.decision.action,
                            "side": d.decision.side,
                            "strike": d.decision.strike,
                            "expiry": d.decision.expiry.as_ref(),
                            "right": d.decision.right.to_string(),
                            "price": d.decision.price,
                            "cancel_old_oid": d.decision.cancel_old_oid,
                        },
                    });
                    serde_json::to_writer(&mut line_buf, &line)
                }
            };
            if let Err(e) = serialize_res {
                log::warn!("jsonl: serialize failed: {}", e);
                continue;
            }
            line_buf.push(b'\n');
            if let Err(e) = w.write_all(&line_buf) {
                log::warn!("jsonl: write failed: {}", e);
            } else {
                current_size += line_buf.len() as u64;
            }
            let d = dropped.load(Ordering::Relaxed);
            if d > last_dropped_logged + 100 {
                log::warn!("jsonl({}): dropped {} messages cumulative", prefix, d);
                last_dropped_logged = d;
            }
            writes_since_flush += 1;
            if writes_since_flush >= WRITES_PER_FLUSH {
                let _ = w.flush();
                writes_since_flush = 0;
            }
        }
    }
}
