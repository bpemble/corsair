//! Position flattener — reads the broker's chain snapshot, finds every
//! non-zero option position, and pushes a closing limit order into the
//! broker's IPC commands ring. Replaces the broken
//! `scripts/flatten_persistent.py` (ib_insync, deleted from
//! requirements.txt 2026-05-02).
//!
//! Build: baked into the corsair-corsair image at
//!   /usr/local/bin/corsair_flatten
//! (built alongside `inject_place_order` in the rust-build stage of
//! the Dockerfile). Inside the container:
//!
//!   docker compose exec corsair-broker-rs /usr/local/bin/corsair_flatten
//!   docker compose exec corsair-broker-rs /usr/local/bin/corsair_flatten --product HG
//!
//! Closing-order pricing logic:
//!   - long  → SELL @ market_bid       (cross immediately into bid)
//!   - short → BUY  @ market_ask       (cross immediately into ask)
//!
//! HG is thin — market orders give bad fills (CLAUDE.md §15). Limit at
//! the opposite-side BBO is the right balance: aggressive enough to
//! cross, but capped at a known price. With `--passive`, posts at
//! same-side BBO instead (no cross, lets a counterparty come to us;
//! GTD-30s default at the broker).
//!
//! The broker's kill state does NOT block this binary — commands hit
//! the IPC ring directly, ahead of the trader's risk gates. That's
//! intentional: flatten is the operator's escape hatch when the trader
//! is halted with positions on.
//!
//! ⚠ Not a fire-and-forget loop like the old python script. This
//! sends ONE closing order per non-zero position and exits. Re-run
//! after partial fills if you need persistence. (A retry loop is a
//! followup; doing it right means watching fills via SHM events
//! ring, which is more plumbing than fits one example file.)

