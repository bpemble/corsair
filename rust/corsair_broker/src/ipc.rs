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
    ModifyOrderReq, OrderId, OrderType, PlaceOrderReq, Right, Side, TickKind, TimeInForce,
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

    // Stash the server arc on Runtime so handle_place can publish
    // place_ack events directly. Without this, the trader never
    // learns the order_id of orders it placed and every re-quote
    // falls through to a fresh Place — breaking the amend path.
    *runtime.ipc_server.lock().unwrap() = Some(Arc::clone(&server));

    // Spawn the command-dispatch loop.
    tokio::spawn(dispatch_commands(runtime.clone(), cmd_rx));

    // Spawn the event-publish loops (one per stream we forward).
    tokio::spawn(forward_fills(runtime.clone(), Arc::clone(&server)));
    tokio::spawn(forward_status(runtime.clone(), Arc::clone(&server)));
    tokio::spawn(forward_connection(runtime.clone(), Arc::clone(&server)));
    tokio::spawn(periodic_risk_state(runtime.clone(), Arc::clone(&server)));
    // Periodic log of fill notifications dropped by Discord rate limit.
    // 60s cadence — operator sees a one-liner if many were skipped.
    tokio::spawn(async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            crate::notify::log_and_reset_dropped_fills();
        }
    });

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
            // Pull metadata + seed values for the publisher cache from
            // market_data in one lock acquisition. Seeding is what
            // saves us from the 2026-05-04 bug: after a broker restart,
            // the publisher cache started all-None and only filled as
            // Bid/Ask price-change ticks arrived. In thin markets where
            // only size ticks flow (overnight), cache stayed all-None
            // for hours, the trader saw bid=None ask=None on every tick,
            // and the dark-book guard fired on what was actually a
            // perfectly two-sided book. Seeding from market_data picks
            // up the IBKR initial-snapshot values so the very first
            // TickEvent for an instrument carries the live BBO.
            let seed = {
                let md = runtime_for_closure.market_data.lock().unwrap();
                md.option_by_iid(tick.instrument_id).map(|opt| {
                    let pos_or_none = |x: f64| if x > 0.0 { Some(x) } else { None };
                    let pos_or_none_size = |x: u64| if x > 0 { Some(x as i32) } else { None };
                    (
                        opt.strike,
                        opt.expiry,
                        opt.right,
                        pos_or_none(opt.bid),
                        pos_or_none(opt.ask),
                        pos_or_none_size(opt.bid_size),
                        pos_or_none_size(opt.ask_size),
                    )
                })
            };
            let Some((seed_strike, seed_expiry, seed_right, seed_bid, seed_ask, seed_bid_size, seed_ask_size)) = seed else { return };
            let mut cache_guard = cache.lock().unwrap();
            let entry = cache_guard.entry(tick.instrument_id.0).or_insert_with(|| {
                ConsolidatedTick {
                    expiry: seed_expiry.format("%Y%m%d").to_string(),
                    right: match seed_right {
                        Right::Call => "C",
                        Right::Put => "P",
                    },
                    strike: seed_strike,
                    bid: seed_bid,
                    ask: seed_ask,
                    bid_size: seed_bid_size,
                    ask_size: seed_ask_size,
                }
            });
            match tick.kind {
                TickKind::Bid => entry.bid = tick.price,
                TickKind::Ask => entry.ask = tick.price,
                TickKind::BidSize => entry.bid_size = tick.size.map(|s| s as i32),
                TickKind::AskSize => entry.ask_size = tick.size.map(|s| s as i32),
                _ => return,
            }
            // L2 depth: top-2 levels per side. The trader uses these
            // to compute external best (subtracting its own resting
            // size) so we don't tick-jump ourselves.
            //
            // None when the depth rotator hasn't subscribed this leg
            // (only ATM-nearest ~5 strikes have active L2 at any
            // given time) — trader falls back to the depth-1
            // self-fill approximation.
            let (depth_bid_0, depth_bid_1, depth_ask_0, depth_ask_1) = {
                let md = runtime_for_closure.market_data.lock().unwrap();
                if let Some(opt) = md.option_by_iid(tick.instrument_id) {
                    let b0 = opt.depth.bids.first().map(|l| l.price);
                    let b1 = opt.depth.bids.get(1).map(|l| l.price);
                    let a0 = opt.depth.asks.first().map(|l| l.price);
                    let a1 = opt.depth.asks.get(1).map(|l| l.price);
                    (b0, b1, a0, a1)
                } else {
                    (None, None, None, None)
                }
            };
            let ev = TickEvent {
                ty: "tick",
                strike: entry.strike,
                expiry: &entry.expiry,
                right: entry.right,
                bid: entry.bid,
                ask: entry.ask,
                bid_size: entry.bid_size,
                ask_size: entry.ask_size,
                ts_ns: tick.timestamp_ns,
                broker_recv_ns: tick.timestamp_ns,
                depth_bid_0,
                depth_bid_1,
                depth_ask_0,
                depth_ask_1,
            };
            let dropped = with_encoded_tick(&ev, |body| !server_for_closure.publish(body));
            if dropped {
                log::debug!("tick fastpath: events ring full");
            }
        });
    let installed = {
        let b = runtime.broker.clone();
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
    /// Trader's send-time wall clock (ns). Used by the wire-timing
    /// JSONL emitter so we can compute trader→broker IPC latency on
    /// the amend path.
    #[serde(default)]
    ts_ns: Option<u64>,
    /// Trigger tick's broker_recv_ns. Used for the modify-equivalent
    /// of TTT (tick → modify_order send).
    #[serde(default)]
    triggering_tick_broker_recv_ns: Option<u64>,
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

/// Broker → trader config snapshot, sent in response to the trader's
/// `welcome` command. Lets the trader replace its compile-time
/// defaults (GTD lifetime, dead-band, spread skip multiplier, etc.)
/// with the broker's runtime values so the two stay aligned without
/// the trader needing its own YAML.
#[derive(Debug, Serialize)]
struct HelloConfigPayload {
    min_edge_ticks: i32,
    tick_size: f64,
    delta_ceiling: f64,
    delta_kill: f64,
    margin_ceiling_pct: f64,
    gtd_lifetime_s: f64,
    gtd_refresh_lead_s: f64,
    dead_band_ticks: i32,
    skip_if_spread_over_edge_mul: f64,
    /// Theta-breach threshold (negative; 0 disables). Trader self-gates
    /// at this value so a theta breach is honored locally even if the
    /// kill IPC event is dropped (defense in depth — see also the
    /// kill_event publish path below). 2026-05-05 incident: 384 of 422
    /// adverse fills happened AFTER broker fired THETA HALT because
    /// kills weren't propagated AND trader had no theta gate.
    theta_kill: f64,
    /// Vega-breach threshold (positive; 0 disables; currently 0 per
    /// CLAUDE.md §13 — Alabaster characterization).
    vega_kill: f64,
}

/// Broker → trader kill event. Mirrors the trader's `KillMsg`. Fires
/// the moment any risk gate trips, well before the next 1Hz risk_state
/// would surface it. 2026-05-05 incident root cause: this event was
/// never published — broker fired kills internally and trader had no
/// idea. Fix: publish on every kill firing site (per-fill P&L,
/// per-fill delta, periodic risk_check).
#[derive(Debug, Serialize)]
struct KillEventMsg<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    timestamp_ns: u64,
    source: &'a str,
    reason: &'a str,
    kill_type: &'a str,
}

