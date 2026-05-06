//! Native Rust IBKR API client.
//!
//! Direct TCP socket → IBKR Gateway implementation; no PyO3, no
//! ib_insync, no Python loop in the hot path. The `corsair_broker`
//! daemon embeds [`NativeBroker`] as its `corsair_broker_api::Broker`
//! impl.
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
    cancel_mkt_data, cancel_mkt_depth, cancel_order, req_account_updates,
    req_auto_open_orders, req_contract_details, req_executions, req_mkt_data, req_mkt_depth,
    req_open_orders, req_positions, ContractRequest, ExecutionFilter, PlaceOrderParams,
};
// Slow-path place_order encoder kept for parity testing only.
// Production hot path is `place_template::place_order_fast`.
#[deprecated(note = "use place_template::place_order_fast")]
#[allow(deprecated)]
pub use requests::place_order;
pub use types::{
    AccountValueMsg, CommissionReportMsg, ContractDecoded, ContractDetailsMsg, ErrorMsg,
    ExecutionMsg, InboundMsg, OpenOrderMsg, OrderStatusMsg, PositionMsg,
    TickOptionComputationMsg, TickPriceMsg, TickSizeMsg,
};
