//! Live smoke test: connect to IBKR Gateway, do the handshake,
//! print serverVersion + nextValidId, disconnect.
//!
//! Run inside the corsair container so localhost:4002 routes to
//! ib-gateway:
//!     docker compose run --rm corsair sh -c \
//!       "/usr/local/bin/connect_smoke 127.0.0.1 4002 11"
//!
//! Or build + run via cargo:
//!     cargo run --example connect_smoke --release -- 127.0.0.1 4002 11

use corsair_broker_ibkr_native::{NativeClient, NativeClientConfig};
use std::time::Duration;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    let args: Vec<String> = std::env::args().collect();
    let host = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = args
        .get(2)
        .map(|s| s.parse().unwrap_or(4002))
        .unwrap_or(4002);
    let client_id: i32 = args
        .get(3)
        .map(|s| s.parse().unwrap_or(11))
        .unwrap_or(11);

    let cfg = NativeClientConfig {
        host: host.clone(),
        port,
        client_id,
        account: None,
        connect_timeout: Duration::from_secs(10),
        handshake_timeout: Duration::from_secs(10),
    };
    let (client, mut rx) = NativeClient::new(cfg);

    println!("[smoke] connecting to {host}:{port} as clientId={client_id}");
    client.connect().await?;
    println!("[smoke] connected; server_version={}", client.server_version().await);

    // Wait up to 5s for nextValidId / managedAccounts to land.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        let timeout_ms = 250;
        match tokio::time::timeout(Duration::from_millis(timeout_ms), rx.recv()).await {
            Ok(Some(fields)) => {
                println!("[smoke] msg: {fields:?}");
            }
            Ok(None) => {
                println!("[smoke] channel closed");
                break;
            }
            Err(_) => {} // keep polling
        }
        let oid = client.next_order_id().await;
        let accts = client.managed_accounts().await;
        if oid > 0 && !accts.is_empty() {
            println!(
                "[smoke] state populated: nextValidId={oid}, managedAccounts={accts:?}"
            );
            break;
        }
    }

    client.disconnect().await?;
    println!(
        "[smoke] done. msgs sent={}, recv={}",
        client.msgs_sent(),
        client.msgs_recv()
    );
    Ok(())
}
