//! Outbound request encoders.
//!
//! Each encoder builds a length-prefixed frame matching ib_insync's
//! wire format. Field order MUST match ib_insync's `Client._send*`
//! methods byte-for-byte — IBKR's TWS server silently rejects
//! malformed requests (or worse, accepts them with weird semantics).

use crate::codec::{encode_bool, encode_f64, encode_int, encode_int64, encode_owned, encode_unset};
use crate::messages::*;

/// Helper: build a frame from owned strings.
pub(crate) fn frame(fields: &[String]) -> Vec<u8> {
    encode_owned(fields)
}

// ─── Account / position requests ──────────────────────────────────

pub fn req_account_updates(subscribe: bool, account: &str) -> Vec<u8> {
    frame(&[
        encode_int(OUT_REQ_ACCT_DATA),
        "2".into(), // version
        encode_bool(subscribe),
        account.into(),
    ])
}

pub fn req_positions() -> Vec<u8> {
    frame(&[encode_int(OUT_REQ_POSITIONS), "1".into()])
}

pub fn cancel_positions() -> Vec<u8> {
    frame(&[encode_int(OUT_CANCEL_POSITIONS), "1".into()])
}

pub fn req_open_orders() -> Vec<u8> {
    frame(&[encode_int(OUT_REQ_OPEN_ORDERS), "1".into()])
}

pub fn req_all_open_orders() -> Vec<u8> {
    frame(&[encode_int(OUT_REQ_ALL_OPEN_ORDERS), "1".into()])
}

pub fn req_auto_open_orders(auto_bind: bool) -> Vec<u8> {
    frame(&[
        encode_int(OUT_REQ_AUTO_OPEN_ORDERS),
        "1".into(),
        encode_bool(auto_bind),
    ])
}

pub fn req_managed_accounts() -> Vec<u8> {
    frame(&[encode_int(OUT_REQ_MANAGED_ACCTS), "1".into()])
}

pub fn req_ids(num: i32) -> Vec<u8> {
    frame(&[encode_int(OUT_REQ_IDS), "1".into(), encode_int(num)])
}

// ─── Contract resolution ──────────────────────────────────────────

/// Resolve a contract by partial fields. Server replies with
/// contractData / contractDataEnd messages keyed by req_id.
#[derive(Debug, Clone, Default)]
pub struct ContractRequest {
    pub con_id: i64,
    pub symbol: String,
    pub sec_type: String, // "FUT", "OPT", "FOP"
    pub last_trade_date: String, // YYYYMMDD
    pub strike: f64,
    pub right: String, // "C", "P", or empty
    pub multiplier: String,
    pub exchange: String,
    pub primary_exchange: String,
    pub currency: String,
    pub local_symbol: String,
    pub trading_class: String,
}

pub fn req_contract_details(req_id: i32, c: &ContractRequest) -> Vec<u8> {
    frame(&[
        encode_int(OUT_REQ_CONTRACT_DATA),
        "8".into(), // version
        encode_int(req_id),
        encode_int64(c.con_id),
        c.symbol.clone(),
        c.sec_type.clone(),
        c.last_trade_date.clone(),
        encode_f64(c.strike),
        c.right.clone(),
        c.multiplier.clone(),
        c.exchange.clone(),
        c.primary_exchange.clone(),
        c.currency.clone(),
        c.local_symbol.clone(),
        c.trading_class.clone(),
        encode_bool(false), // includeExpired
        encode_unset(),     // secIdType
        encode_unset(),     // secId
        encode_unset(),     // issuerId (server >= 176)
    ])
}

// ─── Market data ──────────────────────────────────────────────────

