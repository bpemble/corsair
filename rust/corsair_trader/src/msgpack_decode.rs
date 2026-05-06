//! Hand-rolled msgpack decoder for the trader's hottest inbound
//! message types. Replaces `rmp_serde::from_slice::<T>(&body)` for the
//! high-frequency `tick` message; estimated saves ~1-2 µs per tick at
//! p50 by:
//!   - Walking the msgpack bytes in one pass, no `Value`-tree
//!   - Matching keys by length + first-byte heuristic, no string alloc
//!   - In-place fill of a caller-provided `TickMsg` (reusing its
//!     `String` allocations for `expiry`/`right` across ticks)
//!   - Skipping unknown fields without parsing their values
//!
//! The decoder is hard-coded to the broker's wire format
//! (msgpack-named-fields, key strings exactly as defined in
//! `messages.rs`). On any shape mismatch (unexpected type byte,
//! truncated buffer, unknown required field encoding) it returns
//! `false`; the caller bumps the `dropped_parse_errors` counter and
//! drops the tick. Schema drift between broker and trader produces a
//! clean signal, not silent corruption.
//!
//! See `tests` below for the round-trip property test against
//! `rmp_serde::to_vec_named` — every commit that touches this file
//! should keep that test green.

use crate::messages::TickMsg;

/// Read the `type` (always emitted) and `ts_ns` (optional) fields from
/// a msgpack body without allocating. Returns `(type_bytes, ts_ns)`,
/// where `type_bytes` borrows from `body`. None on malformed input.
///
/// Bundle 2C (2026-05-06): replaces the previous
/// `rmp_serde::from_slice::<MsgHeader>(&body)` round trip, which built
/// `MsgHeader { msg_type: String, ts_ns: Option<u64> }` per event.
/// The String alloc was paid on every inbound event (~50/sec dominant
/// = ticks); switching to a borrow-only walker eliminates the
/// allocation and the reflection cost.
pub fn decode_header(body: &[u8]) -> Option<(&[u8], Option<u64>)> {
    let mut p = 0usize;
    let (n_pairs, after_hdr) = read_map_header(body, p)?;
    p = after_hdr;
    let mut type_bytes: Option<&[u8]> = None;
    let mut ts_ns: Option<u64> = None;
    for _ in 0..n_pairs {
        let (key, key_end) = read_str_bytes(body, p)?;
        p = key_end;
        match key.len() {
            4 if key == b"type" => {
                let (s, end) = read_str_bytes(body, p)?;
                type_bytes = Some(s);
                p = end;
            }
            5 if key == b"ts_ns" => {
                if *body.get(p)? == 0xc0 {
                    p += 1;
                } else {
                    let (v, end) = read_int(body, p)?;
                    ts_ns = Some(v as u64);
                    p = end;
                }
            }
            _ => {
                p = skip_value(body, p)?;
            }
        }
    }
    type_bytes.map(|t| (t, ts_ns))
}

