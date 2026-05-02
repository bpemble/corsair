//! IPC server task — bridges the trader binary to the Rust broker.
//!
//! Responsibilities:
//!   - Create the SHM rings (events + commands) at the configured base path.
//!   - Forward fills / order_status / connection events from the broker
//!     stream to the trader via the events ring.
//!   - Receive place_order / cancel_order commands from the trader and
//!     dispatch to `Broker::place_order` / `Broker::cancel_order`.
//!
//! This is the keystone for Phase 5B cutover. With this task running,
//! the corsair_trader binary can connect to the Rust broker exactly as
//! it currently connects to the Python broker.
//!
//! Phase 5B scope (this session):
//!   ✓ Ring creation
//!   ✓ Forward fills + order_status + connection events
//!   ✓ Dispatch place_order / cancel_order
//!   ⏸ Forward ticks (the trader needs these to make decisions —
//!     wire when corsair_market_data is integrated)
//!   ⏸ Forward vol_surface events (needs SABR fitter orchestration
//!     in Rust — Phase 6 work)
//!   ✓ Forward risk_state at 1Hz (Phase 5B.6, see periodic_risk_state)

use corsair_broker_api::{
    ContractKind, Currency, Exchange, ModifyOrderReq, OrderId,
    OrderType, PlaceOrderReq, Side, TimeInForce,
};
use corsair_ipc::{ServerCommand, ServerConfig, SHMServer};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

use crate::runtime::Runtime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcConfig {
    /// Base path for the SHM rings. Conventional value:
    /// /app/data/corsair_ipc — matches today's Python broker.
    pub base_path: PathBuf,
    /// Per-ring capacity in bytes. Default 1 MiB.
    pub capacity: usize,
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            base_path: PathBuf::from("/app/data/corsair_ipc"),
            capacity: 1 << 20,
        }
    }
}

/// Spawn the IPC server task family. Returns the SHMServer handle so
/// the runtime can query frame-drop counters from telemetry.
pub fn spawn_ipc(
    runtime: Arc<Runtime>,
    cfg: IpcConfig,
) -> std::io::Result<Arc<SHMServer>> {
    let server_cfg = ServerConfig {
        base_path: cfg.base_path,
        capacity: cfg.capacity,
    };
    let server = Arc::new(SHMServer::create(server_cfg)?);
    let (cmd_rx, _drop_rx) = server.start();

    // Spawn the command-dispatch loop.
    tokio::spawn(dispatch_commands(runtime.clone(), cmd_rx));

    // Spawn the event-publish loops (one per stream we forward).
    tokio::spawn(forward_fills(runtime.clone(), Arc::clone(&server)));
    tokio::spawn(forward_status(runtime.clone(), Arc::clone(&server)));
    tokio::spawn(forward_connection(runtime.clone(), Arc::clone(&server)));
    tokio::spawn(periodic_risk_state(runtime.clone(), Arc::clone(&server)));

    // Tick fast-path: install a publisher closure on NativeBroker
    // so the dispatcher writes ticks directly to SHM, bypassing the
    // broadcast channel + forward_ticks pump (~20 µs saved/tick).
    // Falls back to the legacy forward_ticks task if the broker
    // doesn't expose a tick-publisher hook (other adapter impls).
    {
        let runtime = runtime.clone();
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            install_tick_fastpath(&runtime, server).await;
        });
    }

    log::warn!("corsair_broker: IPC server live");
    Ok(server)
}

