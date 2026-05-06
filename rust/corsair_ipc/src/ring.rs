//! SHM ring + FIFO notification primitive.
//!
//! Two modes:
//!   - [`Ring::create_owner`] — broker creates the backing file +
//!     FIFO. Used by [`SHMServer`](crate::SHMServer).
//!   - [`Ring::open_client`] — trader maps the broker's existing
//!     file. Used by [`SHMClient`](crate::SHMClient) and currently
//!     by `corsair_trader/src/ipc/shm.rs` (will migrate to this
//!     crate in Phase 5B.5).
//!
//! # Memory model (cross-process SPSC, 2026-05-05)
//!
//! The ring is a single-producer / single-consumer queue mapped into
//! both the broker and the trader. The two participants synchronize
//! exclusively through the 16-byte header at offset 0:
//!
//! ```text
//!   bytes  0..8 : write_offset (u64 LE) — producer-owned
//!   bytes  8..16: read_offset  (u64 LE) — consumer-owned
//! ```
//!
//! Both fields are accessed via `AtomicU64` ops with explicit
//! `Acquire`/`Release` ordering on the **same address** in both
//! processes, so the kernel's mmap aliasing makes those ops act as a
//! cross-process synchronizes-with edge:
//!
//!   * **Producer (`write_frame`)**:
//!       1. Acquire-load `read_offset` to determine free space.
//!       2. Plain copy the frame bytes into the data region.
//!       3. Release-store `write_offset` — this PUBLISHES every byte
//!          written in step 2 to the consumer's subsequent
//!          Acquire-load of `write_offset`.
//!
//!   * **Consumer (`read_available`)**:
//!       1. Acquire-load `write_offset` — pairs with the producer's
//!          Release in step 3 above; bytes up to that offset are now
//!          guaranteed visible.
//!       2. Plain copy the frame bytes out.
//!       3. Release-store `read_offset` — publishes "I'm done with
//!          those bytes, you may overwrite them" to the producer.
//!
//! This is the standard SPSC dance. WITHOUT real Acquire/Release on
//! the offsets, x86 happens to behave correctly per its strong memory
//! model on a single core but the COMPILER is free to reorder the
//! data-region copies past the offset writes — silent corruption
//! under load. The `from_le_bytes`/`copy_from_slice` path that lived
//! here pre-fix had no compiler fences, so frames could be observed
//! by the consumer before their bytes were committed. Fixed by
//! moving the offset reads/writes onto `AtomicU64` with the ordering
//! pairings documented above.
//!
//! Layout assumption: header is 16-byte aligned (mmap base is page-
//! aligned), so the two `AtomicU64`s land on natural 8-byte
//! boundaries — the cast to `*const AtomicU64` is sound.

