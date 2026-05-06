//! One-off perf harness for the three Tier 2 candidates from the
//! 2026-05-05 cleanup audit:
//!
//! 1. `try_decode_frame` — IBKR codec, current Vec<String> vs zero-copy &[u8]
//! 2. `try_unpack_frame` — IPC protocol, current Vec.drain vs BytesMut.split_to
//! 3. `compute_mtm_pnl` — HashMap lookup w/ String temp key vs borrow-keyed
//!
//! Each bench: 1M iterations, prints median + p99 ns/iter.

use std::collections::HashMap as StdHashMap;
use std::hint::black_box;
use std::time::Instant;

use bytes::{Buf, BytesMut};

const ITERS: usize = 1_000_000;
const WARMUP: usize = 50_000;

fn time_ns<F: FnMut()>(mut f: F) -> Vec<u64> {
    // Warm up.
    for _ in 0..WARMUP {
        f();
    }
    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t = Instant::now();
        f();
        samples.push(t.elapsed().as_nanos() as u64);
    }
    samples
}

fn report(label: &str, mut s: Vec<u64>) {
    s.sort_unstable();
    let n = s.len();
    let p50 = s[n / 2];
    let p99 = s[(n * 99) / 100];
    let mean = s.iter().sum::<u64>() / n as u64;
    println!(
        "  {:38}  p50 {:>5}ns  mean {:>5}ns  p99 {:>6}ns",
        label, p50, mean, p99
    );
}

// ────────────────────────────────────────────────────────────────────
// Bench 1: try_decode_frame (IBKR codec)
// ────────────────────────────────────────────────────────────────────
//
// Replicates corsair_broker_ibkr_native::codec::try_decode_frame.
// Synthetic frame: 10 fields, each ~5 chars (typical TickPrice: 12,
// reqId, tickType, price, size, attrib).

fn make_frame() -> Vec<u8> {
    let fields: [&[u8]; 10] = [
        b"12", b"5", b"123", b"6.0245", b"5", b"1", b"0",
        b"hello", b"world", b"abcdefgh",
    ];
    let mut payload = Vec::new();
    for f in fields {
        payload.extend_from_slice(f);
        payload.push(0);
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Baseline: matches current production codec verbatim.
fn decode_baseline(buf: &mut BytesMut) -> Option<Vec<String>> {
    if buf.len() < 4 {
        return None;
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&buf[..4]);
    let size = u32::from_be_bytes(len_bytes) as usize;
    if buf.len() < 4 + size {
        return None;
    }
    let payload = &buf[4..4 + size];
    let mut fields: Vec<String> = Vec::with_capacity(16);
    for field in payload.split(|&b| b == 0) {
        fields.push(String::from_utf8_lossy(field).into_owned());
    }
    if fields.last().map(|f| f.is_empty()).unwrap_or(false) {
        fields.pop();
    }
    let _ = buf.split_to(4 + size);
    Some(fields)
}

/// Proposed v2: Vec<&str> — same shape as baseline (callers stay
/// the same since &str access pattern is identical to &String) but
/// no per-field heap alloc.
fn decode_zerocopy_str<'a>(buf: &'a [u8]) -> Option<(usize, Vec<&'a str>)> {
    if buf.len() < 4 {
        return None;
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&buf[..4]);
    let size = u32::from_be_bytes(len_bytes) as usize;
    if buf.len() < 4 + size {
        return None;
    }
    let payload = &buf[4..4 + size];
    let mut fields: Vec<&str> = Vec::with_capacity(16);
    for field in payload.split(|&b| b == 0) {
        // Production IBKR fields are pure ASCII; from_utf8 is a
        // validity check, no copy. On invalid bytes (shouldn't happen
        // for real gateway traffic) we substitute "" — matches the
        // current `from_utf8_lossy` semantics for the parse helpers,
        // which already treat the empty string as 0/false.
        let s = std::str::from_utf8(field).unwrap_or("");
        fields.push(s);
    }
    if fields.last().map(|f| f.is_empty()).unwrap_or(false) {
        fields.pop();
    }
    Some((4 + size, fields))
}

/// Proposed v1: zero-copy. Returns Vec<&[u8]> tied to caller's buffer.
/// Caller must consume the slices BEFORE advancing the buffer.
fn decode_zerocopy<'a>(buf: &'a [u8]) -> Option<(usize, Vec<&'a [u8]>)> {
    if buf.len() < 4 {
        return None;
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&buf[..4]);
    let size = u32::from_be_bytes(len_bytes) as usize;
    if buf.len() < 4 + size {
        return None;
    }
    let payload = &buf[4..4 + size];
    let mut fields: Vec<&[u8]> = Vec::with_capacity(16);
    for field in payload.split(|&b| b == 0) {
        fields.push(field);
    }
    if fields.last().map(|f| f.is_empty()).unwrap_or(false) {
        fields.pop();
    }
    Some((4 + size, fields))
}

