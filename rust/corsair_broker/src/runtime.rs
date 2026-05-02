//! `Runtime` — the central state hub for the broker daemon.
//!
//! Owns one instance of every Phase 2-3 component. Tasks (in
//! `tasks.rs`) hold `Arc<Runtime>` and acquire the relevant
//! `Mutex<...>` to read or mutate state.

use corsair_broker_api::Broker;
use corsair_broker_ibkr::{BridgeConfig, IbkrAdapter};
use corsair_constraint::{ConstraintChecker, ConstraintConfig};
use corsair_hedge::{HedgeConfig, HedgeFanout, HedgeManager, HedgeMode};
use corsair_market_data::MarketDataState;
use corsair_oms::{OrderBook, SendOrUpdateConfig};
use corsair_position::{PortfolioState, ProductInfo, ProductRegistry};
use corsair_risk::{RiskConfig, RiskMonitor};
use corsair_snapshot::{SnapshotConfig, SnapshotPublisher};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Async-friendly Mutex for the broker (held across .await).
pub type AsyncMutex<T> = tokio::sync::Mutex<T>;

use crate::config::BrokerDaemonConfig;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("broker bridge error: {0}")]
    Bridge(#[from] corsair_broker_ibkr::BridgeError),
    #[error("broker error: {0}")]
    Broker(#[from] corsair_broker_api::BrokerError),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("internal: {0}")]
    Internal(String),
}

/// Shadow vs live mode. In shadow we don't place orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMode {
    Shadow,
    Live,
}

impl RuntimeMode {
    pub fn from_env() -> Self {
        match std::env::var("CORSAIR_BROKER_SHADOW")
            .unwrap_or_default()
            .as_str()
        {
            "0" | "false" | "FALSE" => Self::Live,
            // Default during Phase 4 development is shadow.
            _ => Self::Shadow,
        }
    }
}

/// Central state. Each component is wrapped in a `Mutex` so tokio
/// tasks can acquire just the slice they need.
///
/// `market_data` is `Rc<RefCell<...>>` because [`MarketDataView`] uses
/// the single-threaded interior-mutability pattern (it implements
/// the `MarketView` trait via `&self`). The runtime ensures all
/// access to `market_data` happens on a single tokio task at a time
/// by routing through [`tasks::pump_ticks`] — this is enforced by
/// having `MarketDataView` be `!Send`.
///
/// Wait — that's not quite right for tokio's multi-thread runtime
/// where tasks can move across threads. We use `Mutex<MarketDataState>`
/// instead and construct `MarketDataView` on-demand from a clone of
/// the Arc when read access is needed. The clone is cheap (Arc
/// bump). See [`with_market_view`].
pub struct Runtime {
    pub mode: RuntimeMode,
    pub config: BrokerDaemonConfig,

    /// Boxed broker so we can swap IBKR ↔ iLink at construction.
    /// Tokio mutex because async methods are called across .await.
    pub broker: AsyncMutex<Box<dyn Broker>>,
    pub portfolio: Mutex<PortfolioState>,
    pub risk: Mutex<RiskMonitor>,
    pub constraint: Mutex<ConstraintChecker>,
    pub hedge: Mutex<HedgeFanout>,
    pub oms: Mutex<OrderBook>,
    pub market_data: Mutex<MarketDataState>,
    pub snapshot: Mutex<SnapshotPublisher>,

    /// Send-or-update config snapshotted at construction.
    pub send_or_update_cfg: SendOrUpdateConfig,
}