/// Request market data on a contract (snapshot=false → streaming).
/// Server delivers tickPrice/tickSize/tickOptionComputation messages
/// keyed by req_id.
pub fn req_mkt_data(
    req_id: i32,
    c: &ContractRequest,
    generic_tick_list: &str,
    snapshot: bool,
    regulatory_snapshot: bool,
) -> Vec<u8> {
    frame(&[
        encode_int(OUT_REQ_MKT_DATA),
        "11".into(), // version
        encode_int(req_id),
        encode_int64(c.con_id),
        c.symbol.clone(),
        c.sec_type.clone(),
        c.last_trade_date.clone(),
        encode_f64(c.strike),
        c.right.clone(),
        c.multiplier.clone(),
        c.exchange.clone(),
        c.primary_exchange.clone(),
        c.currency.clone(),
        c.local_symbol.clone(),
        c.trading_class.clone(),
        encode_unset(), // delta-neutral combo
        generic_tick_list.into(),
        encode_bool(snapshot),
        encode_bool(regulatory_snapshot),
        encode_unset(), // mktDataOptions (TagValue list, empty)
    ])
}

pub fn cancel_mkt_data(req_id: i32) -> Vec<u8> {
    frame(&[
        encode_int(OUT_CANCEL_MKT_DATA),
        "2".into(), // version
        encode_int(req_id),
    ])
}

// ─── Orders ───────────────────────────────────────────────────────

/// Place-order parameters. Mirrors ib_insync's Order struct subset
/// — only fields we actually use. Phase 6.5 keeps this minimal;
/// later phases extend.
#[derive(Debug, Clone)]
pub struct PlaceOrderParams {
    pub action: String,        // "BUY" or "SELL"
    pub total_quantity: f64,   // signed: just the magnitude
    pub order_type: String,    // "LMT" or "MKT"
    pub lmt_price: f64,        // 0.0 for MKT
    pub aux_price: f64,        // for STP_LMT etc; 0 otherwise
    pub tif: String,           // "DAY", "GTD", "IOC", "GTC", "FOK"
    pub good_till_date: String, // "YYYYMMDD HH:MM:SS UTC" for GTD
    pub account: String,
    pub order_ref: String,
    pub outside_rth: bool,
}

