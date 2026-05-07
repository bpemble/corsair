//! `Runtime` — the central state hub for the broker daemon.
//!
//! Owns one instance of every stateful component (broker, portfolio,
//! risk, hedge, OMS, market data, snapshot publisher). Tasks (in
//! `tasks.rs`) hold `Arc<Runtime>` and acquire the relevant
//! `Mutex<...>` to read or mutate state.

use chrono::NaiveDate;
use corsair_broker_api::Broker;
use corsair_broker_ibkr_native::{
    client::NativeClientConfig, NativeBroker, NativeBrokerConfig,
};
use corsair_constraint::{ConstraintChecker, ConstraintConfig};
use corsair_hedge::{HedgeConfig, HedgeFanout, HedgeManager, HedgeMode};
use corsair_market_data::MarketDataState;
use corsair_position::{PortfolioState, ProductInfo, ProductRegistry};
use corsair_risk::{RiskConfig, RiskMonitor};
use corsair_snapshot::{SnapshotConfig, SnapshotPublisher};
use std::sync::{Arc, Mutex};
use thiserror::Error;

use crate::config::BrokerDaemonConfig;

#[derive(Debug, Error)]
pub enum RuntimeError {
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
    /// Default is `Live` since the Phase 6.7 cutover (2026-05-02).
    /// Setting `CORSAIR_BROKER_SHADOW=1` (or `true`) opts into shadow
    /// mode for debugging; any other value (or unset) yields live.
    pub fn from_env() -> Self {
        match std::env::var("CORSAIR_BROKER_SHADOW")
            .unwrap_or_default()
            .as_str()
        {
            "1" | "true" | "TRUE" => Self::Shadow,
            _ => Self::Live,
        }
    }
}

/// Central state. Each component is wrapped in a `Mutex` so tokio
/// tasks can acquire just the slice they need.
///
/// Lock-order convention (used throughout `tasks.rs`):
///   account → hedge → portfolio → market_data → risk → constraint
///
/// All hot paths take at most one lock at a time, or strictly in this
/// order. Holding portfolio across an `await` is forbidden — see
/// `periodic_risk_check` for the canonical pattern (snapshot under
/// locks, drop, then `await` cancel/notify outside).
pub struct Runtime {
    pub mode: RuntimeMode,
    pub config: BrokerDaemonConfig,

    /// Shared broker. Trait methods are now `&self` (interior
    /// mutability via Arc<NativeClient> etc.), so consumers no longer
    /// need an outer Mutex. Audit T1-2: the previous AsyncMutex
    /// serialized every place_order across its 30ms+ IBKR ack wait,
    /// defeating the parallel command dispatch in ipc.rs.
    pub broker: Arc<dyn Broker + Send + Sync>,
    pub portfolio: Mutex<PortfolioState>,
    pub risk: Mutex<RiskMonitor>,
    pub constraint: Mutex<ConstraintChecker>,
    pub hedge: Mutex<HedgeFanout>,
    /// Set of OrderIds we've observed at least one OrderStatus for.
    /// `pump_status` inserts on first sight and removes on terminal.
    /// Used only to drive a debug log for "status update for orderId
    /// we never tracked" — which surfaces stray statuses from other
    /// clientIds on the FA-master clientId=0 connection. Inlined
    /// 2026-05-07 from the retired `corsair_oms` crate.
    pub seen_orders: Mutex<std::collections::HashSet<corsair_broker_api::OrderId>>,
    pub market_data: Mutex<MarketDataState>,
    pub snapshot: Mutex<SnapshotPublisher>,

    /// Qualified Contract cache, keyed by InstrumentId (IBKR conId).
    /// Populated on every qualify_option/qualify_future. Consumers
    /// (e.g. ipc::handle_place) need the actual local_symbol +
    /// trading_class IBKR returned, not a synthesized stand-in —
    /// IBKR's place_order parser cross-checks these against conId
    /// and rejects mismatches with cryptic errors like 110 "VOL
    /// volatility" when fields don't line up.
    pub qualified_contracts:
        Mutex<std::collections::HashMap<corsair_broker_api::InstrumentId, corsair_broker_api::Contract>>,