use corsair_ipc::ring::{Ring, DEFAULT_RING_CAPACITY};
use corsair_ipc::protocol;
use serde::Serialize;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize)]
struct PlaceOrder {
    #[serde(rename = "type")]
    msg_type: &'static str,
    ts_ns: u64,
    strike: f64,
    expiry: String,
    right: String,
    side: String,
    qty: i32,
    price: f64,
    #[serde(rename = "orderRef")]
    order_ref: String,
    triggering_tick_broker_recv_ns: u64,
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[derive(Debug)]
struct Plan {
    strike: f64,
    expiry: String,
    right: String,        // "C" or "P"
    side: String,         // "BUY" or "SELL"
    qty: i32,             // positive
    price: f64,
    note: String,         // human-readable rationale for the log line
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut data_dir = PathBuf::from("/app/data");
    let mut snapshot_path: Option<PathBuf> = None;
    let mut product_filter: Option<String> = Some("HG".to_string());
    let mut dry_run = false;
    let mut passive = false;
    let mut i = 1;
    while i < args.len() {
        let key = args[i].as_str();
        // Helper: take the next arg as the value; bail with a clear
        // usage error instead of panicking on index out of bounds when
        // a flag is passed without its value.
        let take_value = |key: &str, i: usize| -> &str {
            if i + 1 >= args.len() {
                eprintln!("flatten: missing value for {key}");
                print_usage();
                std::process::exit(2);
            }
            args[i + 1].as_str()
        };
        match key {
            "--data-dir" => { data_dir = PathBuf::from(take_value(key, i)); i += 2; }
            "--snapshot" => { snapshot_path = Some(PathBuf::from(take_value(key, i))); i += 2; }
            "--product" => { product_filter = Some(take_value(key, i).to_string()); i += 2; }
            "--all-products" => { product_filter = None; i += 1; }
            "--dry-run" => { dry_run = true; i += 1; }
            "--passive" => { passive = true; i += 1; }
            "--help" | "-h" => { print_usage(); return; }
            other => {
                eprintln!("unknown arg: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }
    let snapshot_path = snapshot_path
        .unwrap_or_else(|| data_dir.join("hg_chain_snapshot.json"));

    println!("flatten: reading snapshot {}", snapshot_path.display());
    let snap_bytes = match std::fs::read(&snapshot_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("snapshot read failed: {e}");
            std::process::exit(1);
        }
    };
    let snap: serde_json::Value = match serde_json::from_slice(&snap_bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("snapshot parse failed: {e}");
            std::process::exit(1);
        }
    };

    let positions = snap
        .pointer("/portfolio/positions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if positions.is_empty() {
        println!("flatten: no positions in snapshot — nothing to do");
        return;
    }

    // Build per-strike-and-right lookup of market BBO from /chains.
    // Snapshot key shape: chains["20260526"]["strikes"]["6.0500"]["call"|"put"].
    let chains = snap.pointer("/chains").cloned().unwrap_or(serde_json::Value::Null);

    let mut plans: Vec<Plan> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for pos in &positions {
        let product = pos.get("product").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(filt) = product_filter.as_deref() {
            if !product.eq_ignore_ascii_case(filt) {
                continue;
            }
        }
        let qty = pos.get("quantity").and_then(|v| v.as_i64()).unwrap_or(0);
        if qty == 0 { continue; }
        let strike = pos.get("strike").and_then(|v| v.as_f64()).unwrap_or(0.0);
        // Snapshot uses YYYY-MM-DD; broker IPC place_order wants YYYYMMDD.
        let expiry_in = pos.get("expiry").and_then(|v| v.as_str()).unwrap_or("");
        let expiry_compact: String = expiry_in.chars().filter(|c| c.is_ascii_digit()).collect();
        let right_word = pos.get("right").and_then(|v| v.as_str()).unwrap_or("");
        let right_char = match right_word.chars().next() {
            Some(c) => c.to_ascii_uppercase().to_string(),
            None => {
                skipped.push(format!("{product} qty={qty} K={strike}: missing right"));
                continue;
            }
        };

        // Look up BBO for this contract.
        let strike_key = format!("{:.4}", strike);
        let side_key = if right_char == "C" { "call" } else { "put" };
        let bbo = chains
            .pointer(&format!("/{}/strikes/{}/{}", expiry_compact, strike_key, side_key));
        let (mkt_bid, mkt_ask) = match bbo {
            Some(v) => (
                v.get("market_bid").and_then(|x| x.as_f64()).unwrap_or(0.0),
                v.get("market_ask").and_then(|x| x.as_f64()).unwrap_or(0.0),
            ),
            None => {
                skipped.push(format!(
                    "{right_char}{strike:.3} qty={qty:+} {expiry_compact}: no BBO in chain"
                ));
                continue;
            }
        };
        if mkt_bid <= 0.0 || mkt_ask <= 0.0 {
            skipped.push(format!(
                "{right_char}{strike:.3} qty={qty:+}: market BBO not live (bid={mkt_bid} ask={mkt_ask})"
            ));
            continue;
        }

        // Closing direction + price.
        let (side, price, note) = if qty > 0 {
            // Long → SELL.
            if passive {
                ("SELL".to_string(), mkt_ask, format!("passive @ ask={mkt_ask:.4}"))
            } else {
                ("SELL".to_string(), mkt_bid, format!("aggressive @ bid={mkt_bid:.4}"))
            }
        } else {
            // Short → BUY.
            if passive {
                ("BUY".to_string(), mkt_bid, format!("passive @ bid={mkt_bid:.4}"))
            } else {
                ("BUY".to_string(), mkt_ask, format!("aggressive @ ask={mkt_ask:.4}"))
            }
        };
        plans.push(Plan {
            strike,
            expiry: expiry_compact,
            right: right_char,
            side,
            qty: qty.unsigned_abs() as i32,
            price,
            note,
        });
    }

    println!("flatten: {} positions to close, {} skipped", plans.len(), skipped.len());
    for s in &skipped {
        println!("  SKIP {s}");
    }
    for p in &plans {
        println!(
            "  PLAN {}{:.3} {} {} @ {:.4}  ({})",
            p.right, p.strike, p.side, p.qty, p.price, p.note
        );
    }
    if dry_run {
        println!("flatten: --dry-run, exiting without sending");
        return;
    }
    if plans.is_empty() {
        return;
    }

    // Open the commands ring once, send all frames.
    let cmd_path = data_dir.join("corsair_ipc.commands");
    let cmd_notify = data_dir.join("corsair_ipc.commands.notify");
    let mut commands = match Ring::open_client(&cmd_path, DEFAULT_RING_CAPACITY) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("open commands ring {} failed: {e}", cmd_path.display());
            std::process::exit(1);
        }
    };
    if let Err(e) = commands.open_notify(&cmd_notify, /*as_writer=*/true) {
        eprintln!("open notify {} failed: {e}", cmd_notify.display());
        std::process::exit(1);
    }

    let mut sent = 0;
    let mut dropped = 0;
    for p in &plans {
        let now = now_ns();
        let order = PlaceOrder {
            msg_type: "place_order",
            ts_ns: now,
            strike: p.strike,
            expiry: p.expiry.clone(),
            right: p.right.clone(),
            side: p.side.clone(),
            qty: p.qty,
            price: p.price,
            order_ref: "corsair_flatten".to_string(),
            triggering_tick_broker_recv_ns: now,
        };
        let body = match rmp_serde::to_vec_named(&order) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("encode failed for {p:?}: {e}");
                continue;
            }
        };
        let frame = protocol::pack_frame(&body);
        if commands.write_frame(&frame) {
            sent += 1;
        } else {
            dropped += 1;
            eprintln!(
                "ring full — dropped frame for {}{:.3} {} {}",
                p.right, p.strike, p.side, p.qty
            );
        }
    }
    println!("flatten: sent {sent} frames, dropped {dropped}");
    if dropped > 0 {
        std::process::exit(2);
    }
}

fn print_usage() {
    eprintln!(
        "Usage: corsair_flatten [--data-dir DIR] [--snapshot PATH] \
         [--product HG | --all-products] [--passive] [--dry-run]\n\
         \n\
         Reads the broker's chain snapshot, computes one closing order \n\
         per non-zero position, and writes them to the IPC commands ring.\n\
         \n\
         Defaults: --data-dir /app/data, snapshot=hg_chain_snapshot.json,\n\
         --product HG, aggressive (cross opposite-side BBO).\n\
         \n\
         Use --passive to post on same-side BBO (rests, doesn't cross).\n\
         Use --dry-run to print plans without sending."
    );
}
