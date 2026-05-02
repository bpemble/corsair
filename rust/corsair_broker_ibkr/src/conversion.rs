//! Convert between `ib_insync` Python objects and `corsair_broker_api`
//! Rust types.
//!
//! All conversions go through Python::with_gil. Failures map to
//! `BrokerError::Protocol` with a descriptive message — they indicate
//! the gateway returned something we didn't expect, which is worth
//! logging and continuing rather than crashing.

use chrono::{DateTime, NaiveDate, Utc};
use corsair_broker_api::{
    capabilities::BrokerCapabilities, AccountSnapshot, BrokerError, Contract, ContractKind,
    Currency, Exchange, InstrumentId, OrderId, OrderStatus, OrderType, Position,
    Result as BrokerResult, Right, Side, TimeInForce,
};
use corsair_broker_api::events::Fill;
use pyo3::prelude::*;
use pyo3::types::PyAny;

// ─── Helpers ────────────────────────────────────────────────────────

/// Extract a string attribute, defaulting to empty if missing/None.
fn py_str_or_default(obj: &Bound<'_, PyAny>, name: &str) -> String {
    obj.getattr(name)
        .ok()
        .and_then(|v| v.extract::<String>().ok())
        .unwrap_or_default()
}

/// Extract an i64 attribute, defaulting to 0.
fn py_i64_or_default(obj: &Bound<'_, PyAny>, name: &str) -> i64 {
    obj.getattr(name)
        .ok()
        .and_then(|v| v.extract::<i64>().ok())
        .unwrap_or(0)
}

/// Extract an f64 attribute, defaulting to 0.0.
fn py_f64_or_default(obj: &Bound<'_, PyAny>, name: &str) -> f64 {
    obj.getattr(name)
        .ok()
        .and_then(|v| v.extract::<f64>().ok())
        .unwrap_or(0.0)
}

/// Parse a YYYYMMDD string into a NaiveDate.
pub fn parse_yyyymmdd(s: &str) -> Option<NaiveDate> {
    if s.len() < 8 {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let m: u32 = s[4..6].parse().ok()?;
    let d: u32 = s[6..8].parse().ok()?;
    NaiveDate::from_ymd_opt(y, m, d)
}

pub fn format_yyyymmdd(d: NaiveDate) -> String {
    d.format("%Y%m%d").to_string()
}

/// Convert ib_insync's exchange string (e.g., "COMEX", "CME").
pub fn parse_exchange(s: &str) -> Exchange {
    match s.to_ascii_uppercase().as_str() {
        "CME" | "GLOBEX" => Exchange::Cme,
        "COMEX" => Exchange::Comex,
        "NYMEX" => Exchange::Nymex,
        "CBOT" | "ECBOT" => Exchange::Cbot,
        _ => Exchange::Other(0),
    }
}

pub fn format_exchange(e: Exchange) -> &'static str {
    match e {
        Exchange::Cme => "CME",
        Exchange::Comex => "COMEX",
        Exchange::Nymex => "NYMEX",
        Exchange::Cbot => "CBOT",
        Exchange::Other(_) => "SMART",
    }
}

pub fn parse_currency(s: &str) -> Currency {
    match s.to_ascii_uppercase().as_str() {
        "EUR" => Currency::Eur,
        "GBP" => Currency::Gbp,
        "JPY" => Currency::Jpy,
        _ => Currency::Usd,
    }
}

pub fn format_currency(c: Currency) -> &'static str {
    match c {
        Currency::Usd => "USD",
        Currency::Eur => "EUR",
        Currency::Gbp => "GBP",
        Currency::Jpy => "JPY",
    }
}

// ─── Contract ──────────────────────────────────────────────────────

/// Convert an ib_insync `Contract` Python object into our Contract type.
/// Caller must hold the GIL.
pub fn contract_from_py(obj: &Bound<'_, PyAny>) -> BrokerResult<Contract> {
    let symbol = py_str_or_default(obj, "symbol");
    let local_symbol = py_str_or_default(obj, "localSymbol");
    let sec_type = py_str_or_default(obj, "secType");
    let con_id = py_i64_or_default(obj, "conId") as u64;
    let exchange_str = py_str_or_default(obj, "exchange");
    let currency_str = py_str_or_default(obj, "currency");
    let multiplier_str = py_str_or_default(obj, "multiplier");
    let multiplier: f64 = multiplier_str.parse().unwrap_or(1.0);
    let expiry_str = py_str_or_default(obj, "lastTradeDateOrContractMonth");
    let expiry = parse_yyyymmdd(&expiry_str).unwrap_or_else(|| {
        // Fallback: next-month end-of-month (ensures the field is
        // populated even when we can't parse). Caller should expect
        // garbage here only on contracts where ib_insync didn't fill
        // in the expiry — typically index/spot.
        NaiveDate::from_ymd_opt(2099, 12, 31).unwrap()
    });

    let kind = match sec_type.as_str() {
        "FUT" => ContractKind::Future,
        "OPT" | "FOP" => ContractKind::Option,
        _ => {
            return Err(BrokerError::Protocol {
                code: None,
                message: format!("unsupported secType: {sec_type}"),
            });
        }
    };

    let strike = match kind {
        ContractKind::Option => Some(py_f64_or_default(obj, "strike")),
        ContractKind::Future => None,
    };

    let right = match kind {
        ContractKind::Option => {
            let r = py_str_or_default(obj, "right");
            r.chars().next().and_then(Right::from_char)
        }
        ContractKind::Future => None,
    };

    Ok(Contract {
        instrument_id: InstrumentId(con_id),
        kind,
        symbol,
        local_symbol,
        expiry,
        strike,
        right,
        multiplier,
        exchange: parse_exchange(&exchange_str),
        currency: parse_currency(&currency_str),
    })
}

