//! Rolling-window latency stats. The broker pushes samples here after
//! every successful place_order; periodic_snapshot reads p50/p99 to
//! emit on the dashboard's TTT/RTT pill.

use std::collections::VecDeque;

const RING_CAPACITY: usize = 1024;

#[derive(Default)]
pub struct LatencySamples {
    /// Tick → broker_order_ack (microseconds).
    ttt_us: VecDeque<u64>,
    /// Broker send → broker recv ack on place_order.
    place_rtt_us: VecDeque<u64>,
    /// Broker send → broker recv ack on modify_order (amend). The
    /// industry-conventional "quote update" latency: in steady-state
    /// MM, the loop is tick → modify, not tick → place, so this is
    /// the metric to optimize.
    modify_rtt_us: VecDeque<u64>,
}

impl LatencySamples {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_ttt(&mut self, us: u64) {
        push_bounded(&mut self.ttt_us, us);
    }

    pub fn push_place_rtt(&mut self, us: u64) {
        push_bounded(&mut self.place_rtt_us, us);
    }

    pub fn push_modify_rtt(&mut self, us: u64) {
        push_bounded(&mut self.modify_rtt_us, us);
    }

    /// Compute (n, p50, p99). Empty buffer returns (0, None, None).
    pub fn ttt_stats(&self) -> (u64, Option<u64>, Option<u64>) {
        stats(&self.ttt_us)
    }

    pub fn place_rtt_stats(&self) -> (u64, Option<u64>, Option<u64>) {
        stats(&self.place_rtt_us)
    }

    pub fn modify_rtt_stats(&self) -> (u64, Option<u64>, Option<u64>) {
        stats(&self.modify_rtt_us)
    }
}

fn push_bounded(q: &mut VecDeque<u64>, v: u64) {
    if q.len() >= RING_CAPACITY {
        q.pop_front();
    }
    q.push_back(v);
}

fn stats(q: &VecDeque<u64>) -> (u64, Option<u64>, Option<u64>) {
    if q.is_empty() {
        return (0, None, None);
    }
    let mut v: Vec<u64> = q.iter().copied().collect();
    v.sort_unstable();
    let n = v.len();
    let p50 = v[n / 2];
    let p99_idx = ((n as f64 * 0.99).ceil() as usize).saturating_sub(1).min(n - 1);
    let p99 = v[p99_idx];
    (n as u64, Some(p50), Some(p99))
}
