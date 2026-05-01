//! Time-to-expiry computation with per-expiry datetime caching.
//!
//! Naive implementation parsed `YYYYMMDD` and constructed a chrono
//! DateTime<Utc> on every tick. With ~240 ticks/sec, that's:
//! - 3 string-slice → integer parses
//! - 1 chrono::TimeZone::with_ymd_and_hms construction
//! per call. Cheap individually but cumulatively a measurable fraction
//! of decision-time CPU.
//!
//! Strategy: thread_local cache `HashMap<expiry → expiry_datetime>`.
//! Bounded to ~16 entries (production has ≤4 expiries, headroom for
//! stress + dev). Each call: O(1) cache hit + one Utc::now() syscall.

use chrono::{DateTime, TimeZone, Utc};
use std::cell::RefCell;
use std::collections::HashMap;

const SECS_PER_YEAR: f64 = 365.0 * 24.0 * 3600.0;
const MAX_CACHE_ENTRIES: usize = 32;

thread_local! {
    /// Maps the YYYYMMDD expiry string → 21:00 UTC DateTime on that
    /// day (CME settlement). Bounded; we LRU-evict by simply clearing
    /// when capacity is hit. New entries refill cheaply.
    static EXPIRY_DT_CACHE: RefCell<HashMap<String, DateTime<Utc>>> =
        RefCell::new(HashMap::with_capacity(MAX_CACHE_ENTRIES));
}

/// Parse `YYYYMMDD` to the 21:00 UTC settlement datetime. Returns None
/// on malformed input.
fn parse_expiry(expiry: &str) -> Option<DateTime<Utc>> {
    if expiry.len() != 8 {
        return None;
    }
    let year: i32 = expiry[0..4].parse().ok()?;
    let month: u32 = expiry[4..6].parse().ok()?;
    let day: u32 = expiry[6..8].parse().ok()?;
    Utc.with_ymd_and_hms(year, month, day, 21, 0, 0).single()
}

/// Time to expiry in years. Caches the parsed expiry datetime.
pub fn time_to_expiry_years(expiry: &str) -> Option<f64> {
    EXPIRY_DT_CACHE.with(|cache| {
        let mut c = cache.borrow_mut();
        let exp_dt = match c.get(expiry) {
            Some(dt) => *dt,
            None => {
                if c.len() >= MAX_CACHE_ENTRIES {
                    c.clear();
                }
                let dt = parse_expiry(expiry)?;
                c.insert(expiry.to_string(), dt);
                dt
            }
        };
        let now = Utc::now();
        let secs = (exp_dt - now).num_seconds() as f64;
        Some(secs / SECS_PER_YEAR)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_expiry() {
        let dt = parse_expiry("20260526").unwrap();
        assert_eq!(dt.to_string(), "2026-05-26 21:00:00 UTC");
    }

    #[test]
    fn cache_hit_on_repeat_call() {
        // Both calls should succeed; the second should hit cache.
        let v1 = time_to_expiry_years("20260526").unwrap();
        let v2 = time_to_expiry_years("20260526").unwrap();
        // Same call within micros — values should be ~equal (with
        // tiny drift from Utc::now() advancing).
        assert!((v1 - v2).abs() < 1e-6);
    }

    #[test]
    fn malformed_returns_none() {
        assert!(time_to_expiry_years("bad").is_none());
        assert!(time_to_expiry_years("20260229").is_none()); // 2026 not a leap year, Feb 29 invalid
    }
}