// ─── Position ──────────────────────────────────────────────────────

/// Convert an ib_insync `Position` namedtuple. Position has fields:
/// account, contract, position, avgCost.
pub fn position_from_py(obj: &Bound<'_, PyAny>) -> BrokerResult<Position> {
    let contract_obj = obj.getattr("contract").map_err(|e| BrokerError::Protocol {
        code: None,
        message: format!("position has no contract: {e}"),
    })?;
    let contract = contract_from_py(&contract_obj)?;
    let quantity = py_f64_or_default(obj, "position") as i32;
    let avg_cost = py_f64_or_default(obj, "avgCost");
    // ib_insync's Position doesn't carry P&L; populate from elsewhere.
    Ok(Position {
        contract,
        quantity,
        avg_cost,
        realized_pnl: 0.0,
        unrealized_pnl: 0.0,
    })
}

// ─── Account values ────────────────────────────────────────────────

/// Reduce a list of ib_insync AccountValue rows to an AccountSnapshot.
/// Fields: tag (str), value (str), currency (str), modelCode (str), account (str).
///
/// We pick the rows with currency=="USD" (or modelCode=="" for the base
/// account) for our keys of interest.
pub fn account_snapshot_from_rows(rows: &Bound<'_, PyAny>) -> BrokerResult<AccountSnapshot> {
    let mut net_liquidation = 0.0;
    let mut maintenance_margin = 0.0;
    let mut initial_margin = 0.0;
    let mut buying_power = 0.0;
    let mut realized_pnl_today = 0.0;

    let iter = rows.iter().map_err(|e| BrokerError::Protocol {
        code: None,
        message: format!("accountValues not iterable: {e}"),
    })?;
    for row_res in iter {
        let row = row_res.map_err(|e| BrokerError::Protocol {
            code: None,
            message: format!("accountValues iter error: {e}"),
        })?;
        let tag = py_str_or_default(&row, "tag");
        let value_str = py_str_or_default(&row, "value");
        let currency = py_str_or_default(&row, "currency");
        if currency != "USD" && currency != "BASE" && !currency.is_empty() {
            continue;
        }
        let value: f64 = value_str.parse().unwrap_or(0.0);
        match tag.as_str() {
            "NetLiquidation" => net_liquidation = value,
            "MaintMarginReq" => maintenance_margin = value,
            "InitMarginReq" => initial_margin = value,
            "BuyingPower" => buying_power = value,
            "RealizedPnL" => realized_pnl_today = value,
            _ => {}
        }
    }
    Ok(AccountSnapshot {
        net_liquidation,
        maintenance_margin,
        initial_margin,
        buying_power,
        realized_pnl_today,
        timestamp_ns: now_ns(),
    })
}

// ─── Fill ──────────────────────────────────────────────────────────

/// Convert an ib_insync `Fill` namedtuple. Has fields: contract (Contract),
/// execution (Execution), commissionReport (CommissionReport), time (datetime).
pub fn fill_from_py(obj: &Bound<'_, PyAny>) -> BrokerResult<Fill> {
    let contract_obj = obj.getattr("contract").map_err(|e| BrokerError::Protocol {
        code: None,
        message: format!("fill has no contract: {e}"),
    })?;
    let contract = contract_from_py(&contract_obj)?;
    let exec_obj = obj.getattr("execution").map_err(|e| BrokerError::Protocol {
        code: None,
        message: format!("fill has no execution: {e}"),
    })?;
    let exec_id = py_str_or_default(&exec_obj, "execId");
    let order_id = py_i64_or_default(&exec_obj, "orderId") as u64;
    let qty = py_f64_or_default(&exec_obj, "shares") as u32;
    let price = py_f64_or_default(&exec_obj, "price");
    let raw_side = py_str_or_default(&exec_obj, "side");
    let side = match raw_side.to_ascii_uppercase().as_str() {
        "BOT" | "BUY" | "B" => Side::Buy,
        _ => Side::Sell,
    };
    let commission = obj
        .getattr("commissionReport")
        .ok()
        .and_then(|cr| cr.getattr("commission").ok())
        .and_then(|c| c.extract::<f64>().ok())
        .filter(|c| c.is_finite() && *c != f64::MAX);

    Ok(Fill {
        exec_id,
        order_id: OrderId(order_id),
        instrument_id: contract.instrument_id,
        side,
        qty,
        price,
        timestamp_ns: now_ns(),
        commission,
    })
}

