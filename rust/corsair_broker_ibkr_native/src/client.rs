//! `NativeClient` — TCP socket + handshake + recv loop.
//!
//! Phase 6.1 deliverable: connect, handshake, recv loop. Future
//! phases add per-message-type request/response and the
//! `corsair_broker_api::Broker` trait impl.

use bytes::BytesMut;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};

use crate::codec::{
    encode_fields, encode_handshake, parse_int, try_decode_all, write_frame,
};
use crate::error::NativeError;
use crate::messages::{
    IN_MANAGED_ACCTS, IN_NEXT_VALID_ID, MAX_CLIENT_VERSION, MIN_SERVER_VERSION,
    OUT_START_API,
};

/// Enable SO_TIMESTAMPNS on a TCP socket so subsequent recvmsg() calls
/// return a CLOCK_REALTIME nanosecond timestamp via SCM_TIMESTAMPNS in
/// the cmsg ancillary data. Returns true on success.
///
/// The timestamp is captured by the kernel when a packet leaves the
/// driver's RX queue, before user-space dispatch — much closer to NIC
/// ingress than the moment our recv() syscall returns. On loopback
/// (gateway → broker) the gap to user-space is small (~µs); on a real
/// NIC at colo it can be tens of µs.
fn enable_so_timestampns(fd: std::os::unix::io::RawFd) -> bool {
    unsafe {
        let on: libc::c_int = 1;
        let r = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TIMESTAMPNS,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as u32,
        );
        if r != 0 {
            log::warn!(
                "SO_TIMESTAMPNS enable failed on fd={}: {}",
                fd,
                std::io::Error::last_os_error()
            );
            false
        } else {
            true
        }
    }
}

/// Non-blocking recvmsg that also extracts the kernel's CLOCK_REALTIME
/// SCM_TIMESTAMPNS ingress timestamp. Returns (bytes_read, timestamp_ns).
/// timestamp_ns is None when SO_TIMESTAMPNS wasn't enabled (caller
/// should fall back to now_ns()).
///
/// Currently unused — the recv loop reverted to tokio's read API
/// after a readiness-clear bug caused 100% CPU spin (readable() didn't
/// auto-clear when we bypassed try_read). Re-introduce later via
/// `try_io(Interest::READABLE, |_| recvmsg(...))` so tokio handles
/// WouldBlock signaling. Kept for that future migration.
#[allow(dead_code)]
fn recvmsg_with_kernel_ts(
    fd: std::os::unix::io::RawFd,
    buf: &mut [u8],
) -> std::io::Result<(usize, Option<u64>)> {
    unsafe {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        // Reserve cmsg space for SCM_TIMESTAMPNS (one timespec).
        let cmsg_capacity =
            libc::CMSG_SPACE(std::mem::size_of::<libc::timespec>() as u32) as usize;
        let mut cmsg_buf = vec![0u8; cmsg_capacity];
        let mut hdr = libc::msghdr {
            msg_name: std::ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: &mut iov,
            msg_iovlen: 1,
            msg_control: cmsg_buf.as_mut_ptr() as *mut libc::c_void,
            msg_controllen: cmsg_capacity,
            msg_flags: 0,
        };
        let n = libc::recvmsg(fd, &mut hdr, libc::MSG_DONTWAIT);
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Walk cmsg list looking for SCM_TIMESTAMPNS.
        let mut ts_ns: Option<u64> = None;
        let mut cmsg = libc::CMSG_FIRSTHDR(&hdr);
        while !cmsg.is_null() {
            let c = &*cmsg;
            if c.cmsg_level == libc::SOL_SOCKET
                && c.cmsg_type == libc::SCM_TIMESTAMPNS
            {
                let data = libc::CMSG_DATA(cmsg) as *const libc::timespec;
                let ts = *data;
                ts_ns = Some(
                    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64,
                );
                break;
            }
            cmsg = libc::CMSG_NXTHDR(&hdr, cmsg);
        }
        Ok((n as usize, ts_ns))
    }
}

/// Configuration for a NativeClient.
#[derive(Debug, Clone)]
pub struct NativeClientConfig {
    pub host: String,
    pub port: u16,
    pub client_id: i32,
    /// Optional account selector (FA accounts).
    pub account: Option<String>,
    /// Connection timeout on the TCP layer.
    pub connect_timeout: Duration,
    /// Handshake timeout — server should reply with version + connTime
    /// within this window. ~5s is comfortable.
    pub handshake_timeout: Duration,
}

