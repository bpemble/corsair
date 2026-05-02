//! Tokio tasks. One task per broker stream; one per periodic timer.
//! All hold an `Arc<Runtime>` and acquire the relevant mutex.

use corsair_broker_api::{ConnectionState, OrderStatus};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::interval;

use crate::runtime::Runtime;

/// Spawn every task. Returns a Vec of join handles so the caller
/// can wait on shutdown.
pub fn spawn_all(runtime: Arc<Runtime>) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();

    handles.push(tokio::spawn(pump_fills(runtime.clone())));
    handles.push(tokio::spawn(pump_status(runtime.clone())));
    handles.push(tokio::spawn(pump_ticks(runtime.clone())));
    handles.push(tokio::spawn(pump_errors(runtime.clone())));
    handles.push(tokio::spawn(pump_connection(runtime.clone())));

    handles.push(tokio::spawn(periodic_greek_refresh(runtime.clone())));
    handles.push(tokio::spawn(periodic_risk_check(runtime.clone())));
    handles.push(tokio::spawn(periodic_hedge(runtime.clone())));
    handles.push(tokio::spawn(periodic_snapshot(runtime.clone())));
    handles.push(tokio::spawn(periodic_account_poll(runtime.clone())));

    handles
}

// ─── Stream pumps ──────────────────────────────────────────────────

async fn pump_fills(runtime: Arc<Runtime>) {
    let mut rx = {
        let b = runtime.broker.lock().await;
        b.subscribe_fills()
    };
    log::info!("pump_fills: subscribed");
    loop {
        match rx.recv().await {
            Ok(fill) => {
                handle_fill(&runtime, fill);
            }
            Err(RecvError::Lagged(n)) => {
                log::warn!("pump_fills: lagged {n} frames");
            }
            Err(RecvError::Closed) => {
                log::info!("pump_fills: channel closed; exiting");
                break;
            }
        }
    }
}

fn handle_fill(runtime: &Arc<Runtime>, fill: corsair_broker_api::events::Fill) {
    // Try hedge first — if it accepts, the fill was a hedge fill, NOT
    // an option fill. (apply_broker_fill returns true only if the
    // instrument matches the hedge contract.)
    {
        let mut h = runtime.hedge.lock().unwrap();
        for m in 0..h.managers().len() {
            // Borrow checker: use for_product_mut keyed by symbol
            // is awkward in a loop; use index-by-position via a
            // helper. The Phase 3 fanout doesn't expose mut iter,
            // so we walk the products manually.
            let products: Vec<String> =
                h.managers().iter().map(|x| x.config().product.clone()).collect();
            let _ = m;
            for prod in &products {
                if let Some(mgr) = h.for_product_mut(prod) {
                    if mgr.apply_broker_fill(&fill) {
                        return;
                    }
                }
            }
            break;
        }
    }

    // Otherwise it's an option fill (or an unknown instrument we
    // ignore). Look up product from market data registry to find
    // strike/expiry/right.
    let md = runtime.market_data.lock().unwrap();
    let instr = fill.instrument_id;
    // Find the option matching this instrument_id.
    // (Phase 4.x: add an instrument_id index to MarketDataState
    // for O(1) lookup; for now we scan registered options per
    // product. This is at most ~60 strikes — microseconds.)
    let mut matched: Option<(
        String,
        f64,
        chrono::NaiveDate,
        corsair_broker_api::Right,
    )> = None;
    for prod in runtime.portfolio.lock().unwrap().registry().products() {
        for t in md.options_for_product(&prod) {
            if t.instrument_id == Some(instr) {
                matched = Some((prod.clone(), t.strike, t.expiry, t.right));
                break;
            }
        }
        if matched.is_some() {
            break;
        }
    }
    drop(md);
    let (product, strike, expiry, right) = match matched {
        Some(v) => v,
        None => {
            log::debug!(
                "fill on unregistered instrument {} — ignoring (likely hedge contract not yet resolved)",
                instr
            );
            return;
        }
    };
    let qty_signed = match fill.side {
        corsair_broker_api::Side::Buy => fill.qty as i32,
        corsair_broker_api::Side::Sell => -(fill.qty as i32),
    };
    let outcome = {
        let mut p = runtime.portfolio.lock().unwrap();
        p.add_fill(&product, strike, expiry, right, qty_signed, fill.price, 0.0, 0.0)
    };
    log::warn!(
        "fill: {} {} {:?} {:+} @ {} → {:?}",
        product,
        strike,
        right,
        qty_signed,
        fill.price,
        outcome
    );

    // Per-fill daily P&L halt check. Mirrors
    // FillHandler.check_daily_pnl_only in Python.
    {
        let p = runtime.portfolio.lock().unwrap();
        let md = runtime.market_data.lock().unwrap();
        let mut r = runtime.risk.lock().unwrap();
        let _ = r.check_daily_pnl_only(&p, &*md);
    }
}

