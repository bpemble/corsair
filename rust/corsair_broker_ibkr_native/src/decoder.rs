//! Inbound message decoder.
//!
//! Each inbound IBKR message starts with a numeric type id (decimal
//! string) followed by version + body fields. We dispatch on type
//! id to a typed parser; anything we don't have a parser for yet
//! returns InboundMsg::Unparsed.
//!
//! Field order must match ib_insync's `Decoder` methods byte-for-
//! byte — IBKR's wire format is positional, not tagged.

use crate::codec::{parse_f64, parse_int, parse_int64};
use crate::error::NativeError;
use crate::messages::*;
use crate::types::*;

/// Top-level dispatch. `fields` is one decoded frame from
/// `try_decode_frame` — already split on `\0`.
pub fn parse_inbound(fields: &[String]) -> Result<InboundMsg, NativeError> {
    if fields.is_empty() {
        return Err(NativeError::Malformed("empty frame".into()));
    }
    let ty = parse_int(&fields[0])?;
    match ty {
        IN_TICK_PRICE => parse_tick_price(fields),
        IN_TICK_SIZE => parse_tick_size(fields),
        IN_TICK_OPTION_COMPUTATION => parse_tick_option_computation(fields),
        IN_MARKET_DEPTH => parse_market_depth(fields, false),
        IN_MARKET_DEPTH_L2 => parse_market_depth(fields, true),
        IN_ORDER_STATUS => parse_order_status(fields),
        IN_ERR_MSG => parse_error(fields),
        IN_OPEN_ORDER => parse_open_order(fields),
        IN_ACCT_VALUE => parse_account_value(fields),
        IN_PORTFOLIO_VALUE => Ok(InboundMsg::Unparsed {
            type_id: ty,
            fields: fields.to_vec(),
        }),
        IN_ACCT_UPDATE_TIME => Ok(InboundMsg::AccountUpdateTime(
            fields.get(2).cloned().unwrap_or_default(),
        )),
        IN_NEXT_VALID_ID => {
            // [9, 1, orderId]
            let oid = parse_int(fields.get(2).map(|s| s.as_str()).unwrap_or("0"))?;
            Ok(InboundMsg::NextValidId(oid))
        }
        IN_CONTRACT_DATA => parse_contract_details(fields),
        IN_CONTRACT_DATA_END => {
            // [52, version=1, reqId]
            let req_id = parse_int(fields.get(2).map(|s| s.as_str()).unwrap_or("0"))?;
            Ok(InboundMsg::ContractDetailsEnd(req_id))
        }
        IN_EXECUTION_DATA => parse_execution(fields),
        IN_MANAGED_ACCTS => {
            // [15, 1, "DFP...,DUP...,..."]
            let csv = fields.get(2).cloned().unwrap_or_default();
            let accts: Vec<String> = csv
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            Ok(InboundMsg::ManagedAccounts(accts))
        }
        IN_COMMISSION_REPORT => parse_commission(fields),
        IN_POSITION_DATA => parse_position(fields),
        IN_POSITION_END => Ok(InboundMsg::PositionEnd),
        IN_OPEN_ORDER_END => Ok(InboundMsg::OpenOrderEnd),
        IN_ACCOUNT_DOWNLOAD_END => Ok(InboundMsg::AccountDownloadEnd(
            fields.get(2).cloned().unwrap_or_default(),
        )),
        IN_EXECUTION_DATA_END => {
            // Same version-stripping as parse_execution: server >=
            // MIN_SERVER_VER_LAST_LIQUIDITY removed the version field
            // on both 11 (execDetails) and 55 (executionEnd). req_id
            // is now at field[1], not field[2].
            let req_id = parse_int(fields.get(1).map(|s| s.as_str()).unwrap_or("0"))?;
            Ok(InboundMsg::ExecutionEnd(req_id))
        }
        _ => Ok(InboundMsg::Unparsed {
            type_id: ty,
            fields: fields.to_vec(),
        }),
    }
}

// ─── Per-type parsers ────────────────────────────────────────────