    /// Fast-path contract lookup for `handle_place` / `handle_modify`,
    /// keyed by (strike-quantized-i64, expiry-YYYYMMDD, right-char).
    ///
    /// Replaces the O(N×M) scan over `MarketDataState::options_for_
    /// product` that handle_place used to do — that scan included a
    /// `format!("%Y%m%d")` allocation per option per call, costing
    /// ~10–15 µs of broker dispatch latency on every place command.
    /// This map is populated in `subscribe_strike` (alongside
    /// `qualified_contracts`) so the hot path can resolve a contract
    /// in a single `HashMap::get`. Bounded by quoted strikes
    /// (~30 in production) so memory is trivial.
    ///
    /// Strike is quantized via `strike_key_i64` (matches the trader's
    /// `SharedState::strike_key` defense from §18). Both producer
    /// (subscribe_strike) and consumer (handle_place) within the
    /// broker derive strike from the same f64s today, but a future
    /// path computing strike from arithmetic (e.g. `atm + n * tick`)
    /// would silently miss the cache under bit-precision drift —
    /// quantization closes that class entirely.
    pub contract_by_key:
        Mutex<std::collections::HashMap<(i64, String, char), corsair_broker_api::Contract>>,

    /// Cached account snapshot from `Broker::account_values()`. Updated
    /// every ~5min by `periodic_account_poll`. Read by
    /// `periodic_risk_check` so margin_kill has a real number to gate
    /// on (CLAUDE.md §7) and by `IBKRMarginChecker.update_cached_margin`
    /// equivalent to compute the synthetic-vs-IBKR scale (§3).
    pub account: Mutex<corsair_broker_api::AccountSnapshot>,

    /// v2 wire-to-wire latency JSONL stream
    /// (logs-paper/wire_timing-YYYY-MM-DD.jsonl). One row per
    /// place_order outcome with the five broker-edge timestamps.
    pub wire_timing: crate::jsonl::JsonlWriter,

    /// Rolling-window samples for the dashboard's TTT/RTT pill.
    /// Pushed on every successful place_order in `handle_place`;
    /// read by `periodic_snapshot` to compute p50/p99 every tick.
    pub latency_samples: Mutex<crate::latency::LatencySamples>,

    /// Most recent SABR fit per (product, expiry_str, side_char).
    /// Populated by the vol_surface fitter; consumed by
    /// build_chain_payload to compute per-leg theo prices.
    ///
    /// 2026-05-06 change: the fitter now produces ONE combined fit
    /// per (product, expiry) on OTM-only data and writes the same
    /// `VolSurfaceCacheEntry` under both side keys ('C' and 'P').
    /// The (expiry, side_char) shape is preserved so per-side
    /// readers (build_chain_payload, ipc.rs theo enrichment, trader
    /// `vol_surfaces`) keep working unchanged — they just see
    /// matching params on both sides instead of independent fits.
    /// See `vol_surface.rs` for the rationale (per-side fits were
    /// pinning C-side ρ at +0.999 ~30% of cycles on noisy deep-ITM
    /// data despite identical underlying smile).
    ///
    /// Stored as `Mutex<Arc<HashMap<...>>>` so readers clone only the
    /// Arc (`vol_surface_cache.lock().unwrap().clone()` is cheap)
    /// instead of deep-cloning the inner map. The fitter mutates by
    /// building a new HashMap from the previous Arc's contents and
    /// publishing it via `Mutex::lock`. Readers and the fitter never
    /// race because the Arc is swapped, not mutated in place.
    pub vol_surface_cache:
        Mutex<Arc<std::collections::HashMap<(String, String, char), VolSurfaceCacheEntry>>>,

    /// IPC events server handle. Held so handle_place can publish
    /// place_ack events directly (without threading the server arc
    /// through the dispatch path). Set during corsair_broker::ipc::
    /// spawn just after server creation; None until then.
    pub ipc_server: Mutex<Option<Arc<corsair_ipc::SHMServer>>>,

    /// §25 trader watchdog: wall-clock ns of the most recent commands-
    /// ring frame received from the trader. Updated on every command
    /// dispatch (place / cancel / modify / telemetry / heartbeat /
    /// welcome). The watchdog task (`trader_watchdog` in `tasks.rs`)
    /// reads this 1Hz and fires `trader_silent` if the gap exceeds
    /// `CORSAIR_TRADER_WATCHDOG_TIMEOUT_S` (default 5s).
    ///
    /// Atomic so the dispatch path doesn't need to take a lock; the
    /// watchdog reads with `Acquire` to pair with the dispatcher's
    /// `Release`.
    pub last_trader_msg_ns: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Clone)]
pub struct VolSurfaceCacheEntry {
    pub forward: f64,
    pub tte: f64,
    pub alpha: f64,
    pub beta: f64,
    pub rho: f64,
    pub nu: f64,
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

