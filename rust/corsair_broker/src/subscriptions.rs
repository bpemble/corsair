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
        let b = runtime.broker.lock().await;
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
        let b = runtime.broker.lock().await;
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
    let lo = product.quote_range_low as f64 * inc;
    let hi = product.quote_range_high as f64 * inc;
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
        let b = runtime.broker.lock().await;
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
        let b = runtime.broker.lock().await;
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