/// `[1, version, reqId, tickType, price, size_or_unset, attribs]`
/// Note: tickPrice may include an embedded size (deprecated path).
fn parse_tick_price(fields: &[String]) -> Result<InboundMsg, NativeError> {
    if fields.len() < 6 {
        return Err(NativeError::Malformed(format!(
            "tickPrice fields={}", fields.len()
        )));
    }
    Ok(InboundMsg::TickPrice(TickPriceMsg {
        req_id: parse_int(&fields[2])?,
        tick_type: parse_int(&fields[3])?,
        price: parse_f64(&fields[4])?,
        attribs: parse_int(fields.get(6).map(|s| s.as_str()).unwrap_or("0"))?,
    }))
}

/// `[2, version, reqId, tickType, size]`
fn parse_tick_size(fields: &[String]) -> Result<InboundMsg, NativeError> {
    if fields.len() < 5 {
        return Err(NativeError::Malformed(format!(
            "tickSize fields={}", fields.len()
        )));
    }
    Ok(InboundMsg::TickSize(TickSizeMsg {
        req_id: parse_int(&fields[2])?,
        tick_type: parse_int(&fields[3])?,
        size: parse_f64(&fields[4])?,
    }))
}

/// Market depth update.
///
/// Layout (no version field — that's normal for these msg types):
///   IN_MARKET_DEPTH (12, single mkt-maker / smart):
///     `[12, reqId, position, operation, side, price, size]`
///   IN_MARKET_DEPTH_L2 (13, level-2 with mkt-maker code):
///     `[13, reqId, position, marketMaker, operation, side, price, size,
///      isSmartDepth]` (isSmartDepth at server >= MIN_SERVER_VER_SMART_DEPTH)
///
/// Operations: 0=insert, 1=update, 2=delete.
/// Sides:      0=ask, 1=bid.
fn parse_market_depth(fields: &[String], is_l2: bool) -> Result<InboundMsg, NativeError> {
    let needed = if is_l2 { 8 } else { 7 };
    if fields.len() < needed {
        return Err(NativeError::Malformed(format!(
            "marketDepth (L2={}) fields={}", is_l2, fields.len()
        )));
    }
    if is_l2 {
        // [13, reqId, pos, mm, op, side, price, size, smart]
        Ok(InboundMsg::MarketDepth(MarketDepthMsg {
            req_id: parse_int(&fields[1])?,
            position: parse_int(&fields[2])?,
            market_maker: Some(fields[3].clone()),
            operation: parse_int(&fields[4])?,
            side: parse_int(&fields[5])?,
            price: parse_f64(&fields[6])?,
            size: parse_f64(&fields[7])?,
            is_smart_depth: fields
                .get(8)
                .and_then(|s| s.parse::<i32>().ok())
                .map(|v| v == 1)
                .unwrap_or(false),
        }))
    } else {
        // [12, reqId, pos, op, side, price, size]
        Ok(InboundMsg::MarketDepth(MarketDepthMsg {
            req_id: parse_int(&fields[1])?,
            position: parse_int(&fields[2])?,
            market_maker: None,
            operation: parse_int(&fields[3])?,
            side: parse_int(&fields[4])?,
            price: parse_f64(&fields[5])?,
            size: parse_f64(&fields[6])?,
            is_smart_depth: false,
        }))
    }
}

/// Server >= 173: tickOptionComputation has an attrib field after
/// tickType. Layout: `[21, reqId, tickType, attrib, iv, delta,
/// optPrice, pvDividend, gamma, vega, theta, undPrice]`.
///
/// VERIFY: field offsets vs ib_insync `wrapper.tickOptionComputation`
/// — covered by `parse_tick_option_computation_basic` in tests.
fn parse_tick_option_computation(fields: &[String]) -> Result<InboundMsg, NativeError> {
    if fields.len() < 12 {
        return Err(NativeError::Malformed(format!(
            "tickOptionComputation fields={}", fields.len()
        )));
    }
    Ok(InboundMsg::TickOptionComputation(TickOptionComputationMsg {
        req_id: parse_int(&fields[1])?,
        tick_type: parse_int(&fields[2])?,
        tick_attrib: parse_int(&fields[3])?,
        implied_vol: parse_f64(&fields[4])?,
        delta: parse_f64(&fields[5])?,
        opt_price: parse_f64(&fields[6])?,
        pv_dividend: parse_f64(&fields[7])?,
        gamma: parse_f64(&fields[8])?,
        vega: parse_f64(&fields[9])?,
        theta: parse_f64(&fields[10])?,
        und_price: parse_f64(&fields[11])?,
    }))
}