impl Default for NativeClientConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 4002,
            client_id: 0,
            account: None,
            connect_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(5),
        }
    }
}

/// Native IBKR API client. Owns the TCP socket; runs the recv loop
/// as a tokio task and dispatches inbound messages onto a channel.
///
/// Phase 6.1 surface — Phase 6.5+ adds request methods per message
/// type and impl Broker for this struct.
pub struct NativeClient {
    cfg: NativeClientConfig,
    /// Wire half: serialized writes via Mutex around the OwnedWriteHalf.
    writer: Arc<Mutex<Option<tokio::net::tcp::OwnedWriteHalf>>>,
    /// Negotiated server version, set during handshake.
    server_version: Arc<Mutex<i32>>,
    /// Connection time string from the server (for telemetry).
    conn_time: Arc<Mutex<String>>,
    /// Next valid order id from the server. Updated on
    /// nextValidId messages. Used by place_order to assign an id.
    next_order_id: Arc<Mutex<i32>>,
    /// Managed accounts list reported by the server. Useful for FA.
    managed_accounts: Arc<Mutex<Vec<String>>>,
    /// Inbound message dispatch — Phase 6.5+ consumers subscribe.
    /// Tuple is (parsed fields, kernel_recv_ns). kernel_recv_ns is
    /// the SCM_TIMESTAMPNS from the recvmsg that produced these
    /// bytes (CLOCK_REALTIME). Falls back to user-space now_ns()
    /// if SO_TIMESTAMPNS wasn't enabled or cmsg was missing.
    msg_tx: mpsc::Sender<(Vec<String>, u64)>,
    /// Telemetry: number of messages sent / received / dropped.
    msgs_sent: Arc<std::sync::atomic::AtomicU64>,
    msgs_recv: Arc<std::sync::atomic::AtomicU64>,
    msgs_dropped: Arc<std::sync::atomic::AtomicU64>,
}

/// Bounded dispatch channel capacity. IBKR streams a few-thousand
/// messages at boot (positions, accountValues × N keys) and ~hundreds
/// per second under busy market data. 16K accommodates burst without
/// unbounded growth; on overflow we drop the oldest with a counter
/// increment so the dispatcher is never stalled by a slow consumer
/// (audit P0-5 follow-on).
const DISPATCH_CHANNEL_CAP: usize = 16_384;

