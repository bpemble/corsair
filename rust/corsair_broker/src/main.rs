//! corsair_broker daemon entrypoint.

use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use corsair_broker::config::BrokerDaemonConfig;
use corsair_broker::runtime::{Runtime, RuntimeMode};
use corsair_broker::tasks::spawn_all;

fn init_logger(level: &str) {
    let env_filter = env::var("RUST_LOG").unwrap_or_else(|_| level.into());
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(env_filter))
        .format_timestamp_micros()
        .init();
}

fn config_path() -> PathBuf {
    env::var("CORSAIR_BROKER_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/app/config/runtime.yaml"))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg_path = config_path();
    let cfg = BrokerDaemonConfig::load(&cfg_path)
        .map_err(|e| format!("config load {}: {}", cfg_path.display(), e))?;

    init_logger(&cfg.runtime.log_level);

    log::warn!(
        "corsair_broker {} starting (config={})",
        env!("CARGO_PKG_VERSION"),
        cfg_path.display()
    );

    let mode = RuntimeMode::from_env();
    log::warn!("runtime mode: {:?}", mode);
    if matches!(mode, RuntimeMode::Shadow) {
        log::warn!(
            "SHADOW MODE: state ingestion only. No orders will be placed. \
             Set CORSAIR_BROKER_SHADOW=0 to enable live operation."
        );
    }

    let runtime: Arc<Runtime> = Runtime::new(cfg, mode).await?;
    let _handles = spawn_all(runtime.clone());

    // Wait for SIGINT / SIGTERM.
    let mut sigint = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::interrupt(),
    )?;
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    )?;
    tokio::select! {
        _ = sigint.recv() => log::warn!("SIGINT received"),
        _ = sigterm.recv() => log::warn!("SIGTERM received"),
    }

    runtime.shutdown().await?;
    Ok(())
}
