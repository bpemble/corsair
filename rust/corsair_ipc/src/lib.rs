//! Shared IPC primitives for the broker ↔ trader split.
//!
//! Wire format:
//!   - Two memory-mapped SPSC ring buffers per pair:
//!     * `events`   — broker → trader (ticks, fills, status, vol_surface, ...)
//!     * `commands` — trader → broker (place_order, cancel_order, ...)
//!   - Each ring is a 16-byte header (write_offset, read_offset) +
//!     N-byte ring data. Header offsets are little-endian u64.
//!   - Frames in the ring use length-prefixed msgpack:
//!     `[4-byte BE u32 length][msgpack body]`.
//!   - Each ring has a paired `<path>.notify` FIFO. Producer writes
//!     a single byte to wake consumer; consumer drains all pending
//!     notification bytes (coalesces multiple wakes).
//!
//! This crate provides:
//!   - [`protocol`] — frame packing/unpacking. Identical to
//!     `src/ipc/protocol.py` and the previous trader-internal code.
//!   - [`ring::Ring`] — both client (open existing) AND server
//!     (create + own) modes.
//!   - [`server::SHMServer`] — broker-side wrapper that owns both
//!     rings and the receive task. Trader-side, [`ring::Ring::open_client`]
//!     is used directly from `corsair_trader` (no client wrapper crate).

pub mod cpu_affinity;
pub mod protocol;
pub mod ring;
pub mod server;

pub use ring::{Ring, DEFAULT_RING_CAPACITY, HDR_SIZE};
pub use server::{ServerCommand, ServerConfig, SHMServer};