/// Encode a placeOrder message. Order id must be set by caller from
/// `NativeClient::next_order_id()`.
///
/// This is the BIG one in IBKR's API — tons of fields. We send a
/// minimal but valid set; unset-defaults handle the rest.
pub fn place_order(
    order_id: i32,
    contract: &ContractRequest,
    params: &PlaceOrderParams,
) -> Vec<u8> {
    let mut fields: Vec<String> = vec![
        encode_int(OUT_PLACE_ORDER),
        encode_int(order_id),
        encode_int64(contract.con_id),
        contract.symbol.clone(),
        contract.sec_type.clone(),
        contract.last_trade_date.clone(),
        encode_f64(contract.strike),
        contract.right.clone(),
        contract.multiplier.clone(),
        contract.exchange.clone(),
        contract.primary_exchange.clone(),
        contract.currency.clone(),
        contract.local_symbol.clone(),
        contract.trading_class.clone(),
        encode_unset(), // secIdType
        encode_unset(), // secId
        // Order body
        params.action.clone(),
        encode_f64(params.total_quantity),
        params.order_type.clone(),
        encode_f64(params.lmt_price),
        encode_f64(params.aux_price),
        params.tif.clone(),
        encode_unset(), // ocaGroup
        params.account.clone(),
        encode_unset(), // openClose
        encode_int(0),  // origin (0 = customer)
        params.order_ref.clone(),
        encode_bool(true),  // transmit
        encode_int(0),      // parentId
        encode_bool(false), // blockOrder
        encode_bool(false), // sweepToFill
        encode_int(0),      // displaySize
        encode_int(0),      // triggerMethod
        encode_bool(params.outside_rth),
        encode_bool(false), // hidden
        // No combo legs / smart combo routing.
        encode_unset(), // sharesAllocation (deprecated)
        encode_f64(0.0), // discretionaryAmt
        params.good_till_date.clone(),
        encode_unset(), // goodAfterTime
        encode_unset(), // faGroup
        encode_unset(), // faMethod
        encode_unset(), // faPercentage
        encode_unset(), // faProfile
        encode_unset(), // modelCode
        encode_unset(), // shortSaleSlot
        encode_unset(), // designatedLocation
        encode_int(-1), // exemptCode
        encode_int(0),  // ocaType
        encode_unset(), // rule80A
        encode_unset(), // settlingFirm
        encode_bool(false), // allOrNone
        encode_unset(), // minQty
        encode_unset(), // percentOffset
        encode_bool(false), // eTradeOnly
        encode_bool(false), // firmQuoteOnly
        encode_unset(), // nbboPriceCap
        encode_int(0),  // auctionStrategy
        encode_unset(), // startingPrice
        encode_unset(), // stockRefPrice
        encode_unset(), // delta
        encode_unset(), // stockRangeLower
        encode_unset(), // stockRangeUpper
        encode_bool(false), // overridePercentageConstraints
        encode_unset(), // volatility
        encode_unset(), // volatilityType
        encode_unset(), // deltaNeutralOrderType
        encode_unset(), // deltaNeutralAuxPrice
        // ... server >= 100 onwards
        encode_unset(), // continuousUpdate
        encode_unset(), // referencePriceType
        encode_unset(), // trailStopPrice
        encode_unset(), // trailingPercent
        encode_unset(), // scaleInitLevelSize
        encode_unset(), // scaleSubsLevelSize
        encode_unset(), // scalePriceIncrement
        encode_unset(), // hedgeType
        encode_unset(), // optOutSmartRouting
        encode_unset(), // clearingAccount
        encode_unset(), // clearingIntent
        encode_bool(false), // notHeld
        encode_bool(false), // delta-neutral contract: false → no further fields
        encode_unset(), // algoStrategy
        encode_bool(false), // whatIf
        encode_unset(), // orderMiscOptions (TagValue list)
        encode_unset(), // solicited
        encode_bool(false), // randomizeSize
        encode_bool(false), // randomizePrice
        // Pegged-to-benchmark fields not used.
        encode_unset(), // referenceContractId
        encode_bool(false), // isPeggedChangeAmountDecrease
        encode_unset(), // peggedChangeAmount
        encode_unset(), // referenceChangeAmount
        encode_unset(), // referenceExchangeId
        encode_int(0),  // conditionsCount
        encode_unset(), // conditionsIgnoreRth
        encode_unset(), // conditionsCancelOrder
        encode_unset(), // adjustedOrderType
        encode_unset(), // triggerPrice
        encode_unset(), // lmtPriceOffset
        encode_unset(), // adjustedStopPrice
        encode_unset(), // adjustedStopLimitPrice
        encode_unset(), // adjustedTrailingAmount
        encode_int(0),  // adjustableTrailingUnit
        encode_unset(), // extOperator
        // Soft-dollar tier
        encode_unset(), // softDollarTier.name
        encode_unset(), // softDollarTier.value
        // server >= MIN_SERVER_VER_CASH_QTY (151)
        encode_unset(), // cashQty
        // server >= MIN_SERVER_VER_DECISION_MAKER (158)
        encode_unset(), // mifid2DecisionMaker
        encode_unset(), // mifid2DecisionAlgo
        encode_unset(), // mifid2ExecutionTrader
        encode_unset(), // mifid2ExecutionAlgo
        encode_bool(false), // dontUseAutoPriceForHedge
        encode_bool(false), // isOmsContainer
        encode_bool(false), // discretionaryUpToLimitPrice
        encode_unset(), // usePriceMgmtAlgo (true/false/None)
        // server >= 173
        encode_int(0), // duration
        // server >= 174
        encode_int(0), // postToAts
        // server >= 176
        encode_unset(), // autoCancelParent
        encode_unset(), // advancedErrorOverride
        encode_unset(), // manualOrderTime
        // server >= MIN_SERVER_VER_PEGBEST_PEGMID_OFFSETS (180) — skipped
    ];
    // Trim trailing unsets that the server doesn't expect — for our
    // server version 178 the order-format is known. Keep all fields
    // up through advancedErrorOverride.
    // (Field count must exactly match what the server expects for
    // its protocol version; if we go too long, we break.)
    let _ = &mut fields;
    encode_owned(&fields)
}