fn bench_decode_frame() {
    println!("─── Bench 1: try_decode_frame (10-field IBKR frame) ───");
    let frame = make_frame();

    // Baseline: full Vec<String> alloc per frame.
    let mut buf = BytesMut::with_capacity(64);
    let s = time_ns(|| {
        buf.clear();
        buf.extend_from_slice(&frame);
        let r = decode_baseline(&mut buf);
        black_box(r);
    });
    report("baseline (Vec<String>)", s);

    // Proposed v1: zero-copy raw bytes.
    let mut buf2 = BytesMut::with_capacity(64);
    let s = time_ns(|| {
        buf2.clear();
        buf2.extend_from_slice(&frame);
        let slice = &buf2[..];
        let r = decode_zerocopy(slice);
        if let Some((advance, fields)) = r {
            for f in &fields {
                black_box(f);
            }
            let _ = advance;
        }
    });
    report("zero-copy (Vec<&[u8]>)", s);

    // Proposed v2: Vec<&str> — preserves call-site shape.
    let mut buf3 = BytesMut::with_capacity(64);
    let s = time_ns(|| {
        buf3.clear();
        buf3.extend_from_slice(&frame);
        let slice = &buf3[..];
        let r = decode_zerocopy_str(slice);
        if let Some((advance, fields)) = r {
            for f in &fields {
                black_box(f);
            }
            let _ = advance;
        }
    });
    report("zero-copy (Vec<&str>)", s);
}

// ────────────────────────────────────────────────────────────────────
// Bench 2: try_unpack_frame (IPC protocol)
// ────────────────────────────────────────────────────────────────────
//
// corsair_ipc::protocol::try_unpack_frame currently does
// `buf[4..4+size].to_vec() + buf.drain(..4+size)`. The drain is O(n)
// in the remainder of buf; for bursty multi-frame buffers this is
// quadratic.

