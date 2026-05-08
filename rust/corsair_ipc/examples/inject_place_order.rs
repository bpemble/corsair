//! Synthetic place_order injector — pushes one msgpack PlaceOrder
//! frame into the broker's IPC commands ring. Used for v2 wire-timing
//! plumbing validation when markets are closed (no real order flow).
//!
//! Build:
//!   cargo build --release --package corsair_ipc --example inject_place_order
//! Run inside the corsair container so the SHM paths match:
//!   /usr/local/cargo/bin/cargo run --release --package corsair_ipc \
//!       --example inject_place_order -- --strike 5.95 --expiry 20260526 --right C
//!
//! Defaults assume an HG option strike that won't match anything cached
//! when markets are closed, so the broker logs `no_contract` and the
//! wire_timing JSONL gets a row with outcome="no_contract" + the four
//! broker-edge timestamps. That's enough to validate the v2 plumbing.

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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut strike = 5.95_f64;
    let mut expiry = "20260526".to_string();
    let mut right = "C".to_string();
    let mut side = "BUY".to_string();
    let mut qty = 1_i32;
    let mut price = 0.0445_f64;
    let mut data_dir = PathBuf::from("/app/data");
    let mut i = 1;
    while i < args.len() {
        let key = args[i].as_str();
        // Helper: take the next arg as the value; bail with a clear
        // usage error instead of panicking on index out of bounds when
        // a flag is passed without its value.
        let take_value = |key: &str, i: usize| -> &str {
            if i + 1 >= args.len() {
                eprintln!("inject_place_order: missing value for {key}");
                std::process::exit(2);
            }
            args[i + 1].as_str()
        };
        match key {
            "--strike" => {
                strike = take_value(key, i).parse().expect("--strike: parse f64");
                i += 2;
            }
            "--expiry" => { expiry = take_value(key, i).to_string(); i += 2; }
            "--right" => { right = take_value(key, i).to_string(); i += 2; }
            "--side" => { side = take_value(key, i).to_string(); i += 2; }
            "--qty" => {
                qty = take_value(key, i).parse().expect("--qty: parse i32");
                i += 2;
            }
            "--price" => {
                price = take_value(key, i).parse().expect("--price: parse f64");
                i += 2;
            }
            "--data-dir" => { data_dir = PathBuf::from(take_value(key, i)); i += 2; }
            other => {
                eprintln!("inject_place_order: unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }

    let cmd_path = data_dir.join("corsair_ipc.commands");
    let cmd_notify = data_dir.join("corsair_ipc.commands.notify");
    println!("inject_place_order: opening {}", cmd_path.display());
    let mut commands = Ring::open_client(&cmd_path, DEFAULT_RING_CAPACITY)
        .expect("open commands ring");
    commands.open_notify(&cmd_notify, /*as_writer=*/true)
        .expect("open commands notify");

    // Fake triggering tick recv: 250 µs ago (typical IPC + decide latency).
    let now = now_ns();
    let p = PlaceOrder {
        msg_type: "place_order",
        ts_ns: now,
        strike,
        expiry,
        right,
        side,
        qty,
        price,
        order_ref: "synthetic_inject".to_string(),
        triggering_tick_broker_recv_ns: now.saturating_sub(250_000),
    };
    let body = rmp_serde::to_vec_named(&p).expect("encode place_order");
    let frame = protocol::pack_frame(&body);
    let ok = commands.write_frame(&frame);
    println!(
        "inject_place_order: wrote frame ({} bytes msgpack, {} bytes framed); ring_write_ok={}",
        body.len(),
        frame.len(),
        ok
    );
    if !ok {
        eprintln!("ring full — frame dropped. Check broker is consuming.");
        std::process::exit(2);
    }
    println!("done — check logs-paper/wire_timing-YYYY-MM-DD.jsonl");
}
