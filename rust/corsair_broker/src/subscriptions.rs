//! Market data subscription orchestration.
//!
//! Mirrors `src/market_data.py` boot path. Without this, the broker
//! has no view of the market — no ticks, no IVs, no Greeks, no vol
//! surface. Phase 5B.0 (renamed: should have been Phase 4.x but
//! deferred) addresses the gap before cutover.
//!
//! Per product, on boot:
//!   1. Resolve the front-month underlying future via `Broker::list_chain`
//!      (filters to lockout-skip cutoff if configured).
//!   2. Subscribe to its ticks → underlying_price.
//!   3. Wait briefly for the first underlying tick (max 10s).
//!   4. Pick ATM strike, generate strike list ATM ± N nickels per
//!      product config quote_range_low/high × strike_increment.
//!   5. For each strike × {Call, Put}:
//!      a. `qualify_option`
//!      b. `subscribe_ticks`
//!      c. `market_data.register_option`
//!
//! Static subscription for Phase 5B.0 — no ATM recentering. ATM
//! drift handling is Phase 6 work; for HG with $0.35 window the
//! intraday drift won't blow past the window in a single session.

use chrono::NaiveDate;
use corsair_broker_api::{
    ChainQuery, Contract, ContractKind, Currency, Exchange, OptionQuery, Right, TickSubscription,
};
use std::sync::Arc;
use std::time::Duration;

use crate::config::ProductConfig;
use crate::runtime::Runtime;

/// Subscribe to market data for every enabled product. Called once
/// during boot (after seed_positions_from_broker).
pub async fn subscribe_market_data(runtime: &Arc<Runtime>) -> Result<(), Box<dyn std::error::Error>> {
    let products: Vec<ProductConfig> = runtime
        .config
        .products
        .iter()
        .filter(|p| p.enabled)
        .cloned()
        .collect();
    for product in products {
        if let Err(e) = subscribe_product(runtime, &product).await {
            log::error!(
                "subscribe[{}]: failed: {} — broker will run without market data for this product",
                product.name, e
            );
        }
    }
    Ok(())
}

