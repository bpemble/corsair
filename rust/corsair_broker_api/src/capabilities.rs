//! Adapter capabilities — what the gateway supports / requires.
//!
//! Consumers query [`Broker::capabilities`](crate::Broker::capabilities)
//! at startup and configure their behavior accordingly. This avoids
//! every consumer having to env-check or feature-flag based on the
//! wire protocol.

use crate::orders::TimeInForce;

/// Identifier of which adapter is in use. Useful for logging /
/// telemetry; consumers should NOT branch on this directly — branch on
/// the specific [`BrokerCapabilities`] field instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerKind {
    /// IBKR API V100+ (native Rust wire client).
    Ibkr,
    /// CME iLink / FIX 4.2 with FCM drop-copy.
    Ilink,
    /// In-memory mock for unit tests.
    Mock,
}

/// Capabilities reported by an adapter.
#[derive(Debug, Clone)]
pub struct BrokerCapabilities {
    /// Which adapter this is.
    pub kind: BrokerKind,
    /// True for FA accounts where every order MUST include
    /// `account=`. CLAUDE.md §1: IBKR FA accounts route status
    /// through the master, so the field is required for proper
    /// dispatch.
    pub requires_account_per_order: bool,
    /// True if drop-copy / exec details arrive via a separate session
    /// (typical for FCM/iLink). The runtime may need to wait briefly
    /// at startup for the drop-copy session to catch up before
    /// reconciliation is reliable.
    pub fills_on_separate_channel: bool,
    /// Time-in-force flavors supported by the gateway. Most gateways
    /// support all of them; the trait surfaces this for adapter
    /// transparency.
    pub supported_tifs: Vec<TimeInForce>,
    /// Typical RTT (ms) for place/cancel acknowledgement.
    /// `(median, p99)`. Used by the runtime to set GTD timeouts and
    /// the watchdog's hang threshold. IBKR is roughly (50, 300)ms;
    /// iLink colo is roughly (0.1, 1.0)ms.
    pub typical_rtt_ms: (u32, u32),
    /// Whether the gateway provides a usable `MaintMarginReq` field
    /// in account values that the constraint checker can scale
    /// against (CLAUDE.md §3). False for adapters that don't expose
    /// margin (e.g., some FCM setups push margin from a separate
    /// system).
    pub provides_maintenance_margin: bool,
}

impl BrokerCapabilities {
    /// Defaults for IBKR (FA paper account).
    pub fn ibkr_default() -> Self {
        Self {
            kind: BrokerKind::Ibkr,
            requires_account_per_order: true,
            fills_on_separate_channel: false,
            supported_tifs: vec![
                TimeInForce::Gtc,
                TimeInForce::Day,
                TimeInForce::Gtd,
                TimeInForce::Ioc,
                TimeInForce::Fok,
            ],
            typical_rtt_ms: (50, 300),
            provides_maintenance_margin: true,
        }
    }

    /// Defaults for an iLink/FCM setup. Speculative until Phase 7.
    pub fn ilink_default() -> Self {
        Self {
            kind: BrokerKind::Ilink,
            requires_account_per_order: false,
            fills_on_separate_channel: true,
            supported_tifs: vec![TimeInForce::Day, TimeInForce::Gtd, TimeInForce::Ioc],
            typical_rtt_ms: (1, 5),
            provides_maintenance_margin: false,
        }
    }
}