/// Install the tick fast-path on the active broker. Adapters that
/// support the fast-path (NativeBroker today) override
/// `Broker::set_tick_publisher` and return true. Adapters that don't
/// return false; we fall back to the legacy forward_ticks pump
/// subscribed to the broadcast channel.
async fn install_tick_fastpath(runtime: &Arc<Runtime>, server: Arc<SHMServer>) {
    let server_for_closure = Arc::clone(&server);
    let publisher: std::sync::Arc<dyn for<'a> Fn(&'a corsair_broker_api::Tick) + Send + Sync + 'static> =
        std::sync::Arc::new(move |tick: &corsair_broker_api::Tick| {
            let kind_str = match tick.kind {
                corsair_broker_api::TickKind::Bid => "bid",
                corsair_broker_api::TickKind::Ask => "ask",
                corsair_broker_api::TickKind::Last => "last",
                corsair_broker_api::TickKind::BidSize => "bid_size",
                corsair_broker_api::TickKind::AskSize => "ask_size",
                corsair_broker_api::TickKind::Volume => "volume",
            };
            let ev = TickEvent {
                ty: "tick",
                instrument_id: tick.instrument_id.0,
                kind: kind_str,
                price: tick.price,
                size: tick.size,
                timestamp_ns: tick.timestamp_ns,
            };
            if let Ok(body) = rmp_serde::to_vec_named(&ev) {
                if !server_for_closure.publish(&body) {
                    log::debug!("tick fastpath: events ring full");
                }
            }
        });
    let installed = {
        let b = runtime.broker.lock().await;
        b.set_tick_publisher(publisher).await
    };
    if installed {
        log::warn!("ipc: tick fast-path active (~20 µs/tick saved vs broadcast pump)");
    } else {
        log::warn!("ipc: tick fast-path unavailable on this adapter — using broadcast pump");
        tokio::spawn(forward_ticks(runtime.clone(), server));
    }
}

// ─── Wire types — must match what the trader expects ─────────────

/// Trader → broker: place_order command. Field names mirror the
/// Python BrokerIPC.set_order_dispatchers protocol.
#[derive(Debug, Deserialize)]
struct PlaceOrderCmd {
    #[serde(rename = "type")]
    _ty: String,
    instrument_id: u64,
    side: String,         // "BUY" or "SELL"
    qty: u32,
    price: f64,
    /// Order type: "limit" or "market". Default "limit".
    #[serde(default = "default_order_type")]
    order_type: String,
    /// Time in force: "GTD", "IOC", etc.
    #[serde(default = "default_tif")]
    tif: String,
    /// goodTillDate seconds from now — used when tif=GTD.
    #[serde(default)]
    gtd_seconds: Option<u32>,
    #[serde(default)]
    client_order_ref: Option<String>,
}

fn default_order_type() -> String {
    "limit".into()
}
fn default_tif() -> String {
    "GTD".into()
}

#[derive(Debug, Deserialize)]
struct CancelOrderCmd {
    #[serde(rename = "type")]
    _ty: String,
    order_id: u64,
}

#[derive(Debug, Deserialize)]
struct ModifyOrderCmd {
    #[serde(rename = "type")]
    _ty: String,
    order_id: u64,
    #[serde(default)]
    price: Option<f64>,
    #[serde(default)]
    qty: Option<u32>,
    #[serde(default)]
    gtd_seconds: Option<u32>,
}

/// Broker → trader event envelopes.
#[derive(Debug, Serialize)]
struct FillEvent<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    exec_id: &'a str,
    order_id: u64,
    instrument_id: u64,
    side: &'a str,
    qty: u32,
    price: f64,
    timestamp_ns: u64,
    commission: Option<f64>,
}

#[derive(Debug, Serialize)]
struct OrderStatusEvent<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    order_id: u64,
    status: &'a str,
    filled_qty: u32,
    remaining_qty: u32,
    avg_fill_price: f64,
    last_fill_price: Option<f64>,
    timestamp_ns: u64,
    reject_reason: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct ConnectionEventMsg<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    state: &'a str,
    reason: Option<&'a str>,
    timestamp_ns: u64,
}

#[derive(Debug, Serialize)]
struct PlaceAckEvent<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    /// Caller-side ref so trader can match this ack to its place call.
    client_order_ref: &'a str,
    /// Broker-assigned id (None on rejection).
    order_id: Option<u64>,
    rejected: bool,
    reason: Option<&'a str>,
}

// ─── Command dispatch ────────────────────────────────────────────

async fn dispatch_commands(
    runtime: Arc<Runtime>,
    mut rx: tokio::sync::mpsc::Receiver<ServerCommand>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd.kind.as_str() {
            "place_order" => handle_place(&runtime, &cmd.body).await,
            "cancel_order" => handle_cancel(&runtime, &cmd.body).await,
            "modify_order" => handle_modify(&runtime, &cmd.body).await,
            // Trader emits a "telemetry" command every 10s with its
            // own observed counters. The broker just acknowledges by
            // dropping it — the trader logs the same numbers locally,
            // so we don't need to surface them here. Logging at
            // trace so it's silent under default filter.
            "telemetry" => log::trace!("ipc: trader telemetry frame"),
            other => log::warn!("ipc: unknown command type: {other}"),
        }
    }
    log::info!("ipc dispatch_commands: channel closed");
}

