//! Kill switches + sentinel-based induced testing for the v3 Rust runtime.
//!
//! Mirrors `src/risk_monitor.py` 1:1 in behavior. The Python tests at
//! `tests/test_risk_monitor.py` are the behavioral spec; this crate
//! must satisfy the same assertions.
//!
//! # Kill taxonomy (preserved from CLAUDE.md §7-8)
//!
//! - `KillType::Halt` — cancel resting quotes, no flatten. Used by
//!   margin_kill, daily_pnl_halt (operator override), theta_halt,
//!   vega_halt.
//! - `KillType::Flatten` — cancel quotes + flatten options + flatten
//!   hedge. Not currently used by any live path post-2026-04-22 but
//!   wiring is preserved for future re-enablement and induced tests.
//! - `KillType::HedgeFlat` — cancel quotes + force hedge to 0
//!   (no options flatten). Delta kill.
//!
//! # Source taxonomy (auto-clear rules)
//!
//! - `Source::Risk` — sticky; manual review.
//! - `Source::Disconnect` — cleared by watchdog after reconnect.
//! - `Source::DailyHalt` — cleared at next CME session rollover.
//! - `Source::Operational` — sticky (SABR RMSE, latency, etc.).
//! - `Source::Induced(inner)` — boot self-test sentinels; inner
//!   carries the source we'd be testing. induced_daily_halt
//!   auto-clears at rollover; others are sticky.
//!
//! Audit T3-12: `Reconciliation` and `ExceptionStorm` variants from
//! the Python taxonomy are not constructed in the Rust runtime and
//! were removed to keep the source enum aligned with reachable
//! states.

pub mod kill;
pub mod monitor;
pub mod sentinel;

pub use kill::{KillSource, KillType, KillEvent};
pub use monitor::{RiskMonitor, RiskConfig, RiskCheckOutcome};
pub use sentinel::{InducedSentinel, INDUCED_SENTINELS};