/// `[3, orderId, status, filled, remaining, avgFillPrice, permId,
///   parentId, lastFillPrice, clientId, whyHeld, mktCapPrice]`
fn parse_order_status(fields: &[String]) -> Result<InboundMsg, NativeError> {
    if fields.len() < 12 {
        return Err(NativeError::Malformed(format!(
            "orderStatus fields={}", fields.len()
        )));
    }
    Ok(InboundMsg::OrderStatus(OrderStatusMsg {
        order_id: parse_int(&fields[1])?,
        status: fields[2].clone(),
        filled: parse_f64(&fields[3])?,
        remaining: parse_f64(&fields[4])?,
        avg_fill_price: parse_f64(&fields[5])?,
        perm_id: parse_int(&fields[6])?,
        parent_id: parse_int(&fields[7])?,
        last_fill_price: parse_f64(&fields[8])?,
        client_id: parse_int(&fields[9])?,
        why_held: fields[10].clone(),
        mkt_cap_price: parse_f64(&fields[11])?,
    }))
}

/// `[4, version, reqId, errorCode, errorString, advancedOrderRejectJson?]`
///
/// VERIFY: field offsets vs ib_insync `wrapper.error` —
/// errMsg always retains the version field on every API version
/// we negotiate (≤176). Covered by `parse_error_basic` and
/// `parse_error_advanced_order_reject_json` in tests.
fn parse_error(fields: &[String]) -> Result<InboundMsg, NativeError> {
    if fields.len() < 5 {
        return Err(NativeError::Malformed(format!(
            "errMsg fields={}", fields.len()
        )));
    }
    Ok(InboundMsg::Error(ErrorMsg {
        req_id: parse_int(&fields[2])?,
        error_code: parse_int(&fields[3])?,
        error_string: fields[4].clone(),
        advanced_order_reject_json: fields.get(5).cloned().unwrap_or_default(),
    }))
}

/// Account value: `[6, version, key, value, currency, accountName]`
fn parse_account_value(fields: &[String]) -> Result<InboundMsg, NativeError> {
    if fields.len() < 6 {
        return Err(NativeError::Malformed(format!(
            "accountValue fields={}", fields.len()
        )));
    }
    Ok(InboundMsg::AccountValue(AccountValueMsg {
        key: fields[2].clone(),
        value: fields[3].clone(),
        currency: fields[4].clone(),
        account: fields[5].clone(),
    }))
}

/// Position: `[61, version, account, conId, symbol, secType, expiry,
/// strike, right, multiplier, exchange, currency, localSymbol,
/// tradingClass, position, avgCost]`
fn parse_position(fields: &[String]) -> Result<InboundMsg, NativeError> {
    if fields.len() < 16 {
        return Err(NativeError::Malformed(format!(
            "position fields={}", fields.len()
        )));
    }
    Ok(InboundMsg::Position(PositionMsg {
        account: fields[2].clone(),
        contract: ContractDecoded {
            con_id: parse_int64(&fields[3])?,
            symbol: fields[4].clone(),
            sec_type: fields[5].clone(),
            last_trade_date: fields[6].clone(),
            strike: parse_f64(&fields[7])?,
            right: fields[8].clone(),
            multiplier: fields[9].clone(),
            exchange: fields[10].clone(),
            primary_exchange: String::new(),
            currency: fields[11].clone(),
            local_symbol: fields[12].clone(),
            trading_class: fields[13].clone(),
        },
        position: parse_f64(&fields[14])?,
        avg_cost: parse_f64(&fields[15])?,
    }))
}

