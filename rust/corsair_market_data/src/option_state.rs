//! Per-option tick state.

use chrono::NaiveDate;
use corsair_broker_api::{InstrumentId, Right};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionTick {
    pub instrument_id: Option<InstrumentId>,
    pub strike: f64,
    pub expiry: NaiveDate,
    pub right: Right,
    pub bid: f64,
    pub ask: f64,
    pub bid_size: u64,
    pub ask_size: u64,
    pub last: f64,
    /// Implied vol from brentq solve on mid; updated on bid/ask change.
    pub iv: f64,
    /// Option open interest for this strike+right. Pushed by IBKR
    /// generic tick "101" (tick types 27=call OI, 28=put OI). 0 if
    /// not yet received.
    #[serde(default)]
    pub open_interest: u64,
    /// Option session volume for this strike+right. Pushed by IBKR
    /// generic tick "100" (tick types 29=call volume, 30=put volume).
    #[serde(default)]
    pub volume: u64,
    /// 5-level L2 depth book. depth.bids[0] = highest bid, ask[0] =
    /// lowest ask. Empty until reqMktDepth fires updates for this
    /// leg (we rotate active L2 subscriptions across the strike set).
    #[serde(default)]
    pub depth: DepthBook,
    pub last_updated_ns: u64,
}

/// L2 depth book. Sorted: bids descending (best first), asks
/// ascending. Capped at 5 levels — IBKR's reqMktDepth(numRows=5)
/// gives us level 0..4 inclusive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DepthBook {
    /// Bid levels, sorted high → low.
    #[serde(default)]
    pub bids: Vec<DepthLevel>,
    /// Ask levels, sorted low → high.
    #[serde(default)]
    pub asks: Vec<DepthLevel>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DepthLevel {
    pub price: f64,
    pub size: u64,
}

impl DepthBook {
    /// Apply an IBKR depth op. `position` is 0-indexed; `op` is
    /// 0=insert, 1=update, 2=delete. Bounds-checks all directions.
    /// Audit T1-5: out-of-range positions are dropped silently rather
    /// than padding with zero-price holes (which corrupt downstream
    /// `external_best_*` lookups).
    pub fn apply(&mut self, is_bid: bool, position: i32, op: i32, price: f64, size: u64) {
        let book = if is_bid { &mut self.bids } else { &mut self.asks };
        let pos = position as usize;
        match op {
            0 => {
                // IBKR L2 spec: insert a new level at `position`;
                // existing levels at and below shift down. On a full
                // book (5 levels), the deepest falls off the end.
                //
                // Pre-2026-05-07: only insert-at-tail (`pos == 5`) was
                // handled when the book was full; insert at pos < 5 on
                // a full book silently dropped the update — losing new
                // best-bid arrivals during fast moves. Audit round 2.
                if pos > 5 {
                    return; // genuinely out of range
                }
                if book.len() == 5 {
                    book.pop();
                }
                let target = pos.min(book.len());
                book.insert(target, DepthLevel { price, size });
            }
            1 => {
                if pos < book.len() {
                    book[pos] = DepthLevel { price, size };
                } else if pos == book.len() && book.len() < 5 {
                    // Update at tail+1 == "create new last level".
                    book.push(DepthLevel { price, size });
                }
                // else: out-of-range update; drop rather than padding.
            }
            2 => {
                if pos < book.len() {
                    book.remove(pos);
                }
            }
            _ => {} // unknown op — ignore silently
        }
    }

    /// External best bid: top of book IF it's not all our size,
    /// otherwise look at the next level. `our_price`/`our_size` is
    /// our resting bid (None if we have no bid). Returns the price
    /// of the highest-priority external bid, or 0.0 if none.
    pub fn external_best_bid(&self, our_price: Option<f64>, our_size: u64) -> f64 {
        external_best(&self.bids, our_price, our_size)
    }

    pub fn external_best_ask(&self, our_price: Option<f64>, our_size: u64) -> f64 {
        external_best(&self.asks, our_price, our_size)
    }
}