impl NativeClient {
    /// Construct, but don't connect yet. Caller invokes
    /// `connect().await` to establish the socket + handshake.
    /// Returns the client handle plus an mpsc::Receiver of decoded
    /// inbound messages — the runtime spawns its own task that
    /// dispatches based on the first field (message type id).
    pub fn new(cfg: NativeClientConfig) -> (Self, mpsc::Receiver<(Vec<String>, u64)>) {
        let (tx, rx) = mpsc::channel(DISPATCH_CHANNEL_CAP);
        let client = Self {
            cfg,
            writer: Arc::new(Mutex::new(None)),
            server_version: Arc::new(Mutex::new(0)),
            conn_time: Arc::new(Mutex::new(String::new())),
            next_order_id: Arc::new(Mutex::new(0)),
            managed_accounts: Arc::new(Mutex::new(Vec::new())),
            msg_tx: tx,
            msgs_sent: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            msgs_recv: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            msgs_dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        (client, rx)
    }

    pub fn config(&self) -> &NativeClientConfig {
        &self.cfg
    }

    pub async fn server_version(&self) -> i32 {
        *self.server_version.lock().await
    }

    pub async fn next_order_id(&self) -> i32 {
        *self.next_order_id.lock().await
    }

    /// Atomically allocate the next order id and advance the local
    /// counter. Returns 0 if the gateway hasn't yet streamed the
    /// initial nextValidId — caller should `wait_for_bootstrap` first.
    pub async fn alloc_order_id(&self) -> i32 {
        let mut g = self.next_order_id.lock().await;
        let id = *g;
        if id > 0 {
            *g = id + 1;
        }
        id
    }

    pub async fn managed_accounts(&self) -> Vec<String> {
        self.managed_accounts.lock().await.clone()
    }

    /// Connect: TCP handshake → API version negotiation → startApi.
    /// On return, the recv task is running and the socket is live.
    pub async fn connect(&self) -> Result<(), NativeError> {
        // 1. TCP connect.
        let addr = format!("{}:{}", self.cfg.host, self.cfg.port);
        log::warn!("native client: TCP connecting to {addr}");
        let stream = tokio::time::timeout(
            self.cfg.connect_timeout,
            TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| NativeError::HandshakeTimeout)?
        .map_err(NativeError::Io)?;
        stream.set_nodelay(true)?;
        // Enable SO_TIMESTAMPNS so the recv loop can stamp each frame
        // with the kernel's RX ingress time (much closer to NIC arrival
        // than the moment recv() returns). Both halves share the same
        // underlying socket, so setting the option pre-split is fine.
        let ts_enabled = enable_so_timestampns(stream.as_raw_fd());
        if ts_enabled {
            log::warn!(
                "native client: SO_TIMESTAMPNS enabled on gateway fd={}",
                stream.as_raw_fd()
            );
        }
        let (read_half, mut write_half) = stream.into_split();

        // 2. Send the API version handshake.
        let handshake = encode_handshake(MIN_SERVER_VERSION, MAX_CLIENT_VERSION);
        write_half.write_all(&handshake).await?;
        write_half.flush().await?;
        log::info!(
            "native client: sent handshake (v{}-v{})",
            MIN_SERVER_VERSION,
            MAX_CLIENT_VERSION
        );

        // 3. Read server's reply: serverVersion + connTime in a
        //    standard length-prefixed frame.
        let (server_version, conn_time) =
            read_handshake_reply(&read_half, self.cfg.handshake_timeout).await?;
        log::warn!(
            "native client: handshake OK (server_version={server_version}, connTime={conn_time})"
        );
        if server_version < MIN_SERVER_VERSION {
            return Err(NativeError::ServerVersionTooLow(
                server_version,
                MIN_SERVER_VERSION,
            ));
        }
        *self.server_version.lock().await = server_version;
        *self.conn_time.lock().await = conn_time.clone();

        // 4. Send startApi. After this, the server starts streaming
        //    nextValidId, managedAccounts, and any farm-status warnings.
        //
        // Field 4 is "optionalCapabilities" per the IBKR spec.
        // ib_insync passes its `self.optCapab` (default ""). We do the
        // same — passing the FA sub-account name here triggers
        // gateway error 10106 ("Enabling 'DUP...' via login is not
        // supported in TWS"). Account selection is per-order via the
        // order's `account` field, not via startApi.
        //
        // IMPORTANT: callers MUST drain the bootstrap response
        // (managedAccounts + nextValidId) before issuing user reqs.
        // Sending reqs racing the bootstrap causes the gateway to
        // disconnect silently (Phase 6.5b root cause). See
        // `wait_for_bootstrap()` below.
        let start_api = encode_fields(&[
            &OUT_START_API.to_string(),
            "2", // version
            &self.cfg.client_id.to_string(),
            "",  // optionalCapabilities — leave empty
        ]);
        write_half.write_all(&start_api).await?;
        write_half.flush().await?;
        self.msgs_sent
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        log::info!("native client: startApi sent (clientId={})", self.cfg.client_id);

        // 5. Stash the writer half for future sends.
        *self.writer.lock().await = Some(write_half);

        // 6. Spawn the recv task. It reads frames forever, parses
        //    IDs, and pushes onto msg_tx.
        spawn_recv_task(
            read_half,
            self.msg_tx.clone(),
            Arc::clone(&self.next_order_id),
            Arc::clone(&self.managed_accounts),
            Arc::clone(&self.msgs_recv),
            Arc::clone(&self.msgs_dropped),
        );
        Ok(())
    }

    /// Wait until the gateway has streamed managedAccounts AND
    /// nextValidId after startApi, or until `timeout` elapses.
    ///
    /// **Callers MUST do this before issuing any user reqs.** The
    /// IBKR gateway disconnects silently when reqs race the
    /// bootstrap on FA paper accounts (Phase 6.5b root cause).
    pub async fn wait_for_bootstrap(
        &self,
        timeout: Duration,
    ) -> Result<(), NativeError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let oid = *self.next_order_id.lock().await;
            let accts_done = !self.managed_accounts.lock().await.is_empty();
            if oid > 0 && accts_done {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(NativeError::HandshakeTimeout);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Send a raw frame on the wire (caller pre-encoded). Used by
    /// per-message-type methods in future phases. Returns
    /// NotConnected if the socket isn't live.
    pub async fn send_raw(&self, frame: &[u8]) -> Result<(), NativeError> {
        let mut guard = self.writer.lock().await;
        let writer = guard
            .as_mut()
            .ok_or_else(|| NativeError::NotConnected)?;
        write_frame(writer, frame).await?;
        self.msgs_sent
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Send pre-encoded fields. Convenience wrapper.
    pub async fn send_fields(&self, fields: &[&str]) -> Result<(), NativeError> {
        let frame = encode_fields(fields);
        self.send_raw(&frame).await
    }

    /// Disconnect cleanly. Drops the writer half (server detects EOF
    /// and tears down its end).
    pub async fn disconnect(&self) -> Result<(), NativeError> {
        let mut guard = self.writer.lock().await;
        if let Some(mut w) = guard.take() {
            // Best-effort flush + shutdown.
            let _ = w.shutdown().await;
        }
        log::info!(
            "native client: disconnected (sent={}, recv={})",
            self.msgs_sent.load(std::sync::atomic::Ordering::Relaxed),
            self.msgs_recv.load(std::sync::atomic::Ordering::Relaxed),
        );
        Ok(())
    }

    pub fn msgs_sent(&self) -> u64 {
        self.msgs_sent.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn msgs_recv(&self) -> u64 {
        self.msgs_recv.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Read the handshake reply: a single length-prefixed frame with two
/// fields — server_version and conn_time.
async fn read_handshake_reply(
    read_half: &tokio::net::tcp::OwnedReadHalf,
    timeout: Duration,
) -> Result<(i32, String), NativeError> {
    // Use a temporary cloned reader buffer for the handshake.
    // We can't actually clone OwnedReadHalf, so we read directly via
    // a small buffer + try_decode_frame loop.
    let mut buf = BytesMut::with_capacity(256);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(NativeError::HandshakeTimeout);
        }
        let remaining = deadline - now;
        let read_fut = async {
            // ReadHalf needs &mut, but we have &. Use a small trick:
            // use AsyncReadExt::read directly via raw fd? No — use
            // AsyncReadExt::read with a polling loop.
            // We yield control by using read via tokio's ready
            // semantics. Easiest path: use readable() + try_read.
            read_half.readable().await?;
            let mut tmp = [0u8; 256];
            let n = match read_half.try_read(&mut tmp) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
                Err(e) => return Err::<usize, std::io::Error>(e),
            };
            Ok::<usize, std::io::Error>(if n == 0 {
                0
            } else {
                buf.extend_from_slice(&tmp[..n]);
                n
            })
        };
        match tokio::time::timeout(remaining, read_fut).await {
            Ok(Ok(0)) => continue,
            Ok(Ok(_)) => {
                if let Some(fields) = crate::codec::try_decode_frame(&mut buf)? {
                    if fields.len() < 2 {
                        return Err(NativeError::Protocol(format!(
                            "handshake reply has {} fields, want ≥2",
                            fields.len()
                        )));
                    }
                    let v = parse_int(&fields[0])?;
                    let conn_time = fields[1].clone();
                    return Ok((v, conn_time));
                }
            }
            Ok(Err(e)) => return Err(NativeError::Io(e)),
            Err(_) => return Err(NativeError::HandshakeTimeout),
        }
    }
}

/// Spawn the inbound message recv task. Reads frames forever; on
/// each frame, parses the message type, dispatches to typed
/// handlers (for ids that we own canonical state for), and forwards
/// the raw fields onto the msg_tx channel for general consumers.
fn spawn_recv_task(
    mut read_half: tokio::net::tcp::OwnedReadHalf,
    msg_tx: mpsc::Sender<(Vec<String>, u64)>,
    next_order_id: Arc<Mutex<i32>>,
    managed_accounts: Arc<Mutex<Vec<String>>>,
    msgs_recv: Arc<std::sync::atomic::AtomicU64>,
    msgs_dropped: Arc<std::sync::atomic::AtomicU64>,
) {
    tokio::spawn(async move {
        let mut buf = BytesMut::with_capacity(64 * 1024);
        let mut tmp = [0u8; 64 * 1024];
        loop {
            // Reverted to tokio's read API. The recvmsg+SO_TIMESTAMPNS
            // path had a tokio readiness-clearing bug — readable()
            // didn't auto-clear since we bypassed try_read, leading to
            // 100% CPU spin once data flow paused. Re-introduce later
            // via try_io(Interest::READABLE, |_| recvmsg(...)) so
            // tokio's machinery handles WouldBlock/clear_ready.
            // Channel-recv `now_ns()` (in dispatcher) gives the recv_ns
            // proxy in the meantime — close enough for non-colo use.
            let n = match read_half.read(&mut tmp).await {
                Ok(0) => {
                    log::warn!("native recv: EOF");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    log::warn!("native recv: read error: {e}");
                    break;
                }
            };
            let recv_ns = now_ns();
            buf.extend_from_slice(&tmp[..n]);
            match try_decode_all(&mut buf) {
                Ok(frames) => {
                    for fields in frames {
                        msgs_recv.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Type-dispatch a few canonical messages we
                        // own state for.
                        if let Some(ty) = fields.first() {
                            if let Ok(id) = parse_int(ty) {
                                if id == IN_NEXT_VALID_ID && fields.len() >= 3 {
                                    if let Ok(oid) = parse_int(&fields[2]) {
                                        // Max-merge so an unsolicited
                                        // re-broadcast from the gateway
                                        // (IBKR sends nextValidId after
                                        // every place_order) doesn't
                                        // stomp our locally-allocated
                                        // higher counter and cause id
                                        // reuse.
                                        let mut g = next_order_id.lock().await;
                                        if oid > *g {
                                            *g = oid;
                                        }
                                        log::info!(
                                            "native recv: nextValidId = {oid} (local={})",
                                            *g
                                        );
                                    }
                                } else if id == IN_MANAGED_ACCTS && fields.len() >= 3 {
                                    let csv = &fields[2];
                                    let accts: Vec<String> = csv
                                        .split(',')
                                        .filter(|s| !s.is_empty())
                                        .map(|s| s.to_string())
                                        .collect();
                                    log::info!(
                                        "native recv: managedAccounts = {accts:?}"
                                    );
                                    *managed_accounts.lock().await = accts;
                                }
                            }
                        }
                        // Forward to subscribers. Bounded channel:
                        // try_send to avoid awaiting (which would
                        // back up the recv loop). On Full we drop the
                        // frame and increment the counter — better to
                        // lose a tick than to block ingestion.
                        match msg_tx.try_send((fields, recv_ns)) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                let dropped = msgs_dropped
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                    + 1;
                                if dropped.is_power_of_two() {
                                    log::warn!(
                                        "native recv: dispatch channel full; dropped frames total={dropped}"
                                    );
                                }
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                log::info!("native recv: dispatch channel closed");
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    log::error!("native recv: decode error: {e}");
                    break;
                }
            }
        }
        log::warn!("native recv task: exiting");
    });
}

fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn config_default_sane() {
        let c = NativeClientConfig::default();
        assert_eq!(c.port, 4002);
        assert_eq!(c.client_id, 0);
        assert_eq!(c.connect_timeout.as_secs(), 10);
    }

    #[tokio::test]
    async fn new_returns_disconnected_client() {
        let (client, _rx) = NativeClient::new(NativeClientConfig::default());
        assert_eq!(client.server_version().await, 0);
        assert_eq!(client.next_order_id().await, 0);
        // send_fields should fail with NotConnected before connect.
        let err = client.send_fields(&["1", "2"]).await.unwrap_err();
        assert!(matches!(err, NativeError::NotConnected));
    }

    #[tokio::test]
    async fn connect_to_unreachable_port_returns_io_error() {
        let mut cfg = NativeClientConfig::default();
        cfg.port = 1; // reserved/unbound — should fail fast
        cfg.connect_timeout = Duration::from_millis(500);
        let (client, _rx) = NativeClient::new(cfg);
        let err = client.connect().await.unwrap_err();
        // Either IO error or HandshakeTimeout depending on TCP behavior.
        match err {
            NativeError::Io(_) | NativeError::HandshakeTimeout => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_to_dead_address_times_out() {
        let mut cfg = NativeClientConfig::default();
        // 192.0.2.1 is RFC-5737 documentation-prefix — guaranteed
        // unreachable. TCP SYN times out.
        cfg.host = "192.0.2.1".into();
        cfg.port = 12345;
        cfg.connect_timeout = Duration::from_millis(300);
        let (client, _rx) = NativeClient::new(cfg);
        let err = client.connect().await.unwrap_err();
        assert!(matches!(err, NativeError::HandshakeTimeout));
    }
}