/// Decode a `tick` msgpack body into `out`. Returns true on success.
/// `out` is reused across calls; pre-existing `String` capacities for
/// `expiry`/`right` are kept to avoid allocator pressure.
pub fn decode_tick(body: &[u8], out: &mut TickMsg) -> bool {
    let mut p = 0usize;
    let n_pairs = match read_map_header(body, p) {
        Some((n, end)) => {
            p = end;
            n
        }
        None => return false,
    };

    // Reset Option fields — caller may be reusing `out` from a previous
    // tick and the new tick may not carry every field. String fields
    // are cleared at write time below; non-Option scalars (`strike`)
    // get overwritten unconditionally.
    out.bid = None;
    out.ask = None;
    out.bid_size = None;
    out.ask_size = None;
    out.ts_ns = None;
    out.broker_recv_ns = None;
    out.depth_bid_0 = None;
    out.depth_bid_1 = None;
    out.depth_ask_0 = None;
    out.depth_ask_1 = None;

    for _ in 0..n_pairs {
        let (key, key_end) = match read_str_bytes(body, p) {
            Some(r) => r,
            None => return false,
        };
        p = key_end;

        // Fast dispatch: match on (length, first byte) to avoid full
        // memcmp until needed. Length-only narrows aggressively.
        let v_end = match key.len() {
            3 => match key {
                b"bid" => assign_opt_f64(body, p, &mut out.bid),
                b"ask" => assign_opt_f64(body, p, &mut out.ask),
                _ => skip_value(body, p),
            },
            4 => match key {
                b"type" => skip_value(body, p),
                _ => skip_value(body, p),
            },
            5 => match key {
                b"ts_ns" => assign_opt_u64(body, p, &mut out.ts_ns),
                b"right" => assign_string(body, p, &mut out.right),
                _ => skip_value(body, p),
            },
            6 => match key {
                b"strike" => match read_f64(body, p) {
                    Some((v, end)) => {
                        out.strike = v;
                        Some(end)
                    }
                    None => None,
                },
                b"expiry" => assign_string(body, p, &mut out.expiry),
                _ => skip_value(body, p),
            },
            8 => match key {
                b"bid_size" => assign_opt_i32(body, p, &mut out.bid_size),
                b"ask_size" => assign_opt_i32(body, p, &mut out.ask_size),
                _ => skip_value(body, p),
            },
            11 => match key {
                b"depth_bid_0" => assign_opt_f64(body, p, &mut out.depth_bid_0),
                b"depth_bid_1" => assign_opt_f64(body, p, &mut out.depth_bid_1),
                b"depth_ask_0" => assign_opt_f64(body, p, &mut out.depth_ask_0),
                b"depth_ask_1" => assign_opt_f64(body, p, &mut out.depth_ask_1),
                _ => skip_value(body, p),
            },
            14 => match key {
                b"broker_recv_ns" => assign_opt_u64(body, p, &mut out.broker_recv_ns),
                _ => skip_value(body, p),
            },
            _ => skip_value(body, p),
        };
        p = match v_end {
            Some(end) => end,
            None => return false,
        };
    }
    true
}

// ─── primitive readers ───────────────────────────────────────────────

#[inline]
fn read_map_header(body: &[u8], p: usize) -> Option<(usize, usize)> {
    let b = *body.get(p)?;
    match b {
        0x80..=0x8f => Some(((b & 0x0f) as usize, p + 1)),
        0xde => {
            let bytes = body.get(p + 1..p + 3)?;
            Some((u16::from_be_bytes([bytes[0], bytes[1]]) as usize, p + 3))
        }
        0xdf => {
            let bytes = body.get(p + 1..p + 5)?;
            Some((
                u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize,
                p + 5,
            ))
        }
        _ => None,
    }
}

#[inline]
fn read_str_bytes(body: &[u8], p: usize) -> Option<(&[u8], usize)> {
    let b = *body.get(p)?;
    let (start, len) = match b {
        0xa0..=0xbf => (p + 1, (b & 0x1f) as usize),
        0xd9 => (p + 2, *body.get(p + 1)? as usize),
        0xda => {
            let bs = body.get(p + 1..p + 3)?;
            (p + 3, u16::from_be_bytes([bs[0], bs[1]]) as usize)
        }
        0xdb => {
            let bs = body.get(p + 1..p + 5)?;
            (
                p + 5,
                u32::from_be_bytes([bs[0], bs[1], bs[2], bs[3]]) as usize,
            )
        }
        _ => return None,
    };
    let end = start + len;
    if end > body.len() {
        return None;
    }
    Some((&body[start..end], end))
}

