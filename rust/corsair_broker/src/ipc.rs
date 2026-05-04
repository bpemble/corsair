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
    OrderType, PlaceOrderReq, Right, Side, TickKind, TimeInForce,
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
    let runtime_for_closure = Arc::clone(runtime);
    // Per-instrument bid/ask/size cache shared across publisher invocations.
    // Mutex lock is brief (HashMap entry update + msgpack encode) and the
    // tick rate is low enough that contention isn't a concern.
    //
    // LOW-005: The publisher is invoked from a SINGLE tokio task —
    // NativeBroker's spawn_dispatcher. If that ever becomes
    // multi-threaded (e.g. broadcast-fanout to multiple consumers
    // running in parallel tasks), this cache needs the per-instrument
    // bid/ask/size state to be atomically updated together. Today the
    // single-task invariant guarantees we never observe a torn state.
    let cache: Arc<std::sync::Mutex<std::collections::HashMap<u64, ConsolidatedTick>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let publisher: std::sync::Arc<dyn for<'a> Fn(&'a corsair_broker_api::Tick) + Send + Sync + 'static> =
        std::sync::Arc::new(move |tick: &corsair_broker_api::Tick| {
            // Trader only consumes bid/ask + sizes (consolidates them into
            // OptionState). Skip volume.
            if !matches!(
                tick.kind,
                TickKind::Bid | TickKind::Ask | TickKind::Last | TickKind::BidSize | TickKind::AskSize
            ) {
                return;
            }
            // Underlying-tick fork: if this instrument is registered as
            // an underlying for some product, emit "underlying_tick"
            // with the price and return. The trader's UnderlyingTickMsg
            // consumer drives state.underlying_price, which gates
            // decide_on_tick (forward must be > 0). Without this fork
            // the trader stays at forward=0 forever (latent since
            // cutover; broker.market_data populated correctly for the
            // hedge subsystem, but no IPC emission to the trader).
            let underlying_product = {
                let md = runtime_for_closure.market_data.lock().unwrap();
                md.product_for_underlying(tick.instrument_id)
            };
            if underlying_product.is_some() {
                if matches!(tick.kind, TickKind::Bid | TickKind::Ask | TickKind::Last) {
                    if let Some(price) = tick.price {
                        if price > 0.0 {
                            let ev = UnderlyingTickEvent {
                                ty: "underlying_tick",
                                price,
                                ts_ns: tick.timestamp_ns,
                            };
                            if let Ok(body) = rmp_serde::to_vec_named(&ev) {
                                if !server_for_closure.publish(&body) {
                                    log::debug!("underlying tick fastpath: events ring full");
                                }
                            }
                        }
                    }
                }
                return;
            }
            // BidSize/AskSize on options need to update the cache too,
            // but option_meta lookup below filters them. Same as before.
            if !matches!(
                tick.kind,
                TickKind::Bid | TickKind::Ask | TickKind::BidSize | TickKind::AskSize
            ) {
                return;
            }
            let meta = {
                let md = runtime_for_closure.market_data.lock().unwrap();
                md.option_meta(tick.instrument_id)
            };
            let Some(meta) = meta else { return };
            let mut cache_guard = cache.lock().unwrap();
            let entry = cache_guard.entry(tick.instrument_id.0).or_insert_with(|| {
                ConsolidatedTick {
                    expiry: meta.expiry.format("%Y%m%d").to_string(),
                    right: match meta.right {
                        Right::Call => "C",
                        Right::Put => "P",
                    },
                    strike: meta.strike,
                    bid: None,
                    ask: None,
                    bid_size: None,
                    ask_size: None,
                }
            });
            match tick.kind {
                TickKind::Bid => entry.bid = tick.price,
                TickKind::Ask => entry.ask = tick.price,
                TickKind::BidSize => entry.bid_size = tick.size.map(|s| s as i32),
                TickKind::AskSize => entry.ask_size = tick.size.map(|s| s as i32),
                _ => return,
            }
            let ev = TickEvent {
                ty: "tick",
                strike: entry.strike,
                expiry: &entry.expiry,
                right: entry.right,
                bid: entry.bid,
                ask: entry.ask,
                bid_size: entry.bid_size,
                ask_size: entry.ask_size,
                // ts_ns is the broker dispatcher's channel-recv time
                // (closest broker-internal proxy for TCP recv); same
                // value reused for v2 wire-timing's broker_recv_ns.
                ts_ns: tick.timestamp_ns,
                broker_recv_ns: tick.timestamp_ns,
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

/// Trader → broker: place_order command.
///
/// Schema mirrors what the Rust trader's `PlaceOrder` struct serializes
/// (rust/corsair_trader/src/messages.rs). The trader doesn't carry an
/// instrument_id (broker→trader TickEvent was de-instrument_id'd in
/// commit f85cb7c to consolidate around strike/expiry/right) — so the
/// broker resolves the contract by (strike, expiry, right) at decode
/// time. The pre-f85cb7c instrument_id-keyed schema was a latent
/// regression that would have blocked all trader-driven orders at the
/// next market open.
#[derive(Debug, Deserialize)]
struct PlaceOrderCmd {
    #[serde(rename = "type")]
    _ty: String,
    strike: f64,
    /// YYYYMMDD, matches what trader received in TickEvent.expiry.
    expiry: String,
    /// "C" or "P", matches what trader received in TickEvent.right.
    right: String,
    side: String,         // "BUY" or "SELL"
    qty: i32,             // trader serializes i32; positive at runtime
    price: f64,
    /// Trader sends as `orderRef`; alias accepts either spelling.
    #[serde(default, alias = "orderRef")]
    client_order_ref: Option<String>,
    /// Order type: "limit" or "market". Default "limit".
    #[serde(default = "default_order_type")]
    order_type: String,
    /// Time in force: "GTD", "IOC", etc.
    #[serde(default = "default_tif")]
    tif: String,
    /// goodTillDate seconds from now — used when tif=GTD.
    #[serde(default)]
    gtd_seconds: Option<u32>,
    /// v2 wire-timing: trader's local clock when decide_quote returned
    /// Place. Same value as PlaceOrder.ts_ns on the trader side.
    #[serde(default)]
    ts_ns: Option<u64>,
    /// v2 wire-timing: broker_recv_ns of the TickEvent the trader
    /// acted on. Echoed back from TickEvent.broker_recv_ns. None when
    /// the trader didn't have a triggering tick (e.g. boot orders).
    #[serde(default)]
    triggering_tick_broker_recv_ns: Option<u64>,
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

// ─── Command dispatch ────────────────────────────────────────────

async fn dispatch_commands(
    runtime: Arc<Runtime>,
    mut rx: tokio::sync::mpsc::Receiver<ServerCommand>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd.kind.as_str() {
            // Spawn each order command so it doesn't serialize behind
            // the previous one's IBKR ack. handle_place awaits a 2s
            // timeout for OpenOrder/OrderStatus internally; without
            // spawning, a burst of N place commands stacks up at
            // (sum of RTTs) wall-clock — observed 45-77ms IPC tail
            // before this fix when cancel-replace cycles fired bursts.
            // The IBKR client's per-orderId waiter ensures each
            // task's ack routes correctly without lock-contention.
            "place_order" => {
                let r = runtime.clone();
                tokio::spawn(async move { handle_place(&r, &cmd.body).await });
            }
            "cancel_order" => {
                let r = runtime.clone();
                tokio::spawn(async move { handle_cancel(&r, &cmd.body).await });
            }
            "modify_order" => {
                let r = runtime.clone();
                tokio::spawn(async move { handle_modify(&r, &cmd.body).await });
            }
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
    let broker_order_recv_ns = now_ns();
    let cmd: PlaceOrderCmd = match rmp_serde::from_slice(body) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("ipc place_order: parse error: {e}");
            return;
        }
    };
    if matches!(runtime.mode, crate::runtime::RuntimeMode::Shadow) {
        log::info!(
            "ipc place_order (SHADOW; not placing): {} {}{}@{} {} @ {} qty {}",
            cmd.side,
            cmd.strike,
            cmd.right,
            cmd.expiry,
            cmd.side,
            cmd.price,
            cmd.qty
        );
        return;
    }
    // Resolve the contract from the broker's contract cache by
    // (strike, expiry, right). Trader carries those fields on the
    // wire; instrument_id is broker-internal and never round-trips.
    // Expiry comparison is YYYYMMDD strings since both sides format
    // the same way.
    let r_norm = cmd.right.chars().next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('C');
    let contract = {
        let md = runtime.market_data.lock().unwrap();
        let products = runtime.portfolio.lock().unwrap().registry().products();
        let mut found_iid: Option<corsair_broker_api::InstrumentId> = None;
        for prod in products {
            for t in md.options_for_product(&prod) {
                let t_right_char = match t.right {
                    corsair_broker_api::Right::Call => 'C',
                    corsair_broker_api::Right::Put => 'P',
                };
                let t_expiry_str = t.expiry.format("%Y%m%d").to_string();
                if (t.strike - cmd.strike).abs() < 1e-9
                    && t_expiry_str == cmd.expiry
                    && t_right_char == r_norm
                {
                    found_iid = t.instrument_id;
                    break;
                }
            }
            if found_iid.is_some() {
                break;
            }
        }
        // Look up the FULL qualified Contract (with IBKR-canonical
        // local_symbol + trading_class). This is what subscribe_product
        // populated when it called qualify_option. Synthesizing the
        // Contract from scratch caused IBKR to reject every place_order
        // with cryptic errors like 110 "VOL volatility" because the
        // server cross-checks fields against conId and our placeholder
        // values didn't match.
        let cached = found_iid.and_then(|iid| {
            runtime.qualified_contracts.lock().unwrap().get(&iid).cloned()
        });
        match cached {
            Some(c) => c,
            None => {
                log::warn!(
                    "ipc place_order: no contract cached for {}{:.4} {} {}",
                    r_norm, cmd.strike, cmd.expiry, cmd.side
                );
                // Still emit a wire_timing row so the failure shows up
                // in the histogram as outcome=rejected.
                let row = serde_json::json!({
                    "schema": "wire_timing/v1",
                    "ts_ns": now_ns(),
                    "outcome": "no_contract",
                    "order_id": serde_json::Value::Null,
                    "client_order_ref": cmd.client_order_ref.clone().unwrap_or_default(),
                    "strike": cmd.strike,
                    "expiry": cmd.expiry,
                    "right": cmd.right,
                    "side": cmd.side,
                    "qty": cmd.qty,
                    "price": cmd.price,
                    "triggering_tick_broker_recv_ns": cmd.triggering_tick_broker_recv_ns,
                    "trader_decide_ts_ns": cmd.ts_ns,
                    "broker_order_recv_ns": broker_order_recv_ns,
                });
                runtime.wire_timing.write(row);
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
        qty: cmd.qty.max(0) as u32,
        order_type,
        price: Some(cmd.price),
        tif,
        gtd_until_utc,
        client_order_ref: cmd.client_order_ref.clone().unwrap_or_default(),
        account: Some(runtime.config.broker.ibkr.as_ref()
            .map(|i| i.account.clone()).unwrap_or_default()),
    };
    let broker_order_send_marker_ns = now_ns();
    let (result, precise_send_ns, precise_ack_ns) = {
        let b = runtime.broker.lock().await;
        let r = b.place_order(req).await;
        // Drain precise broker-internal timestamps (NativeBroker only;
        // mock returns None). On success this is the just-before
        // client.send_raw moment + first ack arrival in route().
        let timing = match &r {
            Ok(oid) => b.drain_wire_timing(oid.0),
            Err(_) => None,
        };
        let (s, a) = timing.unzip();
        (r, s, a)
    };
    let broker_order_ack_marker_ns = now_ns();
    // Prefer precise timestamps when available; fall back to call-
    // boundary markers otherwise.
    let broker_order_send_ns = precise_send_ns.unwrap_or(broker_order_send_marker_ns);
    let broker_order_ack_ns = precise_ack_ns.unwrap_or(broker_order_ack_marker_ns);

    // v2 wire-timing — emit one JSONL row per place outcome. Stages
    // computed by post-processor:
    //   tick_to_decide  = trader_decide_ts_ns − triggering_tick_broker_recv_ns
    //   trader_to_broker = broker_order_recv_ns − trader_decide_ts_ns
    //   broker_handle   = broker_order_send_marker_ns − broker_order_recv_ns
    //   external_rtt    = broker_order_ack_marker_ns − broker_order_send_marker_ns
    //   total           = broker_order_ack_marker_ns − triggering_tick_broker_recv_ns
    let (outcome_str, order_id_val): (&str, Option<u64>) = match &result {
        Ok(oid) => ("ack", Some(oid.0)),
        Err(_) => ("rejected", None),
    };
    let row = serde_json::json!({
        "schema": "wire_timing/v2",
        "ts_ns": broker_order_ack_ns,
        "outcome": outcome_str,
        "order_id": order_id_val,
        "client_order_ref": cmd.client_order_ref.clone().unwrap_or_default(),
        "strike": cmd.strike,
        "expiry": cmd.expiry,
        "right": cmd.right,
        "side": cmd.side,
        "qty": cmd.qty,
        "price": cmd.price,
        "triggering_tick_broker_recv_ns": cmd.triggering_tick_broker_recv_ns,
        "trader_decide_ts_ns": cmd.ts_ns,
        "broker_order_recv_ns": broker_order_recv_ns,
        "broker_order_send_ns": broker_order_send_ns,
        "broker_order_ack_ns": broker_order_ack_ns,
        // Markers retained for diagnostics: the gap between precise
        // and marker reveals place_order setup overhead (~30 µs)
        // and dispatcher wake-up latency (~10-50 µs).
        "broker_order_send_marker_ns": broker_order_send_marker_ns,
        "broker_order_ack_marker_ns": broker_order_ack_marker_ns,
        "send_ns_precise": precise_send_ns.is_some(),
        "ack_ns_precise": precise_ack_ns.is_some(),
    });
    runtime.wire_timing.write(row);

    // Push latency samples for the dashboard's TTT/RTT pill. Only on
    // successful acks — rejected orders skew the rolling window.
    //
    // Definitions (match the Python broker / industry convention):
    //   TTT = trigger tick recv → broker SENDS place_order to IBKR
    //         (purely our hot-path work: trader decide + IPC + broker
    //         frame build + TCP write_all). Tens of µs in steady state.
    //   RTT = broker sends → broker receives ack
    //         (purely IBKR's network + processing). Tens of ms.
    //
    // TTT < RTT is the expected ordering: our internal compute is much
    // faster than the network round-trip to IBKR. (An earlier version
    // accidentally computed TTT = tick → ACK, which inverted the
    // ordering by including the RTT inside the TTT measurement.)
    if matches!(result, Ok(_)) {
        let place_rtt_us = (broker_order_ack_marker_ns
            .saturating_sub(broker_order_send_marker_ns))
            / 1000;
        let trigger_ns = cmd
            .triggering_tick_broker_recv_ns
            .unwrap_or(broker_order_recv_ns);
        let ttt_us = (broker_order_send_marker_ns.saturating_sub(trigger_ns)) / 1000;
        let mut s = runtime.latency_samples.lock().unwrap();
        s.push_ttt(ttt_us);
        s.push_place_rtt(place_rtt_us);
    }

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

// ─── Tick forwarding ─────────────────────────────────────────────

/// Trader-facing consolidated tick. Trader's `TickMsg`
/// (`corsair_trader::messages::TickMsg`) requires `strike`, `expiry`,
/// `right` (no defaults) and overwrites its OptionState entry on each
/// tick — so we need consolidated bid/ask/sizes here, not single-side.
/// Underlying-tick event emitted on bid/ask/last for the registered
/// underlying. Trader's UnderlyingTickMsg consumes this and sets
/// state.underlying_price, which is the gate forward for decide_on_tick.
#[derive(Debug, Serialize)]
struct UnderlyingTickEvent<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    price: f64,
    ts_ns: u64,
}

#[derive(Debug, Serialize)]
struct TickEvent<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    strike: f64,
    /// YYYYMMDD (matches what `vol_surface` emits, so trader's
    /// `(expiry, side_char)` lookup hits).
    expiry: &'a str,
    /// "C" or "P".
    right: &'a str,
    bid: Option<f64>,
    ask: Option<f64>,
    bid_size: Option<i32>,
    ask_size: Option<i32>,
    /// IBKR gateway tick timestamp (existing).
    ts_ns: u64,
    /// Broker's local clock at TickEvent publish time (v2 wire timing).
    /// Within ~5-30 µs of TCP-recv since broker dispatch is direct.
    /// Trader echoes this back on PlaceOrder.triggering_tick_broker_recv_ns
    /// so the wire_timing JSONL row can join tick → order.
    broker_recv_ns: u64,
}

/// Per-instrument bid/ask/size cache. Native ticks arrive per-kind
/// (Bid OR Ask OR BidSize OR AskSize); we accumulate here and emit a
/// consolidated frame on every update so the trader can overwrite
/// OptionState without losing the other side.
struct ConsolidatedTick {
    expiry: String,
    right: &'static str,
    strike: f64,
    bid: Option<f64>,
    ask: Option<f64>,
    bid_size: Option<i32>,
    ask_size: Option<i32>,
}

async fn forward_ticks(runtime: Arc<Runtime>, server: Arc<SHMServer>) {
    let mut rx = {
        let b = runtime.broker.lock().await;
        b.subscribe_ticks_stream()
    };
    log::info!("ipc forward_ticks: subscribed");
    let mut cache: std::collections::HashMap<u64, ConsolidatedTick> =
        std::collections::HashMap::new();
    loop {
        match rx.recv().await {
            Ok(tick) => {
                // Underlying-tick fork (mirrors the fast-path; this
                // pump is the broadcast-channel fallback when the fast
                // publisher returns false).
                let underlying_product = {
                    let md = runtime.market_data.lock().unwrap();
                    md.product_for_underlying(tick.instrument_id)
                };
                if underlying_product.is_some() {
                    if matches!(tick.kind, TickKind::Bid | TickKind::Ask | TickKind::Last) {
                        if let Some(price) = tick.price {
                            if price > 0.0 {
                                let ev = UnderlyingTickEvent {
                                    ty: "underlying_tick",
                                    price,
                                    ts_ns: tick.timestamp_ns,
                                };
                                if let Ok(body) = rmp_serde::to_vec_named(&ev) {
                                    if !server.publish(&body) {
                                        log::debug!("ipc events ring full — dropped underlying tick");
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }
                // Skip tick kinds the trader doesn't consume.
                let is_consumable = matches!(
                    tick.kind,
                    TickKind::Bid | TickKind::Ask | TickKind::BidSize | TickKind::AskSize
                );
                if !is_consumable {
                    continue;
                }
                // Resolve instrument_id → option meta. Unregistered
                // ids return None — silently skipped.
                let meta = {
                    let md = runtime.market_data.lock().unwrap();
                    md.option_meta(tick.instrument_id)
                };
                let Some(meta) = meta else { continue };
                let entry = cache.entry(tick.instrument_id.0).or_insert_with(|| {
                    ConsolidatedTick {
                        expiry: meta.expiry.format("%Y%m%d").to_string(),
                        right: match meta.right {
                            Right::Call => "C",
                            Right::Put => "P",
                        },
                        strike: meta.strike,
                        bid: None,
                        ask: None,
                        bid_size: None,
                        ask_size: None,
                    }
                });
                match tick.kind {
                    TickKind::Bid => entry.bid = tick.price,
                    TickKind::Ask => entry.ask = tick.price,
                    TickKind::BidSize => entry.bid_size = tick.size.map(|s| s as i32),
                    TickKind::AskSize => entry.ask_size = tick.size.map(|s| s as i32),
                    _ => unreachable!(),
                }
                let ev = TickEvent {
                    ty: "tick",
                    strike: entry.strike,
                    expiry: &entry.expiry,
                    right: entry.right,
                    bid: entry.bid,
                    ask: entry.ask,
                    bid_size: entry.bid_size,
                    ask_size: entry.ask_size,
                    // v2 wire-timing — both fields = channel-recv time
                    // (closer to TCP recv than the previous now_ns() at
                    // publish point, which lumped in the broadcast hop).
                    ts_ns: tick.timestamp_ns,
                    broker_recv_ns: tick.timestamp_ns,
                };
                if let Ok(body) = rmp_serde::to_vec_named(&ev) {
                    if !server.publish(&body) {
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
    /// i64 to match trader's RiskStateMsg.hedge_delta exactly. Was
    /// i32 (silent msgpack widening on the trader side); unifying so
    /// the wire schema is canonical and an integer-overflow scenario
    /// (impossible at our position sizes, but cheap to bound) doesn't
    /// truncate. Audit HI-001.
    hedge_delta: i64,
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
        let hedge_delta: i64 = {
            let h = runtime.hedge.lock().unwrap();
            // Sum hedge_qty across products. i64 matches trader.
            // Audit T1-5: fail closed if any manager's state is stale
            // (>300s without reconcile/fill). Effective-delta gating
            // depends on this being a trustworthy IBKR-confirmed view.
            const HEDGE_STATE_MAX_AGE_NS: u64 = 300_000_000_000;
            let now = now_ns();
            h.managers()
                .iter()
                .map(|m| {
                    if m.state().is_fresh(now, HEDGE_STATE_MAX_AGE_NS) {
                        m.hedge_qty() as i64
                    } else {
                        // Stale — pretend hedge_qty is 0 so combined
                        // gate falls back to options-only (the safe
                        // pre-§14 behavior).
                        0
                    }
                })
                .sum()
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