async fn handle_place(runtime: &Arc<Runtime>, body: &[u8]) {
    let cmd: PlaceOrderCmd = match rmp_serde::from_slice(body) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("ipc place_order: parse error: {e}");
            return;
        }
    };
    if matches!(runtime.mode, crate::runtime::RuntimeMode::Shadow) {
        log::info!(
            "ipc place_order (SHADOW; not placing): {:?} {} @ {} qty {}",
            cmd.side,
            cmd.instrument_id,
            cmd.price,
            cmd.qty
        );
        return;
    }
    // Resolve the contract from the broker's contract cache via
    // market_data — for now we look up the cached option to get the
    // strike/expiry/right, and reconstruct a Contract.
    let contract = {
        let md = runtime.market_data.lock().unwrap();
        let products = runtime.portfolio.lock().unwrap().registry().products();
        let mut found = None;
        for prod in products {
            for t in md.options_for_product(&prod) {
                if t.instrument_id == Some(corsair_broker_api::InstrumentId(cmd.instrument_id)) {
                    found = Some((prod, t.strike, t.expiry, t.right));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        match found {
            Some((prod, strike, expiry, right)) => corsair_broker_api::Contract {
                instrument_id: corsair_broker_api::InstrumentId(cmd.instrument_id),
                kind: ContractKind::Option,
                symbol: format!("HXE{}", chrono::Local::now().format("%y%m")),
                local_symbol: format!("{}{}{}{:.3}", prod, expiry, right.as_char(), strike),
                expiry,
                strike: Some(strike),
                right: Some(right),
                multiplier: 25_000.0,
                exchange: Exchange::Comex,
                currency: Currency::Usd,
            },
            None => {
                log::warn!(
                    "ipc place_order: no contract cached for instrument_id={}",
                    cmd.instrument_id
                );
                return;
            }
        }
    };
    let side = match cmd.side.as_str() {
        "BUY" => Side::Buy,
        _ => Side::Sell,
    };
    let order_type = match cmd.order_type.as_str() {
        "market" => OrderType::Market,
        _ => OrderType::Limit,
    };
    let tif = match cmd.tif.as_str() {
        "IOC" => TimeInForce::Ioc,
        "DAY" => TimeInForce::Day,
        "GTC" => TimeInForce::Gtc,
        _ => TimeInForce::Gtd,
    };
    let gtd_until_utc = if matches!(tif, TimeInForce::Gtd) {
        Some(chrono::Utc::now() + chrono::Duration::seconds(cmd.gtd_seconds.unwrap_or(30) as i64))
    } else {
        None
    };
    let req = PlaceOrderReq {
        contract,
        side,
        qty: cmd.qty,
        order_type,
        price: Some(cmd.price),
        tif,
        gtd_until_utc,
        client_order_ref: cmd.client_order_ref.clone().unwrap_or_default(),
        account: Some(runtime.config.broker.ibkr.as_ref()
            .map(|i| i.account.clone()).unwrap_or_default()),
    };
    let result = {
        let b = runtime.broker.lock().await;
        b.place_order(req).await
    };
    match result {
        Ok(oid) => log::info!(
            "ipc place_order placed: oid={} for ref='{}'",
            oid,
            cmd.client_order_ref.unwrap_or_default()
        ),
        Err(e) => log::warn!("ipc place_order failed: {e}"),
    }
}

async fn handle_cancel(runtime: &Arc<Runtime>, body: &[u8]) {
    let cmd: CancelOrderCmd = match rmp_serde::from_slice(body) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("ipc cancel_order: parse error: {e}");
            return;
        }
    };
    if matches!(runtime.mode, crate::runtime::RuntimeMode::Shadow) {
        log::info!("ipc cancel_order (SHADOW; not cancelling): order_id={}", cmd.order_id);
        return;
    }
    let result = {
        let b = runtime.broker.lock().await;
        b.cancel_order(OrderId(cmd.order_id)).await
    };
    if let Err(e) = result {
        log::warn!("ipc cancel_order failed: {e}");
    }
}