async fn subscribe_product(
    runtime: &Arc<Runtime>,
    product: &ProductConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let symbol = &product.name;
    log::warn!("subscribe[{symbol}]: starting market data orchestration");

    // 1. Find the front-month underlying. We list the chain and pick
    //    the earliest expiry past today.
    let underlying = resolve_underlying(runtime, symbol).await?;
    log::warn!(
        "subscribe[{symbol}]: underlying {} (expiry={}, conId={})",
        underlying.local_symbol, underlying.expiry, underlying.instrument_id.0
    );

    // 2. Register the underlying in market_data state and subscribe
    //    to its ticks.
    {
        let mut md = runtime.market_data.lock().unwrap();
        md.register_underlying(symbol, underlying.instrument_id);
    }
    {
        let b = runtime.broker.clone();
        b.subscribe_ticks(TickSubscription {
            instrument_id: underlying.instrument_id,
            tick_by_tick: false,
            consumer_tag: Some("underlying".into()),
            contract: Some(underlying.clone()),
        })
        .await?;
    }

    // 3. Wait briefly for first underlying tick. If we don't get one
    //    in 10s, fall back to picking ATM from product config (e.g.
    //    we know HG trades around $6/lb).
    let underlying_price = wait_for_underlying(runtime, symbol, 10).await;
    let atm = match underlying_price {
        Some(p) => round_to_increment(p, product.strike_increment),
        None => {
            log::warn!(
                "subscribe[{symbol}]: no underlying tick within 10s; \
                 deferring option subscription. Caller may retry later."
            );
            return Ok(());
        }
    };

    // 4. Generate strike list ATM ± range × increment.
    let strikes = generate_strikes(atm, product);
    log::warn!(
        "subscribe[{symbol}]: ATM={} → subscribing {} strikes × 2 rights",
        atm,
        strikes.len()
    );

    // 5. Qualify + subscribe each option (both calls and puts).
    //
    // We subscribe TWO expiry sets:
    //   (a) front_expiry's full ATM±range strike grid — used for
    //       fresh quoting decisions (decide_on_tick reads vol_surface
    //       fitted on these strikes).
    //   (b) ALL unique (expiry, strike, right) tuples from existing
    //       positions — needed for marking those positions to market.
    //       Without this, positions on a back-month expiry sit with
    //       current_price=0 in the dashboard.
    let front_expiry = pick_option_expiry(runtime, symbol, &underlying);
    let mut planned: std::collections::HashSet<
        (chrono::NaiveDate, u64 /* strike bits */, Right)
    > = std::collections::HashSet::new();
    for strike in &strikes {
        for right in [Right::Call, Right::Put] {
            planned.insert((front_expiry, strike.to_bits(), right));
        }
    }
    // Add position-derived (expiry, strike, right) tuples.
    {
        let p = runtime.portfolio.lock().unwrap();
        for pos in p.positions_for_product(symbol) {
            planned.insert((pos.expiry, pos.strike.to_bits(), pos.right));
        }
    }
    log::warn!(
        "subscribe[{symbol}]: ATM={} → subscribing {} (strike, right) on front + position legs (total {} unique tuples)",
        atm,
        strikes.len() * 2,
        planned.len(),
    );
    for (expiry, strike_bits, right) in planned {
        let strike = f64::from_bits(strike_bits);
        match qualify_and_subscribe(runtime, product, strike, expiry, right).await {
            Ok(()) => {}
            Err(e) => log::warn!(
                "subscribe[{symbol}] {} {:?} {}: {}",
                strike, right, expiry, e
            ),
        }
    }
    log::warn!("subscribe[{symbol}]: done");
    Ok(())
}

