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
    handles.push(tokio::spawn(pump_depth(runtime.clone())));
    handles.push(tokio::spawn(pump_errors(runtime.clone())));
    handles.push(tokio::spawn(pump_connection(runtime.clone())));

    handles.push(tokio::spawn(periodic_greek_refresh(runtime.clone())));
    handles.push(tokio::spawn(periodic_risk_check(runtime.clone())));
    handles.push(tokio::spawn(crate::subscriptions::run_depth_rotator(
        runtime.clone(),
    )));
    handles.push(tokio::spawn(periodic_hedge(runtime.clone())));
    handles.push(tokio::spawn(periodic_snapshot(runtime.clone())));
    handles.push(tokio::spawn(periodic_account_poll(runtime.clone())));
    handles.push(tokio::spawn(daily_halt_rollover(runtime.clone())));
    handles.push(tokio::spawn(periodic_tick_type_hist(runtime.clone())));

    handles
}

/// Diagnostic: every 30s, log the histogram of incoming TickSize
/// tick_type values since the last tick. Surfaces routing and
/// gateway-permission issues — e.g. if call OI (27) is missing while
/// put OI (28) flows, that's a data-feed problem, not a code bug.
async fn periodic_tick_type_hist(runtime: Arc<Runtime>) {
    let mut t = interval(Duration::from_secs(30));
    log::info!("periodic_tick_type_hist: cadence 30s");
    loop {
        t.tick().await;
        let hist = runtime.broker.diagnostic_take_tick_type_hist();
        if hist.is_empty() {
            continue;
        }
        let summary: String = hist
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join(" ");
        log::info!("tick_type_hist (30s): {summary}");
    }
}

// ─── Stream pumps ──────────────────────────────────────────────────

async fn pump_fills(runtime: Arc<Runtime>) {
    let mut rx = {
        let b = runtime.broker.clone();
        b.subscribe_fills()
    };
    log::info!("pump_fills: subscribed");
    loop {
        match rx.recv().await {
            Ok(fill) => {
                handle_fill(&runtime, fill).await;
            }
            Err(RecvError::Lagged(n)) => {
                // Lagged = silent fill loss. Each dropped frame is a
                // fill that never updated our portfolio state, leaving
                // local out-of-sync with IBKR until the next periodic
                // reconcile. Treat as a P0 incident: log loud, force
                // an immediate reconcile rather than waiting up to 2
                // min for the next periodic_hedge sweep.
                log::error!(
                    "pump_fills LAGGED {n} frames — fills lost. Triggering \
                     immediate position re-sync from broker."
                );
                let positions_result = {
                    let b = runtime.broker.clone();
                    b.positions().await
                };
                if let Ok(positions) = positions_result {
                    let n = runtime.reconcile_options_with_broker_positions(&positions);
                    log::error!(
                        "pump_fills lag-recovery reconcile: corrected {n} option leg(s)"
                    );
                } else if let Err(e) = positions_result {
                    log::error!("pump_fills lag-recovery: positions() failed: {e}");
                }
            }
            Err(RecvError::Closed) => {
                log::info!("pump_fills: channel closed; exiting");
                break;
            }
        }
    }
}