async fn handle_modify(runtime: &Arc<Runtime>, body: &[u8]) {
    let cmd: ModifyOrderCmd = match rmp_serde::from_slice(body) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("ipc modify_order: parse error: {e}");
            return;
        }
    };
    if matches!(runtime.mode, crate::runtime::RuntimeMode::Shadow) {
        log::info!("ipc modify_order (SHADOW; not modifying): order_id={}", cmd.order_id);
        return;
    }
    let req = ModifyOrderReq {
        price: cmd.price,
        qty: cmd.qty,
        gtd_until_utc: cmd
            .gtd_seconds
            .map(|s| chrono::Utc::now() + chrono::Duration::seconds(s as i64)),
    };
    let result = {
        let b = runtime.broker.lock().await;
        b.modify_order(OrderId(cmd.order_id), req).await
    };
    if let Err(e) = result {
        log::warn!("ipc modify_order failed: {e}");
    }
}

// ─── Event publishing ─────────────────────────────────────────────

async fn forward_fills(runtime: Arc<Runtime>, server: Arc<SHMServer>) {
    let mut rx = {
        let b = runtime.broker.lock().await;
        b.subscribe_fills()
    };
    log::info!("ipc forward_fills: subscribed");
    loop {
        match rx.recv().await {
            Ok(fill) => {
                let side_str = match fill.side {
                    Side::Buy => "BUY",
                    Side::Sell => "SELL",
                };
                let ev = FillEvent {
                    ty: "fill",
                    exec_id: &fill.exec_id,
                    order_id: fill.order_id.0,
                    instrument_id: fill.instrument_id.0,
                    side: side_str,
                    qty: fill.qty,
                    price: fill.price,
                    timestamp_ns: fill.timestamp_ns,
                    commission: fill.commission,
                };
                if let Ok(body) = rmp_serde::to_vec_named(&ev) {
                    if !server.publish(&body) {
                        log::warn!("ipc events ring full — dropped fill");
                    }
                }
            }
            Err(RecvError::Lagged(n)) => log::warn!("forward_fills: lagged {n}"),
            Err(RecvError::Closed) => break,
        }
    }
}

async fn forward_status(runtime: Arc<Runtime>, server: Arc<SHMServer>) {
    let mut rx = {
        let b = runtime.broker.lock().await;
        b.subscribe_order_status()
    };
    log::info!("ipc forward_status: subscribed");
    loop {
        match rx.recv().await {
            Ok(update) => {
                let status_str = format!("{:?}", update.status);
                let ev = OrderStatusEvent {
                    ty: "order_status",
                    order_id: update.order_id.0,
                    status: &status_str,
                    filled_qty: update.filled_qty,
                    remaining_qty: update.remaining_qty,
                    avg_fill_price: update.avg_fill_price,
                    last_fill_price: update.last_fill_price,
                    timestamp_ns: update.timestamp_ns,
                    reject_reason: update.reject_reason.as_deref(),
                };
                if let Ok(body) = rmp_serde::to_vec_named(&ev) {
                    if !server.publish(&body) {
                        log::warn!("ipc events ring full — dropped status update");
                    }
                }
            }
            Err(RecvError::Lagged(n)) => log::warn!("forward_status: lagged {n}"),
            Err(RecvError::Closed) => break,
        }
    }
}

async fn forward_connection(runtime: Arc<Runtime>, server: Arc<SHMServer>) {
    let mut rx = {
        let b = runtime.broker.lock().await;
        b.subscribe_connection()
    };
    log::info!("ipc forward_connection: subscribed");
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let state_str = format!("{:?}", ev.state);
                let msg = ConnectionEventMsg {
                    ty: "connection",
                    state: &state_str,
                    reason: ev.reason.as_deref(),
                    timestamp_ns: ev.timestamp_ns,
                };
                if let Ok(body) = rmp_serde::to_vec_named(&msg) {
                    if !server.publish(&body) {
                        log::warn!("ipc events ring full — dropped connection event");
                    }
                }
            }
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => break,
        }
    }
}

// Suppress dead-code warning on unused PlaceAckEvent (Phase 5B.3+
// will publish it once we add place_ack semantics to the broker
// trait).
#[allow(dead_code)]
fn _suppress_dead_code(e: PlaceAckEvent<'_>) {
    let _ = e.ty;
    let _ = e.client_order_ref;
    let _ = e.order_id;
    let _ = e.rejected;
    let _ = e.reason;
}

// ─── Tick forwarding ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct TickEvent<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    instrument_id: u64,
    /// "bid" / "ask" / "last" / "bid_size" / "ask_size" / "volume"
    kind: &'a str,
    price: Option<f64>,
    size: Option<u64>,
    timestamp_ns: u64,
}