fn external_best(book: &[DepthLevel], our_price: Option<f64>, our_size: u64) -> f64 {
    for level in book {
        if let Some(p) = our_price {
            if (level.price - p).abs() < 1e-9 && level.size <= our_size {
                // This level is all ours — skip to next.
                continue;
            }
        }
        return level.price;
    }
    0.0
}

impl OptionTick {
    pub fn new(strike: f64, expiry: NaiveDate, right: Right) -> Self {
        Self {
            instrument_id: None,
            strike,
            expiry,
            right,
            bid: 0.0,
            ask: 0.0,
            bid_size: 0,
            ask_size: 0,
            last: 0.0,
            iv: 0.0,
            open_interest: 0,
            volume: 0,
            depth: DepthBook::default(),
            last_updated_ns: 0,
        }
    }

    /// Mid price; None if either side is missing.
    pub fn mid(&self) -> Option<f64> {
        if self.bid > 0.0 && self.ask > 0.0 && self.bid < self.ask {
            Some((self.bid + self.ask) / 2.0)
        } else {
            None
        }
    }

    /// Best available current price for MTM:
    /// mid → last → 0.
    pub fn current_price(&self) -> f64 {
        self.mid().unwrap_or(if self.last > 0.0 { self.last } else { 0.0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_bid_book() -> DepthBook {
        let mut b = DepthBook::default();
        // Build a 5-level bid book descending: 100..96.
        for (i, p) in [100.0, 99.0, 98.0, 97.0, 96.0].iter().enumerate() {
            b.apply(true, i as i32, 0, *p, 10);
        }
        b
    }

    #[test]
    fn insert_top_on_full_book_evicts_deepest() {
        let mut b = full_bid_book();
        // New best bid at 101 → insert at pos=0 → 96.0 should fall off.
        b.apply(true, 0, 0, 101.0, 5);
        assert_eq!(b.bids.len(), 5);
        assert_eq!(b.bids[0].price, 101.0);
        assert_eq!(b.bids[0].size, 5);
        assert_eq!(b.bids[4].price, 97.0);
    }

    #[test]
    fn insert_middle_on_full_book_evicts_deepest() {
        let mut b = full_bid_book();
        // Insert at pos=2 (between 99 and 98): shift 98,97,96 down,
        // drop 96. Result: 100, 99, NEW, 98, 97.
        b.apply(true, 2, 0, 98.5, 7);
        assert_eq!(b.bids.len(), 5);
        assert_eq!(b.bids[2].price, 98.5);
        assert_eq!(b.bids[3].price, 98.0);
        assert_eq!(b.bids[4].price, 97.0);
    }

    #[test]
    fn insert_partial_book_grows() {
        let mut b = DepthBook::default();
        b.apply(true, 0, 0, 100.0, 1);
        b.apply(true, 1, 0, 99.0, 2);
        assert_eq!(b.bids.len(), 2);
        assert_eq!(b.bids[1].price, 99.0);
    }

    #[test]
    fn out_of_range_insert_dropped() {
        let mut b = DepthBook::default();
        b.apply(true, 6, 0, 100.0, 1); // pos > 5 — drop
        assert!(b.bids.is_empty());
    }

    #[test]
    fn delete_shifts_up() {
        let mut b = full_bid_book();
        b.apply(true, 1, 2, 0.0, 0); // delete pos=1 (99.0)
        assert_eq!(b.bids.len(), 4);
        assert_eq!(b.bids[0].price, 100.0);
        assert_eq!(b.bids[1].price, 98.0);
    }

    #[test]
    fn external_best_skips_our_full_size() {
        let b = full_bid_book(); // top: 100 size 10
        // Our resting bid at 100 size 10 — the level is all ours.
        let ext = b.external_best_bid(Some(100.0), 10);
        assert_eq!(ext, 99.0);
    }

    #[test]
    fn external_best_keeps_partial_external() {
        let b = full_bid_book(); // top: 100 size 10
        // Our resting bid is 1; external owns 9 of the 10 lots.
        let ext = b.external_best_bid(Some(100.0), 1);
        assert_eq!(ext, 100.0);
    }
}
