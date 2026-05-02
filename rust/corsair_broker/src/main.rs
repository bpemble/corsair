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

    // IPC server — gated by env so we don't accidentally clobber the
    // Python broker's IPC files during shadow validation.
    if std::env::var("CORSAIR_BROKER_IPC_ENABLED")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        let ipc_path = std::env::var("CORSAIR_BROKER_IPC_PATH")
            .unwrap_or_else(|_| "/app/data/corsair_ipc".into());
        let cfg = corsair_broker::IpcConfig {
            base_path: std::path::PathBuf::from(ipc_path),
            capacity: 1 << 20,
        };
        match corsair_broker::spawn_ipc(runtime.clone(), cfg) {
            Ok(server) => {
                log::warn!("ipc server enabled");
                corsair_broker::spawn_vol_surface(runtime.clone(), server);
                log::warn!("vol_surface fitter enabled");
            }
            Err(e) => log::error!("ipc server start failed: {e}"),
        }
    } else {
        log::info!(
            "ipc server NOT enabled — set CORSAIR_BROKER_IPC_ENABLED=1 to host trader IPC"
        );
    }

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
