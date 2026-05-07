//! Kill type + source taxonomy + the persistent kill state.

use serde::{Deserialize, Serialize};

/// What action the kill performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KillType {
    /// Cancel resting quotes, no flatten. Default for halt-style kills.
    Halt,
    /// Cancel quotes + flatten options + flatten hedge. Audit T3-13:
    /// not constructed in Rust today — operator overrides (CLAUDE.md
    /// §7, §8) replaced flatten with halt for both margin and daily-
    /// halt kills. Variant retained (a) for spec parity, (b) so
    /// induced-kill sentinels can drive a flatten path without an
    /// API break, and (c) for serialization forward-compat.
    Flatten,
    /// Cancel quotes + force hedge to 0 (no options flatten). Delta kill.
    HedgeFlat,
}

/// Source category — governs auto-clear rules.
///
/// Audit T3-12: `Reconciliation` and `ExceptionStorm` variants were
/// inherited from the Python taxonomy but are never constructed in
/// the Rust runtime — there's no exception-storm or reconcile-fail
/// kill path on this side. Removed to keep `label()` and the
/// snapshot's `source` field tied to actually-reachable states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KillSource {
    /// Sticky risk-source kill. Requires manual review + restart.
    Risk,
    /// Cleared by watchdog after reconnect.
    Disconnect,
    /// Auto-clears at next CME session rollover.
    DailyHalt,
    /// Sticky (SABR RMSE, latency, abnormal fill rate).
    Operational,
    /// §25 trader watchdog: broker hasn't received any frame from
    /// the trader within the watchdog timeout. Sticky — trader can
    /// reconnect and stream heartbeats again, but the kill stays
    /// active until the operator restarts the broker. This forces
    /// review of the underlying trader fault before resuming.
    TraderSilent,
    /// Boot-self-test sentinel kill. Inner carries the underlying
    /// source we're exercising; induced_daily_halt auto-clears at
    /// rollover, others are sticky.
    Induced(Box<KillSource>),
}

impl KillSource {
    /// True if this source auto-clears at session rollover.
    pub fn is_daily_clearable(&self) -> bool {
        matches!(self, KillSource::DailyHalt)
            || matches!(self, KillSource::Induced(inner) if matches!(**inner, KillSource::DailyHalt))
    }

    /// True if this source clears on watchdog reconnect.
    pub fn is_disconnect(&self) -> bool {
        matches!(self, KillSource::Disconnect)
    }

    /// Telemetry-friendly string label.
    pub fn label(&self) -> String {
        match self {
            KillSource::Risk => "risk".into(),
            KillSource::Disconnect => "disconnect".into(),
            KillSource::DailyHalt => "daily_halt".into(),
            KillSource::Operational => "operational".into(),
            KillSource::TraderSilent => "trader_silent".into(),
            KillSource::Induced(inner) => format!("induced_{}", inner.label()),
        }
    }
}

/// Active kill state. Constructed when a kill fires; cleared by
/// `clear_disconnect` or `clear_daily_halt` per the source rules.
#[derive(Debug, Clone)]
pub struct KillEvent {
    pub reason: String,
    pub source: KillSource,
    pub kill_type: KillType,
    pub timestamp_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_halt_is_daily_clearable() {
        assert!(KillSource::DailyHalt.is_daily_clearable());
        assert!(!KillSource::Risk.is_daily_clearable());
    }

    #[test]
    fn induced_daily_halt_clearable() {
        let s = KillSource::Induced(Box::new(KillSource::DailyHalt));
        assert!(s.is_daily_clearable());
    }

    #[test]
    fn induced_risk_not_clearable() {
        let s = KillSource::Induced(Box::new(KillSource::Risk));
        assert!(!s.is_daily_clearable());
    }

    #[test]
    fn label_strings_match_python_taxonomy() {
        assert_eq!(KillSource::Risk.label(), "risk");
        assert_eq!(KillSource::DailyHalt.label(), "daily_halt");
        assert_eq!(
            KillSource::Induced(Box::new(KillSource::DailyHalt)).label(),
            "induced_daily_halt"
        );
        assert_eq!(
            KillSource::Induced(Box::new(KillSource::Risk)).label(),
            "induced_risk"
        );
    }
}
