//! Minimal `OrderBook` — orderId → terminal-state cache.
//!
//! See crate docs in `lib.rs`. Trimmed 2026-05-05 to only what
//! `corsair_broker/src/tasks.rs::pump_status` actually consumes:
//! `OrderBook::new()` + `apply_status(oid, status) -> bool`.

use corsair_broker_api::{OrderId, OrderStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Lifecycle state we track per orderId. Only the boundary between
/// "still live" and "terminal" matters for the broker — fine-grained
/// states live in OpenOrder.status on the broker_api side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderState {
    /// Sent / acknowledged / live — we've seen at least one non-
    /// terminal status for this id.
    Live,
    /// Filled / Cancelled / Rejected / Inactive — the record has
    /// been removed from the book on this status; this variant is
    /// retained for callers that snapshot state at the moment of
    /// terminal transition.
    Terminal,
}

#[derive(Debug, Default)]
pub struct OrderBook {
    /// orderId → most recent non-terminal state. Entries are
    /// inserted on first observation and removed on terminal status.
    states: HashMap<OrderId, OrderState>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    /// Apply an order-status update from IBKR. Returns `true` if
    /// this orderId was already tracked here (we'd seen a status
    /// for it before this update), otherwise `false`.
    ///
    /// Pre-shrink the bool meant "the record existed via
    /// `link_order_id`"; nothing in the broker daemon ever called
    /// link_order_id, so the bool was effectively always-false in
    /// production. We preserve the "have I seen this oid before?"
    /// semantic so callers that toggle on it behave identically.
    /// Terminal statuses also remove the entry so memory doesn't
    /// grow unbounded.
    pub fn apply_status(&mut self, oid: OrderId, status: OrderStatus) -> bool {
        let known = self.states.contains_key(&oid);
        let terminal = matches!(
            status,
            OrderStatus::Filled
                | OrderStatus::Cancelled
                | OrderStatus::Rejected
                | OrderStatus::Inactive
        );
        if terminal {
            self.states.remove(&oid);
        } else {
            self.states.insert(oid, OrderState::Live);
        }
        known
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_orderid_returns_false() {
        let mut b = OrderBook::new();
        // Never seen before — known is false either way.
        assert!(!b.apply_status(OrderId(1), OrderStatus::Cancelled));
        assert!(!b.apply_status(OrderId(2), OrderStatus::Submitted));
    }

    #[test]
    fn second_observation_returns_true() {
        let mut b = OrderBook::new();
        assert!(!b.apply_status(OrderId(7), OrderStatus::Submitted));
        // Second non-terminal: we now know about 7 → true.
        assert!(b.apply_status(OrderId(7), OrderStatus::Submitted));
    }

    #[test]
    fn terminal_removes_entry() {
        let mut b = OrderBook::new();
        b.apply_status(OrderId(7), OrderStatus::Submitted);
        assert!(b.states.contains_key(&OrderId(7)));
        assert!(b.apply_status(OrderId(7), OrderStatus::Filled));
        assert!(!b.states.contains_key(&OrderId(7)));
    }

    #[test]
    fn rejected_treated_as_terminal() {
        let mut b = OrderBook::new();
        b.apply_status(OrderId(11), OrderStatus::Submitted);
        b.apply_status(OrderId(11), OrderStatus::Rejected);
        assert!(!b.states.contains_key(&OrderId(11)));
    }
}
