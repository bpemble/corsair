//! `SnapshotPublisher` — builds and writes the snapshot JSON.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use corsair_hedge::{HedgeFanout, HedgeMode};
use corsair_position::{PortfolioState, MarketView};
use corsair_risk::{KillSource, RiskMonitor};

use crate::payload::{
    AccountSnapshot, ChainExpirySnapshot, HedgeSnapshot, KillSnapshot,
    PortfolioPerProduct, PortfolioSnapshot, PositionSnapshot, Snapshot,
    SCHEMA_VERSION,
};

/// Pre-built chain data passed by the caller. Decouples the publisher
/// from the broker's market_data crate so this crate stays tree-light.
#[derive(Debug, Default, Clone)]
pub struct ChainBuild {
    pub chains: std::collections::HashMap<String, ChainExpirySnapshot>,
    pub atm_strike: f64,
    pub expiries: Vec<String>,
    pub front_month_expiry: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    /// Where the snapshot JSON lives. Streamlit polls this path.
    pub snapshot_path: PathBuf,
}

pub struct SnapshotPublisher {
    cfg: SnapshotConfig,
    last_write_count: u64,
}

impl SnapshotPublisher {
    pub fn new(cfg: SnapshotConfig) -> Self {
        Self {
            cfg,
            last_write_count: 0,
        }
    }

    pub fn write_count(&self) -> u64 {
        self.last_write_count
    }

    pub fn snapshot_path(&self) -> &Path {
        &self.cfg.snapshot_path
    }

    /// Build a `Snapshot` from the current state of every component.
    pub fn build(
        &self,
        portfolio: &PortfolioState,
        risk: &RiskMonitor,
        hedge: &HedgeFanout,
        market: &dyn MarketView,
        account: AccountSnapshot,
        chain_build: ChainBuild,
        latency: Option<crate::payload::LatencySnapshot>,
    ) -> Snapshot {
        let agg = portfolio.aggregate();

        let mut per_product = HashMap::new();
        for (prod, g) in &agg.per_product {
            per_product.insert(
                prod.clone(),
                PortfolioPerProduct {
                    net_delta: g.net_delta,
                    net_theta: g.net_theta,
                    net_vega: g.net_vega,
                    long_count: g.long_count,
                    short_count: g.short_count,
                },
            );
        }

        let positions: Vec<PositionSnapshot> = portfolio
            .positions()
            .iter()
            .map(|p| PositionSnapshot {
                product: p.product.clone(),
                strike: p.strike,
                expiry: p.expiry,
                right: p.right,
                quantity: p.quantity,
                avg_fill_price: p.avg_fill_price,
                current_price: p.current_price,
                delta: p.delta,
                gamma: p.gamma,
                theta: p.theta,
                vega: p.vega,
                multiplier: p.multiplier,
            })
            .collect();

        let mtm_pnl = portfolio.compute_mtm_pnl(market);
        let daily_pnl = portfolio.realized_pnl_persisted + mtm_pnl;

        // MED-002: surface options_delta / hedge_delta / effective_delta
        // separately so the dashboard's net-delta tile can show the
        // hedge-masking breakdown per CLAUDE.md §14.
        let options_delta = agg.total.net_delta;
        let hedge_delta: i64 = hedge.managers().iter().map(|m| m.hedge_qty() as i64).sum();
        let effective_delta = options_delta + (hedge_delta as f64);
        let portfolio_s = PortfolioSnapshot {
            net_delta: agg.total.net_delta,
            net_theta: agg.total.net_theta,
            net_vega: agg.total.net_vega,
            net_gamma: agg.total.net_gamma,
            options_delta,
            hedge_delta,
            effective_delta,
            long_count: agg.total.long_count,
            short_count: agg.total.short_count,
            gross_positions: agg.total.gross_positions,
            fills_today: portfolio.fills_today,
            realized_pnl_persisted: portfolio.realized_pnl_persisted,
            mtm_pnl,
            daily_pnl,
            spread_capture_today: portfolio.spread_capture_today,
            session_open_nlv: portfolio.session_open_nlv,
            positions,
            per_product,
        };

        let kill = match risk.kill_event() {
            None => KillSnapshot {
                killed: false,
                source: None,
                kill_type: None,
                reason: None,
            },
            Some(ev) => KillSnapshot {
                killed: true,
                source: Some(ev.source.label()),
                kill_type: Some(format!("{:?}", ev.kill_type).to_lowercase()),
                reason: Some(ev.reason.clone()),
            },
        };

        let hedges: Vec<HedgeSnapshot> = hedge
            .managers()
            .iter()
            .map(|m| {
                let cfg = m.config();
                let f = market.underlying_price(&cfg.product).unwrap_or(0.0);
                let (expiry, forward) = match m.hedge_contract() {
                    Some(c) => (c.expiry.format("%Y%m%d").to_string(), f),
                    None => (String::new(), f),
                };
                HedgeSnapshot {
                    product: cfg.product.clone(),
                    hedge_qty: m.hedge_qty(),
                    avg_entry_f: m.state().avg_entry_f,
                    realized_pnl_usd: m.state().realized_pnl_usd,
                    mtm_usd: m.mtm_usd(f),
                    mode: match cfg.mode {
                        HedgeMode::Observe => "observe".into(),
                        HedgeMode::Execute => "execute".into(),
                    },
                    expiry,
                    forward,
                }
            })
            .collect();

        let mut underlying = HashMap::new();
        for prod in portfolio.registry().products() {
            if let Some(p) = market.underlying_price(&prod) {
                underlying.insert(prod, p);
            }
        }

        let _ = KillSource::Risk; // suppress unused warning
        Snapshot {
            schema_version: SCHEMA_VERSION,
            timestamp_ns: now_ns(),
            portfolio: portfolio_s,
            kill,
            hedges,
            account,
            underlying,
            chains: chain_build.chains,
            atm_strike: chain_build.atm_strike,
            expiries: chain_build.expiries,
            front_month_expiry: chain_build.front_month_expiry,
            latency,
        }
    }