async fn handle_fill(runtime: &Arc<Runtime>, fill: corsair_broker_api::events::Fill) {
    // Try hedge first — if it accepts, the fill was a hedge fill, NOT
    // an option fill. (apply_broker_fill returns true only if the
    // instrument matches the hedge contract.)
    {
        let mut h = runtime.hedge.lock().unwrap();
        // HedgeFanout exposes `for_product_mut(name)` but no mut iter,
        // so collect product names then dispatch each.
        let products: Vec<String> =
            h.managers().iter().map(|x| x.config().product.clone()).collect();
        for prod in &products {
            if let Some(mgr) = h.for_product_mut(prod) {
                if mgr.apply_broker_fill(&fill) {
                    return;
                }
            }
        }
    }

    // Otherwise it's an option fill (or an unknown instrument we
    // ignore). Look up product from market data registry to find
    // strike/expiry/right. Wrap in a scope so the MutexGuard is
    // strictly released before any .await downstream — rustc's NLL
    // is conservative across await points.
    let instr = fill.instrument_id;
    // Lock order: portfolio THEN market_data (matches the rest of the
    // codebase). Inverting causes deadlock when periodic_risk_check
    // grabs portfolio→md while we hold md→portfolio.
    let matched: Option<(
        String,
        f64,
        chrono::NaiveDate,
        corsair_broker_api::Right,
    )> = {
        let products: Vec<String> = {
            let p = runtime.portfolio.lock().unwrap();
            p.registry().products()
        };
        let md = runtime.market_data.lock().unwrap();
        let mut found: Option<(String, f64, chrono::NaiveDate, corsair_broker_api::Right)> = None;
        for prod in products {
            for t in md.options_for_product(&prod) {
                if t.instrument_id == Some(instr) {
                    found = Some((prod.clone(), t.strike, t.expiry, t.right));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        found
    };
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
    let halt_outcome = {
        let p = runtime.portfolio.lock().unwrap();
        let md = runtime.market_data.lock().unwrap();
        let mut r = runtime.risk.lock().unwrap();
        r.check_daily_pnl_only(&p, &*md)
    };
    if let corsair_risk::RiskCheckOutcome::Killed(ref ev) = halt_outcome {
        crate::notify::notify_kill(ev.clone());
        cancel_all_resting(runtime, "daily_halt_per_fill").await;
    }

    // Per-fill DELTA enforcement. The 300s periodic risk_check is too
    // slow to catch a fast cascade — on 2026-05-04 we accumulated ~38
    // contracts in ~4 minutes (options_delta 0 → -14.6) before the
    // periodic check fired. This gate runs on EVERY option fill and
    // forces the kill the moment effective_delta crosses delta_kill.
    let effective_delta_post_fill: f64 = {
        let p = runtime.portfolio.lock().unwrap();
        let h = runtime.hedge.lock().unwrap();
        let agg = p.aggregate();
        let hedge_qty: i32 = h.managers().iter().map(|m| m.hedge_qty()).sum();
        agg.total.net_delta + hedge_qty as f64
    };
    let delta_outcome = {
        let mut r = runtime.risk.lock().unwrap();
        r.check_per_fill_delta(effective_delta_post_fill)
    };
    if let corsair_risk::RiskCheckOutcome::Killed(ref ev) = delta_outcome {
        crate::notify::notify_kill(ev.clone());
        cancel_all_resting(runtime, "delta_kill_per_fill").await;
    }

    // CLAUDE.md §10: rebalance hedge on every option fill (in
    // addition to the 30s periodic). Without this, every option fill
    // waits up to 30s for the periodic hedge tick — net delta drifts
    // unhedged for that window.
    let hedge_action = {
        let p = runtime.portfolio.lock().unwrap();
        let mut h = runtime.hedge.lock().unwrap();
        h.for_product_mut(&product)
            .map(|mgr| mgr.rebalance_on_fill(&p))
    };
    if let Some(corsair_hedge::HedgeAction::Place { is_buy, qty, reason, .. }) = hedge_action {
        if matches!(runtime.mode, crate::runtime::RuntimeMode::Live) {
            place_hedge_order(runtime, &product, is_buy, qty, &reason).await;
        } else {
            log::info!("hedge[{product}]: shadow Place {is_buy} qty={qty} ({reason})");
        }
    }
}

async fn pump_status(runtime: Arc<Runtime>) {
    let mut rx = {
        let b = runtime.broker.clone();
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

/// Drain L2 depth updates from the broker and apply to MarketDataState.
/// Each update describes ONE level (insert/update/delete) on ONE side.
/// MarketDataState's depth book aggregates the ops into a 5-level
/// per-leg book that the trader can query for "external best".
async fn pump_depth(runtime: Arc<Runtime>) {
    let mut rx = {
        let b = runtime.broker.clone();
        b.subscribe_depth_stream()
    };
    log::info!("pump_depth: subscribed");
    loop {
        match rx.recv().await {
            Ok(d) => {
                let mut md = runtime.market_data.lock().unwrap();
                md.apply_depth(
                    d.instrument_id,
                    d.is_bid,
                    d.position,
                    d.operation,
                    d.price,
                    d.size,
                    d.timestamp_ns,
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                log::warn!("pump_depth: lagged {n}");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn pump_ticks(runtime: Arc<Runtime>) {
    let mut rx = {
        let b = runtime.broker.clone();
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
                    TickKind::BidSize => {
                        // Size ticks come as separate TickSize msgs from
                        // the price ticks; without explicit handling
                        // bid_size stays 0 and the trader's dark-book
                        // guard refuses to quote.
                        if let Some(s) = tick.size {
                            md.update_bid_size(tick.instrument_id, s, tick.timestamp_ns);
                        }
                    }
                    TickKind::AskSize => {
                        if let Some(s) = tick.size {
                            md.update_ask_size(tick.instrument_id, s, tick.timestamp_ns);
                        }
                    }
                    TickKind::OptionOpenInterest => {
                        if let Some(s) = tick.size {
                            md.update_open_interest(tick.instrument_id, s, tick.timestamp_ns);
                        }
                    }
                    TickKind::OptionVolume => {
                        if let Some(s) = tick.size {
                            md.update_option_volume(tick.instrument_id, s, tick.timestamp_ns);
                        }
                    }
                    TickKind::Volume => {} // underlying volume — not currently surfaced
                }
            }
            Err(RecvError::Lagged(n)) => log::warn!("pump_ticks: lagged {n}"),
            Err(RecvError::Closed) => break,
        }
    }
}

async fn pump_errors(runtime: Arc<Runtime>) {
    let mut rx = {
        let b = runtime.broker.clone();
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
        let b = runtime.broker.clone();
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
    // Was 300s — way too slow for the dashboard's mark column.
    // refresh_greeks updates Position.current_price from market_data
    // (mid price), which the snapshot writer reads at 250ms cadence.
    // 5s gives the dashboard near-live position marks at negligible
    // CPU cost (the loop is just a hashmap lookup per position).
    let mut t = interval(Duration::from_secs(5));
    log::info!("periodic_greek_refresh: cadence 5s");
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
    // Race condition fix: tokio::interval fires the first tick
    // immediately. At boot, options Greeks haven't been computed yet
    // (greek_refresh runs every 5s starting after first interval),
    // so the seeded positions show delta=0. If hedge_qty is non-zero
    // at seed, options_delta+hedge_qty == hedge_qty alone, which can
    // wrongly trip delta_kill (sticky → trader stuck blocked).
    // Burn the immediate tick so the first real risk eval happens
    // ~6s after boot, well after the first greek_refresh has run.
    t.tick().await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    loop {
        t.tick().await;
        // Compute outcome under locks; release before any await.
        let (outcome, agg_summary) = {
            let p = runtime.portfolio.lock().unwrap();
            let md = runtime.market_data.lock().unwrap();
            let mut r = runtime.risk.lock().unwrap();
            let agg = p.aggregate();
            let (worst_delta, worst_theta, worst_vega) =
                worst_per_product(&agg, &runtime);
            let margin_used = runtime
                .account
                .lock()
                .map(|a| a.maintenance_margin)
                .unwrap_or(0.0);
            let outcome = r.check(
                &p,
                margin_used,
                worst_delta,
                worst_theta,
                worst_vega,
                &*md,
            );
            let summary = (
                agg.total.gross_positions,
                agg.total.long_count,
                agg.total.short_count,
                worst_delta,
                worst_theta,
                worst_vega,
            );
            (outcome, summary)
        };
        match &outcome {
            corsair_risk::RiskCheckOutcome::Killed(ev) => {
                log::error!("risk check fired kill: {ev:?}");
                crate::notify::notify_kill(ev.clone());
                // CLAUDE.md §7/§8: every kill must cancel all resting
                // orders. Without this, kill is cosmetic — orders
                // continue resting until GTD-expiry.
                cancel_all_resting(&runtime, "risk_kill").await;
            }
            corsair_risk::RiskCheckOutcome::AlreadyKilled(_) => {}
            corsair_risk::RiskCheckOutcome::Healthy => {
                log::info!(
                    "RISK: positions={} long={} short={} delta={:+.2} theta={:+.0} vega={:+.0}",
                    agg_summary.0,
                    agg_summary.1,
                    agg_summary.2,
                    agg_summary.3,
                    agg_summary.4,
                    agg_summary.5,
                );
            }
        }
    }
}

/// Cancel every live order at the broker. Called from any kill path
/// (risk check, per-fill daily-halt). Mirrors `quote_engine.cancel_all_quotes`
/// in Python broker. Best-effort: logs cancel failures but doesn't propagate
/// (kill semantics need to fire even if a cancel races a fill).
///
/// Uses `Broker::open_orders()` (IBKR-authoritative) instead of the
/// runtime OMS, which is not populated by the broker daemon — using
/// the OMS would cancel zero orders and the kill would be cosmetic.
async fn cancel_all_resting(runtime: &Arc<Runtime>, reason: &str) {
    let b = runtime.broker.clone();
    let opens = match b.open_orders().await {
        Ok(v) => v,
        Err(e) => {
            log::warn!("cancel_all_resting[{reason}]: open_orders query failed: {e}");
            return;
        }
    };
    let actionable: Vec<corsair_broker_api::OrderId> = opens
        .into_iter()
        .filter(|o| o.remaining_qty > 0)
        .map(|o| o.order_id)
        .collect();
    if actionable.is_empty() {
        log::warn!("cancel_all_resting[{reason}]: no live orders at broker");
        return;
    }
    log::warn!(
        "cancel_all_resting[{reason}]: cancelling {} orders",
        actionable.len()
    );
    for oid in actionable {
        if let Err(e) = b.cancel_order(oid).await {
            log::warn!("cancel_all_resting[{reason}]: cancel {oid:?} failed: {e}");
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
    let mut tick_count: u64 = 0;
    loop {
        t.tick().await;
        tick_count = tick_count.wrapping_add(1);

        // CLAUDE.md §10 periodic reconcile: every 4 ticks (~2 min)
        // call broker.positions() and reconcile BOTH options portfolio
        // AND hedge_qty against IBKR's view. Options reconcile catches
        // the silent-fill-loss class (broadcast lag, fill-on-
        // unregistered-instrument); hedge reconcile catches non-filling
        // IOCs. Done every 4 ticks rather than every tick to keep the
        // periodic loop light — divergence accumulates slowly.
        if tick_count.is_multiple_of(4) {
            let positions_result = {
                let b = runtime.broker.clone();
                b.positions().await
            };
            if let Ok(positions) = positions_result {
                // Options portfolio reconcile — silent no-op when
                // local matches IBKR; loud + auto-correcting on drift.
                let opts_changes =
                    runtime.reconcile_options_with_broker_positions(&positions);
                if opts_changes > 0 {
                    log::error!(
                        "periodic_reconcile: corrected {opts_changes} option leg(s) — \
                         likely missed fills from pump_fills lag or unregistered \
                         instrument. Investigate logs for RecvError::Lagged or \
                         'fill on unregistered instrument'."
                    );
                }
                let mut h = runtime.hedge.lock().unwrap();
                // First touch all managers so flat legs (which won't
                // appear in `positions`) keep their freshness gate
                // alive. Audit T1-1: without this, is_fresh() returns
                // false after 300s on flat legs and the effective-
                // delta gate quietly falls back to options-only.
                for mgr in h.managers_mut() {
                    mgr.touch_freshness();
                }
                for pos in &positions {
                    if pos.contract.kind != corsair_broker_api::ContractKind::Future {
                        continue;
                    }
                    if let Some(mgr) = h.for_product_mut(&pos.contract.symbol) {
                        let mult = if pos.contract.multiplier > 0.0 {
                            pos.contract.multiplier
                        } else {
                            25_000.0
                        };
                        let avg = pos.avg_cost / mult;
                        mgr.reconcile_with_position(pos.quantity, avg, true);
                    }
                }
            } else if let Err(e) = positions_result {
                log::error!("periodic_reconcile: broker.positions() failed: {e}");
            }
        }

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
            if let corsair_hedge::HedgeAction::Place {
                is_buy,
                qty,
                reason,
                ..
            } = action
            {
                // Live mode only — shadow logs but doesn't place.
                if !matches!(runtime.mode, crate::runtime::RuntimeMode::Live) {
                    continue;
                }
                place_hedge_order(&runtime, &prod, is_buy, qty, &reason).await;
            }
        }
    }
}

/// Place a hedge order via the broker. Resolves the hedge contract
/// from the per-product manager's cached resolved contract; if no
/// contract is set, logs and skips.
async fn place_hedge_order(
    runtime: &Arc<Runtime>,
    product: &str,
    is_buy: bool,
    qty: u32,
    reason: &str,
) {
    let (contract_opt, ioc_offset, hedge_tick) = {
        let h = runtime.hedge.lock().unwrap();
        match h.for_product(product) {
            Some(mgr) => (
                mgr.hedge_contract().cloned(),
                mgr.config().ioc_tick_offset,
                mgr.config().hedge_tick_size,
            ),
            None => {
                log::warn!("hedge[{product}]: no manager registered, skipping");
                return;
            }
        }
    };
    let contract = match contract_opt {
        Some(c) => c,
        None => {
            log::warn!("hedge[{product}]: no hedge contract resolved yet, skipping");
            return;
        }
    };
    // IOC limit anchored at current underlying ± ioc_offset ticks.
    let f = {
        let md = runtime.market_data.lock().unwrap();
        md.underlying_price(product).unwrap_or(0.0)
    };
    if f <= 0.0 {
        log::warn!("hedge[{product}]: no underlying price, skipping");
        return;
    }
    // Hedge order type: MARKET. The original design was IOC ±N ticks
    // — small bounded slippage if filled, but the IOC dies when
    // market moves >N ticks during the ~140ms IBKR RTT. In a fast
    // move (exactly when we MOST need to be hedged) the IOC reliably
    // missed. HG futures depth is deep enough that a 4-contract
    // market order slips 1-2 ticks at worst. "Fill at any reasonable
    // price" beats "fill at +N ticks or never". `_ioc_offset` and
    // `_hedge_tick` are retained for the price-anchor field but
    // unused on the market path. 2026-05-04: switched after
    // observing 4 consecutive 30s-periodic IOC misses while
    // options_delta sat at -4.3.
    let _ = (ioc_offset, hedge_tick);
    let req = corsair_broker_api::PlaceOrderReq {
        contract,
        side: if is_buy {
            corsair_broker_api::Side::Buy
        } else {
            corsair_broker_api::Side::Sell
        },
        qty,
        order_type: corsair_broker_api::OrderType::Market,
        price: None,
        // IOC so a market order doesn't queue if for some reason the
        // contract has no bid/ask momentarily — caller's next periodic
        // tick will retry with fresh underlying.
        tif: corsair_broker_api::TimeInForce::Ioc,
        gtd_until_utc: None,
        client_order_ref: format!("corsair_hedge_{}", reason),
        account: runtime
            .config
            .broker
            .ibkr
            .as_ref()
            .map(|i| i.account.clone()),
    };
    let result = {
        let b = runtime.broker.clone();
        b.place_order(req).await
    };
    match result {
        Ok(oid) => log::warn!(
            "hedge[{product}]: placed MKT {} {} oid={} ({})",
            if is_buy { "BUY" } else { "SELL" },
            qty,
            oid,
            reason
        ),
        Err(e) => log::error!("hedge[{product}]: place_order failed: {e}"),
    }
}

async fn periodic_snapshot(runtime: Arc<Runtime>) {
    let cadence = runtime.config.snapshot.cadence_ms;
    let mut t = interval(Duration::from_millis(cadence));
    log::info!("periodic_snapshot: cadence {cadence}ms");
    loop {
        t.tick().await;
        // Build the AccountSnapshot payload from `runtime.account` and
        // the constraint checker's IBKR scale. Previously this passed
        // `AccountSnapshot::default()` so the dashboard saw zero NLV /
        // margin / buying_power even though periodic_account_poll was
        // updating runtime.account every 5 min.
        let acct_payload = {
            let a = runtime.account.lock().unwrap();
            let cc = runtime.constraint.lock().unwrap();
            corsair_snapshot::payload::AccountSnapshot {
                net_liquidation: a.net_liquidation,
                maintenance_margin: a.maintenance_margin,
                initial_margin: a.initial_margin,
                buying_power: a.buying_power,
                realized_pnl_today: a.realized_pnl_today,
                ibkr_scale: cc.ibkr_scale(),
            }
        };
        // Pull open_orders from broker BEFORE entering the snapshot
        // critical section. open_orders() returns the broker's
        // in-memory cache (no IO), populated by route() on every
        // OpenOrder/OrderStatus event from IBKR. Used by the chain
        // payload to populate our_bid / our_ask per strike so the
        // dashboard's chain table can highlight our resting prices.
        let open_orders_snapshot = {
            let b = runtime.broker.clone();
            b.open_orders().await.unwrap_or_default()
        };
        // Diagnostic: log status histogram every snapshot tick (info
        // level temporarily so we can see WITHOUT a custom RUST_LOG).
        // If build_chain_payload's filter (Submitted | PendingSubmit)
        // is dropping everything, this surfaces what status IBKR is
        // actually leaving them in.
        {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n % 40 == 0 {
                // Once every ~10s (snapshot is 250ms cadence)
                let mut by_status = std::collections::HashMap::<String, u32>::new();
                for o in &open_orders_snapshot {
                    *by_status.entry(format!("{:?}", o.status)).or_insert(0) += 1;
                }
                log::info!(
                    "open_orders snapshot: total={} statuses={:?}",
                    open_orders_snapshot.len(),
                    by_status
                );
            }
        }
        // Pull latency stats from the rolling sample buffer.
        let latency_snapshot = {
            let s = runtime.latency_samples.lock().unwrap();
            let (ttt_n, ttt_p50, ttt_p99) = s.ttt_stats();
            let (rtt_n, rtt_p50, rtt_p99) = s.place_rtt_stats();
            let (mod_n, mod_p50, mod_p99) = s.modify_rtt_stats();
            if ttt_n > 0 || rtt_n > 0 || mod_n > 0 {
                Some(corsair_snapshot::payload::LatencySnapshot {
                    ttt_us: corsair_snapshot::payload::LatencyStats {
                        n: ttt_n,
                        p50: ttt_p50,
                        p99: ttt_p99,
                    },
                    place_rtt_us: corsair_snapshot::payload::LatencyStats {
                        n: rtt_n,
                        p50: rtt_p50,
                        p99: rtt_p99,
                    },
                    amend_us: corsair_snapshot::payload::LatencyStats {
                        n: mod_n,
                        p50: mod_p50,
                        p99: mod_p99,
                    },
                })
            } else {
                None
            }
        };
        let result = {
            let p = runtime.portfolio.lock().unwrap();
            let r = runtime.risk.lock().unwrap();
            let h = runtime.hedge.lock().unwrap();
            let md = runtime.market_data.lock().unwrap();
            let chain_build = build_chain_payload(&runtime, &md, &p, &open_orders_snapshot);
            let mut s = runtime.snapshot.lock().unwrap();
            s.publish(&p, &r, &h, &*md, acct_payload, chain_build, latency_snapshot)
        };
        if let Err(e) = result {
            log::warn!("snapshot publish failed: {e}");
        }
    }
}

/// Session rollover at 17:00 CT — clears daily P&L halt (CLAUDE.md §8).
/// `RiskMonitor::clear_daily_halt` is a noop when no daily-halt kill is
/// active, so it's safe to fire every cycle. Implementation: tick every
/// 60s and check if the current US/Central time is 17:00..17:01.
async fn daily_halt_rollover(runtime: Arc<Runtime>) {
    use chrono::Timelike;
    let mut t = interval(Duration::from_secs(60));
    log::info!("daily_halt_rollover: armed; clears at 17:00 US/Central");
    // Track the last day we fired so we don't double-fire if a tick
    // lands twice in the same minute window. Day key is in CT zone.
    let mut last_fired_day: Option<chrono::NaiveDate> = None;
    loop {
        t.tick().await;
        // True US/Central with DST handling. Without this we'd be off
        // by 1h for ~8 months of the year (CST vs CDT).
        let now_ct = chrono::Utc::now().with_timezone(&chrono_tz::US::Central);
        let today_ct = now_ct.date_naive();
        // Fire on first sample at-or-past 17:00 CT each day. Robust
        // against tick scheduling drift (the 60s interval may land
        // anywhere in the minute) — only the first sample after the
        // boundary fires.
        let past_boundary = now_ct.hour() >= 17;
        let already_fired_today = last_fired_day == Some(today_ct);
        if past_boundary && !already_fired_today {
            let mut r = runtime.risk.lock().unwrap();
            if r.clear_daily_halt() {
                log::warn!(
                    "daily_halt_rollover: cleared daily halt at {now_ct} (US/Central)"
                );
            }
            last_fired_day = Some(today_ct);
        }
    }
}

async fn periodic_account_poll(runtime: Arc<Runtime>) {
    // Cadence dropped 300s → 15s on 2026-05-04. The 5-min refresh
    // meant the dashboard's Margin tile and the trader's margin gate
    // worked off stale data — during the 12:16 cascade we had ~$280K
    // of fresh maint margin while the cached snapshot still showed $0
    // (boot-time value). 15s is a comfortable trade — IBKR pushes
    // AccountValue updates every ~5s anyway; this is just refreshing
    // our cache from the streamed map.
    let mut t = interval(Duration::from_secs(15));
    log::info!("periodic_account_poll: cadence 15s");
    loop {
        t.tick().await;
        let result = {
            let b = runtime.broker.clone();
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
                let ibkr_actual = snap.maintenance_margin;
                if let Ok(mut a) = runtime.account.lock() {
                    *a = snap;
                }
                // CLAUDE.md §3: ibkr_scale calibration. Compute the
                // current raw synthetic SPAN against current positions
                // and forward, then divide IBKR's MaintMarginReq by it.
                if ibkr_actual > 0.0 {
                    let raw = compute_raw_synthetic_margin(&runtime);
                    if raw > 0.0 {
                        let mut cc = runtime.constraint.lock().unwrap();
                        cc.update_cached_margin(ibkr_actual, raw, now_ns());
                    }
                }
            }
            Err(e) => log::warn!("account_values poll failed: {e}"),
        }
    }
}

fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Build the chain payload for the snapshot from market_data state +
/// portfolio. Per-expiry blocks of strikes, with call/put market quotes
/// and current position. ATM strike picked relative to the underlying
/// price; expiries sorted ascending; front_month_expiry is the first.
fn build_chain_payload(
    runtime: &Arc<Runtime>,
    md: &corsair_market_data::MarketDataState,
    portfolio: &corsair_position::PortfolioState,
    open_orders: &[corsair_broker_api::OpenOrder],
) -> corsair_snapshot::ChainBuild {
    use corsair_broker_api::{Right, Side};
    use corsair_snapshot::{ChainExpirySnapshot, SideBlockSnapshot, StrikeBlockSnapshot};
    use std::collections::HashMap;
    let mut chains: HashMap<String, HashMap<String, StrikeBlockSnapshot>> = HashMap::new();
    let products = portfolio.registry().products();
    let mut underlying_price = 0.0_f64;
    // Snapshot vol_surface cache once for theo computation. Keyed by
    // (product, expiry_str). Empty when no fit has run yet.
    let vol_cache = runtime.vol_surface_cache.lock().unwrap().clone();
    for prod in &products {
        if let Some(p) = corsair_position::MarketView::underlying_price(md, prod) {
            underlying_price = p;
        }
        for opt in md.options_for_product(prod) {
            let expiry_str = opt.expiry.format("%Y%m%d").to_string();
            let strike_str = format!("{:.4}", opt.strike);
            let block = chains.entry(expiry_str.clone()).or_default();
            let strike_block = block.entry(strike_str).or_insert(StrikeBlockSnapshot {
                call: None,
                put: None,
            });
            // Per-leg theo from cached SABR fit + Black76. None if no
            // fit yet for this expiry, or if the fit math degenerates.
            let theo: Option<f64> = vol_cache
                .get(&(prod.clone(), expiry_str.clone()))
                .and_then(|e| {
                    let iv = corsair_pricing::sabr_implied_vol(
                        e.forward, opt.strike, e.tte,
                        e.alpha, e.beta, e.rho, e.nu,
                    );
                    if !iv.is_finite() || iv <= 0.0 {
                        return None;
                    }
                    let right_char = match opt.right {
                        Right::Call => 'C',
                        Right::Put => 'P',
                    };
                    let p = corsair_pricing::black76_price_inner(
                        e.forward, opt.strike, e.tte, iv, 0.0, right_char,
                    );
                    if p.is_finite() && p > 0.0 { Some(p) } else { None }
                });
            // Position quantity for this leg (may be 0).
            let pos_qty: i32 = portfolio
                .positions()
                .iter()
                .find(|p| {
                    p.product == *prod
                        && (p.strike - opt.strike).abs() < 1e-6
                        && p.expiry == opt.expiry
                        && p.right == opt.right
                })
                .map(|p| p.quantity)
                .unwrap_or(0);
            // Look up our resting orders for this leg.
            // Audit T1-6: filter to actively-live statuses FIRST so a
            // stale Cancelled/Filled/Rejected order can never win the
            // overwrite race against a fresh Submitted/PendingSubmit
            // one. The broker's open_orders cache is a HashMap whose
            // iteration order is undefined, so without this filter
            // cancel-before-replace can flicker the dashboard between
            // the live new price and the dead old one.
            use corsair_broker_api::OrderStatus;
            let mut our_bid: Option<f64> = None;
            let mut our_ask: Option<f64> = None;
            let mut bid_live = false;
            let mut ask_live = false;
            for o in open_orders.iter().filter(|o| {
                matches!(
                    o.status,
                    OrderStatus::Submitted | OrderStatus::PendingSubmit
                ) && o.contract.kind == corsair_broker_api::ContractKind::Option
                    && o.contract.right == Some(opt.right)
                    && o.contract.expiry == opt.expiry
                    && (o.contract.strike.unwrap_or(0.0) - opt.strike).abs() < 1e-6
            }) {
                if let Some(p) = o.price {
                    // Submitted = live at exchange (green); PendingSubmit
                    // = gateway-accepted but exchange not yet confirmed
                    // (yellow on the dashboard).
                    let live = matches!(o.status, OrderStatus::Submitted);
                    match o.side {
                        Side::Buy => {
                            our_bid = Some(p);
                            bid_live = live;
                        }
                        Side::Sell => {
                            our_ask = Some(p);
                            ask_live = live;
                        }
                    }
                }
            }
            // External (peer) best — strips our resting orders from
            // the top of book using L2 depth. When we're the lone bid
            // the L1 bid IS our quote; the dashboard wants to show
            // the next-best peer quote so the operator can see whether
            // we're inside the spread or matching it. Falls back to
            // raw L1 (= 0.0) on strikes outside the L2-rotator window;
            // dashboard's `_fmt_side` then displays raw_bid/raw_ask.
            let ext_bid = opt.depth.external_best_bid(our_bid, 1);
            let ext_ask = opt.depth.external_best_ask(our_ask, 1);
            let side = SideBlockSnapshot {
                market_bid: if ext_bid > 0.0 { ext_bid } else { opt.bid },
                market_ask: if ext_ask > 0.0 { ext_ask } else { opt.ask },
                bid_size: opt.bid_size,
                ask_size: opt.ask_size,
                last: opt.last,
                pos: pos_qty,
                our_bid,
                our_ask,
                theo,
                open_interest: opt.open_interest,
                volume: opt.volume,
                bid_live,
                ask_live,
                raw_bid: opt.bid,
                raw_ask: opt.ask,
            };
            match opt.right {
                Right::Call => strike_block.call = Some(side),
                Right::Put => strike_block.put = Some(side),
            }
        }
    }
    // Convert nested map to ChainExpirySnapshot.
    let chains_typed: HashMap<String, ChainExpirySnapshot> = chains
        .into_iter()
        .map(|(exp, strikes)| (exp, ChainExpirySnapshot { strikes }))
        .collect();
    let mut expiries: Vec<String> = chains_typed.keys().cloned().collect();
    expiries.sort();
    let front_month_expiry = expiries.first().cloned();
    // ATM strike: round underlying_price to nearest 0.05 (HG grid).
    // Could be product-aware in future; HG-only for now.
    let atm_strike = if underlying_price > 0.0 {
        (underlying_price * 20.0).round() / 20.0
    } else {
        0.0
    };
    corsair_snapshot::ChainBuild {
        chains: chains_typed,
        atm_strike,
        expiries,
        front_month_expiry,
    }
}

/// Compute raw synthetic SPAN margin for the current portfolio.
/// Used by `periodic_account_poll` to recalibrate the ibkr_scale.
/// Returns 0 when there are no positions, no underlying price, or no
/// per-product config — caller treats those as "no calibration this
/// cycle".
fn compute_raw_synthetic_margin(runtime: &Arc<Runtime>) -> f64 {
    use chrono::Utc;
    let p = runtime.portfolio.lock().unwrap();
    let md = runtime.market_data.lock().unwrap();
    if p.position_count() == 0 {
        return 0.0;
    }

    // Group by product: each product has its own multiplier and
    // forward; SPAN's portfolio_margin assumes a single multiplier.
    let mut by_product: std::collections::HashMap<String, Vec<(f64, char, f64, f64, i64)>> =
        Default::default();
    let now = Utc::now();
    for pos in p.positions() {
        let t = (pos.expiry.and_hms_opt(16, 0, 0).unwrap()
            .and_utc()
            - now)
            .num_seconds() as f64
            / (365.0 * 86400.0);
        if t <= 0.0 {
            continue;
        }
        let right_char = match pos.right {
            corsair_broker_api::Right::Call => 'C',
            corsair_broker_api::Right::Put => 'P',
        };
        // IV: prefer market-implied via greek refresh. We don't have
        // the per-strike IV cached cheaply here, so fall back to
        // product default_iv. The scale calibration is forgiving —
        // raw_synthetic doesn't need to be exact, just stable.
        let iv = p
            .registry()
            .get(&pos.product)
            .map(|i| i.default_iv)
            .unwrap_or(0.30);
        by_product
            .entry(pos.product.clone())
            .or_default()
            .push((pos.strike, right_char, t, iv, pos.quantity as i64));
    }

    let mut total = 0.0;
    for (product, positions) in by_product {
        let f = match md.underlying_price(&product) {
            Some(f) => f,
            None => continue,
        };
        let info = match p.registry().get(&product) {
            Some(i) => i,
            None => continue,
        };
        let cfg = corsair_pricing::span::SpanConfig {
            up_scan_pct: 0.05,
            down_scan_pct: 0.05,
            vol_scan_pct: 0.25,
            extreme_mult: 2.0,
            extreme_cover: 0.35,
            short_option_minimum: 50.0,
            multiplier: info.multiplier,
        };
        let m = corsair_pricing::span::portfolio_margin(f, &positions, &cfg);
        total += m.total_margin;
    }
    total
}
