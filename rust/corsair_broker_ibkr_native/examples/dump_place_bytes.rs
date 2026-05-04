//! Dumps the bytes our place_order encoder produces for a known set
//! of parameters. Used to diff against ib_insync's reference frame.
//!
//! Run via:
//!   cargo run --example dump_place_bytes --release
//! Or via docker:
//!   docker run --rm corsair-corsair sh -c '/usr/local/bin/dump_place_bytes'
//! (we don't actually bake this in production; just `cargo run` from
//! the build container or copy out manually.)
//!
//! Output format mirrors `scripts/capture_place_order_bytes.py` so a
//! diff is meaningful: each NUL-delimited field on its own line,
//! prefixed by index.

use corsair_broker_ibkr_native::messages::OUT_PLACE_ORDER;
use corsair_broker_ibkr_native::place_template::{place_order_fast, ContractTemplate};
use corsair_broker_ibkr_native::requests::{ContractRequest, PlaceOrderParams};

fn main() {
    // Match the Python harness exactly.
    let contract = ContractRequest {
        con_id: 718634609, // HXEM6 C650 from prior qualify
        symbol: "HG".into(),
        sec_type: "FOP".into(),
        last_trade_date: "20260526".into(),
        strike: 6.5,
        right: "C".into(),
        multiplier: "25000".into(),
        exchange: "COMEX".into(),
        primary_exchange: String::new(),
        currency: "USD".into(),
        local_symbol: "HXEM6 C650".into(),
        trading_class: "HXE".into(),
    };
    let params = PlaceOrderParams {
        action: "BUY".into(),
        total_quantity: 1.0,
        order_type: "LMT".into(),
        lmt_price: 0.001,
        aux_price: 0.0,
        tif: "GTD".into(),
        good_till_date: "20260504 01:43:45 UTC".into(),
        account: "DUP553657".into(),
        order_ref: "corsair_byte_test".into(),
        outside_rth: false,
    };

    let template = ContractTemplate::from_contract(&contract);
    let order_id: i32 = 9; // Match ib_insync's auto-allocated orderId.
    let bytes = place_order_fast(order_id, &template, &params);

    // Strip 4-byte length prefix.
    let length = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let payload = &bytes[4..];
    println!("--- Our frame: 4-byte length={length}, total={} bytes ---", bytes.len());
    println!("OUT_PLACE_ORDER msg type id: {OUT_PLACE_ORDER}");

    // Split by \0 and print each field. payload includes a trailing \0
    // that produces an empty string; drop it.
    let mut parts: Vec<&[u8]> = payload.split(|b| *b == 0).collect();
    if let Some(last) = parts.last() {
        if last.is_empty() {
            parts.pop();
        }
    }
    for (i, p) in parts.iter().enumerate() {
        let s = String::from_utf8_lossy(p);
        println!("  [{:3}] {:?}", i, s);
    }
}
