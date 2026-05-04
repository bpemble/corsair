//! Contract types — symbol/expiry/strike identity for instruments.
//!
//! Every Broker assigns a [`InstrumentId`] (u64) to each instrument it
//! knows about. IBKR's `conId` and iLink's `security_id` both map to
//! this. Adapters MUST provide a stable mapping for the lifetime of
//! the connection; reconnect may invalidate ids (consumer must
//! re-qualify).

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Broker-assigned instrument identifier. u64 for cheap copy / hash key.
/// Lifetime: stable for the connection. Reconnect may invalidate;
/// consumers re-qualify via [`Broker::qualify_*`](crate::Broker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstrumentId(pub u64);

impl std::fmt::Display for InstrumentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "instr#{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractKind {
    Future,
    Option,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Right {
    Call,
    Put,
}

impl Right {
    pub fn as_char(self) -> char {
        match self {
            Right::Call => 'C',
            Right::Put => 'P',
        }
    }
    pub fn from_char(c: char) -> Option<Self> {
        match c.to_ascii_uppercase() {
            'C' => Some(Right::Call),
            'P' => Some(Right::Put),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exchange {
    /// CME (HG/copper, ETH/ether, NQ etc.)
    Cme,
    /// COMEX division of CME
    Comex,
    /// NYMEX division of CME
    Nymex,
    /// CBOT division of CME
    Cbot,
    /// Catch-all for adapters that need to round-trip an unknown venue.
    /// Discouraged; if you hit this in production, add the variant.
    Other(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Currency {
    Usd,
    Eur,
    Gbp,
    Jpy,
}

/// A fully-qualified contract. Returned by `Broker::qualify_*`.
/// Consumers cache these (ATM strike subscriptions, hedge contract
/// resolution) and pass `instrument_id` for subsequent operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contract {
    pub instrument_id: InstrumentId,
    pub kind: ContractKind,
    /// Root symbol: "HG", "ETHUSDRR", "ES".
    pub symbol: String,
    /// Local symbol used at the exchange: "HGM6", "ETHUSDRR  260424P02100000".
    pub local_symbol: String,
    pub expiry: NaiveDate,
    /// None for futures, Some(strike) for options.
    pub strike: Option<f64>,
    /// None for futures, Some(C/P) for options.
    pub right: Option<Right>,
    /// Contract multiplier ($/point). HG=25000, ETHUSDRR=50, ES=50, NQ=20.
    pub multiplier: f64,
    pub exchange: Exchange,
    pub currency: Currency,
    /// IBKR tradingClass — required on place_order at server v178+ for
    /// FUT contracts under FA accounts (audit T1-1). For OPT, IBKR
    /// usually resolves by conId so this can be empty, but populating
    /// it is harmless. For HG: futures = "HG", options = "HXE".
    /// Returned by IBKR's contractDetails response.
    #[serde(default)]
    pub trading_class: String,
}

// ── Query types ─────────────────────────────────────────────────────

/// Parameters for `Broker::qualify_future`.
#[derive(Debug, Clone)]
pub struct FutureQuery {
    pub symbol: String,
    pub expiry: NaiveDate,
    pub exchange: Exchange,
    pub currency: Currency,
}

/// Parameters for `Broker::qualify_option`.
#[derive(Debug, Clone)]
pub struct OptionQuery {
    pub symbol: String,
    pub expiry: NaiveDate,
    pub strike: f64,
    pub right: Right,
    pub exchange: Exchange,
    pub currency: Currency,
    pub multiplier: f64,
}

/// Parameters for `Broker::list_chain`. Returns all contracts matching
/// the symbol/exchange filter, optionally restricted to a kind.
#[derive(Debug, Clone)]
pub struct ChainQuery {
    pub symbol: String,
    pub exchange: Exchange,
    pub currency: Currency,
    /// If Some, restrict to this kind. None returns futures + options.
    pub kind: Option<ContractKind>,
    /// If Some, restrict to expiries on or after this date. Used by
    /// hedge_manager's lockout-skip logic to enumerate non-front-month
    /// contracts.
    pub min_expiry: Option<NaiveDate>,
}