/// Periodic depth-subscription rotator. Mirrors the Python broker's
/// `rotate_depth_subscriptions` (CLAUDE.md note from market_data.py).
/// Every CADENCE_SEC, picks the K options closest to ATM that have
/// L1 data, cancels any active depth subs not in the set, and adds
/// new ones. K is capped to MAX_ACTIVE so we never exceed IBKR's
/// concurrent reqMktDepth limit (~3 free, more with quote booster).
///
/// Without L2 the trader can't see what's behind us when we're the
/// BBO; quoting compresses to our own min-edge spread on those
/// strikes. With L2 the trader's external_best_bid/ask call returns
/// the next-best after subtracting our resting size, so tick-jumping
/// targets external incumbents instead of self-quotes.
pub async fn run_depth_rotator(runtime: Arc<Runtime>) {
    use std::collections::HashSet;
    use std::time::Duration;
    use corsair_broker_api::TickStreamHandle;

    const CADENCE_SEC: u64 = 30;
    const MAX_ACTIVE: usize = 5; // IBKR concurrent reqMktDepth limit
    const NUM_ROWS: i32 = 5;     // depth levels per side

    // Map (instrument_id) → handle, so we can unsubscribe later.
    let mut active: std::collections::HashMap<corsair_broker_api::InstrumentId, TickStreamHandle> =
        std::collections::HashMap::new();
    let mut t = tokio::time::interval(Duration::from_secs(CADENCE_SEC));
    // First tick fires immediately — let market_data accumulate first.
    t.tick().await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    log::info!("depth_rotator: cadence {CADENCE_SEC}s, max_active={MAX_ACTIVE}");
    loop {
        t.tick().await;
        // Pick target instruments: strikes nearest ATM with valid L1
        // mid, capped at MAX_ACTIVE (call+put treated separately so
        // we cover both sides of ATM).
        //
        // Audit T1-3: lock order MUST be portfolio → market_data,
        // matching tasks.rs:periodic_*. Hoist the products list out
        // of the md+qc lock-block so we don't hold md across portfolio.
        let products = runtime.portfolio.lock().unwrap().registry().products();
        let target: Vec<(corsair_broker_api::InstrumentId, corsair_broker_api::Contract)> = {
            let md = runtime.market_data.lock().unwrap();
            let qc = runtime.qualified_contracts.lock().unwrap();
            let mut candidates: Vec<(f64, corsair_broker_api::InstrumentId, corsair_broker_api::Contract)> =
                Vec::new();
            for prod in &products {
                let und = match corsair_position::MarketView::underlying_price(&*md, prod) {
                    Some(p) => p,
                    None => continue,
                };
                for opt in md.options_for_product(prod) {
                    if opt.bid <= 0.0 && opt.ask <= 0.0 {
                        continue;
                    }
                    let iid = match opt.instrument_id {
                        Some(i) => i,
                        None => continue,
                    };
                    if let Some(c) = qc.get(&iid) {
                        let dist = (opt.strike - und).abs();
                        candidates.push((dist, iid, c.clone()));
                    }
                }
            }
            candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            candidates
                .into_iter()
                .take(MAX_ACTIVE)
                .map(|(_, iid, c)| (iid, c))
                .collect()
        };

        let want: HashSet<corsair_broker_api::InstrumentId> =
            target.iter().map(|(iid, _)| *iid).collect();

        // Cancel ones no longer wanted.
        let to_cancel: Vec<corsair_broker_api::InstrumentId> = active
            .keys()
            .copied()
            .filter(|iid| !want.contains(iid))
            .collect();
        for iid in to_cancel {
            if let Some(h) = active.remove(&iid) {
                let b = runtime.broker.clone();
                if let Err(e) = b.unsubscribe_market_depth(h).await {
                    log::warn!("depth_rotator: unsubscribe {:?} failed: {}", iid, e);
                }
            }
        }

        // Add new ones.
        for (iid, c) in target {
            if active.contains_key(&iid) {
                continue;
            }
            let b = runtime.broker.clone();
            match b
                .subscribe_market_depth(
                    corsair_broker_api::TickSubscription {
                        instrument_id: iid,
                        tick_by_tick: false,
                        consumer_tag: Some(format!("depth {}", c.local_symbol)),
                        contract: Some(c),
                    },
                    NUM_ROWS,
                )
                .await
            {
                Ok(h) => {
                    active.insert(iid, h);
                }
                Err(e) => {
                    log::warn!("depth_rotator: subscribe {:?} failed: {}", iid, e);
                }
            }
        }
        if !active.is_empty() {
            log::info!("depth_rotator: {} active L2 subs", active.len());
        }
    }
}

/// Resolve the front-month underlying contract.
async fn resolve_underlying(
    runtime: &Arc<Runtime>,
    symbol: &str,
) -> Result<Contract, Box<dyn std::error::Error>> {
    let q = ChainQuery {
        symbol: symbol.into(),
        exchange: Exchange::Comex,
        currency: Currency::Usd,
        kind: Some(ContractKind::Future),
        min_expiry: Some(chrono::Utc::now().date_naive()),
    };
    let chain = {
        let b = runtime.broker.clone();
        b.list_chain(q).await?
    };
    if chain.is_empty() {
        return Err(format!("no futures in chain for {symbol}").into());
    }
    let mut sorted = chain;
    sorted.sort_by(|a, b| a.expiry.cmp(&b.expiry));
    Ok(sorted.into_iter().next().unwrap())
}

