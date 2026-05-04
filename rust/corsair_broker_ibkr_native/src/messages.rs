//! Outbound and inbound message type IDs.
//!
//! Each IBKR API message starts with a numeric type id (decimal
//! string in the wire encoding). These are stable across versions —
//! field layouts within messages have versions, but the IDs don't.
//!
//! Phase 6.1 establishes the constants; Phase 6.5+ implements the
//! message bodies one at a time.

// ─── Outbound (client → server) ──────────────────────────────────

pub const OUT_REQ_MKT_DATA: i32 = 1;
pub const OUT_CANCEL_MKT_DATA: i32 = 2;
pub const OUT_PLACE_ORDER: i32 = 3;
pub const OUT_CANCEL_ORDER: i32 = 4;
pub const OUT_REQ_OPEN_ORDERS: i32 = 5;
pub const OUT_REQ_ACCT_DATA: i32 = 6;
pub const OUT_REQ_EXECUTIONS: i32 = 7;
pub const OUT_REQ_IDS: i32 = 8;
pub const OUT_REQ_CONTRACT_DATA: i32 = 9;
pub const OUT_REQ_AUTO_OPEN_ORDERS: i32 = 15;
pub const OUT_REQ_ALL_OPEN_ORDERS: i32 = 16;
pub const OUT_REQ_MANAGED_ACCTS: i32 = 17;
pub const OUT_REQ_POSITIONS: i32 = 61;
pub const OUT_CANCEL_POSITIONS: i32 = 64;
pub const OUT_START_API: i32 = 71;

// ─── Inbound (server → client) ────────────────────────────────────

pub const IN_TICK_PRICE: i32 = 1;
pub const IN_TICK_SIZE: i32 = 2;
pub const IN_ORDER_STATUS: i32 = 3;
pub const IN_ERR_MSG: i32 = 4;
pub const IN_OPEN_ORDER: i32 = 5;
pub const IN_ACCT_VALUE: i32 = 6;
pub const IN_PORTFOLIO_VALUE: i32 = 7;
pub const IN_ACCT_UPDATE_TIME: i32 = 8;
pub const IN_NEXT_VALID_ID: i32 = 9;
pub const IN_CONTRACT_DATA: i32 = 10;
/// `contractDataEnd` — terminator for a `reqContractDetails` response
/// stream. IBKR API sends ContractData (id=10) zero-or-more times then
/// ContractDataEnd (id=52). list_chain's oneshot waits for End; without
/// the decoder handling 52 it timed out forever (latent since cutover
/// 2026-05-02; first surfaced when markets re-opened 2026-05-03).
pub const IN_CONTRACT_DATA_END: i32 = 52;
pub const IN_EXECUTION_DATA: i32 = 11;
pub const IN_MANAGED_ACCTS: i32 = 15;
pub const IN_TICK_OPTION_COMPUTATION: i32 = 21;
pub const IN_TICK_GENERIC: i32 = 45;
pub const IN_TICK_STRING: i32 = 46;
pub const IN_CURRENT_TIME: i32 = 49;
pub const IN_COMMISSION_REPORT: i32 = 59;
pub const IN_POSITION_DATA: i32 = 61;
pub const IN_POSITION_END: i32 = 62;
pub const IN_OPEN_ORDER_END: i32 = 53;
pub const IN_ACCOUNT_DOWNLOAD_END: i32 = 54;
pub const IN_EXECUTION_DATA_END: i32 = 55;
pub const IN_TICK_REQ_PARAMS: i32 = 81;

// ─── Protocol versions ────────────────────────────────────────────

/// Minimum server version we'll talk to. v176 is what IBKR Gateway
/// 10.30+ ships. Everything older lacks fields we depend on.
pub const MIN_SERVER_VERSION: i32 = 176;

/// Highest version we know how to speak. Capped at 176 to match
/// ib_insync 0.9.86's reference negotiation, which is the only
/// known-working wire format on our IBKR gateway. At v178+ the
/// server expects post-v176 trailing fields (customerAccount,
/// professionalCustomer, etc.) that we haven't fully verified;
/// claiming v178 without sending those exact fields causes IBKR
/// to mis-parse our place_order frame and reject with cryptic
/// errors like 110 "VOL volatility required" — even though our
/// fields up through manualOrderTime are byte-perfect against
/// ib_insync's own v176-formatted frame.
pub const MAX_CLIENT_VERSION: i32 = 176;