        // Constraint checker: one per ENABLED product. Multi-product
        // scope drives one checker per product ("delta_for_product"
        // is per-product). For now we pick the first enabled product
        // as the primary; further products would need a fanout
        // layer.
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

        let market_data = MarketDataState::new();

        let snapshot_cfg = SnapshotConfig {
            snapshot_path: cfg.snapshot.path.clone().into(),
        };
        let snapshot = SnapshotPublisher::new(snapshot_cfg);

        let runtime = Arc::new(Self {
            mode,
            config: cfg,
            broker,
            portfolio: Mutex::new(portfolio),
            risk: Mutex::new(risk),
            constraint: Mutex::new(constraint),
            hedge: Mutex::new(hedge),
            seen_orders: Mutex::new(std::collections::HashSet::new()),
            market_data: Mutex::new(market_data),
            snapshot: Mutex::new(snapshot),
            qualified_contracts: Mutex::new(std::collections::HashMap::new()),
            contract_by_key: Mutex::new(std::collections::HashMap::new()),
            latency_samples: Mutex::new(crate::latency::LatencySamples::new()),
            vol_surface_cache: Mutex::new(Arc::new(std::collections::HashMap::new())),
            ipc_server: Mutex::new(None),
            last_trader_msg_ns: std::sync::atomic::AtomicU64::new(0),
            account: Mutex::new(corsair_broker_api::AccountSnapshot {
                net_liquidation: 0.0,
                maintenance_margin: 0.0,
                initial_margin: 0.0,
                buying_power: 0.0,
                realized_pnl_today: 0.0,
                timestamp_ns: 0,
            }),
            wire_timing: crate::jsonl::JsonlWriter::start(
                std::path::PathBuf::from(
                    std::env::var("CORSAIR_LOGS_DIR")
                        .unwrap_or_else(|_| "/app/logs-paper".into()),
                ),
                "wire_timing",
            ),
        });

        // ── Connect ───────────────────────────────────────────────
        runtime.connect().await?;

        // ── Wait for initial position/account/openOrder snapshot ──
        //
        // The native client streams Position / OpenOrder /
        // AccountValue messages asynchronously after reqXxx is sent.
        // We must wait for the matching "End" signals before seeding
        // PortfolioState, otherwise a partial snapshot can mask short
        // inventory at boot — the exact failure mode CLAUDE.md §10
        // names as the live-deployment hard prerequisite.
        runtime.wait_for_native_seeding().await;

        // ── Resolve hedge contract per product (CLAUDE.md §10) ────
        //
        // Without this, hedge_contract stays None on every manager,
        // place_hedge_order skips silently, apply_broker_fill never
        // matches, and the entire hedge subsystem is dead — even with
        // mode=execute. Boot-time resolution: list_chain for FUT
        // matching the underlying symbol, skip contracts within
        // hedge_lockout_days, pick the first surviving expiry.
        runtime.resolve_hedge_contracts().await;

        // ── Seed positions from broker ────────────────────────────
        runtime.seed_positions_from_broker().await?;

        // ── Seed realized_pnl_persisted from IBKR's session total ──
        //
        // `Portfolio::new` initializes the accumulator at 0.0 and the
        // only reset is `daily_halt_rollover` at CME 17:00 CT. Without
        // a boot seed, every mid-session restart silently re-anchors
        // the dashboard's "Today's P&L" tile and the −5% daily halt
        // threshold to boot time, dropping all pre-boot realized P&L
        // (observed 2026-05-07: IBKR realized_today=$343 vs local
        // realized_pnl_persisted=−$327 after a 10:00 CT restart that
        // missed +$673 of pre-boot realized).
        //
        // Best-effort: if the account fetch fails, the next
        // `periodic_account_poll` (15 s cadence) will seed via the
        // same resync path.
        match runtime.broker.account_values().await {
            Ok(snap) => {
                if let Ok(mut a) = runtime.account.lock() {
                    *a = snap;
                }
                crate::tasks::resync_realized_pnl_from_ibkr(&runtime, "boot");
            }
            Err(e) => log::warn!(
                "boot account_values failed: {e}; realized_pnl_persisted left at 0 \
                 (next periodic_account_poll will seed within 15s)"
            ),
        }

