//! Runtime config loader. Reads YAML at a configurable path.
//!
//! Schema is the v3 boundary contract per spec §4: research tools
//! write the YAML, the Rust runtime reads it. No hot reload —
//! restart is the unit of change.

use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

/// Top-level config. Maps to `config/runtime.yaml` in the spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerDaemonConfig {
    pub broker: BrokerSection,
    pub risk: RiskSection,
    pub constraints: ConstraintsSection,
    pub products: Vec<ProductConfig>,
    pub quoting: QuotingSection,
    pub hedging: HedgingSection,
    #[serde(default)]
    pub snapshot: SnapshotSection,
    #[serde(default)]
    pub runtime: RuntimeSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerSection {
    /// "ibkr" today; "ilink" later.
    pub kind: String,
    #[serde(default)]
    pub ibkr: Option<IbkrSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IbkrSection {
    pub gateway: GatewaySection,
    #[serde(default)]
    pub client_id: i32,
    pub account: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewaySection {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskSection {
    /// Preferred over `max_daily_loss`. e.g. 0.05 → halt at -5% of capital.
    #[serde(default)]
    pub daily_pnl_halt_pct: Option<f64>,
    /// Legacy absolute (negative). Used when daily_pnl_halt_pct is None.
    #[serde(default)]
    pub max_daily_loss: Option<f64>,
    pub margin_kill_pct: f64,
    pub delta_kill: f64,
    /// 0 disables (current operational state).
    #[serde(default)]
    pub vega_kill: f64,
    /// Negative; 0 disables.
    #[serde(default)]
    pub theta_kill: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstraintsSection {
    pub capital: f64,
    pub margin_ceiling_pct: f64,
    pub delta_ceiling: f64,
    pub theta_floor: f64,
    #[serde(default = "default_true")]
    pub effective_delta_gating: bool,
    #[serde(default)]
    pub margin_escape_enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductConfig {
    pub name: String,
    pub multiplier: f64,
    /// Default IV when MarketView has no value. 0.30 typical.
    #[serde(default = "default_iv")]
    pub default_iv: f64,
    /// **Deprecated for quoting** (CLAUDE.md §12). The trader's quoting
    /// window is now hardcoded in `corsair_trader/src/decision.rs`
    /// (`MAX_STRIKE_OFFSET_USD` + OTM-only branches), not driven from
    /// YAML. Retained ONLY as the fallback default for
    /// `strike_range_low/high` — i.e., if `strike_range_*` is unset
    /// the market-data subscription window falls back to these.
    /// Older configs that set only `quote_range_*` continue working;
    /// new configs should use `strike_range_*` directly.
    pub quote_range_low: i32,
    pub quote_range_high: i32,
    /// Strike SUBSCRIPTION range, low/high in increments. Wider than
    /// the trader's quoting window so SABR has wing data for stable
    /// fits — CLAUDE.md §12. Defaults to `quote_range_*` when unset
    /// (back-compat with older configs).
    #[serde(default)]
    pub strike_range_low: Option<i32>,
    #[serde(default)]
    pub strike_range_high: Option<i32>,
    pub strike_increment: f64,
    pub enabled: bool,
}

fn default_iv() -> f64 {
    0.30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotingSection {
    pub tick_size: f64,
    pub min_edge_ticks: i32,
    #[serde(default = "default_max_strike_offset")]
    pub max_strike_offset_usd: f64,
    #[serde(default = "default_gtd")]
    pub gtd_lifetime_s: f64,
    #[serde(default = "default_gtd_lead")]
    pub gtd_refresh_lead_s: f64,
    #[serde(default = "default_dead_band")]
    pub dead_band_ticks: i32,
    #[serde(default = "default_min_send")]
    pub min_send_interval_ms: u64,
    /// Spec §3.4: skip quoting both sides if half-spread > N × min_edge.
    /// 4.0 = skip when half-spread > 4 × (min_edge_ticks × tick_size).
    /// 0 disables the gate.
    #[serde(default = "default_spread_over_edge")]
    pub skip_if_spread_over_edge_mul: f64,
}

fn default_max_strike_offset() -> f64 {
    0.30
}
fn default_gtd() -> f64 {
    30.0
}
fn default_gtd_lead() -> f64 {
    3.5
}
fn default_dead_band() -> i32 {
    1
}
fn default_min_send() -> u64 {
    250
}
fn default_spread_over_edge() -> f64 {
    4.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HedgingSection {
    #[serde(default)]
    pub enabled: bool,
    /// "observe" or "execute".
    #[serde(default = "default_hedge_mode")]
    pub mode: String,
    #[serde(default = "default_tolerance")]
    pub tolerance_deltas: f64,
    #[serde(default = "default_true")]
    pub rebalance_on_fill: bool,
    #[serde(default = "default_hedge_cadence")]
    pub rebalance_cadence_sec: f64,
    #[serde(default = "default_true")]
    pub include_in_daily_pnl: bool,
    #[serde(default = "default_true")]
    pub flatten_on_halt: bool,
    /// Skip contracts within N days of expiry (CLAUDE.md §10).
    #[serde(default = "default_lockout")]
    pub hedge_lockout_days: i32,
}

fn default_hedge_mode() -> String {
    "observe".into()
}
fn default_tolerance() -> f64 {
    0.5
}
fn default_hedge_cadence() -> f64 {
    30.0
}
fn default_lockout() -> i32 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotSection {
    #[serde(default = "default_snapshot_path")]
    pub path: String,
    /// Cadence in milliseconds (4Hz default).
    #[serde(default = "default_snapshot_cadence")]
    pub cadence_ms: u64,
}

fn default_snapshot_path() -> String {
    "/app/data/snapshot.json".into()
}
fn default_snapshot_cadence() -> u64 {
    250
}

impl Default for SnapshotSection {
    fn default() -> Self {
        Self {
            path: default_snapshot_path(),
            cadence_ms: default_snapshot_cadence(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeSection {
    /// Override snapshot cadence; logging level; any other knobs that
    /// don't fit the strategy sections.
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_log_level() -> String {
    "info".into()
}

// ─── Loading ────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error reading config: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("config validation: {0}")]
    Validation(String),
}

impl BrokerDaemonConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let bytes = std::fs::read(path.as_ref())?;
        let cfg: BrokerDaemonConfig = serde_yaml::from_slice(&bytes)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.constraints.capital <= 0.0 {
            return Err(ConfigError::Validation(
                "constraints.capital must be > 0".into(),
            ));
        }
        if self.products.is_empty() {
            return Err(ConfigError::Validation("at least one product required".into()));
        }
        if !self.products.iter().any(|p| p.enabled) {
            // All products disabled would silently boot a broker with
            // zero quoting instruments — operationally visible only via
            // the absence of subscriptions. Audit round 2.
            return Err(ConfigError::Validation(
                "at least one product must be enabled".into(),
            ));
        }
        if self.broker.kind != "ibkr" && self.broker.kind != "ilink" {
            return Err(ConfigError::Validation(format!(
                "broker.kind must be 'ibkr' or 'ilink', got {}",
                self.broker.kind
            )));
        }
        if self.broker.kind == "ibkr" && self.broker.ibkr.is_none() {
            return Err(ConfigError::Validation(
                "broker.kind=ibkr requires broker.ibkr section".into(),
            ));
        }
        for p in &self.products {
            if p.multiplier <= 0.0 {
                return Err(ConfigError::Validation(format!(
                    "product {}: multiplier must be > 0",
                    p.name
                )));
            }
        }
        Ok(())
    }

    /// Resolve the daily-halt threshold using the same rule as
    /// corsair_risk's helper. Returns None when both knobs are absent.
    pub fn resolve_daily_halt_threshold(&self) -> Option<f64> {
        corsair_risk::RiskConfig::resolve_daily_halt_threshold(
            self.risk.daily_pnl_halt_pct,
            self.risk.max_daily_loss,
            self.constraints.capital,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const SAMPLE_YAML: &str = r#"
broker:
  kind: ibkr
  ibkr:
    gateway:
      host: 127.0.0.1
      port: 4002
    client_id: 0
    account: DUP000000

risk:
  daily_pnl_halt_pct: 0.05
  margin_kill_pct: 0.70
  delta_kill: 5.0
  vega_kill: 0
  theta_kill: -500

constraints:
  capital: 500000
  margin_ceiling_pct: 0.50
  delta_ceiling: 3.0
  theta_floor: -500
  effective_delta_gating: true

products:
  - name: HG
    multiplier: 25000
    default_iv: 0.30
    quote_range_low: -5
    quote_range_high: 5
    strike_increment: 0.05
    enabled: true

quoting:
  tick_size: 0.0005
  min_edge_ticks: 2

hedging:
  enabled: true
  mode: execute
  tolerance_deltas: 0.5
  rebalance_cadence_sec: 30
  hedge_lockout_days: 30
"#;

    #[test]
    fn loads_sample_config() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(SAMPLE_YAML.as_bytes()).unwrap();
        let cfg = BrokerDaemonConfig::load(f.path()).unwrap();
        assert_eq!(cfg.broker.kind, "ibkr");
        assert_eq!(cfg.products.len(), 1);
        assert_eq!(cfg.products[0].name, "HG");
        assert_eq!(cfg.constraints.capital, 500_000.0);
    }

    #[test]
    fn resolves_daily_halt() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(SAMPLE_YAML.as_bytes()).unwrap();
        let cfg = BrokerDaemonConfig::load(f.path()).unwrap();
        assert_eq!(cfg.resolve_daily_halt_threshold(), Some(-25_000.0));
    }

    #[test]
    fn defaults_apply_for_optional_fields() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(SAMPLE_YAML.as_bytes()).unwrap();
        let cfg = BrokerDaemonConfig::load(f.path()).unwrap();
        assert_eq!(cfg.snapshot.cadence_ms, 250);
        assert_eq!(cfg.snapshot.path, "/app/data/snapshot.json");
        assert_eq!(cfg.quoting.gtd_lifetime_s, 30.0);
        assert_eq!(cfg.quoting.dead_band_ticks, 1);
        assert_eq!(cfg.quoting.skip_if_spread_over_edge_mul, 4.0);
    }

    #[test]
    fn rejects_invalid_capital() {
        let bad = SAMPLE_YAML.replace("capital: 500000", "capital: 0");
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(bad.as_bytes()).unwrap();
        assert!(BrokerDaemonConfig::load(f.path()).is_err());
    }

    #[test]
    fn rejects_unknown_broker_kind() {
        let bad = SAMPLE_YAML.replace("kind: ibkr", "kind: unknown");
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(bad.as_bytes()).unwrap();
        assert!(BrokerDaemonConfig::load(f.path()).is_err());
    }

    #[test]
    fn rejects_empty_products() {
        let bad = SAMPLE_YAML.replace(
            "products:\n  - name: HG\n    multiplier: 25000\n    default_iv: 0.30\n    quote_range_low: -5\n    quote_range_high: 5\n    strike_increment: 0.05\n    enabled: true\n",
            "products: []\n",
        );
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(bad.as_bytes()).unwrap();
        assert!(BrokerDaemonConfig::load(f.path()).is_err());
    }

    #[test]
    fn rejects_all_products_disabled() {
        let bad = SAMPLE_YAML.replace("enabled: true", "enabled: false");
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(bad.as_bytes()).unwrap();
        let err = BrokerDaemonConfig::load(f.path())
            .err()
            .expect("expected validation error");
        let msg = format!("{err}");
        assert!(msg.contains("enabled"), "got: {msg}");
    }
}