/// Contract details (subset): the IBKR message has many fields.
/// We extract just enough to reconstruct a Contract; the rest can
/// be added as needed.
///
/// Layout (post-server-176): `[10, reqId, symbol, secType, expiry,
///   strike, right, exchange, currency, localSymbol, marketName,
///   tradingClass, conId, minTick, mdSizeMultiplier, multiplier,
///   ...many more...]`
fn parse_contract_details(fields: &[String]) -> Result<InboundMsg, NativeError> {
    // Bumped 13→16 to cover the multiplier slot at fields[15] that
    // we read below. Without this, a truncated contractDetails
    // (rare but possible on partially-permissioned products) would
    // pass the gate and then silently default-empty the multiplier
    // via .get().unwrap_or_default(); fail loud instead.
    if fields.len() < 16 {
        return Err(NativeError::Malformed(format!(
            "contractDetails fields={}", fields.len()
        )));
    }
    Ok(InboundMsg::ContractDetails(ContractDetailsMsg {
        req_id: parse_int(&fields[1])?,
        contract: ContractDecoded {
            symbol: fields[2].clone(),
            sec_type: fields[3].clone(),
            last_trade_date: fields[4].clone(),
            strike: parse_f64(&fields[5])?,
            right: fields[6].clone(),
            exchange: fields[7].clone(),
            currency: fields[8].clone(),
            local_symbol: fields[9].clone(),
            // marketName at fields[10]
            trading_class: fields[11].clone(),
            con_id: parse_int64(&fields[12])?,
            // multiplier at fields[15]
            multiplier: fields.get(15).cloned().unwrap_or_default(),
            primary_exchange: String::new(),
        },
    }))
}

/// IBKR removed the version field on execDetails at server version
/// 78 (MIN_SERVER_VER_LAST_LIQUIDITY). Modern gateways (>=176, our
/// MAX_CLIENT_VERSION) never send it. Layout used here:
///   `[11, reqId, orderId, conId, symbol, secType, expiry, strike,
///    right, multiplier, exchange, currency, localSymbol,
///    tradingClass, execId, time, acctNumber, exchange, side,
///    shares, price, permId, clientId, liquidation, cumQty,
///    avgPrice, orderRef, evRule, evMultiplier, modelCode,
///    lastLiquidity]`
///
/// Pre-fix this parser assumed a version field at field[1] and read
/// the symbol as conId, so every execDetails on a fresh gateway
/// failed with "expected int64, got HG". The dispatcher silently
/// dropped these, causing pump_fills to never see fills, hedge
/// rebalance_on_fill never to fire, and a $30K cascade on
/// 2026-05-04 11:51 because we couldn't observe our own pile-up.
fn parse_execution(fields: &[String]) -> Result<InboundMsg, NativeError> {
    if fields.len() < 23 {
        return Err(NativeError::Malformed(format!(
            "execDetails fields={}", fields.len()
        )));
    }
    Ok(InboundMsg::Execution(ExecutionMsg {
        req_id: parse_int(&fields[1])?,
        order_id: parse_int(&fields[2])?,
        contract: ContractDecoded {
            con_id: parse_int64(&fields[3])?,
            symbol: fields[4].clone(),
            sec_type: fields[5].clone(),
            last_trade_date: fields[6].clone(),
            strike: parse_f64(&fields[7])?,
            right: fields[8].clone(),
            multiplier: fields[9].clone(),
            exchange: fields[10].clone(),
            currency: fields[11].clone(),
            local_symbol: fields[12].clone(),
            trading_class: fields[13].clone(),
            primary_exchange: String::new(),
        },
        exec_id: fields[14].clone(),
        time: fields[15].clone(),
        account: fields[16].clone(),
        exchange: fields[17].clone(),
        side: fields[18].clone(),
        shares: parse_f64(&fields[19])?,
        price: parse_f64(&fields[20])?,
        perm_id: parse_int(&fields[21])?,
        client_id: parse_int(&fields[22])?,
        liquidation: parse_int(fields.get(23).map(|s| s.as_str()).unwrap_or("0"))?,
        cum_qty: parse_f64(fields.get(24).map(|s| s.as_str()).unwrap_or("0"))?,
        avg_price: parse_f64(fields.get(25).map(|s| s.as_str()).unwrap_or("0"))?,
        order_ref: fields.get(26).cloned().unwrap_or_default(),
    }))
}

