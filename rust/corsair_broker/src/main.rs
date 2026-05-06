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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Init logging first so build_pinner's startup log lands.
    // (RUST_LOG dominates; runtime config-driven log level is
    // applied in async_main once the config is loaded.)
    init_logger("info");

    // Resolve the Discord webhook URL once at boot. notify_kill is
    // safe to call before this (it'll return None) but the log line
    // confirms whether notifications are wired.
    corsair_broker::notify::init_from_env();

    // Build a multi-thread tokio runtime with worker threads pinned
    // to specific CPUs. Pinning is honored within the container's
    // cpuset (sched_getaffinity), so docker compose `cpuset:` carries
    // through automatically. Default 4 workers; clamps to allowed
    // CPU count on smaller hosts (4-core dev VPS gets 4 workers,
    // 12-core bare-metal where broker has cpuset 2,3,8,9 also gets 4).
    // CORSAIR_BROKER_WORKERS env var overrides if set.
    let desired = std::env::var("CORSAIR_BROKER_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(4);
    let (workers, _pins, pinner) =
        corsair_ipc::cpu_affinity::build_pinner("corsair_broker", desired);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .on_thread_start(pinner)
        .build()?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg_path = config_path();
    let cfg = BrokerDaemonConfig::load(&cfg_path)
        .map_err(|e| format!("config load {}: {}", cfg_path.display(), e))?;

    // Logger already initialized in main() before runtime build so
    // build_pinner's WARN logs appear; config-driven log level is
    // a future enhancement (would need log::set_max_level dynamic).

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
             Unset CORSAIR_BROKER_SHADOW (or set to 0) to enable live operation."
        );
    }

    let runtime: Arc<Runtime> = Runtime::new(cfg, mode).await?;
    let _handles = spawn_all(runtime.clone());

    // Subscribe to market data so the broker has a live view (greeks,
    // vol surface, MTM all depend on it).
    if let Err(e) = corsair_broker::subscribe_market_data(&runtime).await {
        log::error!("market data subscription failed: {e}");
    }

    // IPC server — gated by env so the broker daemon can be spun up
    // for diagnostics without exposing IPC rings.
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
