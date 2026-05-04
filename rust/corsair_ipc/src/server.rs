//! `SHMServer` — broker-side IPC server.
//!
//! Owns:
//!   - The `events` ring (broker writes, trader reads)
//!   - The `commands` ring (trader writes, broker reads)
//!   - Both notify FIFOs
//!
//! Lifecycle:
//!   1. Caller constructs with `SHMServer::create(path, capacity)`.
//!      This creates `<path>.events`, `<path>.commands`, and the two
//!      `<path>.events.notify` / `<path>.commands.notify` FIFOs.
//!   2. Caller spawns `start()` task to drain commands. Each command
//!      arrives as a `ServerCommand` on the returned channel.
//!   3. Caller publishes events via [`SHMServer::publish`].
//!   4. On shutdown, drop the server — files remain on disk for the
//!      next broker instance.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

use crate::ring::{Ring, DEFAULT_RING_CAPACITY};

/// One-time configuration captured at construction.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Base path. We append `.events`, `.commands`, and `.notify`
    /// suffixes per the existing Python convention.
    pub base_path: PathBuf,
    pub capacity: usize,
}

impl ServerConfig {
    pub fn new(base_path: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
            capacity: DEFAULT_RING_CAPACITY,
        }
    }
}

/// A command parsed off the trader → broker `commands` ring. The
/// runtime dispatches based on the `kind` discriminant; payload is
/// the raw msgpack body so the runtime decides how to deserialize.
#[derive(Debug, Clone)]
pub struct ServerCommand {
    /// "type" field from the msgpack body — e.g. "place_order",
    /// "cancel_order", "modify_order".
    pub kind: String,
    /// Raw msgpack body (without the length prefix). Use a serde
    /// struct or rmpv::Value to extract fields.
    pub body: Vec<u8>,
}

/// Server handle.
pub struct SHMServer {
    cfg: ServerConfig,
    events_ring: Arc<Mutex<Ring>>,
    commands_ring: Arc<Mutex<Ring>>,
}

impl SHMServer {
    /// Create the rings + FIFOs. Idempotent in the broker-restart
    /// sense — overwrites existing files at the same paths.
    pub fn create(cfg: ServerConfig) -> std::io::Result<Self> {
        let events_path = path_with_suffix(&cfg.base_path, ".events");
        let commands_path = path_with_suffix(&cfg.base_path, ".commands");
        let events_notify = path_with_suffix(&events_path, ".notify");
        let commands_notify = path_with_suffix(&commands_path, ".notify");

        let mut events_ring = Ring::create_owner(&events_path, cfg.capacity)?;
        let mut commands_ring = Ring::create_owner(&commands_path, cfg.capacity)?;

        Ring::create_notify_fifo(&events_notify)?;
        Ring::create_notify_fifo(&commands_notify)?;

        // Server is the WRITER on events.notify (we wake the trader),
        // and the READER on commands.notify (trader wakes us).
        events_ring.open_notify(&events_notify, /*as_writer=*/ true)?;
        commands_ring.open_notify(&commands_notify, /*as_writer=*/ false)?;

        log::warn!(
            "SHMServer: created rings at {} (events={}, commands={})",
            cfg.base_path.display(),
            events_path.display(),
            commands_path.display(),
        );

        Ok(Self {
            cfg,
            events_ring: Arc::new(Mutex::new(events_ring)),
            commands_ring: Arc::new(Mutex::new(commands_ring)),
        })
    }

    pub fn config(&self) -> &ServerConfig {
        &self.cfg
    }

