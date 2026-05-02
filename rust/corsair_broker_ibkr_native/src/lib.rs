//! Native Rust IBKR API client.
//!
//! # Phase 6 of the v3 migration
//!
//! Replaces the PyO3 + ib_insync bridge in `corsair_broker_ibkr` with
//! a direct TCP socket → IBKR Gateway implementation. This unblocks
//! Phase 5B.7 cutover: the asyncio loop binding issue that hangs
//! `ib.client.connectAsync` when driven from Rust simply doesn't
//! exist when there's no Python loop in the picture.
//!
//! # Wire protocol
//!
//! IBKR API V100+ over TCP. Per message:
//!
//! ```text
//! [4-byte big-endian length] [null-separated string fields]
//! ```
//!
//! Each field is a UTF-8 string terminated by `\0`. Last field has
//! a trailing `\0` (so the byte count includes a terminating null).
//! Numeric fields are sent as decimal strings; booleans as "0" or
//! "1"; missing/unset fields as empty string.
//!
//! # Phase status (this crate)
//!
//! Phase 6.1: TCP connect + handshake → ✅ this commit
//! Phase 6.2: Message framing (send + recv codec) → ✅ this commit
//! Phase 6.3: startApi + nextValidId → ✅ this commit
//! Phase 6.5+: per-message-type implementations
//!   - reqAccountUpdates / accountValue / accountDownloadEnd
//!   - reqPositions / position / positionEnd
//!   - reqOpenOrders / openOrder / openOrderEnd
//!   - reqMktData / tickPrice / tickSize / tickOptionComputation
//!   - placeOrder / orderStatus
//!   - cancelOrder
//!   - reqExecutions / execDetails / commissionReport
//!   - errMsg
//! Phase 6.X: Implement `corsair_broker_api::Broker` trait.
//!
//! Each message type is its own focused PR. The codec + handshake
//! foundation built here is the load-bearing scaffolding.

pub mod client;
pub mod codec;
pub mod error;
pub mod messages;

pub use client::{NativeClient, NativeClientConfig};
pub use error::NativeError;