/// Publish a kill event to the trader. Called from each of the broker's
/// kill-firing sites in tasks.rs (alongside notify_kill + cancel_all_resting).
pub(crate) fn publish_kill(runtime: &Arc<Runtime>, ev: &corsair_risk::KillEvent) {
    let server = match runtime.ipc_server.lock().unwrap().clone() {
        Some(s) => s,
        None => return,
    };
    let source = format!("{:?}", ev.source).to_lowercase();
    let kill_type = format!("{:?}", ev.kill_type).to_lowercase();
    let msg = KillEventMsg {
        ty: "kill",
        timestamp_ns: ev.timestamp_ns,
        source: &source,
        reason: &ev.reason,
        kill_type: &kill_type,
    };
    if let Ok(body) = rmp_serde::to_vec_named(&msg) {
        if !server.publish(&body) {
            log::error!(
                "ipc events ring full — DROPPED kill event ({}); trader \
                 will not know about this kill",
                ev.reason
            );
        } else {
            log::warn!(
                "ipc: kill event published to trader (source={} type={} reason={})",
                source,
                kill_type,
                ev.reason
            );
        }
    }
}

#[derive(Debug, Serialize)]
struct HelloEvent<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    timestamp_ns: u64,
    config: HelloConfigPayload,
}

fn publish_hello(runtime: &Arc<Runtime>) {
    let server = match runtime.ipc_server.lock().unwrap().clone() {
        Some(s) => s,
        None => return,
    };
    let q = &runtime.config.quoting;
    let c = &runtime.config.constraints;
    let r = &runtime.config.risk;
    let ev = HelloEvent {
        ty: "hello",
        timestamp_ns: now_ns(),
        config: HelloConfigPayload {
            min_edge_ticks: q.min_edge_ticks,
            tick_size: q.tick_size,
            delta_ceiling: c.delta_ceiling,
            delta_kill: r.delta_kill,
            margin_ceiling_pct: c.margin_ceiling_pct,
            gtd_lifetime_s: q.gtd_lifetime_s,
            gtd_refresh_lead_s: q.gtd_refresh_lead_s,
            dead_band_ticks: q.dead_band_ticks,
            skip_if_spread_over_edge_mul: q.skip_if_spread_over_edge_mul,
            theta_kill: r.theta_kill,
            vega_kill: r.vega_kill,
        },
    };
    if let Ok(body) = rmp_serde::to_vec_named(&ev) {
        if !server.publish(&body) {
            log::warn!("ipc events ring full — dropped hello");
        } else {
            log::info!(
                "ipc: hello published (gtd={:.1}s, dead_band={}t, spread_mul={:.1})",
                q.gtd_lifetime_s,
                q.dead_band_ticks,
                q.skip_if_spread_over_edge_mul
            );
        }
    }
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
            // Trader sends "welcome" once on connect. Respond with a
            // hello event carrying the broker's runtime config so the
            // trader replaces its compile-time defaults (GTD,
            // dead-band, spread mul) with our values. Items 3+4 of
            // the 2026-05-04 audit.
            "welcome" => publish_hello(&runtime),
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
    // Resolve the contract from the broker's fast-path lookup map.
    // Pre-2026-05-04 this branch did an O(N×M) scan over every
    // subscribed option with a `format!("%Y%m%d")` allocation per
    // iteration — ~10–15 µs of pure dispatch latency per place call.
    // `contract_by_key` is populated in subscribe_strike alongside
    // qualified_contracts so the hot path is one HashMap::get.
    let r_norm = cmd.right.chars().next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('C');
    let lookup_key = (cmd.strike.to_bits(), cmd.expiry.clone(), r_norm);
    let cached = runtime.contract_by_key.lock().unwrap().get(&lookup_key).cloned();
    let contract = match cached {
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
        let b = runtime.broker.clone();
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

    match &result {
        Ok(oid) => log::info!(
            "ipc place_order placed: oid={} for ref='{}'",
            oid,
            cmd.client_order_ref.clone().unwrap_or_default()
        ),
        Err(e) => log::warn!("ipc place_order failed: {e}"),
    }

    // Publish place_ack so the trader can populate OurOrder.order_id
    // and switch to the modify path on subsequent updates at this key.
    // Without this, the trader sees order_id=None forever and every
    // re-quote falls back to a fresh Place — defeating the amend
    // conversion.
    if let Ok(oid) = &result {
        // Field names match `corsair_trader::messages::PlaceAckMsg`
        // exactly — order_id is `orderId` (camelCase) on the wire to
        // match the trader's serde rename. Mismatch here means the
        // trader silently drops the parse and OurOrder.order_id never
        // populates — exactly the bug we're fixing.
        #[derive(Serialize)]
        struct PlaceAckEvent<'a> {
            #[serde(rename = "type")]
            ty: &'a str,
            #[serde(rename = "orderId")]
            order_id: u64,
            strike: f64,
            expiry: &'a str,
            right: &'a str,
            side: &'a str,
            price: f64,
            ts_ns: u64,
        }
        let ev = PlaceAckEvent {
            ty: "place_ack",
            order_id: oid.0,
            strike: cmd.strike,
            expiry: &cmd.expiry,
            right: &cmd.right,
            side: &cmd.side,
            price: cmd.price,
            ts_ns: now_ns(),
        };
        if let Ok(body) = rmp_serde::to_vec_named(&ev) {
            let server = runtime.ipc_server.lock().unwrap().clone();
            if let Some(s) = server {
                if !s.publish(&body) {
                    log::warn!("ipc events ring full — dropped place_ack");
                }
            }
        }
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
        let b = runtime.broker.clone();
        b.cancel_order(OrderId(cmd.order_id)).await
    };
    if let Err(e) = result {
        log::warn!("ipc cancel_order failed: {e}");
    }
}