impl Runtime {
    /// Construct + boot the runtime. Connects to the broker,
    /// qualifies contracts, seeds positions. Caller spawns the
    /// task loops separately.
    pub async fn new(
        cfg: BrokerDaemonConfig,
        mode: RuntimeMode,
    ) -> Result<Arc<Self>, RuntimeError> {
        log::warn!(
            "corsair_broker daemon starting in {:?} mode",
            mode
        );

        // ── Construct the broker adapter ──────────────────────────
        let broker = build_broker(&cfg)?;

        // ── Construct stateful crates ─────────────────────────────
        let mut registry = ProductRegistry::new();
        for p in &cfg.products {
            if !p.enabled {
                continue;
            }
            registry.register(ProductInfo {
                product: p.name.clone(),
                multiplier: p.multiplier,
                default_iv: p.default_iv,
            });
        }
        let portfolio = PortfolioState::new(registry);

        let risk_cfg = RiskConfig {
            capital: cfg.constraints.capital,
            margin_kill_pct: cfg.risk.margin_kill_pct,
            daily_halt_threshold: cfg.resolve_daily_halt_threshold(),
            delta_kill: cfg.risk.delta_kill,
            vega_kill: cfg.risk.vega_kill,
            theta_kill: cfg.risk.theta_kill,
            margin_ceiling_pct: cfg.constraints.margin_ceiling_pct,
            effective_delta_gating: cfg.constraints.effective_delta_gating,
        };
        let risk = RiskMonitor::new(risk_cfg);

        // Constraint checker: Phase 4 wires one per ENABLED product.
        // Multi-product scope drives one checker per product
        // ("delta_for_product" is per-product). For now we pick the
        // first enabled product as the primary; further products would
        // need a fanout layer (Phase 4.x follow-up).
        let primary_product = cfg
            .products
            .iter()
            .find(|p| p.enabled)
            .ok_or_else(|| RuntimeError::Internal("no enabled product".into()))?;
        let constraint_cfg = ConstraintConfig {
            product: primary_product.name.clone(),
            capital: cfg.constraints.capital,
            margin_ceiling_pct: cfg.constraints.margin_ceiling_pct,
            delta_ceiling: cfg.constraints.delta_ceiling,
            theta_floor: cfg.constraints.theta_floor,
            margin_kill_pct: cfg.risk.margin_kill_pct,
            delta_kill: cfg.risk.delta_kill,
            theta_kill: cfg.risk.theta_kill,
            effective_delta_gating: cfg.constraints.effective_delta_gating,
            margin_escape_enabled: cfg.constraints.margin_escape_enabled,
        };
        let constraint = ConstraintChecker::new(constraint_cfg);

        // Hedge fanout: one HedgeManager per enabled product if hedging
        // is enabled.
        let hedge = build_hedge_fanout(&cfg);

        let oms = OrderBook::new();
        let market_data = MarketDataState::new();

        let snapshot_cfg = SnapshotConfig {
            snapshot_path: cfg.snapshot.path.clone().into(),
        };
        let snapshot = SnapshotPublisher::new(snapshot_cfg);

        let send_or_update_cfg = SendOrUpdateConfig {
            tick_size: cfg.quoting.tick_size,
            dead_band_ticks: cfg.quoting.dead_band_ticks,
            gtd_lifetime_s: cfg.quoting.gtd_lifetime_s,
            gtd_refresh_lead_s: cfg.quoting.gtd_refresh_lead_s,
            min_send_interval_ms: cfg.quoting.min_send_interval_ms,
        };

        let runtime = Arc::new(Self {
            mode,
            config: cfg,
            broker: AsyncMutex::new(broker),
            portfolio: Mutex::new(portfolio),
            risk: Mutex::new(risk),
            constraint: Mutex::new(constraint),
            hedge: Mutex::new(hedge),
            oms: Mutex::new(oms),
            market_data: Mutex::new(market_data),
            snapshot: Mutex::new(snapshot),
            send_or_update_cfg,
        });

        // ── Connect ───────────────────────────────────────────────
        runtime.connect().await?;

        // ── Seed positions from broker ────────────────────────────
        runtime.seed_positions_from_broker().await?;

        log::warn!("corsair_broker boot complete; tasks will start next");
        Ok(runtime)
    }

    async fn connect(self: &Arc<Self>) -> Result<(), RuntimeError> {
        let mut b = self.broker.lock().await;
        b.connect().await?;
        Ok(())
    }

    /// Read positions from the broker, seed PortfolioState. Mirrors
    /// `seed_from_ibkr` in Python.
    async fn seed_positions_from_broker(self: &Arc<Self>) -> Result<(), RuntimeError> {
        let positions = {
            let b = self.broker.lock().await;
            b.positions().await?
        };

        let mut to_insert = Vec::new();
        let registry_products: Vec<String> = {
            let p = self.portfolio.lock().unwrap();
            p.registry().products()
        };
        for pos in positions {
            // Translate corsair_broker_api::Position into
            // corsair_position::Position.
            if !registry_products.contains(&pos.contract.symbol) {
                log::debug!(
                    "seed: skipping unregistered product {}",
                    pos.contract.symbol
                );
                continue;
            }
            // For options we need strike + right; futures hedge state
            // lives in HedgeManager, not PortfolioState.
            if pos.contract.kind != corsair_broker_api::ContractKind::Option {
                continue;
            }
            let right = match pos.contract.right {
                Some(r) => r,
                None => continue,
            };
            let strike = match pos.contract.strike {
                Some(s) => s,
                None => continue,
            };
            to_insert.push(corsair_position::Position {
                product: pos.contract.symbol.clone(),
                strike,
                expiry: pos.contract.expiry,
                right,
                quantity: pos.quantity,
                avg_fill_price: pos.avg_cost / pos.contract.multiplier,
                fill_time: chrono::Utc::now(),
                multiplier: pos.contract.multiplier,
                delta: 0.0,
                gamma: 0.0,
                theta: 0.0,
                vega: 0.0,
                current_price: 0.0,
            });
        }
        let count = to_insert.len();
        {
            let mut p = self.portfolio.lock().unwrap();
            p.replace_positions(to_insert);
        }
        log::warn!(
            "corsair_broker seeded {} positions from broker",
            count
        );
        Ok(())
    }

    /// Run a closure with a `&dyn MarketView` borrowed from
    /// MarketDataState. The closure runs while the mutex is held;
    /// keep it short.
    pub fn with_market_view<R>(
        &self,
        f: impl FnOnce(&dyn corsair_position::MarketView) -> R,
    ) -> R {
        let s = self.market_data.lock().unwrap();
        f(&*s)
    }

