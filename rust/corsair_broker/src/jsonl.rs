//! Append-only JSONL writer with daily + size-based rotation.
//! Mirror of corsair_trader::jsonl. Used for wire_timing-YYYY-MM-DD.jsonl.
//!
//! Background-task design: hot path sends serde_json::Value over a
//! bounded mpsc channel; a tokio task drains and writes. Hot path
//! never blocks on disk I/O. On overflow we drop the message and
//! bump a counter (preferred over backpressuring handle_place).

use chrono::{Datelike, Utc};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

const MAX_BYTES_PER_FILE: u64 = 256 * 1024 * 1024;
const CHANNEL_CAPACITY: usize = 10_000;

pub struct JsonlWriter {
    sender: mpsc::Sender<serde_json::Value>,
    pub dropped: Arc<AtomicU64>,
}

impl JsonlWriter {
    pub fn start(log_dir: PathBuf, prefix: &'static str) -> Self {
        let (tx, rx) = mpsc::channel::<serde_json::Value>(CHANNEL_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_clone = Arc::clone(&dropped);
        tokio::spawn(async move {
            writer_task(log_dir, prefix, rx, dropped_clone).await;
        });
        Self { sender: tx, dropped }
    }

    /// Hot-path entry. Non-blocking: drops on full channel.
    pub fn write(&self, value: serde_json::Value) -> bool {
        match self.sender.try_send(value) {
            Ok(()) => true,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }
}

async fn writer_task(
    log_dir: PathBuf,
    prefix: &'static str,
    mut rx: mpsc::Receiver<serde_json::Value>,
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
    // Reusable serialization buffer — saves one Vec<u8> alloc per
    // line at the cost of a single 1 KiB heap reservation. For
    // wire_timing rows (~250 bytes) the buffer never reallocs.
    let mut line_buf: Vec<u8> = Vec::with_capacity(1024);

    while let Some(value) = rx.recv().await {
        // Daily file boundary is UTC, NOT the CME session-anchored
        // 17:00 CT rollover. UTC is location-independent — the day
        // boundary stays stable if the service relocates (e.g., NYC
        // colo, off-hours batch reprocessing across regions). Session-
        // local groupings (CT for CME audit trails, ET for NYC
        // operations) are produced downstream by post-processors that
        // know their own timezone.
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
            if let Err(e) = serde_json::to_writer(&mut line_buf, &value) {
                log::warn!("jsonl: serialize failed: {}", e);
                continue;
            }
            line_buf.push(b'\n');
            if let Err(e) = w.write_all(&line_buf) {
                log::warn!("jsonl: write failed: {}", e);
                // Re-sync `current_size` from disk so size-based
                // rotation can still fire after transient write errors.
                if let Ok(meta) = w.get_ref().metadata() {
                    current_size = meta.len();
                }
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