async fn pump_status(runtime: Arc<Runtime>) {
    let mut rx = {
        let b = runtime.broker.lock().await;
        b.subscribe_order_status()
    };
    log::info!("pump_status: subscribed");
    loop {
        match rx.recv().await {
            Ok(update) => {
                let mut oms = runtime.oms.lock().unwrap();
                let resolved = oms.apply_status(update.order_id, update.status);
                if !resolved {
                    log::debug!(
                        "status update for unknown orderId {}: {:?}",
                        update.order_id,
                        update.status
                    );
                }
                if matches!(
                    update.status,
                    OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected
                ) {
                    log::info!(
                        "order {} terminal: {:?} (filled={}, remaining={})",
                        update.order_id,
                        update.status,
                        update.filled_qty,
                        update.remaining_qty
                    );
                }
            }
            Err(RecvError::Lagged(n)) => log::warn!("pump_status: lagged {n}"),
            Err(RecvError::Closed) => break,
        }
    }
}

async fn pump_ticks(runtime: Arc<Runtime>) {
    let mut rx = {
        let b = runtime.broker.lock().await;
        b.subscribe_ticks_stream()
    };
    log::info!("pump_ticks: subscribed");
    loop {
        match rx.recv().await {
            Ok(tick) => {
                let mut md = runtime.market_data.lock().unwrap();
                use corsair_broker_api::TickKind;
                match tick.kind {
                    TickKind::Bid => {
                        if let Some(p) = tick.price {
                            md.update_bid(
                                tick.instrument_id,
                                p,
                                tick.size.unwrap_or(0),
                                tick.timestamp_ns,
                            );
                        }
                    }
                    TickKind::Ask => {
                        if let Some(p) = tick.price {
                            md.update_ask(
                                tick.instrument_id,
                                p,
                                tick.size.unwrap_or(0),
                                tick.timestamp_ns,
                            );
                        }
                    }
                    TickKind::Last => {
                        if let Some(p) = tick.price {
                            md.update_last(tick.instrument_id, p, tick.timestamp_ns);
                        }
                    }
                    _ => {} // BidSize/AskSize/Volume — handled implicitly via update_bid/ask args
                }
            }
            Err(RecvError::Lagged(n)) => log::warn!("pump_ticks: lagged {n}"),
            Err(RecvError::Closed) => break,
        }
    }
}

