//! Pre-encoded place_order templates — latency optimization.
//!
//! The IBKR `placeOrder` wire frame has ~80 fields, most of which are
//! constant across every place call (deprecated fields, default
//! booleans, server-version-specific placeholders) or per-instrument
//! constant (con_id, symbol, sec_type, exchange, multiplier, etc.).
//! Only a handful change per order: orderId, action, qty, price, tif,
//! gtd_until, account, order_ref.
//!
//! This module pre-encodes the static portions so the hot path on a
//! refresh-cycle place_order does a memcpy + sprintf-equivalent for
//! the volatile fields, instead of building a Vec<String> of 80 owned
//! Strings and concatenating.
//!
//! # Layout
//!
//! ```text
//! [4-byte length] [type_id] [order_id] [contract_section]
//!                                       └─ 14 fields, per-instrument constant
//!                  [order_body_dynamic]
//!                                       └─ 14 fields, per-call dynamic
//!                  [trailing_constants]
//!                                       └─ ~50 fields, global constant
//! ```
//!
//! The contract_section + trailing_constants are computed once and
//! reused. The hot path produces just the dynamic order_body and
//! splices everything together.

use std::sync::OnceLock;

use crate::codec::{encode_bool, encode_f64, encode_int, encode_unset, write_f64, write_int};
use crate::requests::{ContractRequest, PlaceOrderParams};

/// Per-instrument cached prefix bytes — fields immediately after
/// `[order_id]` and before the order body. 14 fields total
/// (12 contract + 2 secId placeholders), each \0-terminated.
///
/// Cache key is typically the InstrumentId; the broker holds a
/// `HashMap<InstrumentId, ContractTemplate>` and looks up on each
/// place_order.
#[derive(Debug, Clone)]
pub struct ContractTemplate {
    bytes: Vec<u8>,
}

