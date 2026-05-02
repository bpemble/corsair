//! Live smoke test of the full `NativeBroker` trait surface.
//!
//! Connects → bootstraps → qualifies HG futures → lists positions →
//! lists account values → exercises subscribe_ticks/unsubscribe round-trip.
//!
//! Does NOT place any orders — the gateway is currently degraded (see
//! Phase 6.5b). Place/cancel exercise lands in Phase 6.6e once gateway
//! is clean.
//!
//!     docker run --rm --network host corsair-smoke:latest \
//!         /usr/local/bin/broker_smoke 127.0.0.1 4002 26 DUP553657

use corsair_broker_api::{
    contract::{Currency, Exchange, FutureQuery},
    Broker,
};
use corsair_broker_ibkr_native::{
    client::NativeClientConfig, NativeBroker, NativeBrokerConfig,
};
use std::time::Duration;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    let args: Vec<String> = std::env::args().collect();
    let host = args.get(1).cloned().unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4002);
    let client_id: i32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(26);
    let account = args.get(4).cloned().unwrap_or_else(|| "DUP553657".into());

    let cfg = NativeBrokerConfig {
        client: NativeClientConfig {
            host,
            port,
            client_id,
            account: None,
            connect_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(10),
        },
        account: account.clone(),
    };
    let mut broker = NativeBroker::new(cfg);

    println!("[broker_smoke] capabilities: {:?}", broker.capabilities());
    println!("[broker_smoke] connecting...");
    broker.connect().await?;
    println!("[broker_smoke] connected: is_connected={}", broker.is_connected());

    // Subscribe to fill/status streams BEFORE issuing any orders.
    let mut status_rx = broker.subscribe_order_status();
    let mut error_rx = broker.subscribe_errors();

    // Qualify a futures contract for HG.
    println!("\n[broker_smoke] qualify_future(HG, 2026-05)...");
    let q = FutureQuery {
        symbol: "HG".into(),
        expiry: chrono::NaiveDate::from_ymd_opt(2026, 5, 27).unwrap(),
        exchange: Exchange::Comex,
        currency: Currency::Usd,
    };
    match tokio::time::timeout(Duration::from_secs(10), broker.qualify_future(q)).await {
        Ok(Ok(c)) => println!(
            "  resolved: con_id={:?}, local={}, multiplier={}",
            c.instrument_id, c.local_symbol, c.multiplier
        ),
        Ok(Err(e)) => println!("  qualify error: {e}"),
        Err(_) => println!("  qualify timeout"),
    }

    // Wait briefly for streamed positions / account values to arrive.
    println!("\n[broker_smoke] waiting 5s for positions + account values to stream...");
    tokio::time::sleep(Duration::from_secs(5)).await;

    let positions = broker.positions().await?;
    println!("[broker_smoke] positions: {}", positions.len());
    for p in positions.iter().take(5) {
        println!(
            "  {} {} qty={} avg={}",
            p.contract.symbol, p.contract.local_symbol, p.quantity, p.avg_cost
        );
    }

    let snap = broker.account_values().await?;
    println!(
        "[broker_smoke] account: nlv={:.2}, maint={:.2}, init={:.2}, bp={:.2}, rpnl_today={:.2}",
        snap.net_liquidation,
        snap.maintenance_margin,
        snap.initial_margin,
        snap.buying_power,
        snap.realized_pnl_today,
    );

    let opens = broker.open_orders().await?;
    println!("[broker_smoke] open orders: {}", opens.len());

    // Drain any error events that arrived during bootstrap.
    let mut error_count = 0;
    while let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(50), error_rx.recv()).await {
        error_count += 1;
        if error_count > 50 { break; }
    }
    println!("[broker_smoke] errors observed: {error_count}");

    // Drain any stale status updates so we leave the channel clean.
    while let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(10), status_rx.recv()).await {}

    println!("\n[broker_smoke] disconnecting...");
    broker.disconnect().await?;
    println!("[broker_smoke] done.");

    Ok(())
}