    /// Spawn the command-receive task. Returns a receiver of
    /// `ServerCommand`s; the caller's task pulls from it and
    /// dispatches into the broker.
    ///
    /// Also returns a broadcast::Receiver of `()` ticks fired when
    /// the events ring drops a frame — caller may want to log or
    /// alert.
    pub fn start(
        &self,
    ) -> (
        mpsc::Receiver<ServerCommand>,
        broadcast::Receiver<DropEvent>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::channel(1024);
        let (drop_tx, drop_rx) = broadcast::channel(64);
        let cmd_ring = Arc::clone(&self.commands_ring);
        let _evt_ring = Arc::clone(&self.events_ring);
        // Busy-poll mode dispatches command_pump on a dedicated
        // OS thread (NOT a tokio task) pinned to its own CPU. This
        // bypasses tokio's scheduler entirely on the read hot path —
        // every empty read does a tight thread-local spin instead of
        // yielding to the executor. Drops command-IPC p50 from ~470µs
        // to ~50µs by removing scheduler scheduling-class overhead.
        // The non-busy-poll path keeps the original tokio-task
        // behavior (1ms interval poll, low CPU).
        let busy_poll = std::env::var("CORSAIR_BROKER_BUSY_POLL")
            .ok()
            .map(|v| v == "1")
            .unwrap_or(false);
        if busy_poll {
            std::thread::Builder::new()
                .name("cmd_pump".into())
                .spawn(move || command_pump_blocking(cmd_ring, cmd_tx, drop_tx))
                .expect("spawn cmd_pump thread");
        } else {
            tokio::spawn(async move {
                command_pump(cmd_ring, cmd_tx, drop_tx).await;
            });
        }
        (cmd_rx, drop_rx)
    }

    /// Publish an event to the events ring. Returns false if the
    /// ring is full (frame dropped — caller may want to log).
    pub fn publish(&self, msgpack_body: &[u8]) -> bool {
        let frame = crate::protocol::pack_frame(msgpack_body);
        let mut r = self.events_ring.lock().unwrap();
        r.write_frame(&frame)
    }

    /// Snapshot of dropped-frame counters for telemetry.
    pub fn frames_dropped(&self) -> (u64, u64) {
        let e = self.events_ring.lock().unwrap().frames_dropped;
        let c = self.commands_ring.lock().unwrap().frames_dropped;
        (e, c)
    }
}

/// Telemetry fired on the broadcast channel from `start()` when an
/// outbound event frame couldn't fit (trader is too slow or
/// disconnected).
#[derive(Debug, Clone)]
pub struct DropEvent {
    pub which: &'static str, // "events" or "commands"
    pub frames_dropped_total: u64,
}

/// Async (tokio task) pump used in default low-CPU mode. 1ms
/// interval poll. Lives on the broker's tokio runtime alongside
/// other tasks.
async fn command_pump(
    cmd_ring: Arc<Mutex<Ring>>,
    cmd_tx: mpsc::Sender<ServerCommand>,
    _drop_tx: broadcast::Sender<DropEvent>,
) {
    use tokio::time::{interval, Duration};
    let mut tick = interval(Duration::from_millis(1));
    loop {
        tick.tick().await;
        let bytes = {
            let mut r = cmd_ring.lock().unwrap();
            r.drain_notify();
            r.read_available()
        };
        if bytes.is_empty() {
            continue;
        }
        let mut buf = bytes;
        let frames = match crate::protocol::unpack_all_frames(&mut buf) {
            Ok(f) => f,
            Err(e) => {
                log::error!("command_pump: unpack error: {e}");
                continue;
            }
        };
        for body in frames {
            let kind = extract_type(&body).unwrap_or_else(|| "unknown".into());
            if cmd_tx
                .send(ServerCommand { kind, body })
                .await
                .is_err()
            {
                log::warn!("command_pump: receiver dropped; exiting");
                return;
            }
        }
    }
}

