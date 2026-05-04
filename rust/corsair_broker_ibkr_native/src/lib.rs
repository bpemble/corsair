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

pub mod broker;
pub mod client;
pub mod codec;
pub mod decoder;
pub mod error;
pub mod messages;
pub mod place_template;
pub mod requests;
pub mod types;

pub use broker::{NativeBroker, NativeBrokerConfig};

pub use client::{NativeClient, NativeClientConfig};
pub use decoder::parse_inbound;
pub use error::NativeError;
pub use requests::{
    cancel_mkt_data, cancel_mkt_depth, cancel_order, place_order, req_account_updates,
    req_auto_open_orders, req_contract_details, req_executions, req_mkt_data, req_mkt_depth,
    req_open_orders, req_positions, ContractRequest, ExecutionFilter, PlaceOrderParams,
};
pub use types::{
    AccountValueMsg, CommissionReportMsg, ContractDecoded, ContractDetailsMsg, ErrorMsg,
    ExecutionMsg, InboundMsg, OpenOrderMsg, OrderStatusMsg, PositionMsg,
    TickOptionComputationMsg, TickPriceMsg, TickSizeMsg,
};