        log::warn!("corsair_broker boot complete; tasks will start next");
        Ok(runtime)
    }

    /// Boot-time hedge contract resolution. For each product with
    /// hedging enabled and a registered HedgeManager, call
    /// `Broker::list_chain` to enumerate FUT contracts and pick the
    /// first whose expiry is past the configured lockout window.
    /// Best-effort: failure to resolve logs a warning but doesn't
    /// fail boot — operator can manually flatten or restart once
    /// gateway is healthy.
    async fn resolve_hedge_contracts(self: &Arc<Self>) {
        use chrono::Datelike;
        let products: Vec<(String, i64)> = {
            let h = self.hedge.lock().unwrap();
            h.managers()
                .iter()
                .map(|m| {
                    (
                        m.config().product.clone(),
                        m.config().lockout_days as i64,
                    )
                })
                .collect()
        };
        if products.is_empty() {
            return;
        }
        for (symbol, lockout_days) in products {
            let min_expiry = chrono::Utc::now().date_naive()
                + chrono::Duration::days(lockout_days);
            let q = corsair_broker_api::ChainQuery {
                symbol: symbol.clone(),
                exchange: corsair_broker_api::Exchange::Comex,
                currency: corsair_broker_api::Currency::Usd,
                kind: Some(corsair_broker_api::ContractKind::Future),
                min_expiry: Some(min_expiry),
            };
            let contracts_result = {
                let b = self.broker.clone();
                b.list_chain(q).await
            };
            match contracts_result {
                Ok(mut chain) => {
                    chain.sort_by_key(|c| c.expiry);
                    if let Some(c) = chain.into_iter().find(|c| c.expiry >= min_expiry) {
                        log::warn!(
                            "hedge[{symbol}]: resolved {} ({}-{:02}-{:02})",
                            c.local_symbol,
                            c.expiry.year(),
                            c.expiry.month(),
                            c.expiry.day()
                        );
                        let mut h = self.hedge.lock().unwrap();
                        if let Some(mgr) = h.for_product_mut(&symbol) {
                            mgr.set_hedge_contract(c);
                        }
                    } else {
                        log::warn!(
                            "hedge[{symbol}]: list_chain returned no contracts past lockout ({lockout_days}d)"
                        );
                    }
                }
                Err(e) => {
                    log::warn!(
                        "hedge[{symbol}]: list_chain failed: {e} — hedge will retry on next periodic"
                    );
                }
            }
        }
    }

    async fn connect(self: &Arc<Self>) -> Result<(), RuntimeError> {
        let b = self.broker.clone();
        b.connect().await?;
        Ok(())
    }

    /// Wait for the broker's initial state snapshot (positions / open
    /// orders / account values). Calls `Broker::wait_for_initial_snapshot`,
    /// which on `NativeBroker` gates on PositionEnd / OpenOrderEnd /
    /// AccountDownloadEnd. The default trait impl is a no-op for
    /// adapters that bootstrap synchronously inside `connect()`.
    async fn wait_for_native_seeding(self: &Arc<Self>) {
        let timeout = std::time::Duration::from_secs(15);
        log::info!(
            "waiting up to {}s for broker initial snapshot...",
            timeout.as_secs()
        );
        let b = self.broker.clone();
        if let Err(e) = b.wait_for_initial_snapshot(timeout).await {
            log::warn!("broker initial snapshot wait failed: {e}");
        }
    }

    /// Read positions from the broker, seed PortfolioState (options) and
    /// HedgeManager (futures). Implements the boot reconcile step
    /// CLAUDE.md §10 names as a hard live prerequisite. Skipping the
    /// hedge reconcile leaves hedge_qty=0 locally on every restart and
    /// triggers spurious delta_kill.
    async fn seed_positions_from_broker(self: &Arc<Self>) -> Result<(), RuntimeError> {
        let positions = {
            let b = self.broker.clone();
            b.positions().await?
        };

        let mut to_insert = Vec::new();
        let registry_products: Vec<String> = {
            let p = self.portfolio.lock().unwrap();
            p.registry().products()
        };

        // Track hedge reconciliation per product. Populated as we walk
        // futures positions; applied AFTER the loop so we don't hold
        // both the portfolio and hedge locks simultaneously.
        let mut hedge_seeds: Vec<(String, i32, f64)> = Vec::new();

        for pos in positions {
            if !registry_products.contains(&pos.contract.symbol) {
                log::debug!(
                    "seed: skipping unregistered product {}",
                    pos.contract.symbol
                );
                continue;
            }
            match pos.contract.kind {
                corsair_broker_api::ContractKind::Option => {
                    let right = match pos.contract.right {
                        Some(r) => r,
                        None => continue,
                    };
                    let strike = match pos.contract.strike {
                        Some(s) => s,
                        None => continue,
                    };
                    let multiplier = if pos.contract.multiplier > 0.0 {
                        pos.contract.multiplier
                    } else {
                        log::warn!(
                            "seed: invalid multiplier=0 for {} {} {:?}; skipping",
                            pos.contract.symbol,
                            strike,
                            right
                        );
                        continue;
                    };
                    // Greeks/current_price seeded to 0 here; the
                    // 5-second `periodic_greek_refresh` task in
                    // `tasks.rs` populates them from market_data once
                    // ticks are flowing. The 6-second boot-burn in
                    // `periodic_risk_check` (also tasks.rs) is keyed
                    // off this 5 s cadence — it ensures we don't run
                    // a delta_kill check before the first refresh has
                    // populated greeks.
                    to_insert.push(corsair_position::Position {
                        product: pos.contract.symbol.clone(),
                        strike,
                        expiry: pos.contract.expiry,
                        right,
                        quantity: pos.quantity,
                        avg_fill_price: pos.avg_cost / multiplier,
                        fill_time: chrono::Utc::now(),
                        multiplier,
                        delta: 0.0,
                        gamma: 0.0,
                        theta: 0.0,
                        vega: 0.0,
                        current_price: 0.0,
                    });
                }
                corsair_broker_api::ContractKind::Future => {
                    if pos.quantity == 0 {
                        continue;
                    }
                    // avg_cost from IBKR is per-contract notional; the
                    // hedge state stores avg_entry_F (per-unit price).
                    let multiplier = if pos.contract.multiplier > 0.0 {
                        pos.contract.multiplier
                    } else {
                        25_000.0 // HG default if missing
                    };
                    let avg_entry_f = pos.avg_cost / multiplier;
                    hedge_seeds.push((
                        pos.contract.symbol.clone(),
                        pos.quantity,
                        avg_entry_f,
                    ));
                }
            }
        }

        let opt_count = to_insert.len();
        {
            let mut p = self.portfolio.lock().unwrap();
            p.replace_positions(to_insert);
        }

        // Apply hedge seeds. CLAUDE.md §10: "boot reconcile reads
        // ib.positions(), finds the FUT position matching the resolved
        // hedge contract by conId/localSymbol, sets hedge_qty and
        // avg_entry_F to match." We don't yet match by conId — products
        // with multiple hedge contracts (calendar) would need that.
        let mut hedge_count = 0;
        if !hedge_seeds.is_empty() {
            let mut hedge = self.hedge.lock().unwrap();
            for (product, qty, avg) in &hedge_seeds {
                if let Some(mgr) = hedge.for_product_mut(product) {
                    let changed = mgr.reconcile_with_position(*qty, *avg, false);
                    if changed {
                        hedge_count += 1;
                    }
                }
            }
        }

        log::warn!(
            "corsair_broker seeded {opt_count} option positions, {hedge_count} hedge reconciles from broker"
        );
        Ok(())
    }

    /// Periodic options-position reconcile against an already-fetched
    /// `positions` slice (the caller is expected to be running this
    /// alongside hedge reconcile and has the data in hand). Compares
    /// IBKR's view against PortfolioState; on any divergence (qty
    /// mismatch, missing leg, extra leg) replaces wholesale and logs
    /// loudly so the operator sees the recovery.
    ///
    /// This is the safety net for the silent-fill-loss class of bugs:
    /// if `pump_fills` lags + drops, or a fill arrives on an instrument
    /// not yet in the market_data registry, the per-fill update misses
    /// — and without this call, the divergence persists indefinitely.
    /// Returns the number of leg-quantity changes applied.
    pub fn reconcile_options_with_broker_positions(
        &self,
        positions: &[corsair_broker_api::position::Position],
    ) -> usize {
        let registry_products: Vec<String> = {
            let p = self.portfolio.lock().unwrap();
            p.registry().products()
        };

        let mut to_insert: Vec<corsair_position::Position> = Vec::new();
        for pos in positions {
            if !registry_products.contains(&pos.contract.symbol) {
                continue;
            }
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
            let multiplier = if pos.contract.multiplier > 0.0 {
                pos.contract.multiplier
            } else {
                continue;
            };
            to_insert.push(corsair_position::Position {
                product: pos.contract.symbol.clone(),
                strike,
                expiry: pos.contract.expiry,
                right,
                quantity: pos.quantity,
                avg_fill_price: pos.avg_cost / multiplier,
                fill_time: chrono::Utc::now(),
                multiplier,
                delta: 0.0,
                gamma: 0.0,
                theta: 0.0,
                vega: 0.0,
                current_price: 0.0,
            });
        }

        let mut p = self.portfolio.lock().unwrap();
        // Build before/after fingerprints keyed only by identity +
        // quantity. Greeks/current_price differ every tick so we must
        // not let those diffs trigger spurious "changed" logs.
        // f64 strike doesn't impl Hash/Eq, so we key on a string-
        // serialized form for HashMap lookup.
        let key = |q: &corsair_position::Position| -> String {
            format!("{}|{:.4}|{}|{:?}", q.product, q.strike, q.expiry, q.right)
        };
        let before_map: std::collections::HashMap<String, (i32, f64, NaiveDate, corsair_broker_api::Right, String)> =
            p.positions()
                .iter()
                .map(|q| {
                    (
                        key(q),
                        (q.quantity, q.strike, q.expiry, q.right, q.product.clone()),
                    )
                })
                .collect();
        let after_map: std::collections::HashMap<String, (i32, f64, NaiveDate, corsair_broker_api::Right, String)> =
            to_insert
                .iter()
                .map(|q| {
                    (
                        key(q),
                        (q.quantity, q.strike, q.expiry, q.right, q.product.clone()),
                    )
                })
                .collect();

        // Quick equal-quantity check first — if every key + qty matches,
        // there's nothing to report or replace.
        let same = before_map.len() == after_map.len()
            && before_map.iter().all(|(k, (qb, ..))| {
                after_map.get(k).map(|(qa, ..)| qa == qb).unwrap_or(false)
            });
        if same {
            return 0;
        }

        // DIVERGENCE detected — count and log each delta before we
        // overwrite so the operator can audit what was lost.
        let mut changes = 0usize;
        for (k, (qa, strike, expiry, right, product)) in &after_map {
            let qb = before_map.get(k).map(|(q, ..)| *q).unwrap_or(0);
            if qb != *qa {
                changes += 1;
                log::error!(
                    "POSITION DRIFT: {} K={} exp={} {:?}  local={} → ibkr={} (correcting)",
                    product, strike, expiry, right, qb, qa
                );
            }
        }
        for (k, (qb, strike, expiry, right, product)) in &before_map {
            if !after_map.contains_key(k) {
                changes += 1;
                log::error!(
                    "POSITION DRIFT: {} K={} exp={} {:?}  local={} → ibkr=0 (closing)",
                    product, strike, expiry, right, qb
                );
            }
        }
        p.replace_positions(to_insert);
        changes
    }

    /// Disconnect cleanly. Called from the shutdown handler.
    pub async fn shutdown(self: &Arc<Self>) -> Result<(), RuntimeError> {
        log::warn!("corsair_broker daemon shutting down");
        let b = self.broker.clone();
        b.disconnect().await?;
        Ok(())
    }
}