    /// Disconnect cleanly. Called from the shutdown handler.
    pub async fn shutdown(self: &Arc<Self>) -> Result<(), RuntimeError> {
        log::warn!("corsair_broker daemon shutting down");
        let mut b = self.broker.lock().await;
        b.disconnect().await?;
        Ok(())
    }
}

fn build_broker(cfg: &BrokerDaemonConfig) -> Result<Box<dyn Broker>, RuntimeError> {
    match cfg.broker.kind.as_str() {
        "ibkr" => {
            let ibkr = cfg
                .broker
                .ibkr
                .as_ref()
                .ok_or_else(|| RuntimeError::Internal("missing broker.ibkr".into()))?;
            let bridge_cfg = BridgeConfig {
                gateway_host: ibkr.gateway.host.clone(),
                gateway_port: ibkr.gateway.port,
                client_id: ibkr.client_id,
                account: ibkr.account.clone(),
                poll_interval_ms: 1,
            };
            // Initialize embedded Python before constructing the bridge.
            corsair_broker_ibkr::init_python();
            let adapter = IbkrAdapter::new(bridge_cfg)?;
            Ok(Box::new(adapter))
        }
        "ilink" => Err(RuntimeError::Internal(
            "ilink adapter not implemented (Phase 7)".into(),
        )),
        other => Err(RuntimeError::Internal(format!(
            "unknown broker.kind: {other}"
        ))),
    }
}

fn build_hedge_fanout(cfg: &BrokerDaemonConfig) -> HedgeFanout {
    if !cfg.hedging.enabled {
        return HedgeFanout::new(vec![]);
    }
    let mode = match cfg.hedging.mode.as_str() {
        "execute" => HedgeMode::Execute,
        _ => HedgeMode::Observe,
    };
    let mut managers = Vec::new();
    for p in &cfg.products {
        if !p.enabled {
            continue;
        }
        managers.push(HedgeManager::new(HedgeConfig {
            product: p.name.clone(),
            multiplier: p.multiplier,
            mode,
            tolerance_deltas: cfg.hedging.tolerance_deltas,
            rebalance_on_fill: cfg.hedging.rebalance_on_fill,
            rebalance_cadence_sec: cfg.hedging.rebalance_cadence_sec,
            include_in_daily_pnl: cfg.hedging.include_in_daily_pnl,
            flatten_on_halt_enabled: cfg.hedging.flatten_on_halt,
            ioc_tick_offset: cfg.hedging.ioc_tick_offset,
            hedge_tick_size: cfg.hedging.hedge_tick_size,
            lockout_days: cfg.hedging.hedge_lockout_days,
        }));
    }
    HedgeFanout::new(managers)
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn sample_config() -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(
            br#"
broker:
  kind: ibkr
  ibkr:
    gateway:
      host: 127.0.0.1
      port: 4002
    client_id: 0
    account: TEST

risk:
  daily_pnl_halt_pct: 0.05
  margin_kill_pct: 0.70
  delta_kill: 5.0
  vega_kill: 0
  theta_kill: -500

constraints:
  capital: 200000
  margin_ceiling_pct: 0.50
  delta_ceiling: 3.0
  theta_floor: -200

products:
  - name: HG
    multiplier: 25000
    quote_range_low: -5
    quote_range_high: 5
    strike_increment: 0.05
    enabled: true

quoting:
  tick_size: 0.0005
  min_edge_ticks: 2

hedging:
  enabled: true
  mode: observe
  tolerance_deltas: 0.5
  rebalance_cadence_sec: 30
  hedge_lockout_days: 30
"#,
        )
        .unwrap();
        f
    }

    #[test]
    fn config_loads() {
        let f = sample_config();
        let cfg = BrokerDaemonConfig::load(f.path()).unwrap();
        assert_eq!(cfg.products[0].name, "HG");
    }

    #[test]
    fn shadow_mode_default_when_unset() {
        std::env::remove_var("CORSAIR_BROKER_SHADOW");
        assert_eq!(RuntimeMode::from_env(), RuntimeMode::Shadow);
    }

    #[test]
    fn live_mode_when_zero() {
        std::env::set_var("CORSAIR_BROKER_SHADOW", "0");
        assert_eq!(RuntimeMode::from_env(), RuntimeMode::Live);
        std::env::remove_var("CORSAIR_BROKER_SHADOW");
    }

    #[test]
    fn build_hedge_fanout_observes_disabled() {
        let f = sample_config();
        let mut cfg = BrokerDaemonConfig::load(f.path()).unwrap();
        cfg.hedging.enabled = false;
        let fanout = build_hedge_fanout(&cfg);
        assert!(fanout.managers().is_empty());
    }

    #[test]
    fn build_hedge_fanout_observe_mode() {
        let f = sample_config();
        let cfg = BrokerDaemonConfig::load(f.path()).unwrap();
        let fanout = build_hedge_fanout(&cfg);
        assert_eq!(fanout.managers().len(), 1);
    }
}