#[inline]
fn read_f64(body: &[u8], p: usize) -> Option<(f64, usize)> {
    let b = *body.get(p)?;
    match b {
        0xcb => {
            let bs = body.get(p + 1..p + 9)?;
            let arr: [u8; 8] = bs.try_into().ok()?;
            Some((f64::from_be_bytes(arr), p + 9))
        }
        0xca => {
            let bs = body.get(p + 1..p + 5)?;
            let arr: [u8; 4] = bs.try_into().ok()?;
            Some((f32::from_be_bytes(arr) as f64, p + 5))
        }
        // Some encoders compact whole-number f64s as ints. Tolerate.
        _ => read_int(body, p).map(|(v, end)| (v as f64, end)),
    }
}

#[inline]
fn read_int(body: &[u8], p: usize) -> Option<(i64, usize)> {
    let b = *body.get(p)?;
    match b {
        0x00..=0x7f => Some((b as i64, p + 1)),
        0xe0..=0xff => Some(((b as i8) as i64, p + 1)),
        0xcc => Some((*body.get(p + 1)? as i64, p + 2)),
        0xcd => {
            let bs = body.get(p + 1..p + 3)?;
            Some((u16::from_be_bytes([bs[0], bs[1]]) as i64, p + 3))
        }
        0xce => {
            let bs = body.get(p + 1..p + 5)?;
            Some((
                u32::from_be_bytes([bs[0], bs[1], bs[2], bs[3]]) as i64,
                p + 5,
            ))
        }
        0xcf => {
            let bs = body.get(p + 1..p + 9)?;
            let arr: [u8; 8] = bs.try_into().ok()?;
            Some((u64::from_be_bytes(arr) as i64, p + 9))
        }
        0xd0 => Some(((*body.get(p + 1)? as i8) as i64, p + 2)),
        0xd1 => {
            let bs = body.get(p + 1..p + 3)?;
            Some((i16::from_be_bytes([bs[0], bs[1]]) as i64, p + 3))
        }
        0xd2 => {
            let bs = body.get(p + 1..p + 5)?;
            Some((
                i32::from_be_bytes([bs[0], bs[1], bs[2], bs[3]]) as i64,
                p + 5,
            ))
        }
        0xd3 => {
            let bs = body.get(p + 1..p + 9)?;
            let arr: [u8; 8] = bs.try_into().ok()?;
            Some((i64::from_be_bytes(arr), p + 9))
        }
        _ => None,
    }
}

#[inline]
fn assign_opt_f64(body: &[u8], p: usize, out: &mut Option<f64>) -> Option<usize> {
    if *body.get(p)? == 0xc0 {
        *out = None;
        return Some(p + 1);
    }
    let (v, end) = read_f64(body, p)?;
    *out = Some(v);
    Some(end)
}

#[inline]
fn assign_opt_u64(body: &[u8], p: usize, out: &mut Option<u64>) -> Option<usize> {
    if *body.get(p)? == 0xc0 {
        *out = None;
        return Some(p + 1);
    }
    let (v, end) = read_int(body, p)?;
    *out = Some(v as u64);
    Some(end)
}

#[inline]
fn assign_opt_i32(body: &[u8], p: usize, out: &mut Option<i32>) -> Option<usize> {
    if *body.get(p)? == 0xc0 {
        *out = None;
        return Some(p + 1);
    }
    let (v, end) = read_int(body, p)?;
    *out = Some(v as i32);
    Some(end)
}

#[inline]
fn assign_string(body: &[u8], p: usize, out: &mut String) -> Option<usize> {
    let (s, end) = read_str_bytes(body, p)?;
    out.clear();
    // SAFETY: msgpack str type guarantees valid UTF-8 per the spec.
    // The broker emits these via rmp_serde which enforces UTF-8 at
    // encode time. Skipping the validation cost here saves ~10-30 ns
    // per string field on the hot path.
    out.push_str(unsafe { std::str::from_utf8_unchecked(s) });
    Some(end)
}