pub fn cancel_order(order_id: i32) -> Vec<u8> {
    frame(&[
        encode_int(OUT_CANCEL_ORDER),
        "1".into(),
        encode_int(order_id),
        encode_unset(), // manualOrderCancelTime (server >= 102)
    ])
}

// ─── Executions ───────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct ExecutionFilter {
    pub client_id: i32,
    pub account: String,
    /// "yyyymmdd-hh:mm:ss" or empty for "from beginning"
    pub time: String,
    pub symbol: String,
    pub sec_type: String,
    pub exchange: String,
    pub side: String,
}

pub fn req_executions(req_id: i32, f: &ExecutionFilter) -> Vec<u8> {
    frame(&[
        encode_int(OUT_REQ_EXECUTIONS),
        "3".into(),
        encode_int(req_id),
        encode_int(f.client_id),
        f.account.clone(),
        f.time.clone(),
        f.symbol.clone(),
        f.sec_type.clone(),
        f.exchange.clone(),
        f.side.clone(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn req_account_updates_field_order() {
        // Strip the 4-byte length prefix so we can inspect payload.
        let bytes = req_account_updates(true, "DUP553657");
        let payload = &bytes[4..];
        // Should split into: "6", "2", "1", "DUP553657" + trailing \0
        let s = std::str::from_utf8(payload).unwrap();
        let fields: Vec<&str> = s.split('\0').filter(|f| !f.is_empty()).collect();
        assert_eq!(fields, vec!["6", "2", "1", "DUP553657"]);
    }

    #[test]
    fn req_positions_one_byte_payload() {
        let bytes = req_positions();
        // length prefix is 4, payload is "61\01\0" = 5 bytes
        assert_eq!(&bytes[..4], &[0, 0, 0, 5]);
        assert_eq!(&bytes[4..], b"61\x001\x00");
    }

    #[test]
    fn cancel_order_includes_manual_time_field() {
        let bytes = cancel_order(42);
        let payload = &bytes[4..];
        let s = std::str::from_utf8(payload).unwrap();
        // "4", "1", "42", "" + trailing \0
        let fields: Vec<&str> = s.split('\0').collect();
        // After split there should be 5 entries (last empty from trailing \0)
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[0], "4");
        assert_eq!(fields[1], "1");
        assert_eq!(fields[2], "42");
        assert_eq!(fields[3], "");
    }

    #[test]
    fn place_order_starts_with_type_id_and_orderid() {
        let c = ContractRequest {
            con_id: 712565973,
            symbol: "HG".into(),
            sec_type: "FUT".into(),
            last_trade_date: "20260626".into(),
            multiplier: "25000".into(),
            exchange: "COMEX".into(),
            currency: "USD".into(),
            local_symbol: "HGM6".into(),
            trading_class: "HG".into(),
            ..Default::default()
        };
        let p = PlaceOrderParams {
            action: "BUY".into(),
            total_quantity: 1.0,
            order_type: "LMT".into(),
            lmt_price: 6.05,
            aux_price: 0.0,
            tif: "GTD".into(),
            good_till_date: "20260626 14:30:00 UTC".into(),
            account: "DUP553657".into(),
            order_ref: "test".into(),
            outside_rth: false,
        };
        let bytes = place_order(123, &c, &p);
        let payload = &bytes[4..];
        let s = std::str::from_utf8(payload).unwrap();
        let fields: Vec<&str> = s.split('\0').collect();
        assert_eq!(fields[0], "3"); // OUT_PLACE_ORDER
        assert_eq!(fields[1], "123"); // orderId
        assert_eq!(fields[2], "712565973"); // conId
        assert!(fields.iter().any(|&f| f == "BUY"));
        assert!(fields.iter().any(|&f| f == "LMT"));
        assert!(fields.iter().any(|&f| f == "GTD"));
    }
}