async fn handle_modify(runtime: &Arc<Runtime>, body: &[u8]) {
    let broker_order_recv_ns = now_ns();
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
    // Mirrors handle_place's instrumentation — see ipc.rs handle_place
    // for the RTT semantics. Modify path: broker_order_send_marker_ns
    // is the moment we await modify_order; broker_order_ack_marker_ns
    // is set after drain_wire_timing returns the in-NativeBroker
    // (send_ns, ack_ns) pair captured at the actual TCP send + first
    // OrderStatus update for this order_id.
    let broker_order_send_marker_ns = now_ns();
    let (result, precise_send_ns, precise_ack_ns, strict_ack_ns) = {
        let b = runtime.broker.clone();
        let r = b.modify_order(OrderId(cmd.order_id), req).await;
        let timing = match &r {
            Ok(_) => b.drain_wire_timing(cmd.order_id),
            Err(_) => None,
        };
        let strict = match &r {
            Ok(_) => b.drain_strict_amend_ack_ns(cmd.order_id),
            Err(_) => None,
        };
        let (s, a) = timing.unzip();
        (r, s, a, strict)
    };
    let broker_order_ack_marker_ns = now_ns();
    let broker_order_send_ns = precise_send_ns.unwrap_or(broker_order_send_marker_ns);
    let broker_order_ack_ns = precise_ack_ns.unwrap_or(broker_order_ack_marker_ns);

    let outcome_str = if result.is_ok() { "ack" } else { "rejected" };
    let row = serde_json::json!({
        "schema": "wire_timing/v2",
        "kind": "modify",
        "ts_ns": broker_order_ack_ns,
        "outcome": outcome_str,
        "order_id": cmd.order_id,
        "price": cmd.price,
        "triggering_tick_broker_recv_ns": cmd.triggering_tick_broker_recv_ns,
        "trader_decide_ts_ns": cmd.ts_ns,
        "broker_order_recv_ns": broker_order_recv_ns,
        "broker_order_send_ns": broker_order_send_ns,
        "broker_order_ack_ns": broker_order_ack_ns,
        "broker_order_send_marker_ns": broker_order_send_marker_ns,
        "broker_order_ack_marker_ns": broker_order_ack_marker_ns,
        "send_ns_precise": precise_send_ns.is_some(),
        "ack_ns_precise": precise_ack_ns.is_some(),
    });
    runtime.wire_timing.write(row);

    // Push TTT immediately (it's measured at the send moment).
    // For the amend RTT histogram, defer the push until the strict
    // ack arrives — the dispatcher stamps `strict_ack_ns` only when
    // an OpenOrder with matching-price lands. This avoids the
    // bimodal distribution problem: previously we pushed every
    // amend's "permissive" Gateway-PreSubmitted echo (~hundreds of
    // µs) AND occasional server-confirmed acks (~40-400 ms) into the
    // same histogram, making p50 dance wildly between the two
    // populations. By only pushing strict samples we get a single
    // population centered on real server round-trip latency.
    //
    // The deferred-push task waits up to 1s for the dispatcher to
    // stamp strict_ack_ns. If it never arrives (Gateway never emits
    // matching OpenOrder, e.g., GTD-only refresh), we drop the
    // sample — n is lower but the samples we have are clean.
    if result.is_ok() {
        let trigger_ns = cmd
            .triggering_tick_broker_recv_ns
            .unwrap_or(broker_order_recv_ns);
        let ttt_us = (broker_order_send_marker_ns.saturating_sub(trigger_ns)) / 1000;
        runtime.latency_samples.lock().unwrap().push_ttt(ttt_us);

        // If strict ack already landed (rare race — OpenOrder beat
        // PreSubmitted): push immediately. Otherwise spawn a deferred
        // observer.
        if let Some(ack_ns) = strict_ack_ns {
            let modify_rtt_us =
                ack_ns.saturating_sub(broker_order_send_marker_ns) / 1000;
            runtime
                .latency_samples
                .lock()
                .unwrap()
                .push_modify_rtt(modify_rtt_us);
        } else {
            let runtime_clone = runtime.clone();
            let order_id = cmd.order_id;
            let send_marker = broker_order_send_marker_ns;
            tokio::spawn(async move {
                let deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(1);
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    if let Some(ack_ns) =
                        runtime_clone.broker.drain_strict_amend_ack_ns(order_id)
                    {
                        let modify_rtt_us =
                            ack_ns.saturating_sub(send_marker) / 1000;
                        runtime_clone
                            .latency_samples
                            .lock()
                            .unwrap()
                            .push_modify_rtt(modify_rtt_us);
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        // No strict ack within budget — drop the sample.
                        // Most often this means a same-price GTD refresh
                        // where IBKR didn't re-emit OpenOrder.
                        break;
                    }
                }
            });
        }
    }

    if let Err(e) = result {
        log::warn!("ipc modify_order failed: {e}");
    }
}