#[inline]
fn skip_value(body: &[u8], p: usize) -> Option<usize> {
    let b = *body.get(p)?;
    match b {
        // nil / bool / fixint
        0xc0 | 0xc2 | 0xc3 | 0x00..=0x7f | 0xe0..=0xff => Some(p + 1),
        // u8 / i8 / fixext1
        0xcc | 0xd0 | 0xd4 => Some(p + 2),
        // u16 / i16 / fixext2
        0xcd | 0xd1 | 0xd5 => Some(p + 3),
        // u32 / i32 / float32 / fixext4
        0xce | 0xd2 | 0xca | 0xd6 => Some(p + 5),
        // u64 / i64 / float64 / fixext8
        0xcf | 0xd3 | 0xcb | 0xd7 => Some(p + 9),
        // fixext16
        0xd8 => Some(p + 18),
        // fixstr
        0xa0..=0xbf => Some(p + 1 + (b & 0x1f) as usize),
        // str8 / bin8
        0xd9 | 0xc4 => Some(p + 2 + *body.get(p + 1)? as usize),
        0xda | 0xc5 => {
            let bs = body.get(p + 1..p + 3)?;
            Some(p + 3 + u16::from_be_bytes([bs[0], bs[1]]) as usize)
        }
        0xdb | 0xc6 => {
            let bs = body.get(p + 1..p + 5)?;
            Some(p + 5 + u32::from_be_bytes([bs[0], bs[1], bs[2], bs[3]]) as usize)
        }
        // fixarray
        0x90..=0x9f => skip_n_values(body, p + 1, (b & 0x0f) as usize),
        0xdc => {
            let bs = body.get(p + 1..p + 3)?;
            skip_n_values(body, p + 3, u16::from_be_bytes([bs[0], bs[1]]) as usize)
        }
        0xdd => {
            let bs = body.get(p + 1..p + 5)?;
            skip_n_values(
                body,
                p + 5,
                u32::from_be_bytes([bs[0], bs[1], bs[2], bs[3]]) as usize,
            )
        }
        // fixmap
        0x80..=0x8f => skip_n_values(body, p + 1, ((b & 0x0f) as usize) * 2),
        0xde => {
            let bs = body.get(p + 1..p + 3)?;
            skip_n_values(body, p + 3, u16::from_be_bytes([bs[0], bs[1]]) as usize * 2)
        }
        0xdf => {
            let bs = body.get(p + 1..p + 5)?;
            skip_n_values(
                body,
                p + 5,
                u32::from_be_bytes([bs[0], bs[1], bs[2], bs[3]]) as usize * 2,
            )
        }
        _ => None,
    }
}

#[inline]
fn skip_n_values(body: &[u8], mut p: usize, n: usize) -> Option<usize> {
    for _ in 0..n {
        p = skip_value(body, p)?;
    }
    Some(p)
}

