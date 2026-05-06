//! Shared wall-clock helpers. Hoisted so tasks.rs / ipc.rs /
//! vol_surface.rs share a single definition.

/// Unix epoch nanoseconds. Saturates to 0 on the (impossible)
/// SystemTime-before-epoch case.
#[inline]
pub fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