fn make_ipc_frame(size: usize) -> Vec<u8> {
    let body = vec![0xc0u8; size];
    let mut frame = Vec::with_capacity(4 + size);
    frame.extend_from_slice(&(size as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

/// Baseline (current).
fn unpack_baseline(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buf.len() < 4 {
        return None;
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&buf[..4]);
    let size = u32::from_be_bytes(len_bytes) as usize;
    if buf.len() < 4 + size {
        return None;
    }
    let body = buf[4..4 + size].to_vec();
    buf.drain(..4 + size);
    Some(body)
}

/// Proposed: BytesMut + split_to, returns Bytes (cheap clone).
fn unpack_bytesmut(buf: &mut BytesMut) -> Option<bytes::Bytes> {
    if buf.len() < 4 {
        return None;
    }
    let size = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + size {
        return None;
    }
    let _hdr = buf.split_to(4); // discard length
    Some(buf.split_to(size).freeze())
}

fn bench_unpack_frame_burst() {
    println!("─── Bench 2: try_unpack_frame, 10-frame burst (200B body) ───");
    let frame = make_ipc_frame(200);
    let frames: Vec<u8> = (0..10).flat_map(|_| frame.clone()).collect();

    // Baseline: Vec<u8>::drain — quadratic in burst size.
    let s = time_ns(|| {
        let mut buf = frames.clone();
        for _ in 0..10 {
            let r = unpack_baseline(&mut buf);
            black_box(r);
        }
    });
    report("baseline (Vec.drain) ×10", s);

    // Proposed: BytesMut split_to.
    let s = time_ns(|| {
        let mut buf = BytesMut::from(frames.as_slice());
        for _ in 0..10 {
            let r = unpack_bytesmut(&mut buf);
            black_box(r);
        }
    });
    report("BytesMut.split_to ×10", s);
}

fn bench_unpack_frame_single() {
    println!("─── Bench 2b: try_unpack_frame, single-frame (200B body) ───");
    let frame = make_ipc_frame(200);

    let s = time_ns(|| {
        let mut buf = frame.clone();
        let r = unpack_baseline(&mut buf);
        black_box(r);
    });
    report("baseline (Vec.drain) ×1", s);

    let s = time_ns(|| {
        let mut buf = BytesMut::from(frame.as_slice());
        let r = unpack_bytesmut(&mut buf);
        black_box(r);
    });
    report("BytesMut.split_to ×1", s);
}

// ────────────────────────────────────────────────────────────────────
// Bench 3: compute_mtm_pnl HashMap lookup (corsair_market_data)
// ────────────────────────────────────────────────────────────────────
//
// MarketDataState::option() builds a temporary OptionKey with
// product.to_string() per call. With 30 positions × 4Hz snapshot,
// that's ~120 String allocs/sec. Production cost: small but real.

#[derive(Hash, PartialEq, Eq, Clone)]
struct OptionKey {
    product: String,
    strike_key: i64,
    expiry: i64, // simulated NaiveDate as i64 ordinal
    right: char,
}

fn make_option_map() -> StdHashMap<OptionKey, f64> {
    let mut m = StdHashMap::new();
    for strike_i in 0..30 {
        let key = OptionKey {
            product: "HG".to_string(),
            strike_key: 60000 + strike_i * 5,
            expiry: 19_807, // some date
            right: 'C',
        };
        m.insert(key, 0.025);
    }
    m
}

fn make_option_hashbrown() -> hashbrown::HashMap<OptionKey, f64> {
    let mut m = hashbrown::HashMap::new();
    for strike_i in 0..30 {
        let key = OptionKey {
            product: "HG".to_string(),
            strike_key: 60000 + strike_i * 5,
            expiry: 19_807,
            right: 'C',
        };
        m.insert(key, 0.025);
    }
    m
}

fn bench_option_lookup() {
    println!("─── Bench 3: OptionKey HashMap lookup ───");
    let std_map = make_option_map();
    let hb_map = make_option_hashbrown();

    // Baseline: build a temp OptionKey with product.to_string().
    let s = time_ns(|| {
        let key = OptionKey {
            product: "HG".to_string(),
            strike_key: 60050,
            expiry: 19_807,
            right: 'C',
        };
        let v = std_map.get(&key);
        black_box(v);
    });
    report("std::HashMap with String alloc", s);

    // Proposed v1: hashbrown raw_entry — borrow-keyed (no alloc).
    use std::hash::{BuildHasher, Hasher};
    let s = time_ns(|| {
        let product: &str = "HG";
        let strike_key: i64 = 60050;
        let expiry: i64 = 19_807;
        let right: char = 'C';
        let mut hasher = hb_map.hasher().build_hasher();
        product.hash(&mut hasher);
        strike_key.hash(&mut hasher);
        expiry.hash(&mut hasher);
        right.hash(&mut hasher);
        let h = hasher.finish();
        let v = hb_map.raw_entry().from_hash(h, |k| {
            k.product == product
                && k.strike_key == strike_key
                && k.expiry == expiry
                && k.right == right
        });
        black_box(v);
    });
    report("hashbrown raw_entry (no alloc)", s);
}

// `Hash` for OptionKey — needed for raw_entry.from_hash to compute the
// same hash the map uses internally.
use std::hash::Hash;

fn main() {
    println!();
    println!("═══════════════════════════════════════════════════════════");
    println!("  Tier 2 perf-item benchmarks");
    println!("  ITERS={}, WARMUP={}", ITERS, WARMUP);
    println!("═══════════════════════════════════════════════════════════");
    println!();

    bench_decode_frame();
    println!();
    bench_unpack_frame_burst();
    println!();
    bench_unpack_frame_single();
    println!();
    bench_option_lookup();
    println!();
}