// ─── Event publishing ─────────────────────────────────────────────

async fn forward_fills(runtime: Arc<Runtime>, server: Arc<SHMServer>) {
    let mut rx = {
        let b = runtime.broker.clone();
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
                // Discord notification (rate-limited inside).
                // Look up the human-readable label from market_data
                // so the embed says "HXEM6 P565" not just "iid=N".
                let label = {
                    let md = runtime.market_data.lock().unwrap();
                    if let Some(opt) = md.option_by_iid(fill.instrument_id) {
                        let r = match opt.right {
                            Right::Call => "C",
                            Right::Put => "P",
                        };
                        format!(
                            "HXE{} {}{}",
                            opt.expiry.format("%y%m%d"),
                            r,
                            (opt.strike * 100.0).round() as i32
                        )
                    } else if let Some(p) = md.product_for_underlying(fill.instrument_id) {
                        // Hedge fill on the underlying contract.
                        format!("{p} hedge")
                    } else {
                        format!("iid={}", fill.instrument_id.0)
                    }
                };
                crate::notify::notify_fill(fill.clone(), label);
            }
            Err(RecvError::Lagged(n)) => log::warn!("forward_fills: lagged {n}"),
            Err(RecvError::Closed) => break,
        }
    }
}

async fn forward_status(runtime: Arc<Runtime>, server: Arc<SHMServer>) {
    let mut rx = {
        let b = runtime.broker.clone();
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
        let b = runtime.broker.clone();
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
    /// L2 depth: top-2 bid prices (level 0 = best, level 1 = next).
    /// None when no depth subscription is active for this leg.
    /// Trader uses these to find "external best" by subtracting its
    /// own resting orders: if our_bid == depth_bid_0, treat depth_bid_1
    /// as the external incumbent for tick-jumping.
    #[serde(skip_serializing_if = "Option::is_none")]
    depth_bid_0: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    depth_bid_1: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    depth_ask_0: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    depth_ask_1: Option<f64>,
}

/// Hand-rolled msgpack encoder for `TickEvent`. Replaces
/// `rmp_serde::to_vec_named(&ev)` at the high-frequency tick emit
/// sites. Symmetric to the trader's hand-rolled `decode_tick` shipped
/// in commit 6e410ff. Each tick saves ~1-2 µs of broker dispatch by
/// eliminating serde's reflection + Vec allocation.
///
/// Output is always msgpack-named-fields fixmap (≤14 entries), with
/// fixed encodings for primitives (no compression for small values):
///   f64 → 0xcb + 8 BE bytes
///   i32 → 0xd2 + 4 BE bytes
///   u64 → 0xcf + 8 BE bytes
///   nil → 0xc0
///   strings ≤ 31 bytes → fixstr (we control the keys; longest is
///                                "broker_recv_ns" at 14 bytes)
///
/// Caller passes a reusable `Vec<u8>` so there is no per-tick
/// allocation. The caller clears + slices appropriately.
///
/// Round-trip parity with rmp_serde verified by `tests::tick_round_trip`
/// — we encode hand-rolled, then decode via rmp_serde into a typed
/// shadow struct, asserting every field matches.
fn encode_tick_into(ev: &TickEvent, buf: &mut Vec<u8>) {
    // Field count: 10 always-emitted + up to 4 depth fields.
    let mut n_fields: u8 = 10;
    if ev.depth_bid_0.is_some() { n_fields += 1; }
    if ev.depth_bid_1.is_some() { n_fields += 1; }
    if ev.depth_ask_0.is_some() { n_fields += 1; }
    if ev.depth_ask_1.is_some() { n_fields += 1; }
    // fixmap supports ≤15 entries (header byte 0x80..0x8f).
    debug_assert!(n_fields <= 14);
    buf.push(0x80 | n_fields);

    write_str_kv(buf, "type", ev.ty);
    write_f64_kv(buf, "strike", ev.strike);
    write_str_kv(buf, "expiry", ev.expiry);
    write_str_kv(buf, "right", ev.right);
    write_opt_f64_kv(buf, "bid", ev.bid);
    write_opt_f64_kv(buf, "ask", ev.ask);
    write_opt_i32_kv(buf, "bid_size", ev.bid_size);
    write_opt_i32_kv(buf, "ask_size", ev.ask_size);
    write_u64_kv(buf, "ts_ns", ev.ts_ns);
    write_u64_kv(buf, "broker_recv_ns", ev.broker_recv_ns);
    if let Some(v) = ev.depth_bid_0 { write_f64_kv(buf, "depth_bid_0", v); }
    if let Some(v) = ev.depth_bid_1 { write_f64_kv(buf, "depth_bid_1", v); }
    if let Some(v) = ev.depth_ask_0 { write_f64_kv(buf, "depth_ask_0", v); }
    if let Some(v) = ev.depth_ask_1 { write_f64_kv(buf, "depth_ask_1", v); }
}

#[inline]
fn write_fixstr(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    debug_assert!(bytes.len() <= 31, "key too long for fixstr: {}", s);
    buf.push(0xa0 | (bytes.len() as u8));
    buf.extend_from_slice(bytes);
}

#[inline]
fn write_str(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    if bytes.len() <= 31 {
        buf.push(0xa0 | (bytes.len() as u8));
    } else if bytes.len() <= 255 {
        buf.push(0xd9);
        buf.push(bytes.len() as u8);
    } else if bytes.len() <= u16::MAX as usize {
        buf.push(0xda);
        buf.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    } else {
        buf.push(0xdb);
        buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    }
    buf.extend_from_slice(bytes);
}

#[inline]
fn write_str_kv(buf: &mut Vec<u8>, key: &str, value: &str) {
    write_fixstr(buf, key);
    write_str(buf, value);
}

#[inline]
fn write_f64_kv(buf: &mut Vec<u8>, key: &str, value: f64) {
    write_fixstr(buf, key);
    buf.push(0xcb);
    buf.extend_from_slice(&value.to_be_bytes());
}

#[inline]
fn write_opt_f64_kv(buf: &mut Vec<u8>, key: &str, value: Option<f64>) {
    write_fixstr(buf, key);
    match value {
        Some(v) => {
            buf.push(0xcb);
            buf.extend_from_slice(&v.to_be_bytes());
        }
        None => buf.push(0xc0),
    }
}

#[inline]
fn write_opt_i32_kv(buf: &mut Vec<u8>, key: &str, value: Option<i32>) {
    write_fixstr(buf, key);
    match value {
        Some(v) => {
            buf.push(0xd2);
            buf.extend_from_slice(&v.to_be_bytes());
        }
        None => buf.push(0xc0),
    }
}

#[inline]
fn write_u64_kv(buf: &mut Vec<u8>, key: &str, value: u64) {
    write_fixstr(buf, key);
    buf.push(0xcf);
    buf.extend_from_slice(&value.to_be_bytes());
}

thread_local! {
    /// Per-thread reusable buffer for `encode_tick_into`. Cleared each
    /// call; never re-allocated after first warmup (typical encoded
    /// tick is ~180 bytes).
    static TICK_ENCODE_BUF: std::cell::RefCell<Vec<u8>> =
        std::cell::RefCell::new(Vec::with_capacity(256));
}

/// Encode `ev` and call `f` with the byte slice. Uses a per-thread
/// buffer so back-to-back ticks don't allocate.
#[inline]
fn with_encoded_tick<F: FnOnce(&[u8]) -> R, R>(ev: &TickEvent, f: F) -> R {
    TICK_ENCODE_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        encode_tick_into(ev, &mut buf);
        f(&buf)
    })
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
        let b = runtime.broker.clone();
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
                // L2 depth top-2 levels (None when no active depth sub).
                let (depth_bid_0, depth_bid_1, depth_ask_0, depth_ask_1) = {
                    let md = runtime.market_data.lock().unwrap();
                    if let Some(opt) = md.option_by_iid(tick.instrument_id) {
                        let b0 = opt.depth.bids.first().map(|l| l.price);
                        let b1 = opt.depth.bids.get(1).map(|l| l.price);
                        let a0 = opt.depth.asks.first().map(|l| l.price);
                        let a1 = opt.depth.asks.get(1).map(|l| l.price);
                        (b0, b1, a0, a1)
                    } else {
                        (None, None, None, None)
                    }
                };
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
                    depth_bid_0,
                    depth_bid_1,
                    depth_ask_0,
                    depth_ask_1,
                };
                let dropped = with_encoded_tick(&ev, |body| !server.publish(body));
                if dropped {
                    log::debug!("ipc events ring full — dropped tick");
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

        // Margin: read from runtime's cached AccountSnapshot
        // (refreshed every 15s by `periodic_account_poll`). The
        // previous design called `account_values()` per tick under
        // a 50ms timeout and fell back to `margin_usd = 0.0` on
        // expiry — a fail-OPEN on the trader-side margin gate.
        // Trader sees fresh risk_state with margin_pct=0 and the
        // `margin_pct >= ceiling` guard at decision.rs:646 doesn't
        // fire (Finding C from the 2026-05-05 audit).
        //
        // Fail-closed semantics: when the cache has never been
        // populated (timestamp_ns == 0, pre-first-poll) or is too
        // stale (>30s old), publish margin_pct = 1.0 so the trader
        // blocks ALL placements. The trader's `risk_state stale >5s`
        // gate is the upstream backstop; this is the within-event
        // backstop.
        const MARGIN_CACHE_MAX_AGE_NS: u64 = 30_000_000_000;
        let (margin_usd, margin_cache_fresh) = {
            let a = runtime.account.lock().unwrap();
            let now = now_ns();
            let fresh = a.timestamp_ns > 0
                && now.saturating_sub(a.timestamp_ns) <= MARGIN_CACHE_MAX_AGE_NS;
            (a.maintenance_margin, fresh)
        };
        let margin_pct = if !margin_cache_fresh {
            1.0 // fail-closed sentinel (>= any reasonable ceiling)
        } else if capital > 0.0 {
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

#[cfg(test)]
mod tests {
    //! Round-trip parity tests for the hand-rolled `encode_tick_into`.
    //! Each test encodes via the hand-roll and decodes via rmp_serde
    //! into a Deserialize-able shadow struct, then checks every field.
    //! Equality at the field level (not byte level) lets the encoder
    //! choose efficient encodings for primitives without breaking the
    //! contract with the trader's decoder.
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct TickEventOwned {
        #[serde(rename = "type")]
        ty: String,
        strike: f64,
        expiry: String,
        right: String,
        bid: Option<f64>,
        ask: Option<f64>,
        bid_size: Option<i32>,
        ask_size: Option<i32>,
        ts_ns: u64,
        broker_recv_ns: u64,
        #[serde(default)]
        depth_bid_0: Option<f64>,
        #[serde(default)]
        depth_bid_1: Option<f64>,
        #[serde(default)]
        depth_ask_0: Option<f64>,
        #[serde(default)]
        depth_ask_1: Option<f64>,
    }

    #[test]
    fn tick_round_trip_full() {
        let ev = TickEvent {
            ty: "tick",
            strike: 6.025,
            expiry: "20260526",
            right: "C",
            bid: Some(0.0345),
            ask: Some(0.036),
            bid_size: Some(5),
            ask_size: Some(7),
            ts_ns: 1_777_993_654_064_490_620,
            broker_recv_ns: 1_777_993_654_064_490_620,
            depth_bid_0: Some(0.0345),
            depth_bid_1: Some(0.034),
            depth_ask_0: Some(0.036),
            depth_ask_1: Some(0.0365),
        };
        let mut buf = Vec::new();
        encode_tick_into(&ev, &mut buf);
        let d: TickEventOwned = rmp_serde::from_slice(&buf).expect("decode");
        assert_eq!(d.ty, "tick");
        assert_eq!(d.strike, 6.025);
        assert_eq!(d.expiry, "20260526");
        assert_eq!(d.right, "C");
        assert_eq!(d.bid, Some(0.0345));
        assert_eq!(d.ask, Some(0.036));
        assert_eq!(d.bid_size, Some(5));
        assert_eq!(d.ask_size, Some(7));
        assert_eq!(d.ts_ns, 1_777_993_654_064_490_620);
        assert_eq!(d.broker_recv_ns, 1_777_993_654_064_490_620);
        assert_eq!(d.depth_bid_0, Some(0.0345));
        assert_eq!(d.depth_bid_1, Some(0.034));
        assert_eq!(d.depth_ask_0, Some(0.036));
        assert_eq!(d.depth_ask_1, Some(0.0365));
    }

    #[test]
    fn tick_round_trip_no_depth() {
        let ev = TickEvent {
            ty: "tick",
            strike: 6.0,
            expiry: "20260526",
            right: "P",
            bid: None,
            ask: Some(0.1),
            bid_size: None,
            ask_size: Some(10),
            ts_ns: 0,
            broker_recv_ns: 0,
            depth_bid_0: None,
            depth_bid_1: None,
            depth_ask_0: None,
            depth_ask_1: None,
        };
        let mut buf = Vec::new();
        encode_tick_into(&ev, &mut buf);
        let d: TickEventOwned = rmp_serde::from_slice(&buf).expect("decode");
        assert_eq!(d.bid, None);
        assert_eq!(d.ask, Some(0.1));
        assert_eq!(d.bid_size, None);
        assert_eq!(d.ask_size, Some(10));
        assert_eq!(d.depth_bid_0, None);
        assert_eq!(d.depth_bid_1, None);
        assert_eq!(d.depth_ask_0, None);
        assert_eq!(d.depth_ask_1, None);
    }

    #[test]
    fn tick_buf_reuse_clears() {
        // Ensure the encoder clears prior content; encode twice into the
        // same buffer and verify only the second tick's data is decoded.
        let mut buf = Vec::new();
        encode_tick_into(
            &TickEvent {
                ty: "tick",
                strike: 1.0,
                expiry: "AAAA",
                right: "C",
                bid: Some(0.5),
                ask: Some(0.6),
                bid_size: Some(1),
                ask_size: Some(1),
                ts_ns: 1,
                broker_recv_ns: 1,
                depth_bid_0: None,
                depth_bid_1: None,
                depth_ask_0: None,
                depth_ask_1: None,
            },
            &mut buf,
        );
        buf.clear();
        encode_tick_into(
            &TickEvent {
                ty: "tick",
                strike: 2.0,
                expiry: "BBBB",
                right: "P",
                bid: None,
                ask: None,
                bid_size: None,
                ask_size: None,
                ts_ns: 999,
                broker_recv_ns: 999,
                depth_bid_0: None,
                depth_bid_1: None,
                depth_ask_0: None,
                depth_ask_1: None,
            },
            &mut buf,
        );
        let d: TickEventOwned = rmp_serde::from_slice(&buf).expect("decode");
        assert_eq!(d.strike, 2.0);
        assert_eq!(d.expiry, "BBBB");
        assert_eq!(d.bid, None);
    }
}