async fn pump_errors(runtime: Arc<Runtime>) {
    let mut rx = {
        let b = runtime.broker.lock().await;
        b.subscribe_errors()
    };
    log::info!("pump_errors: subscribed");
    loop {
        match rx.recv().await {
            Ok(err) => {
                log::warn!("broker error: {err}");
                // Phase 4.x: route certain protocol errors (e.g.
                // 1100 disconnect) to risk.fire as
                // KillSource::Disconnect. Today the connection
                // stream handles disconnects.
                let _ = runtime;
            }
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
}

async fn pump_connection(runtime: Arc<Runtime>) {
    let mut rx = {
        let b = runtime.broker.lock().await;
        b.subscribe_connection()
    };
    log::info!("pump_connection: subscribed");
    loop {
        match rx.recv().await {
            Ok(ev) => {
                log::warn!(
                    "connection event: {:?} {}",
                    ev.state,
                    ev.reason.as_deref().unwrap_or("")
                );
                // On reconnect, clear disconnect-source kills.
                if matches!(ev.state, ConnectionState::Connected) {
                    let mut r = runtime.risk.lock().unwrap();
                    let cleared = r.clear_disconnect_kill();
                    if cleared {
                        log::warn!("cleared disconnect-induced kill on reconnect");
                    }
                }
            }
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
}

// ─── Periodic tasks ──────────────────────────────────────────────

async fn periodic_greek_refresh(runtime: Arc<Runtime>) {
    let mut t = interval(Duration::from_secs(300)); // 5 min
    log::info!("periodic_greek_refresh: cadence 300s");
    loop {
        t.tick().await;
        let mut p = runtime.portfolio.lock().unwrap();
        let md = runtime.market_data.lock().unwrap();
        p.refresh_greeks(&*md);
    }
}

async fn periodic_risk_check(runtime: Arc<Runtime>) {
    let mut t = interval(Duration::from_secs(300));
    log::info!("periodic_risk_check: cadence 300s");
    loop {
        t.tick().await;
        let p = runtime.portfolio.lock().unwrap();
        let md = runtime.market_data.lock().unwrap();
        let mut r = runtime.risk.lock().unwrap();
        // Compute worst per-product Greeks from aggregate.
        let agg = p.aggregate();
        let (worst_delta, worst_theta, worst_vega) = worst_per_product(&agg, &runtime);
        let outcome = r.check(&p, 0.0, worst_delta, worst_theta, worst_vega, &*md);
        match &outcome {
            corsair_risk::RiskCheckOutcome::Killed(ev) => {
                log::error!("risk check fired kill: {ev:?}");
            }
            corsair_risk::RiskCheckOutcome::AlreadyKilled(_) => {}
            corsair_risk::RiskCheckOutcome::Healthy => {
                log::info!(
                    "RISK: positions={} long={} short={} delta={:+.2} theta={:+.0} vega={:+.0}",
                    agg.total.gross_positions,
                    agg.total.long_count,
                    agg.total.short_count,
                    worst_delta,
                    worst_theta,
                    worst_vega
                );
            }
        }
    }
}

/// Find the worst per-product Greeks. For delta we use absolute
/// magnitude (worst is largest |delta|); for theta, the most-negative
/// number; for vega, largest magnitude. Mirrors the per-product
/// loop in risk_monitor.py.
fn worst_per_product(
    agg: &corsair_position::aggregation::AggregateResult,
    runtime: &Arc<Runtime>,
) -> (f64, f64, f64) {
    let mut worst_delta = 0.0_f64;
    let mut worst_theta = 0.0_f64;
    let mut worst_vega = 0.0_f64;

    for (prod, g) in &agg.per_product {
        // Effective delta = options + hedge_qty when gating on.
        let hedge_qty = if runtime.config.constraints.effective_delta_gating {
            runtime.hedge.lock().unwrap().hedge_qty_for_product(prod)
        } else {
            0
        };
        let d = g.net_delta + hedge_qty as f64;
        if d.abs() > worst_delta.abs() {
            worst_delta = d;
        }
        if g.net_theta < worst_theta {
            worst_theta = g.net_theta;
        }
        if g.net_vega.abs() > worst_vega.abs() {
            worst_vega = g.net_vega;
        }
    }
    (worst_delta, worst_theta, worst_vega)
}

async fn periodic_hedge(runtime: Arc<Runtime>) {
    let mut t = interval(Duration::from_secs(30));
    log::info!("periodic_hedge: cadence 30s");
    loop {
        t.tick().await;
        // Take a snapshot of products + forwards while holding
        // market_data + portfolio briefly, then release before
        // hitting hedge.
        let products_and_forwards: Vec<(String, f64)> = {
            let p = runtime.portfolio.lock().unwrap();
            let md = runtime.market_data.lock().unwrap();
            p.registry()
                .products()
                .iter()
                .map(|prod| {
                    let f = md.underlying_price(prod).unwrap_or(0.0);
                    (prod.clone(), f)
                })
                .collect()
        };
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        let actions: Vec<(String, corsair_hedge::HedgeAction)> = {
            let p = runtime.portfolio.lock().unwrap();
            let mut h = runtime.hedge.lock().unwrap();
            let mut out = Vec::new();
            for (prod, fwd) in &products_and_forwards {
                if let Some(mgr) = h.for_product_mut(prod) {
                    let action = mgr.rebalance_periodic(&p, *fwd, now_ns);
                    out.push((prod.clone(), action));
                }
            }
            out
        };
        for (prod, action) in actions {
            log::info!("hedge[{prod}]: {action:?}");
            // Phase 4.x: wire HedgeAction::Place to broker.place_order
            // when mode=Live. For now we log only; the existing Python
            // broker still does the actual hedging.
            let _ = runtime.mode;
        }
    }
}

async fn periodic_snapshot(runtime: Arc<Runtime>) {
    let cadence = runtime.config.snapshot.cadence_ms;
    let mut t = interval(Duration::from_millis(cadence));
    log::info!("periodic_snapshot: cadence {cadence}ms");
    loop {
        t.tick().await;
        let result = {
            let p = runtime.portfolio.lock().unwrap();
            let r = runtime.risk.lock().unwrap();
            let h = runtime.hedge.lock().unwrap();
            let md = runtime.market_data.lock().unwrap();
            let mut s = runtime.snapshot.lock().unwrap();
            s.publish(
                &p,
                &r,
                &h,
                &*md,
                corsair_snapshot::payload::AccountSnapshot::default(),
            )
        };
        if let Err(e) = result {
            log::warn!("snapshot publish failed: {e}");
        }
    }
}

async fn periodic_account_poll(runtime: Arc<Runtime>) {
    let mut t = interval(Duration::from_secs(300));
    log::info!("periodic_account_poll: cadence 300s");
    loop {
        t.tick().await;
        let result = {
            let b = runtime.broker.lock().await;
            b.account_values().await
        };
        match result {
            Ok(snap) => {
                log::info!(
                    "ACCOUNT: NLV=${:.0} maint=${:.0} init=${:.0} BP=${:.0} realized=${:.0}",
                    snap.net_liquidation,
                    snap.maintenance_margin,
                    snap.initial_margin,
                    snap.buying_power,
                    snap.realized_pnl_today
                );
            }
            Err(e) => log::warn!("account_values poll failed: {e}"),
        }
    }
}