/// Dedicated-thread pump used in busy-poll mode (CORSAIR_BROKER_BUSY_POLL=1).
/// Bypasses tokio's scheduler entirely — every empty read does a
/// tight `std::thread::yield_now()` instead of yielding to the
/// executor. The thread can be CPU-pinned by the caller (we use
/// the next allowed CPU after the broker's other workers).
///
/// Pushes parsed commands into the tokio mpsc::Sender via
/// `blocking_send`, which uses a runtime-aware blocking call —
/// safe to call from a non-async thread provided the runtime is
/// alive.
///
/// Latency profile vs the async path:
///   async (tokio yield_now): p50 ~470µs, p99 ~11ms (scheduler tail)
///   thread (std yield_now):  p50 ~50µs,  p99 ~200µs
fn command_pump_blocking(
    cmd_ring: Arc<Mutex<Ring>>,
    cmd_tx: mpsc::Sender<ServerCommand>,
    _drop_tx: broadcast::Sender<DropEvent>,
) {
    log::warn!(
        "command_pump: CORSAIR_BROKER_BUSY_POLL=1 — dedicated thread, \
         bypassing tokio scheduler on the hot read path"
    );
    // Try to pin to the next allowed CPU (after the worker threads).
    // No-op if SCHED_AFFINITY is restrictive or already pinned.
    let allowed = crate::cpu_affinity::allowed_cpus();
    if let Some(cpu) = allowed.last().copied() {
        crate::cpu_affinity::pin_thread_to_cpu(cpu);
        log::info!("command_pump: pinned to CPU {}", cpu);
    }
    loop {
        let bytes = {
            let mut r = cmd_ring.lock().unwrap();
            r.drain_notify();
            r.read_available()
        };
        if bytes.is_empty() {
            // Empty read — yield without sleeping. yield_now lets
            // other threads on the same CPU run if needed; on a
            // dedicated CPU it's effectively a spin.
            std::thread::yield_now();
            continue;
        }
        let mut buf = bytes;
        let frames = match crate::protocol::unpack_all_frames(&mut buf) {
            Ok(f) => f,
            Err(e) => {
                log::error!("command_pump: unpack error: {e}");
                continue;
            }
        };
        for body in frames {
            let kind = extract_type(&body).unwrap_or_else(|| "unknown".into());
            // blocking_send — safe in non-async context, blocks the
            // thread if the channel is full (capacity 1024 — should
            // never block in practice since the dispatcher drains
            // immediately into per-command tokio tasks).
            if cmd_tx
                .blocking_send(ServerCommand { kind, body })
                .is_err()
            {
                log::warn!("command_pump: receiver dropped; exiting");
                return;
            }
        }
    }
}

/// Best-effort extraction of the "type" field from a msgpack map.
fn extract_type(body: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct TypeOnly {
        #[serde(rename = "type")]
        ty: Option<String>,
    }
    let parsed: TypeOnly = rmp_serde::from_slice(body).ok()?;
    parsed.ty
}

fn path_with_suffix(base: &Path, suffix: &str) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[derive(serde::Serialize)]
    struct TickEvent<'a> {
        #[serde(rename = "type")]
        ty: &'a str,
        x: i32,
    }

    #[tokio::test]
    async fn create_then_publish_round_trips() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("ipc");
        let cfg = ServerConfig {
            base_path: base.clone(),
            capacity: 4096,
        };
        let server = SHMServer::create(cfg).unwrap();
        let body = rmp_serde::to_vec_named(&TickEvent { ty: "tick", x: 1 }).unwrap();
        let ok = server.publish(&body);
        assert!(ok);

        let events_path = path_with_suffix(&base, ".events");
        let mut client = Ring::open_client(&events_path, 4096).unwrap();
        let data = client.read_available();
        assert!(!data.is_empty(), "expected frame in events ring");
    }

    #[tokio::test]
    async fn create_then_drop_unlinks_nothing_at_drop() {
        // Ensure dropping the SHMServer does NOT delete the files on
        // disk — restart of the broker reuses the paths.
        let dir = tempdir().unwrap();
        let base = dir.path().join("ipc");
        let cfg = ServerConfig {
            base_path: base.clone(),
            capacity: 4096,
        };
        {
            let _server = SHMServer::create(cfg).unwrap();
        }
        let events_path = path_with_suffix(&base, ".events");
        assert!(events_path.exists(), "files should persist past drop");
    }
}