/// Wait for the first underlying price update for `product`. Returns
/// `Some(price)` on success, `None` if no tick arrives within
/// `timeout_secs`.
async fn wait_for_underlying(
    runtime: &Arc<Runtime>,
    product: &str,
    timeout_secs: u64,
) -> Option<f64> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        {
            let md = runtime.market_data.lock().unwrap();
            if let Some(p) = md.underlying_price(product) {
                if p > 0.0 {
                    return Some(p);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

fn round_to_increment(value: f64, increment: f64) -> f64 {
    if increment <= 0.0 {
        return value;
    }
    (value / increment).round() * increment
}

fn generate_strikes(atm: f64, product: &ProductConfig) -> Vec<f64> {
    let inc = product.strike_increment;
    // Subscription is wider than quoting per CLAUDE.md §12 — SABR
    // needs wing data points to fit stably. Falls back to quote_range
    // when strike_range_low/high are unset, preserving existing
    // single-window configs.
    let lo_steps = product.strike_range_low.unwrap_or(product.quote_range_low);
    let hi_steps = product.strike_range_high.unwrap_or(product.quote_range_high);
    let lo = lo_steps as f64 * inc;
    let hi = hi_steps as f64 * inc;
    let mut out = Vec::new();
    let mut k = atm + lo;
    while k <= atm + hi + 1e-9 {
        out.push(round_to_increment(k, inc));
        k += inc;
    }
    out
}

/// Pick the FRONT-MONTH option expiry — the one we want to quote on.
/// HG copper options trade until the day BEFORE the underlying
/// future's last trade date — observed from IBKR position data:
///   HXEM6 (May options) lastTradeDate=20260526; HGK6 (May futures) =20260527
///   HXEN6 (Jun options) lastTradeDate=20260625; HGM6 (Jun futures) =20260626
///
/// Audit T2-4: previously fell back to the FIRST position's expiry,
/// which broke quoting if the only position was on back-month. Now
/// always derives from underlying.expiry; back-month positions are
/// covered separately via the position-leg union in subscribe_product.
///
/// Both observed cases (Wed→Tue, Fri→Thu) land on a weekday with -1
/// day; CME doesn't list HG options expiring on weekends. TODO:
/// proper fix is to reqContractDetails for the HXE option chain and
/// pick the first lastTradeDate past today.
fn pick_option_expiry(
    _runtime: &Arc<Runtime>,
    _product: &str,
    underlying: &Contract,
) -> NaiveDate {
    underlying.expiry - chrono::Duration::days(1)
}

async fn qualify_and_subscribe(
    runtime: &Arc<Runtime>,
    product: &ProductConfig,
    strike: f64,
    expiry: NaiveDate,
    right: Right,
) -> Result<(), Box<dyn std::error::Error>> {
    let q = OptionQuery {
        symbol: option_symbol_for(product),
        expiry,
        strike,
        right,
        exchange: Exchange::Comex,
        currency: Currency::Usd,
        multiplier: product.multiplier,
    };
    let qualified = {
        let b = runtime.broker.clone();
        b.qualify_option(q).await?
    };
    {
        let mut md = runtime.market_data.lock().unwrap();
        md.register_option(
            &product.name,
            strike,
            expiry,
            right,
            qualified.instrument_id,
        );
    }
    {
        // Cache full Contract for IPC place_order use (audit/wire fix:
        // place_order needs the IBKR-canonical local_symbol +
        // trading_class, not a synthesized placeholder).
        let mut qc = runtime.qualified_contracts.lock().unwrap();
        qc.insert(qualified.instrument_id, qualified.clone());
    }
    {
        let b = runtime.broker.clone();
        b.subscribe_ticks(TickSubscription {
            instrument_id: qualified.instrument_id,
            tick_by_tick: false,
            consumer_tag: Some(format!(
                "{} {} {} {:?}",
                product.name, strike, expiry, right
            )),
            contract: Some(qualified.clone()),
        })
        .await?;
    }
    Ok(())
}

/// HG copper options trade as "HXE" — option symbol differs from the
/// underlying future's "HG". Phase 5B.0: hardcode this mapping. Phase
/// 6 should drive it from product config.
fn option_symbol_for(product: &ProductConfig) -> String {
    match product.name.as_str() {
        "HG" => "HXE".into(),
        other => other.into(),
    }
}