    /// Atomically write the snapshot to disk: write to a tempfile in
    /// the same directory, then rename. Streamlit's read either sees
    /// the previous version or the new one — never a partially-
    /// written file.
    pub fn publish(
        &mut self,
        portfolio: &PortfolioState,
        risk: &RiskMonitor,
        hedge: &HedgeFanout,
        market: &dyn MarketView,
        account: AccountSnapshot,
        chain_build: ChainBuild,
        latency: Option<crate::payload::LatencySnapshot>,
    ) -> std::io::Result<()> {
        let snap = self.build(portfolio, risk, hedge, market, account, chain_build, latency);
        let json = serde_json::to_vec_pretty(&snap)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        atomic_write(&self.cfg.snapshot_path, &json)?;
        self.last_write_count += 1;
        Ok(())
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent"))?;
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("snapshot.json")
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use corsair_hedge::{HedgeConfig, HedgeManager};
    use corsair_position::{NoOpMarketView, ProductInfo, ProductRegistry};
    use corsair_risk::RiskConfig;
    use tempfile::TempDir;

    fn fanout_empty() -> HedgeFanout {
        HedgeFanout::new(vec![])
    }

    fn portfolio_empty() -> PortfolioState {
        let mut r = ProductRegistry::new();
        r.register(ProductInfo {
            product: "HG".into(),
            multiplier: 25_000.0,
            default_iv: 0.30,
        });
        PortfolioState::new(r)
    }

    fn risk_healthy() -> RiskMonitor {
        RiskMonitor::new(RiskConfig {
            capital: 200_000.0,
            margin_kill_pct: 0.70,
            daily_halt_threshold: Some(-2_500.0),
            delta_kill: 5.0,
            vega_kill: 1_000.0,
            theta_kill: -200.0,
            margin_ceiling_pct: 0.50,
            effective_delta_gating: true,
        })
    }

    #[test]
    fn build_produces_well_formed_snapshot() {
        let p = portfolio_empty();
        let r = risk_healthy();
        let h = fanout_empty();
        let pub_ = SnapshotPublisher::new(SnapshotConfig {
            snapshot_path: "/tmp/_snap_test.json".into(),
        });
        let snap = pub_.build(&p, &r, &h, &NoOpMarketView, AccountSnapshot::default(), ChainBuild::default(), None);
        assert_eq!(snap.schema_version, SCHEMA_VERSION);
        assert!(!snap.kill.killed);
        assert_eq!(snap.portfolio.gross_positions, 0);
    }

    #[test]
    fn publish_writes_file_atomically() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("snap.json");
        let mut pub_ = SnapshotPublisher::new(SnapshotConfig {
            snapshot_path: path.clone(),
        });
        let p = portfolio_empty();
        let r = risk_healthy();
        let h = fanout_empty();
        pub_.publish(&p, &r, &h, &NoOpMarketView, AccountSnapshot::default(), ChainBuild::default(), None)
            .expect("publish");
        assert!(path.exists());
        let bytes = std::fs::read(&path).unwrap();
        let parsed: Snapshot = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.schema_version, SCHEMA_VERSION);
        assert_eq!(pub_.write_count(), 1);
    }

    #[test]
    fn publish_overwrites_on_repeat() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("snap.json");
        let mut pub_ = SnapshotPublisher::new(SnapshotConfig {
            snapshot_path: path.clone(),
        });
        let p = portfolio_empty();
        let r = risk_healthy();
        let h = fanout_empty();
        for _ in 0..3 {
            pub_.publish(&p, &r, &h, &NoOpMarketView, AccountSnapshot::default(), ChainBuild::default(), None)
                .expect("publish");
        }
        assert_eq!(pub_.write_count(), 3);
    }

    #[test]
    fn killed_snapshot_includes_kill_fields() {
        let p = portfolio_empty();
        let mut r = risk_healthy();
        // Force a kill via check_daily_pnl_only.
        let mut p_with_pnl = portfolio_empty();
        p_with_pnl.realized_pnl_persisted = -3_000.0;
        let _ = r.check_daily_pnl_only(&p_with_pnl, &NoOpMarketView);
        assert!(r.is_killed());
        let h = fanout_empty();
        let pub_ = SnapshotPublisher::new(SnapshotConfig {
            snapshot_path: "/tmp/_killed_test.json".into(),
        });
        let snap = pub_.build(&p, &r, &h, &NoOpMarketView, AccountSnapshot::default(), ChainBuild::default(), None);
        assert!(snap.kill.killed);
        assert_eq!(snap.kill.source.as_deref(), Some("daily_halt"));
    }

    #[test]
    fn hedge_managers_appear_in_snapshot() {
        let p = portfolio_empty();
        let r = risk_healthy();
        let m = HedgeManager::new(HedgeConfig {
            product: "HG".into(),
            multiplier: 25_000.0,
            mode: HedgeMode::Execute,
            tolerance_deltas: 0.5,
            rebalance_on_fill: true,
            rebalance_cadence_sec: 30.0,
            include_in_daily_pnl: true,
            flatten_on_halt_enabled: true,
            ioc_tick_offset: 2,
            hedge_tick_size: 0.0005,
            lockout_days: 30,
        });
        let h = HedgeFanout::new(vec![m]);
        let pub_ = SnapshotPublisher::new(SnapshotConfig {
            snapshot_path: "/tmp/_hedge_snap_test.json".into(),
        });
        let snap = pub_.build(&p, &r, &h, &NoOpMarketView, AccountSnapshot::default(), ChainBuild::default(), None);
        assert_eq!(snap.hedges.len(), 1);
        assert_eq!(snap.hedges[0].product, "HG");
        assert_eq!(snap.hedges[0].mode, "execute");
    }
}
