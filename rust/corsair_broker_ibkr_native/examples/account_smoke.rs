//! Live smoke test: connect, request accountUpdates + positions,
//! print every parsed inbound message for ~10s.
//!
//! Run inside docker network:
//!     cargo run --release --example account_smoke -p corsair_broker_ibkr_native -- 127.0.0.1 4002 12

use corsair_broker_ibkr_native::{
    parse_inbound, req_account_updates, req_positions, InboundMsg, NativeClient,
    NativeClientConfig,
};
use std::time::Duration;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    let args: Vec<String> = std::env::args().collect();
    let host = args.get(1).cloned().unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4002);
    let client_id: i32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(12);
    let account = args.get(4).cloned().unwrap_or_else(|| "DUP553657".into());

    let cfg = NativeClientConfig {
        host: host.clone(),
        port,
        client_id,
        account: Some(account.clone()),
        connect_timeout: Duration::from_secs(10),
        handshake_timeout: Duration::from_secs(10),
    };
    let (client, mut rx) = NativeClient::new(cfg);

    println!("[smoke] connecting to {host}:{port} as clientId={client_id}");
    client.connect().await?;
    println!(
        "[smoke] connected; server_version={}",
        client.server_version().await
    );

    // Issue lean bypass requests.
    client.send_raw(&req_account_updates(true, &account)).await?;
    client.send_raw(&req_positions()).await?;
    println!("[smoke] sent reqAccountUpdates + reqPositions");

    // Drain inbound messages for ~10s and print parsed types.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut counts: std::collections::HashMap<&'static str, u32> = Default::default();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(fields)) => {
                let parsed = parse_inbound(&fields);
                match parsed {
                    Ok(InboundMsg::Position(p)) => {
                        *counts.entry("Position").or_default() += 1;
                        if counts.get("Position").unwrap() <= &3 {
                            println!(
                                "  Position: {} {} {} qty={} avg={}",
                                p.account,
                                p.contract.symbol,
                                p.contract.local_symbol,
                                p.position,
                                p.avg_cost
                            );
                        }
                    }
                    Ok(InboundMsg::AccountValue(a)) => {
                        *counts.entry("AccountValue").or_default() += 1;
                        if matches!(
                            a.key.as_str(),
                            "NetLiquidation"
                                | "MaintMarginReq"
                                | "InitMarginReq"
                                | "BuyingPower"
                                | "RealizedPnL"
                        ) {
                            println!("  AccountValue: {} = {} {}", a.key, a.value, a.currency);
                        }
                    }
                    Ok(InboundMsg::PositionEnd) => {
                        println!("  PositionEnd");
                        *counts.entry("PositionEnd").or_default() += 1;
                    }
                    Ok(InboundMsg::AccountDownloadEnd(a)) => {
                        println!("  AccountDownloadEnd: {a}");
                        *counts.entry("AccountDownloadEnd").or_default() += 1;
                    }
                    Ok(InboundMsg::Error(e)) => {
                        println!(
                            "  Error: code={} reqId={} msg={}",
                            e.error_code, e.req_id, e.error_string
                        );
                        *counts.entry("Error").or_default() += 1;
                    }
                    Ok(InboundMsg::ManagedAccounts(_)) => {
                        *counts.entry("ManagedAccounts").or_default() += 1;
                    }
                    Ok(InboundMsg::NextValidId(_)) => {
                        *counts.entry("NextValidId").or_default() += 1;
                    }
                    Ok(InboundMsg::Unparsed { type_id, .. }) => {
                        let key = Box::leak(format!("Unparsed_{type_id}").into_boxed_str());
                        *counts.entry(key).or_default() += 1;
                    }
                    Ok(other) => {
                        let key = Box::leak(format!("{other:?}").split('(').next().unwrap_or("?").to_string().into_boxed_str());
                        *counts.entry(key).or_default() += 1;
                    }
                    Err(e) => println!("  parse error: {e}"),
                }
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    client.disconnect().await?;
    println!();
    println!("[smoke] message counts:");
    let mut sorted: Vec<_> = counts.iter().collect();
    sorted.sort();
    for (k, v) in sorted {
        println!("  {k}: {v}");
    }
    println!(
        "[smoke] sent={}, recv={}",
        client.msgs_sent(),
        client.msgs_recv()
    );
    Ok(())
}