async fn forward_ticks(runtime: Arc<Runtime>, server: Arc<SHMServer>) {
    let mut rx = {
        let b = runtime.broker.lock().await;
        b.subscribe_ticks_stream()
    };
    log::info!("ipc forward_ticks: subscribed");
    loop {
        match rx.recv().await {
            Ok(tick) => {
                let kind_str = match tick.kind {
                    corsair_broker_api::TickKind::Bid => "bid",
                    corsair_broker_api::TickKind::Ask => "ask",
                    corsair_broker_api::TickKind::Last => "last",
                    corsair_broker_api::TickKind::BidSize => "bid_size",
                    corsair_broker_api::TickKind::AskSize => "ask_size",
                    corsair_broker_api::TickKind::Volume => "volume",
                };
                let ev = TickEvent {
                    ty: "tick",
                    instrument_id: tick.instrument_id.0,
                    kind: kind_str,
                    price: tick.price,
                    size: tick.size,
                    timestamp_ns: tick.timestamp_ns,
                };
                if let Ok(body) = rmp_serde::to_vec_named(&ev) {
                    if !server.publish(&body) {
                        // Tick stream is high-volume; only log dropped
                        // frames every now and then to avoid log flood.
                        log::debug!("ipc events ring full — dropped tick");
                    }
                }
            }
            Err(RecvError::Lagged(n)) => log::warn!("forward_ticks: lagged {n}"),
            Err(RecvError::Closed) => break,
        }
    }
}

// ─── Periodic risk_state publish ─────────────────────────────────

#[derive(Debug, Serialize)]
struct RiskStateEvent {
    #[serde(rename = "type")]
    ty: &'static str,
    ts_ns: u64,
    margin_usd: f64,
    margin_pct: f64,
    options_delta: f64,
    hedge_delta: i32,
    effective_delta: f64,
    theta: f64,
    vega: f64,
    gamma: f64,
    total_contracts: i64,
    n_positions: u32,
}

/// Publish risk aggregates to the trader at 1Hz. Mirrors
/// `BrokerIPC.publish_risk_state` in Python — same field names so
/// the trader's existing self-gating logic works unchanged.
async fn periodic_risk_state(runtime: Arc<Runtime>, server: Arc<SHMServer>) {
    let mut t = tokio::time::interval(std::time::Duration::from_secs(1));
    log::info!("ipc periodic_risk_state: cadence 1s");
    let capital = runtime.config.constraints.capital;
    loop {
        t.tick().await;
        // Acquire portfolio + hedge briefly for state.
        let (options_delta, theta, vega, gamma, total_contracts, n_positions) = {
            let p = runtime.portfolio.lock().unwrap();
            let agg = p.aggregate();
            let total_contracts: i64 = p
                .positions()
                .iter()
                .map(|pos| pos.quantity.abs() as i64)
                .sum();
            (
                agg.total.net_delta,
                agg.total.net_theta,
                agg.total.net_vega,
                agg.total.net_gamma,
                total_contracts,
                agg.total.gross_positions,
            )
        };
        let hedge_delta: i32 = {
            let h = runtime.hedge.lock().unwrap();
            // Sum hedge_qty across products.
            h.managers().iter().map(|m| m.hedge_qty()).sum()
        };
        let effective_delta = options_delta + (hedge_delta as f64);

        // Margin: poll account_values asynchronously; if too slow,
        // just skip this tick. (Account refreshes every ~5s in
        // IBKR's stream so this is best-effort.)
        let margin_usd = match {
            let b = runtime.broker.lock().await;
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                b.account_values(),
            )
            .await
        } {
            Ok(Ok(snap)) => snap.maintenance_margin,
            _ => 0.0, // unavailable this tick
        };
        let margin_pct = if capital > 0.0 {
            margin_usd / capital
        } else {
            0.0
        };

        let ev = RiskStateEvent {
            ty: "risk_state",
            ts_ns: now_ns(),
            margin_usd,
            margin_pct,
            options_delta,
            hedge_delta,
            effective_delta,
            theta,
            vega,
            gamma,
            total_contracts,
            n_positions,
        };
        if let Ok(body) = rmp_serde::to_vec_named(&ev) {
            if !server.publish(&body) {
                log::warn!("ipc events ring full — dropped risk_state");
            }
        }
    }
}

fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