use memmap2::{MmapMut, MmapOptions};
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{IntoRawFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub const HDR_SIZE: usize = 16;
pub const DEFAULT_RING_CAPACITY: usize = 1 << 20; // 1 MiB

/// Unidirectional SPSC ring backed by mmap.
pub struct Ring {
    mmap: MmapMut,
    capacity: usize,
    notify_w_fd: Option<RawFd>,
    notify_r_fd: Option<RawFd>,
    /// Counter of frames the producer dropped because the ring was
    /// full or the frame oversize. Public field for back-compat with
    /// trader's main.rs telemetry; prefer the [`Ring::frames_dropped`]
    /// getter for new callers.
    pub frames_dropped: u64,
}

impl Ring {
    /// Server (broker) side: create the backing file at `path`,
    /// truncated to `HDR_SIZE + capacity`. Header bytes are zeroed
    /// so write_offset == read_offset == 0 (empty ring).
    ///
    /// If the file already exists, it's overwritten — caller's
    /// responsibility to ensure the previous broker isn't using it.
    pub fn create_owner(path: &Path, capacity: usize) -> io::Result<Self> {
        let total = HDR_SIZE + capacity;
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(total as u64)?;
        let mmap = unsafe { MmapOptions::new().len(total).map_mut(&file)? };
        let ring = Self {
            mmap,
            capacity,
            notify_w_fd: None,
            notify_r_fd: None,
            frames_dropped: 0,
        };
        // Zero the header explicitly via the same atomic-store path
        // the hot loops use, so even if the file existed at the same
        // size and had stale offset bytes, the consumer/producer see
        // a clean (0, 0) state when they map us.
        ring.store_write(0, Ordering::Release);
        ring.store_read(0, Ordering::Release);
        Ok(ring)
    }

    /// Client (trader) side: open an existing ring file.
    pub fn open_client(path: &Path, capacity: usize) -> io::Result<Self> {
        let total = HDR_SIZE + capacity;
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mmap = unsafe { MmapOptions::new().len(total).map_mut(&file)? };
        Ok(Self {
            mmap,
            capacity,
            notify_w_fd: None,
            notify_r_fd: None,
            frames_dropped: 0,
        })
    }

    /// Backwards-compat alias for `open_client`. The trader binary's
    /// existing main.rs uses `Ring::open(path, capacity)`.
    pub fn open(path: &Path, capacity: usize) -> io::Result<Self> {
        Self::open_client(path, capacity)
    }

    /// Server side: create the FIFO at `<ring>.notify`.
    pub fn create_notify_fifo(path: &Path) -> io::Result<()> {
        let path_str = path.to_string_lossy();
        // mkfifo(3) — only fails if the path exists. We tolerate
        // that case (broker restart on existing files).
        let cstr = std::ffi::CString::new(path_str.as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        unsafe {
            let r = libc::mkfifo(cstr.as_ptr(), 0o666);
            if r == 0 {
                Ok(())
            } else {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EEXIST) {
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    /// Open the notification FIFO. Same flags as Python:
    /// `O_RDWR | O_NONBLOCK`. `as_writer=true` registers as the
    /// notify-source; `false` registers as the wake-target.
    pub fn open_notify(&mut self, path: &Path, as_writer: bool) -> io::Result<()> {
        // `IntoRawFd::into_raw_fd` consumes the File without closing
        // the underlying fd — exactly what the previous custom
        // `IntoRawFdKeep` shim was reinventing.
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?
            .into_raw_fd();
        if as_writer {
            self.notify_w_fd = Some(fd);
        } else {
            self.notify_r_fd = Some(fd);
        }
        Ok(())
    }

    pub fn notify_r_fd(&self) -> Option<RawFd> {
        self.notify_r_fd
    }

    /// Producer side: signal the consumer.
    pub fn notify(&self) {
        if let Some(fd) = self.notify_w_fd {
            let buf = [0u8; 1];
            unsafe {
                libc::write(fd, buf.as_ptr() as *const _, 1);
            }
        }
    }

    /// Consumer side: drain pending notification bytes.
    pub fn drain_notify(&self) {
        if let Some(fd) = self.notify_r_fd {
            let mut buf = [0u8; 4096];
            unsafe {
                loop {
                    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len());
                    if n <= 0 {
                        break;
                    }
                }
            }
        }
    }

    /// Snapshot of total frames dropped by this ring's producer
    /// since construction.
    pub fn frames_dropped(&self) -> u64 {
        self.frames_dropped
    }

    /// Pointer to the write-offset atomic in the shared header.
    /// Header is 16-byte aligned (mmap base is page-aligned), so the
    /// cast to `*const AtomicU64` is sound on x86_64 / aarch64.
    #[inline]
    fn write_atomic(&self) -> &AtomicU64 {
        unsafe {
            let ptr = self.mmap.as_ptr() as *const AtomicU64;
            &*ptr
        }
    }

    /// Pointer to the read-offset atomic.
    #[inline]
    fn read_atomic(&self) -> &AtomicU64 {
        unsafe {
            let ptr = self.mmap.as_ptr().add(8) as *const AtomicU64;
            &*ptr
        }
    }

    #[inline]
    fn load_write(&self, ord: Ordering) -> u64 {
        self.write_atomic().load(ord)
    }

    #[inline]
    fn load_read(&self, ord: Ordering) -> u64 {
        self.read_atomic().load(ord)
    }

    #[inline]
    fn store_write(&self, v: u64, ord: Ordering) {
        self.write_atomic().store(v, ord);
    }

    #[inline]
    fn store_read(&self, v: u64, ord: Ordering) {
        self.read_atomic().store(v, ord);
    }

    /// Producer side: write a body as a length-prefixed frame, in one
    /// pass. The length prefix (4-byte BE u32) is written directly into
    /// the mmap region — no intermediate `Vec` allocation. Equivalent
    /// to `write_frame(&pack_frame(body))` but skips the alloc + copy.
    ///
    /// Bundle 1B (2026-05-06). The pack_frame Vec allocation cost is
    /// most measurable at the broker tick fast-path (high frequency)
    /// where every published TickEvent paid for one allocation.
    pub fn write_body(&mut self, body: &[u8]) -> bool {
        let n = body.len();
        let frame_len = 4 + n;
        if frame_len > self.capacity / 2 {
            self.frames_dropped += 1;
            return false;
        }
        let w = self.load_write(Ordering::Relaxed);
        let r = self.load_read(Ordering::Acquire);
        let free = self.capacity as u64 - (w - r);
        if (free as usize) < frame_len {
            self.frames_dropped += 1;
            return false;
        }
        let prefix = (n as u32).to_be_bytes();
        let pos = (w as usize) % self.capacity;
        // Write the 4-byte length prefix. The `pos > capacity-4` wrap
        // case is rare but must be handled (capacity is always exactly
        // a power of two ≥ 1MiB so it's not a corner-case for tiny
        // rings used in tests).
        let end_p = pos + 4;
        if end_p <= self.capacity {
            self.mmap[HDR_SIZE + pos..HDR_SIZE + end_p].copy_from_slice(&prefix);
        } else {
            let tail = self.capacity - pos;
            self.mmap[HDR_SIZE + pos..HDR_SIZE + self.capacity]
                .copy_from_slice(&prefix[..tail]);
            self.mmap[HDR_SIZE..HDR_SIZE + (4 - tail)]
                .copy_from_slice(&prefix[tail..]);
        }
        // Write body starting at pos+4 (modulo capacity).
        let body_pos = (pos + 4) % self.capacity;
        let end_b = body_pos + n;
        if end_b <= self.capacity {
            self.mmap[HDR_SIZE + body_pos..HDR_SIZE + end_b].copy_from_slice(body);
        } else {
            let tail = self.capacity - body_pos;
            self.mmap[HDR_SIZE + body_pos..HDR_SIZE + self.capacity]
                .copy_from_slice(&body[..tail]);
            self.mmap[HDR_SIZE..HDR_SIZE + (n - tail)]
                .copy_from_slice(&body[tail..]);
        }
        let new_w = w + frame_len as u64;
        self.store_write(new_w, Ordering::Release);
        self.notify();
        true
    }

    /// Producer side: write a complete frame. Returns false on
    /// "buffer full" (caller drops or retries).
    pub fn write_frame(&mut self, frame: &[u8]) -> bool {
        let n = frame.len();
        if n > self.capacity / 2 {
            self.frames_dropped += 1;
            return false;
        }
        // Producer owns `write_offset` (Relaxed read of our own write
        // is fine), but `read_offset` is published by the consumer's
        // Release-store in `read_available`. Acquire-load it so we
        // synchronize-with that release: any byte the consumer
        // committed to "free for reuse" before its store is visible
        // to us before we begin overwriting that region.
        let w = self.load_write(Ordering::Relaxed);
        let r = self.load_read(Ordering::Acquire);
        let free = self.capacity as u64 - (w - r);
        if (free as usize) < n {
            self.frames_dropped += 1;
            return false;
        }
        let pos = (w as usize) % self.capacity;
        let end = pos + n;
        if end <= self.capacity {
            self.mmap[HDR_SIZE + pos..HDR_SIZE + end].copy_from_slice(frame);
        } else {
            let tail = self.capacity - pos;
            self.mmap[HDR_SIZE + pos..HDR_SIZE + self.capacity]
                .copy_from_slice(&frame[..tail]);
            self.mmap[HDR_SIZE..HDR_SIZE + (n - tail)]
                .copy_from_slice(&frame[tail..]);
        }
        // Release-store: publishes the bytes copied above to any
        // consumer that subsequently Acquire-loads write_offset and
        // observes >= new_w. Without Release here, a weaker
        // architecture (or the COMPILER on x86 — reordering across
        // the offset write) could let the consumer's read see the
        // header advance before the data bytes are visible.
        let new_w = w + n as u64;
        self.store_write(new_w, Ordering::Release);
        self.notify();
        true
    }

    /// Consumer side: read everything available, appending to the
    /// caller-provided `buf` instead of allocating a new Vec. Returns
    /// the number of bytes appended.
    ///
    /// Lever 2 (2026-05-06): the trader's hot loop calls this on every
    /// busy-poll iteration; under steady-state ~50 ticks/sec the prior
    /// `read_available -> Vec; buf.extend_from_slice(&Vec)` did one Vec
    /// alloc per drain plus a redundant memcpy. Direct extend from
    /// mmap eliminates the alloc and folds two memcpys into one.
    pub fn read_available_into(&mut self, buf: &mut Vec<u8>) -> usize {
        let w = self.load_write(Ordering::Acquire);
        let r = self.load_read(Ordering::Relaxed);
        let avail = w.saturating_sub(r) as usize;
        if avail == 0 {
            return 0;
        }
        let pos = (r as usize) % self.capacity;
        let end = pos + avail;
        if end <= self.capacity {
            buf.extend_from_slice(&self.mmap[HDR_SIZE + pos..HDR_SIZE + end]);
        } else {
            let tail = self.capacity - pos;
            let head_len = avail - tail;
            buf.extend_from_slice(&self.mmap[HDR_SIZE + pos..HDR_SIZE + self.capacity]);
            buf.extend_from_slice(&self.mmap[HDR_SIZE..HDR_SIZE + head_len]);
        }
        self.store_read(r + avail as u64, Ordering::Release);
        avail
    }

    /// Consumer side: read everything available since last bump.
    pub fn read_available(&mut self) -> Vec<u8> {
        // Acquire-load `write_offset` — pairs with the producer's
        // Release-store in `write_frame`. Bytes up through `w` are
        // guaranteed visible after this load returns.
        let w = self.load_write(Ordering::Acquire);
        // Our own offset; Relaxed.
        let r = self.load_read(Ordering::Relaxed);
        let avail = w.saturating_sub(r) as usize;
        if avail == 0 {
            return Vec::new();
        }
        let pos = (r as usize) % self.capacity;
        let end = pos + avail;
        let mut data = Vec::with_capacity(avail);
        if end <= self.capacity {
            data.extend_from_slice(&self.mmap[HDR_SIZE + pos..HDR_SIZE + end]);
        } else {
            let tail = self.capacity - pos;
            let head_len = avail - tail;
            data.extend_from_slice(&self.mmap[HDR_SIZE + pos..HDR_SIZE + self.capacity]);
            data.extend_from_slice(&self.mmap[HDR_SIZE..HDR_SIZE + head_len]);
        }
        // Release-store `read_offset`: publishes "I'm done reading
        // those bytes; producer may reclaim the region" so the
        // producer's next Acquire-load of read_offset sees the new
        // value AND all our consumption-side activity is ordered
        // before any byte the producer subsequently writes there.
        self.store_read(r + avail as u64, Ordering::Release);
        data
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        for fd in [self.notify_w_fd.take(), self.notify_r_fd.take()] {
            if let Some(fd) = fd {
                unsafe {
                    libc::close(fd);
                }
            }
        }
    }
}

/// Wait for the broker to create both ring files at the given base
/// path. Returns once `<base>.events` and `<base>.commands` exist.
/// The trader's startup uses this to gate connection on broker boot.
pub async fn wait_for_rings(base: &str) -> io::Result<()> {
    let events = format!("{}.events", base);
    let commands = format!("{}.commands", base);
    loop {
        if std::path::Path::new(&events).exists()
            && std::path::Path::new(&commands).exists()
        {
            return Ok(());
        }
        log::info!("shm: rings not ready ({events}, {commands}); retry in 1s");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn create_open_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.events");
        let _server = Ring::create_owner(&path, 4096).unwrap();
        let _client = Ring::open_client(&path, 4096).unwrap();
    }

    #[test]
    fn write_then_read_via_two_handles() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ring.events");
        let mut server = Ring::create_owner(&path, 4096).unwrap();
        assert!(server.write_frame(b"hello"));
        let mut client = Ring::open_client(&path, 4096).unwrap();
        let data = client.read_available();
        assert_eq!(data, b"hello");
    }

    #[test]
    fn buffer_full_drops_frame() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ring.events");
        let mut ring = Ring::create_owner(&path, 64).unwrap();
        // Frame > capacity/2 fails immediately.
        let big = vec![0u8; 40];
        assert!(!ring.write_frame(&big));
        assert_eq!(ring.frames_dropped(), 1);
    }

    /// Spec test — establish that the offset-store-after-data ordering
    /// is preserved end-to-end. Smoke test only; the real proof is
    /// the documented Acquire/Release pairing on the AtomicU64s and
    /// the absence of pre-fix behavior under loom-style reordering.
    #[test]
    fn write_body_round_trip_matches_write_frame() {
        // Two rings, same input, must produce byte-identical mmap.
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("a.events");
        let p2 = dir.path().join("b.events");
        let mut r1 = Ring::create_owner(&p1, 4096).unwrap();
        let mut r2 = Ring::create_owner(&p2, 4096).unwrap();
        let body = b"hello world";
        let frame = crate::protocol::pack_frame(body);
        assert!(r1.write_frame(&frame));
        assert!(r2.write_body(body));
        let mut c1 = Ring::open_client(&p1, 4096).unwrap();
        let mut c2 = Ring::open_client(&p2, 4096).unwrap();
        assert_eq!(c1.read_available(), c2.read_available());
    }

    #[test]
    fn write_body_wrap_around_prefix_split() {
        // Force the 4-byte length prefix to straddle the buffer wrap:
        // advance write_offset to land at (capacity-2), then write a
        // body whose prefix splits between [cap-2..cap] and [0..2].
        // Capacity must be ≥ 2× frame size (write_body sanity check),
        // hence 256 with ~60-byte frames.
        let dir = tempdir().unwrap();
        let path = dir.path().join("ring.events");
        const CAP: usize = 256;
        let mut server = Ring::create_owner(&path, CAP).unwrap();
        let mut client = Ring::open_client(&path, CAP).unwrap();
        // Each round (write then drain) advances the write offset by
        // (4 + body_len). We want offsets to land at CAP-2 = 254.
        // 4 frames × 60B body × (4+60) = 256 — overflows. Instead: 4
        // frames @ 50-byte body → 4*54 = 216, then a 5th frame whose
        // header lands at offset 216, body at 220, ends at 220+B.
        // Set B such that next-frame-pos = 254. After this 5th frame
        // ends at 216 + 4 + B; new pos = (216+4+B) mod 256. We need
        // pos = 254, so 220 + B ≡ 254 → B = 34.
        for _ in 0..4 {
            let body = vec![0xAAu8; 50];
            assert!(server.write_body(&body));
            let _ = client.read_available();
        }
        let priming = vec![0xCCu8; 34];
        assert!(server.write_body(&priming));
        let _ = client.read_available();
        // Now write_offset % CAP == 254. Next prefix occupies [254..258],
        // wrapping to [254..256] + [0..2]. Body small.
        let body_split = vec![0xBBu8; 16];
        assert!(server.write_body(&body_split));
        let data = client.read_available();
        assert_eq!(&data[..4], &(16u32).to_be_bytes());
        assert_eq!(&data[4..], &body_split[..]);
    }

    #[test]
    fn write_then_read_preserves_byte_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ring.events");
        let mut server = Ring::create_owner(&path, 4096).unwrap();
        let frames: Vec<Vec<u8>> =
            (0..16).map(|i| vec![i as u8; 32]).collect();
        for f in &frames {
            assert!(server.write_frame(f));
        }
        let mut client = Ring::open_client(&path, 4096).unwrap();
        let bytes = client.read_available();
        // Concatenated frames in order.
        let expected: Vec<u8> = frames.iter().flatten().copied().collect();
        assert_eq!(bytes, expected);
    }
}