/// `[59, version, execId, commission, currency, realizedPNL,
///   yield, yieldRedemptionDate]`
fn parse_commission(fields: &[String]) -> Result<InboundMsg, NativeError> {
    if fields.len() < 6 {
        return Err(NativeError::Malformed(format!(
            "commissionReport fields={}", fields.len()
        )));
    }
    Ok(InboundMsg::CommissionReport(CommissionReportMsg {
        exec_id: fields[2].clone(),
        commission: parse_f64(&fields[3])?,
        currency: fields[4].clone(),
        realized_pnl: parse_f64(&fields[5])?,
    }))
}

/// `[5, orderId, conId, symbol, secType, expiry, strike, right,
///   multiplier, exchange, currency, localSymbol, tradingClass,
///   action, totalQty, orderType, lmtPrice, ...many more...]`
///
/// The full openOrder message is enormous — we extract just the
/// fields we route. Phase 6.5+ can extend this as the OMS needs more.
fn parse_open_order(fields: &[String]) -> Result<InboundMsg, NativeError> {
    if fields.len() < 17 {
        return Err(NativeError::Malformed(format!(
            "openOrder fields={}", fields.len()
        )));
    }
    // Parse the order body fields safely. IBKR's openOrder is
    // ~80 fields long; we extract the ones that matter for
    // OMS / modify_order semantics. Use .get() so we degrade
    // gracefully if a field is missing rather than panicking.
    let get = |idx: usize| -> &str {
        fields.get(idx).map(|s| s.as_str()).unwrap_or("")
    };
    Ok(InboundMsg::OpenOrder(OpenOrderMsg {
        order_id: parse_int(&fields[1])?,
        contract: ContractDecoded {
            con_id: parse_int64(&fields[2])?,
            symbol: fields[3].clone(),
            sec_type: fields[4].clone(),
            last_trade_date: fields[5].clone(),
            strike: parse_f64(&fields[6])?,
            right: fields[7].clone(),
            multiplier: fields[8].clone(),
            exchange: fields[9].clone(),
            currency: fields[10].clone(),
            local_symbol: fields[11].clone(),
            trading_class: fields[12].clone(),
            primary_exchange: String::new(),
        },
        action: fields[13].clone(),
        total_quantity: parse_f64(&fields[14])?,
        order_type: fields[15].clone(),
        lmt_price: parse_f64(&fields[16])?,
        // Order body continues. ib_insync field order:
        //   17: aux_price (used by STP / STP LMT)
        //   18: tif
        //   19: oca_group
        //   20: account
        //   21: open_close
        //   22: origin
        //   23: order_ref
        // We only need tif, account, order_ref for modify path.
        // aux_price isn't kept on OpenOrderMsg today; STP_LMT
        // modify isn't a path corsair uses.
        tif: get(18).to_string(),
        account: get(20).to_string(),
        order_ref: get(23).to_string(),
        status: String::new(),
        filled: 0.0,
        remaining: 0.0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_next_valid_id() {
        let f = fields(&["9", "1", "42"]);
        let m = parse_inbound(&f).unwrap();
        assert!(matches!(m, InboundMsg::NextValidId(42)));
    }

    #[test]
    fn parse_managed_accounts_csv() {
        let f = fields(&["15", "1", "DFP1,DUP2,DUP3"]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::ManagedAccounts(v) => assert_eq!(v, vec!["DFP1", "DUP2", "DUP3"]),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_position_end() {
        let f = fields(&["62", "1"]);
        let m = parse_inbound(&f).unwrap();
        assert!(matches!(m, InboundMsg::PositionEnd));
    }

    #[test]
    fn parse_account_download_end_includes_account() {
        let f = fields(&["54", "1", "DUP553657"]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::AccountDownloadEnd(a) => assert_eq!(a, "DUP553657"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_account_value_basic() {
        let f = fields(&["6", "2", "NetLiquidation", "500000.00", "USD", "DUP553657"]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::AccountValue(av) => {
                assert_eq!(av.key, "NetLiquidation");
                assert_eq!(av.value, "500000.00");
                assert_eq!(av.currency, "USD");
                assert_eq!(av.account, "DUP553657");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_error_basic() {
        let f = fields(&["4", "2", "1", "200", "No security definition", ""]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::Error(e) => {
                assert_eq!(e.req_id, 1);
                assert_eq!(e.error_code, 200);
                assert_eq!(e.error_string, "No security definition");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_position_basic() {
        let f = fields(&[
            "61", "3", "DUP553657", "712565973", "HG", "FUT", "20260626", "0", "",
            "25000", "COMEX", "USD", "HGM6", "HG", "5", "150125.00",
        ]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::Position(p) => {
                assert_eq!(p.account, "DUP553657");
                assert_eq!(p.contract.symbol, "HG");
                assert_eq!(p.contract.con_id, 712565973);
                assert_eq!(p.position, 5.0);
                assert!((p.avg_cost - 150125.0).abs() < 1e-6);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_tick_price_basic() {
        let f = fields(&["1", "6", "1001", "1", "6.05", "0", "0"]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::TickPrice(t) => {
                assert_eq!(t.req_id, 1001);
                assert_eq!(t.tick_type, 1); // bid
                assert!((t.price - 6.05).abs() < 1e-9);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_tick_size_basic() {
        let f = fields(&["2", "6", "1001", "0", "5"]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::TickSize(t) => {
                assert_eq!(t.req_id, 1001);
                assert_eq!(t.tick_type, 0); // bid_size
                assert_eq!(t.size, 5.0);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_unknown_type_returns_unparsed() {
        let f = fields(&["999", "1", "garbage"]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::Unparsed { type_id, .. } => assert_eq!(type_id, 999),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_order_status_basic() {
        let f = fields(&[
            "3", "42", "Submitted", "0", "1", "0", "0", "0", "0", "0", "", "0",
        ]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::OrderStatus(o) => {
                assert_eq!(o.order_id, 42);
                assert_eq!(o.status, "Submitted");
                assert_eq!(o.remaining, 1.0);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_tick_option_computation_basic() {
        // Server >= 173 layout: [21, reqId, tickType, attrib, iv,
        //   delta, optPrice, pvDividend, gamma, vega, theta, undPrice].
        // Cross-checked against ib_insync's wrapper.tickOptionComputation
        // decoder reference (no version field on this msg type).
        let f = fields(&[
            "21", "1001", "13", "0", "0.42", "-0.31", "0.0125",
            "0.001", "0.012", "0.05", "-0.02", "6.05",
        ]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::TickOptionComputation(t) => {
                assert_eq!(t.req_id, 1001);
                assert_eq!(t.tick_type, 13);
                assert!((t.implied_vol - 0.42).abs() < 1e-9);
                assert!((t.delta - -0.31).abs() < 1e-9);
                assert!((t.gamma - 0.012).abs() < 1e-9);
                assert!((t.vega - 0.05).abs() < 1e-9);
                assert!((t.theta - -0.02).abs() < 1e-9);
                assert!((t.und_price - 6.05).abs() < 1e-9);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_error_advanced_order_reject_json() {
        // VERIFY: field offsets vs ib_insync — errMsg always carries
        // a version field on all wire-protocol versions we speak (we
        // hold MAX_CLIENT_VERSION at 176; ib_insync's errMsg decoder
        // has not changed shape).
        let f = fields(&[
            "4", "2", "42", "201",
            "Order rejected — insufficient margin",
            "{\"reason\":\"margin\"}",
        ]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::Error(e) => {
                assert_eq!(e.req_id, 42);
                assert_eq!(e.error_code, 201);
                assert!(e.error_string.contains("rejected"));
                assert!(e.advanced_order_reject_json.contains("margin"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_commission_basic() {
        let f = fields(&[
            "59", "1", "00018037.6816a72b.01.01", "2.50", "USD", "0.00", "0", "0",
        ]);
        let m = parse_inbound(&f).unwrap();
        match m {
            InboundMsg::CommissionReport(c) => {
                assert_eq!(c.exec_id, "00018037.6816a72b.01.01");
                assert!((c.commission - 2.50).abs() < 1e-9);
                assert_eq!(c.currency, "USD");
            }
            _ => panic!("wrong variant"),
        }
    }
}