// ─── tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::TickMsg;

    fn assert_round_trip(t: &TickMsg) {
        let body = rmp_serde::to_vec_named(t).expect("encode");
        let mut decoded = TickMsg::default();
        let ok = decode_tick(&body, &mut decoded);
        assert!(ok, "decode failed for {t:?}");
        assert_eq!(decoded.strike, t.strike, "strike mismatch");
        assert_eq!(decoded.expiry, t.expiry, "expiry mismatch");
        assert_eq!(decoded.right, t.right, "right mismatch");
        assert_eq!(decoded.bid, t.bid, "bid");
        assert_eq!(decoded.ask, t.ask, "ask");
        assert_eq!(decoded.bid_size, t.bid_size, "bid_size");
        assert_eq!(decoded.ask_size, t.ask_size, "ask_size");
        assert_eq!(decoded.ts_ns, t.ts_ns, "ts_ns");
        assert_eq!(decoded.broker_recv_ns, t.broker_recv_ns, "broker_recv_ns");
        assert_eq!(decoded.depth_bid_0, t.depth_bid_0, "depth_bid_0");
        assert_eq!(decoded.depth_bid_1, t.depth_bid_1, "depth_bid_1");
        assert_eq!(decoded.depth_ask_0, t.depth_ask_0, "depth_ask_0");
        assert_eq!(decoded.depth_ask_1, t.depth_ask_1, "depth_ask_1");
    }

    #[test]
    fn typical_tick() {
        assert_round_trip(&TickMsg {
            strike: 6.025,
            expiry: "20260526".into(),
            right: "C".into(),
            bid: Some(0.0345),
            ask: Some(0.036),
            bid_size: Some(5),
            ask_size: Some(7),
            ts_ns: Some(1_777_993_654_064_490_620),
            broker_recv_ns: Some(1_777_993_654_064_490_620),
            depth_bid_0: Some(0.0345),
            depth_bid_1: Some(0.034),
            depth_ask_0: Some(0.036),
            depth_ask_1: Some(0.0365),
        });
    }

    #[test]
    fn nones_and_defaults() {
        assert_round_trip(&TickMsg {
            strike: 0.0,
            expiry: "".into(),
            right: "P".into(),
            bid: None,
            ask: None,
            bid_size: None,
            ask_size: None,
            ts_ns: None,
            broker_recv_ns: None,
            depth_bid_0: None,
            depth_bid_1: None,
            depth_ask_0: None,
            depth_ask_1: None,
        });
    }

    #[test]
    fn negative_strike_and_zero_sizes() {
        // Defensive: nothing in the spec says strike can't be negative
        // (it can't on the wire, but the decoder shouldn't care).
        assert_round_trip(&TickMsg {
            strike: -1.5,
            expiry: "20271231".into(),
            right: "P".into(),
            bid: Some(0.0),
            ask: Some(0.0005),
            bid_size: Some(0),
            ask_size: Some(0),
            ts_ns: Some(0),
            broker_recv_ns: None,
            depth_bid_0: None,
            depth_bid_1: None,
            depth_ask_0: None,
            depth_ask_1: None,
        });
    }

    #[test]
    fn buffer_reuse_clears_old_options() {
        // First decode populates a tick with all fields set.
        let t1 = TickMsg {
            strike: 5.0,
            expiry: "AAAA".into(),
            right: "C".into(),
            bid: Some(0.10),
            ask: Some(0.12),
            bid_size: Some(10),
            ask_size: Some(20),
            ts_ns: Some(1),
            broker_recv_ns: Some(2),
            depth_bid_0: Some(0.10),
            depth_bid_1: Some(0.09),
            depth_ask_0: Some(0.12),
            depth_ask_1: Some(0.13),
        };
        let body1 = rmp_serde::to_vec_named(&t1).unwrap();
        let mut buf = TickMsg::default();
        assert!(decode_tick(&body1, &mut buf));
        assert_eq!(buf.bid, Some(0.10));

        // Second decode of a tick with bid=None must clear the prior value.
        let t2 = TickMsg {
            strike: 5.0,
            expiry: "BBBB".into(),
            right: "C".into(),
            bid: None,
            ask: Some(0.13),
            bid_size: None,
            ask_size: Some(15),
            ts_ns: None,
            broker_recv_ns: None,
            depth_bid_0: None,
            depth_bid_1: None,
            depth_ask_0: None,
            depth_ask_1: None,
        };
        let body2 = rmp_serde::to_vec_named(&t2).unwrap();
        assert!(decode_tick(&body2, &mut buf));
        assert_eq!(buf.bid, None);
        assert_eq!(buf.bid_size, None);
        assert_eq!(buf.ts_ns, None);
        assert_eq!(buf.expiry, "BBBB");
    }

    #[test]
    fn malformed_returns_false() {
        let mut buf = TickMsg::default();
        // Truncated map header
        assert!(!decode_tick(&[0xde], &mut buf));
        // Empty body
        assert!(!decode_tick(&[], &mut buf));
        // Map with 1 pair but no key/value
        assert!(!decode_tick(&[0x81], &mut buf));
        // Map with key but truncated value
        assert!(!decode_tick(&[0x81, 0xa6, b's', b't', b'r', b'i', b'k', b'e'], &mut buf));
    }
}