fn build_broker(cfg: &BrokerDaemonConfig) -> Result<Arc<dyn Broker + Send + Sync>, RuntimeError> {
    match cfg.broker.kind.as_str() {
        // Native Rust IBKR client. The legacy PyO3+ib_insync bridge
        // was retired in the Phase 6.7 cutover and deleted in Phase
        // 6.11. There is no in-place rollback path — reverting to
        // ib_insync requires git revert + rebuild.
        "ibkr" | "ibkr_native" => {
            let ibkr = cfg
                .broker
                .ibkr
                .as_ref()
                .ok_or_else(|| RuntimeError::Internal("missing broker.ibkr".into()))?;
            let nb_cfg = NativeBrokerConfig {
                client: NativeClientConfig {
                    host: ibkr.gateway.host.clone(),
                    port: ibkr.gateway.port,
                    client_id: ibkr.client_id,
                    account: None,
                    connect_timeout: std::time::Duration::from_secs(10),
                    handshake_timeout: std::time::Duration::from_secs(10),
                },
                account: ibkr.account.clone(),
            };
            let adapter = NativeBroker::new(nb_cfg);
            Ok(Arc::new(adapter))
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
    fn live_mode_default_when_unset() {
        std::env::remove_var("CORSAIR_BROKER_SHADOW");
        assert_eq!(RuntimeMode::from_env(), RuntimeMode::Live);
    }

    #[test]
    fn shadow_mode_when_one() {
        std::env::set_var("CORSAIR_BROKER_SHADOW", "1");
        assert_eq!(RuntimeMode::from_env(), RuntimeMode::Shadow);
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
