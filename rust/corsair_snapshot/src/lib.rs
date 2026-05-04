//! Dashboard snapshot publisher.
//!
//! Mirrors `src/snapshot.py`. Builds a single JSON payload at 4 Hz
//! containing the dashboard's view of the system: positions,
//! aggregated Greeks, kill state, hedge status, account values.
//! Written atomically via tempfile + rename.
//!
//! The Streamlit dashboard polls the snapshot file at the same
//! cadence — the boundary contract per spec §4.

pub mod payload;
pub mod publisher;

pub use payload::{
    ChainExpirySnapshot, HedgeSnapshot, KillSnapshot, PortfolioSnapshot, PositionSnapshot,
    SideBlockSnapshot, Snapshot, StrikeBlockSnapshot,
};
pub use publisher::{ChainBuild, SnapshotConfig, SnapshotPublisher};