impl ContractTemplate {
    /// Build the cached prefix for a given contract. Encodes the 14
    /// contract-fixed fields once; returns an opaque template the
    /// caller stores per-instrument.
    pub fn from_contract(c: &ContractRequest) -> Self {
        let fields: [String; 14] = [
            encode_int_long(c.con_id),
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
            encode_unset(), // secIdType
            encode_unset(), // secId
        ];
        let mut bytes = Vec::with_capacity(128);
        for f in &fields {
            bytes.extend_from_slice(f.as_bytes());
            bytes.push(0);
        }
        Self { bytes }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

fn encode_int_long(v: i64) -> String {
    v.to_string()
}

/// Trailing constants — the ~50 fields after the dynamic order body.
/// These never change, so we encode them once on first call and
/// reuse the bytes across every place_order.
fn trailing_bytes() -> &'static [u8] {
    static TRAILING: OnceLock<Vec<u8>> = OnceLock::new();
    TRAILING.get_or_init(|| {
        let fields: Vec<String> = vec![
            // After good_till_date / goodAfterTime / fa* / modelCode /
            // shortSaleSlot / designatedLocation:
            encode_int(-1),     // exemptCode
            encode_int(0),      // ocaType
            encode_unset(),     // rule80A
            encode_unset(),     // settlingFirm
            encode_bool(false), // allOrNone
            encode_unset(),     // minQty
            encode_unset(),     // percentOffset
            encode_bool(false), // eTradeOnly
            encode_bool(false), // firmQuoteOnly
            encode_unset(),     // nbboPriceCap
            encode_int(0),      // auctionStrategy
            encode_unset(),     // startingPrice
            encode_unset(),     // stockRefPrice
            encode_unset(),     // delta
            encode_unset(),     // stockRangeLower
            encode_unset(),     // stockRangeUpper
            encode_bool(false), // overridePercentageConstraints
            encode_unset(),     // volatility
            encode_unset(),     // volatilityType
            encode_unset(),     // deltaNeutralOrderType
            encode_unset(),     // deltaNeutralAuxPrice
            encode_unset(),     // continuousUpdate
            encode_unset(),     // referencePriceType
            encode_unset(),     // trailStopPrice
            encode_unset(),     // trailingPercent
            encode_unset(),     // scaleInitLevelSize
            encode_unset(),     // scaleSubsLevelSize
            encode_unset(),     // scalePriceIncrement
            // Always-sent scale-block trailing fields (we never
            // populate scalePriceIncrement so the conditional
            // scale_* extras are omitted, but these three are
            // required regardless).
            encode_unset(),     // scaleTable
            encode_unset(),     // activeStartTime
            encode_unset(),     // activeStopTime
            encode_unset(),     // hedgeType
            encode_unset(),     // optOutSmartRouting
            encode_unset(),     // clearingAccount
            encode_unset(),     // clearingIntent
            encode_bool(false), // notHeld
            encode_bool(false), // delta-neutral contract: false
            encode_unset(),     // algoStrategy
            encode_bool(false), // whatIf
            encode_unset(),     // orderMiscOptions
            encode_unset(),     // solicited
            encode_bool(false), // randomizeSize
            encode_bool(false), // randomizePrice
            // PEG BENCH fields conditional on order_type — omitted
            // for LMT/IOC.
            encode_int(0),      // conditionsCount
            // conditionsIgnoreRth + conditionsCancelOrder are
            // conditional on conditionsCount > 0 per IBKR API. Skip.
            encode_unset(),     // adjustedOrderType
            encode_unset(),     // triggerPrice
            encode_unset(),     // lmtPriceOffset
            encode_unset(),     // adjustedStopPrice
            encode_unset(),     // adjustedStopLimitPrice
            encode_unset(),     // adjustedTrailingAmount
            encode_int(0),      // adjustableTrailingUnit
            encode_unset(),     // extOperator
            encode_unset(),     // softDollarTier.name
            encode_unset(),     // softDollarTier.value
            encode_unset(),     // cashQty
            encode_unset(),     // mifid2DecisionMaker
            encode_unset(),     // mifid2DecisionAlgo
            encode_unset(),     // mifid2ExecutionTrader
            encode_unset(),     // mifid2ExecutionAlgo
            encode_bool(false), // dontUseAutoPriceForHedge
            encode_bool(false), // isOmsContainer
            encode_bool(false), // discretionaryUpToLimitPrice
            encode_unset(),     // usePriceMgmtAlgo
            encode_int(0),      // duration
            encode_int(0),      // postToAts
            encode_unset(),     // autoCancelParent
            encode_unset(),     // advancedErrorOverride
            encode_unset(),     // manualOrderTime
        ];
        let mut bytes = Vec::with_capacity(256);
        for f in &fields {
            bytes.extend_from_slice(f.as_bytes());
            bytes.push(0);
        }
        bytes
    })
}

/// Hot-path place_order encoder. Splices:
///   [4-byte length] type_id\0 order_id\0 [contract_template] [body] [trailing]
/// Avoids the ~80-string Vec<String> allocation that the original
/// `requests::place_order` does.
pub fn place_order_fast(
    order_id: i32,
    template: &ContractTemplate,
    params: &PlaceOrderParams,
) -> Vec<u8> {
    // Estimate capacity: type+order_id (~12) + template (~80) +
    // body (~100) + trailing (~256) = ~450 bytes. One alloc.
    let trailing = trailing_bytes();
    let est = 4 + 16 + template.bytes.len() + 200 + trailing.len();
    let mut payload = Vec::with_capacity(est);

    // Header: type_id, order_id — both ints, encoded with itoa
    // (no String alloc).
    write_int(&mut payload, crate::messages::OUT_PLACE_ORDER);
    payload.push(0);
    write_int(&mut payload, order_id);
    payload.push(0);

    // Pre-encoded contract section (14 fields)
    payload.extend_from_slice(&template.bytes);

    // Order body — 14 dynamic fields. Floats use ryu directly to
    // the destination buffer (no String allocation per call).
    push_field(&mut payload, params.action.as_bytes());
    write_f64(&mut payload, params.total_quantity);
    payload.push(0);
    push_field(&mut payload, params.order_type.as_bytes());
    write_f64(&mut payload, params.lmt_price);
    payload.push(0);
    write_f64(&mut payload, params.aux_price);
    payload.push(0);
    push_field(&mut payload, params.tif.as_bytes());
    push_field(&mut payload, b""); // ocaGroup
    push_field(&mut payload, params.account.as_bytes());
    push_field(&mut payload, b""); // openClose
    push_field(&mut payload, b"0"); // origin (0 = customer)
    push_field(&mut payload, params.order_ref.as_bytes());
    push_field(&mut payload, b"1"); // transmit
    push_field(&mut payload, b"0"); // parentId
    push_field(&mut payload, b"0"); // blockOrder
    push_field(&mut payload, b"0"); // sweepToFill
    push_field(&mut payload, b"0"); // displaySize
    push_field(&mut payload, b"0"); // triggerMethod
    push_field(&mut payload, if params.outside_rth { b"1" } else { b"0" });
    push_field(&mut payload, b"0"); // hidden
    push_field(&mut payload, b""); // sharesAllocation
    write_f64(&mut payload, 0.0); // discretionaryAmt — must use float
    payload.push(0);               //   format ("0.0") to match the legacy
                                   //   encoder's encode_f64(0.0).
    push_field(&mut payload, params.good_till_date.as_bytes());
    push_field(&mut payload, b""); // goodAfterTime
    push_field(&mut payload, b""); // faGroup
    push_field(&mut payload, b""); // faMethod
    push_field(&mut payload, b""); // faPercentage
    push_field(&mut payload, b""); // faProfile
    push_field(&mut payload, b""); // modelCode
    push_field(&mut payload, b""); // shortSaleSlot
    push_field(&mut payload, b""); // designatedLocation

    // Trailing constants (~50 fields)
    payload.extend_from_slice(trailing);

    // Wrap with length prefix
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

#[inline]
fn push_field(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(bytes);
    buf.push(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requests::{place_order, ContractRequest, PlaceOrderParams};

    fn sample_contract() -> ContractRequest {
        ContractRequest {
            con_id: 12345,
            symbol: "HG".into(),
            sec_type: "FOP".into(),
            last_trade_date: "20260526".into(),
            strike: 6.05,
            right: "C".into(),
            multiplier: "25000".into(),
            exchange: "COMEX".into(),
            primary_exchange: String::new(),
            currency: "USD".into(),
            local_symbol: "HXEK6 P5950".into(),
            trading_class: "HXE".into(),
        }
    }

    fn sample_params() -> PlaceOrderParams {
        PlaceOrderParams {
            action: "SELL".into(),
            total_quantity: 1.0,
            order_type: "LMT".into(),
            lmt_price: 0.0285,
            aux_price: 0.0,
            tif: "GTD".into(),
            good_till_date: "20260526 16:00:00 UTC".into(),
            account: "DUP553657".into(),
            order_ref: "k123".into(),
            outside_rth: false,
        }
    }

    #[test]
    fn fast_template_matches_legacy_byte_for_byte() {
        let c = sample_contract();
        let p = sample_params();
        let order_id = 42;

        let template = ContractTemplate::from_contract(&c);
        let fast = place_order_fast(order_id, &template, &p);
        let slow = place_order(order_id, &c, &p);

        // The two encoders MUST produce identical wire bytes. If they
        // diverge, fast will silently send malformed orders.
        assert_eq!(fast, slow, "fast template encoder diverged from legacy");
    }

    #[test]
    fn template_idempotent() {
        let c = sample_contract();
        let t1 = ContractTemplate::from_contract(&c);
        let t2 = ContractTemplate::from_contract(&c);
        assert_eq!(t1.bytes, t2.bytes);
    }

    #[test]
    fn trailing_bytes_singleton() {
        let a = trailing_bytes();
        let b = trailing_bytes();
        assert!(std::ptr::eq(a, b), "trailing should be cached singleton");
    }
}