// ─── Trade / OrderStatusUpdate ─────────────────────────────────────

/// Convert an ib_insync `Trade.orderStatus` to OrderStatus enum.
pub fn parse_order_status(s: &str) -> OrderStatus {
    match s {
        "PendingSubmit" | "ApiPending" => OrderStatus::PendingSubmit,
        "Submitted" | "PreSubmitted" => OrderStatus::Submitted,
        "Filled" => OrderStatus::Filled,
        "Cancelled" | "ApiCancelled" => OrderStatus::Cancelled,
        "PendingCancel" => OrderStatus::PendingCancel,
        "Inactive" => OrderStatus::Inactive,
        // Anything we don't recognize maps to Rejected; better to be
        // explicit than to silently treat unknown states as Submitted.
        _ => OrderStatus::Rejected,
    }
}

// ─── OrderType + TIF ───────────────────────────────────────────────

/// Convert our OrderType to ib_insync's order class string.
/// Returns the class name; LimitOrder, MarketOrder, etc.
pub fn order_class_for(t: OrderType) -> &'static str {
    match t {
        OrderType::Limit => "LimitOrder",
        OrderType::Market => "MarketOrder",
        OrderType::StopLimit { .. } => "StopLimitOrder",
    }
}

pub fn tif_str(tif: TimeInForce) -> &'static str {
    match tif {
        TimeInForce::Gtc => "GTC",
        TimeInForce::Day => "DAY",
        TimeInForce::Gtd => "GTD",
        TimeInForce::Ioc => "IOC",
        TimeInForce::Fok => "FOK",
    }
}

pub fn side_str(s: Side) -> &'static str {
    match s {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

pub fn parse_tif(s: &str) -> TimeInForce {
    match s {
        "GTC" => TimeInForce::Gtc,
        "DAY" | "" => TimeInForce::Day,
        "GTD" => TimeInForce::Gtd,
        "IOC" => TimeInForce::Ioc,
        "FOK" => TimeInForce::Fok,
        _ => TimeInForce::Day,
    }
}

pub fn format_gtd(dt: DateTime<Utc>) -> String {
    // IBKR expects YYYYMMDD HH:MM:SS UTC (matching our existing
    // Python _gtd_string format in src/quote_engine_helpers.py).
    dt.format("%Y%m%d %H:%M:%S UTC").to_string()
}

// ─── Time helpers ──────────────────────────────────────────────────

pub fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ─── IBKR capabilities preset ──────────────────────────────────────

pub fn ibkr_caps() -> BrokerCapabilities {
    BrokerCapabilities::ibkr_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yyyymmdd_round_trip() {
        let d = NaiveDate::from_ymd_opt(2026, 6, 26).unwrap();
        let s = format_yyyymmdd(d);
        assert_eq!(s, "20260626");
        assert_eq!(parse_yyyymmdd(&s), Some(d));
    }

    #[test]
    fn parse_exchange_known() {
        assert_eq!(parse_exchange("COMEX"), Exchange::Comex);
        assert_eq!(parse_exchange("CME"), Exchange::Cme);
        assert_eq!(parse_exchange("Globex"), Exchange::Cme);
    }

    #[test]
    fn parse_order_status_terminal_states() {
        assert_eq!(parse_order_status("Filled"), OrderStatus::Filled);
        assert_eq!(parse_order_status("Cancelled"), OrderStatus::Cancelled);
        assert!(parse_order_status("UnknownGarbage").is_terminal());
    }

    #[test]
    fn tif_str_matches_ibkr_format() {
        assert_eq!(tif_str(TimeInForce::Gtd), "GTD");
        assert_eq!(tif_str(TimeInForce::Ioc), "IOC");
    }

    #[test]
    fn format_gtd_matches_python_pattern() {
        // `_gtd_string` in src/quote_engine_helpers.py emits
        // "YYYYMMDD HH:MM:SS UTC".
        let dt = DateTime::<Utc>::from_naive_utc_and_offset(
            NaiveDate::from_ymd_opt(2026, 6, 26)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
            Utc,
        );
        assert_eq!(format_gtd(dt), "20260626 14:30:00 UTC");
    }
}
