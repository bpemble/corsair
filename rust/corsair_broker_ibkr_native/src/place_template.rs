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

use crate::codec::{encode_bool, encode_f64, encode_int, encode_unset, write_f64, write_int, write_qty};
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
        // Field order verified byte-for-byte against ib_insync 0.9.86's
        // Client.placeOrder. Indices below are the on-wire frame
        // position (after type [0], orderId [1], conId [2], contract
        // [3..15], action [16] ... designatedLocation [45]).
        let fields: Vec<String> = vec![
            encode_int(-1),     // [46] exemptCode
            encode_int(0),      // [47] ocaType
            encode_unset(),     // [48] rule80A
            encode_unset(),     // [49] settlingFirm
            encode_bool(false), // [50] allOrNone
            encode_unset(),     // [51] minQty
            encode_unset(),     // [52] percentOffset
            encode_bool(false), // [53] eTradeOnly
            encode_bool(false), // [54] firmQuoteOnly
            encode_unset(),     // [55] nbboPriceCap
            encode_int(0),      // [56] auctionStrategy
            encode_unset(),     // [57] startingPrice
            encode_unset(),     // [58] stockRefPrice
            encode_unset(),     // [59] delta
            encode_unset(),     // [60] stockRangeLower
            encode_unset(),     // [61] stockRangeUpper
            encode_bool(false), // [62] overridePercentageConstraints
            encode_unset(),     // [63] volatility
            encode_unset(),     // [64] volatilityType
            encode_unset(),     // [65] deltaNeutralOrderType
            encode_unset(),     // [66] deltaNeutralAuxPrice
            encode_bool(false), // [67] continuousUpdate ('0' not unset)
            encode_unset(),     // [68] referencePriceType
            encode_unset(),     // [69] trailStopPrice
            encode_unset(),     // [70] trailingPercent
            encode_unset(),     // [71] scaleInitLevelSize
            encode_unset(),     // [72] scaleSubsLevelSize
            encode_unset(),     // [73] scalePriceIncrement
            encode_unset(),     // [74] scaleTable
            encode_unset(),     // [75] activeStartTime
            encode_unset(),     // [76] activeStopTime
            encode_unset(),     // [77] hedgeType
            encode_bool(false), // [78] optOutSmartRouting ('0' not unset)
            encode_unset(),     // [79] clearingAccount
            encode_unset(),     // [80] clearingIntent
            encode_bool(false), // [81] notHeld
            encode_bool(false), // [82] delta-neutral contract: false
            encode_unset(),     // [83] algoStrategy
            // [84] algoId — was MISSING in our prior version. Caused
            // every field after this to shift by 1, making the server
            // mis-align and reject with a parade of misleading errors
            // (10300 "Manual Order Time", 391 "End Time", etc.).
            encode_unset(),     // [84] algoId
            encode_bool(false), // [85] whatIf
            encode_unset(),     // [86] orderMiscOptions
            encode_bool(false), // [87] solicited ('0' not unset)
            encode_bool(false), // [88] randomizeSize
            encode_bool(false), // [89] randomizePrice
            // PEG BENCH fields conditional on orderType — omitted (LMT).
            encode_int(0),      // [90] conditionsCount (no conditions)
            // conditionsIgnoreRth + conditionsCancelOrder are
            // conditional on conditionsCount > 0. Skip.
            encode_unset(),     // [91] adjustedOrderType
            encode_unset(),     // [92] triggerPrice
            encode_unset(),     // [93] lmtPriceOffset
            encode_unset(),     // [94] adjustedStopPrice
            encode_unset(),     // [95] adjustedStopLimitPrice
            encode_unset(),     // [96] adjustedTrailingAmount
            encode_int(0),      // [97] adjustableTrailingUnit
            encode_unset(),     // [98] extOperator
            encode_unset(),     // [99] softDollarTier.name
            encode_unset(),     // [100] softDollarTier.value
            encode_unset(),     // [101] cashQty
            encode_unset(),     // [102] mifid2DecisionMaker
            encode_unset(),     // [103] mifid2DecisionAlgo
            encode_unset(),     // [104] mifid2ExecutionTrader
            encode_unset(),     // [105] mifid2ExecutionAlgo
            encode_bool(false), // [106] dontUseAutoPriceForHedge
            encode_bool(false), // [107] isOmsContainer
            encode_bool(false), // [108] discretionaryUpToLimitPrice
            encode_bool(false), // [109] usePriceMgmtAlgo (False default)
            // server >= 158: duration. Default UNSET_INT renders ''.
            encode_unset(),     // [110] duration
            // server >= 160: postToAts. Default UNSET_INT renders ''.
            encode_unset(),     // [111] postToAts
            // server >= 162: autoCancelParent. Default False renders '0'.
            encode_bool(false), // [112] autoCancelParent
            // server >= 166: advancedErrorOverride.
            encode_unset(),     // [113] advancedErrorOverride
            // server >= 169: manualOrderTime.
            encode_unset(),     // [114] manualOrderTime
            // server >= 170 IBKRATS / PEG conditionals: not applicable
            // for LMT on COMEX, so nothing more is sent.
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

    // Order body — dynamic fields. Floats use ryu directly to
    // the destination buffer (no String allocation per call).
    //
    // Field order verified byte-for-byte against ib_insync 0.9.86's
    // Client.placeOrder (the known-good reference). Three bugs were
    // fixed during the v3 cutover after a byte-diff with ib_insync:
    //   1. goodAfterTime + goodTillDate were SWAPPED — ib_insync
    //      sends goodAfterTime FIRST, then goodTillDate. The swap
    //      caused IBKR to read goodTillDate at the goodAfterTime
    //      position and reject with error 391 "End Time invalid".
    //   2. openClose default was empty — should be "O" (open).
    //   3. aux_price for non-stop orders should be UNSET_DOUBLE (sent
    //      as empty string) not "0.0".
    push_field(&mut payload, params.action.as_bytes());
    write_qty(&mut payload, params.total_quantity);
    payload.push(0);
    push_field(&mut payload, params.order_type.as_bytes());
    write_f64(&mut payload, params.lmt_price);
    payload.push(0);
    // aux_price: empty when 0.0 (UNSET_DOUBLE convention) — only
    // populated for STP/TRAIL/STP LMT.
    if params.aux_price == 0.0 {
        push_field(&mut payload, b"");
    } else {
        write_f64(&mut payload, params.aux_price);
        payload.push(0);
    }
    push_field(&mut payload, params.tif.as_bytes());
    push_field(&mut payload, b""); // ocaGroup
    push_field(&mut payload, params.account.as_bytes());
    push_field(&mut payload, b"O"); // openClose (default 'O' = open)
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
    write_f64(&mut payload, 0.0); // discretionaryAmt
    payload.push(0);
    push_field(&mut payload, b""); // goodAfterTime (FIRST per ib_insync)
    push_field(&mut payload, params.good_till_date.as_bytes()); // goodTillDate (SECOND)
    push_field(&mut payload, b""); // faGroup
    push_field(&mut payload, b""); // faMethod
    push_field(&mut payload, b""); // faPercentage
    push_field(&mut payload, b""); // faProfile
    push_field(&mut payload, b""); // modelCode
    push_field(&mut payload, b"0"); // shortSaleSlot — ib_insync default 0
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
#[allow(deprecated)] // parity test deliberately exercises the slow path.
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
