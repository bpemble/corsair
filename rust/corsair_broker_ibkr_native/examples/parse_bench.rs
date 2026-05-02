//! Microbenchmark for `try_decode_frame`. Synthesizes representative
//! inbound frames (tickPrice, accountValue, position, openOrder) and
//! measures decode time per call.
//!
//! Run with: `cargo run --release --example parse_bench`
//!
//! Numbers are not authoritative latency — the recv path also has TCP
//! read + dispatch overhead. They're useful for tracking parser
//! regressions.

use bytes::BytesMut;
use corsair_broker_ibkr_native::codec::{encode_fields, try_decode_frame};
use std::time::Instant;

fn main() {
    let frames = build_test_frames();
    println!("Frames: {} samples", frames.len());

    // Warmup
    for _ in 0..10_000 {
        for f in &frames {
            let mut buf = BytesMut::from(&f[..]);
            let _ = try_decode_frame(&mut buf);
        }
    }

    let n = 1_000_000;
    let start = Instant::now();
    let mut total = 0usize;
    for _ in 0..n {
        for f in &frames {
            let mut buf = BytesMut::from(&f[..]);
            if let Ok(Some(fields)) = try_decode_frame(&mut buf) {
                total += fields.len();
            }
        }
    }
    let elapsed = start.elapsed();
    let total_calls = n * frames.len();
    let per_call_ns = elapsed.as_nanos() / total_calls as u128;

    println!("Total time:    {:.2?}", elapsed);
    println!("Total calls:   {}", total_calls);
    println!("Per call:      {} ns", per_call_ns);
    println!("Total fields:  {}", total);
}

fn build_test_frames() -> Vec<Vec<u8>> {
    let mut out = Vec::new();

    // Tick price: 7 fields
    out.push(encode_fields(&[
        "1", "1", "100", "1", "6.0345", "10", "0",
    ]));

    // Account value: 5 fields
    out.push(encode_fields(&[
        "6", "1", "NetLiquidation", "500000.50", "USD",
    ]));

    // Position: 11 fields
    out.push(encode_fields(&[
        "61", "3", "DUP553657", "12345", "HG", "FUT", "20260526", "0", "",
        "25000", "COMEX",
    ]));

    // Open order: 17 fields (matches what decoder expects minimum)
    out.push(encode_fields(&[
        "5", "42", "12345", "HG", "FOP", "20260526", "6.05", "C", "25000",
        "COMEX", "USD", "HXEK6 P6050", "HXE", "SELL", "1", "LMT", "0.0285",
    ]));

    // ManagedAccounts: 3 fields
    out.push(encode_fields(&[
        "15", "1", "DFP553653,DUP553654,DUP553655,DUP553656,DUP553657,DUP553658",
    ]));

    out
}
