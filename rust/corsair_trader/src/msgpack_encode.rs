//! Hand-rolled msgpack encoders for the trader's outbound hot-path
//! command messages. Replaces `rmp_serde::encode::write_named(buf, val)`
//! at the high-frequency `place_order` / `modify_order` / `cancel_order`
//! sites. Symmetric to the broker's `encode_tick_into` shipped in
//! commit 3006627.
//!
//! Each encoder writes msgpack-named-fields fixmap into a caller-
//! provided `Vec<u8>` (the trader's per-tick `wire_buf`). No
//! allocation on the hot path; reflection-free.
//!
//! Output schema (must match what `corsair_broker::ipc::PlaceOrderCmd`
//! et al. deserialize):
//!   - type: fixstr
//!   - all f64 fields: 0xcb + 8 BE bytes
//!   - all i32 / i64 fields: 0xd2 / 0xd3 + BE bytes
//!   - all u32 / u64 fields: 0xce / 0xcf + BE bytes
//!   - Option<u64>: omitted from fixmap when None (mirrors the
//!     `#[serde(skip_serializing_if = "Option::is_none")]` attribute
//!     on the typed structs)
//!
//! Round-trip parity with `rmp_serde::to_vec_named` is verified by the
//! tests below — every commit touching this file should keep them green.

use crate::messages::{CancelOrder, ModifyOrder, PlaceOrder};

// ─── primitive writers ───────────────────────────────────────────────

#[inline]
fn write_fixstr_short(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    debug_assert!(bytes.len() <= 31, "key too long for fixstr: {}", s);
    buf.push(0xa0 | (bytes.len() as u8));
    buf.extend_from_slice(bytes);
}

/// Same as `write_fixstr_short` but handles strings up to u32::MAX bytes.
/// Hot-path expiry strings are 8 bytes (YYYYMMDD); fixstr branch dominates.
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
    write_fixstr_short(buf, key);
    write_str(buf, value);
}

#[inline]
fn write_f64_kv(buf: &mut Vec<u8>, key: &str, value: f64) {
    write_fixstr_short(buf, key);
    buf.push(0xcb);
    buf.extend_from_slice(&value.to_be_bytes());
}

#[inline]
fn write_u32_kv(buf: &mut Vec<u8>, key: &str, value: u32) {
    write_fixstr_short(buf, key);
    buf.push(0xce);
    buf.extend_from_slice(&value.to_be_bytes());
}

#[inline]
fn write_u64_kv(buf: &mut Vec<u8>, key: &str, value: u64) {
    write_fixstr_short(buf, key);
    buf.push(0xcf);
    buf.extend_from_slice(&value.to_be_bytes());
}

#[inline]
fn write_i32_kv(buf: &mut Vec<u8>, key: &str, value: i32) {
    write_fixstr_short(buf, key);
    buf.push(0xd2);
    buf.extend_from_slice(&value.to_be_bytes());
}

#[inline]
fn write_i64_kv(buf: &mut Vec<u8>, key: &str, value: i64) {
    write_fixstr_short(buf, key);
    buf.push(0xd3);
    buf.extend_from_slice(&value.to_be_bytes());
}

// ─── outbound command encoders ───────────────────────────────────────

/// Encode a `PlaceOrder` into `buf` as msgpack-named-fields.
/// Mirrors the rmp_serde-named encoding used by the broker's
/// `PlaceOrderCmd` deserialize path. `triggering_tick_broker_recv_ns`
/// is omitted from the map when None (matches `skip_serializing_if`).
pub fn encode_place_into(buf: &mut Vec<u8>, p: &PlaceOrder<'_>) {
    let mut n_fields: u8 = 9; // type + ts_ns + strike + expiry + right + side + qty + price + orderRef
    if p.triggering_tick_broker_recv_ns.is_some() {
        n_fields += 1;
    }
    debug_assert!(n_fields <= 15);
    buf.push(0x80 | n_fields);

    write_str_kv(buf, "type", p.msg_type);
    write_u64_kv(buf, "ts_ns", p.ts_ns);
    write_f64_kv(buf, "strike", p.strike);
    write_str_kv(buf, "expiry", p.expiry);
    write_str_kv(buf, "right", p.right);
    write_str_kv(buf, "side", p.side);
    write_i32_kv(buf, "qty", p.qty);
    write_f64_kv(buf, "price", p.price);
    write_str_kv(buf, "orderRef", p.order_ref);
    if let Some(v) = p.triggering_tick_broker_recv_ns {
        write_u64_kv(buf, "triggering_tick_broker_recv_ns", v);
    }
}

/// Encode a `ModifyOrder` into `buf`.
pub fn encode_modify_into(buf: &mut Vec<u8>, m: &ModifyOrder) {
    let mut n_fields: u8 = 5; // type + ts_ns + order_id + price + gtd_seconds
    if m.triggering_tick_broker_recv_ns.is_some() {
        n_fields += 1;
    }
    debug_assert!(n_fields <= 15);
    buf.push(0x80 | n_fields);

    write_str_kv(buf, "type", m.msg_type);
    write_u64_kv(buf, "ts_ns", m.ts_ns);
    write_i64_kv(buf, "order_id", m.order_id);
    write_f64_kv(buf, "price", m.price);
    write_u32_kv(buf, "gtd_seconds", m.gtd_seconds);
    if let Some(v) = m.triggering_tick_broker_recv_ns {
        write_u64_kv(buf, "triggering_tick_broker_recv_ns", v);
    }
}

/// Encode a `CancelOrder` into `buf`. CancelOrder uses the field name
/// `orderId` on the wire (matches the broker's `CancelOrderCmd`).
pub fn encode_cancel_into(buf: &mut Vec<u8>, c: &CancelOrder) {
    buf.push(0x80 | 3); // 3 fields: type + ts_ns + orderId
    write_str_kv(buf, "type", c.msg_type);
    write_u64_kv(buf, "ts_ns", c.ts_ns);
    write_i64_kv(buf, "orderId", c.order_id);
}

// ─── tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Round-trip parity tests vs `rmp_serde::to_vec_named`. Decode the
    //! hand-rolled output back through rmp_serde into a shadow struct
    //! and assert every field.
    //!
    //! Defending the schema invariant: any change to PlaceOrder /
    //! ModifyOrder / CancelOrder fields needs both the typed struct in
    //! messages.rs AND the encoder here to update — these tests catch
    //! the drift at compile/test time.
    use super::*;
    use crate::messages::{CancelOrder, ModifyOrder, PlaceOrder};
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct PlaceShadow {
        #[serde(rename = "type")]
        ty: String,
        ts_ns: u64,
        strike: f64,
        expiry: String,
        right: String,
        side: String,
        qty: i32,
        price: f64,
        #[serde(rename = "orderRef")]
        order_ref: String,
        #[serde(default)]
        triggering_tick_broker_recv_ns: Option<u64>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct ModifyShadow {
        #[serde(rename = "type")]
        ty: String,
        ts_ns: u64,
        order_id: i64,
        price: f64,
        gtd_seconds: u32,
        #[serde(default)]
        triggering_tick_broker_recv_ns: Option<u64>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct CancelShadow {
        #[serde(rename = "type")]
        ty: String,
        ts_ns: u64,
        #[serde(rename = "orderId")]
        order_id: i64,
    }

    #[test]
    fn place_round_trip_full() {
        let p = PlaceOrder {
            msg_type: "place_order",
            ts_ns: 1_777_993_654_064_490_620,
            strike: 6.025,
            expiry: "20260526",
            right: "C",
            side: "BUY",
            qty: 1,
            price: 0.0345,
            order_ref: "corsair_trader_rust",
            triggering_tick_broker_recv_ns: Some(1_777_993_654_000_000_000),
        };
        let mut hand = Vec::new();
        encode_place_into(&mut hand, &p);
        let serde_bytes = rmp_serde::to_vec_named(&p).unwrap();
        // Hand-rolled and serde may differ in field ordering and
        // numeric encoding compactness, but BOTH must round-trip
        // through rmp_serde decode to the same logical shadow.
        let h: PlaceShadow = rmp_serde::from_slice(&hand).expect("hand decode");
        let s: PlaceShadow = rmp_serde::from_slice(&serde_bytes).expect("serde decode");
        assert_eq!(h, s);
    }

    #[test]
    fn place_round_trip_no_triggering_ns() {
        let p = PlaceOrder {
            msg_type: "place_order",
            ts_ns: 1,
            strike: 6.0,
            expiry: "20260526",
            right: "P",
            side: "SELL",
            qty: 1,
            price: 0.10,
            order_ref: "corsair_trader_rust",
            triggering_tick_broker_recv_ns: None,
        };
        let mut hand = Vec::new();
        encode_place_into(&mut hand, &p);
        let h: PlaceShadow = rmp_serde::from_slice(&hand).expect("hand decode");
        assert_eq!(h.triggering_tick_broker_recv_ns, None);
        assert_eq!(h.right, "P");
        assert_eq!(h.side, "SELL");
        assert_eq!(h.qty, 1);
    }

    #[test]
    fn modify_round_trip_full() {
        let m = ModifyOrder {
            msg_type: "modify_order",
            ts_ns: 100,
            order_id: 12345,
            price: 0.0335,
            gtd_seconds: 30,
            triggering_tick_broker_recv_ns: Some(99),
        };
        let mut hand = Vec::new();
        encode_modify_into(&mut hand, &m);
        let h: ModifyShadow = rmp_serde::from_slice(&hand).expect("hand decode");
        assert_eq!(h.ts_ns, 100);
        assert_eq!(h.order_id, 12345);
        assert_eq!(h.price, 0.0335);
        assert_eq!(h.gtd_seconds, 30);
        assert_eq!(h.triggering_tick_broker_recv_ns, Some(99));
    }

    #[test]
    fn modify_round_trip_no_triggering_ns() {
        let m = ModifyOrder {
            msg_type: "modify_order",
            ts_ns: 1,
            order_id: 7,
            price: 0.0,
            gtd_seconds: 5,
            triggering_tick_broker_recv_ns: None,
        };
        let mut hand = Vec::new();
        encode_modify_into(&mut hand, &m);
        let h: ModifyShadow = rmp_serde::from_slice(&hand).expect("hand decode");
        assert_eq!(h.triggering_tick_broker_recv_ns, None);
    }

    #[test]
    fn cancel_round_trip() {
        let c = CancelOrder {
            msg_type: "cancel_order",
            ts_ns: 42,
            order_id: -1, // negative is structurally allowed; broker rejects later
        };
        let mut hand = Vec::new();
        encode_cancel_into(&mut hand, &c);
        let h: CancelShadow = rmp_serde::from_slice(&hand).expect("hand decode");
        assert_eq!(h.ts_ns, 42);
        assert_eq!(h.order_id, -1);
        assert_eq!(h.ty, "cancel_order");
    }

    #[test]
    fn negative_orderid_round_trips() {
        // Sanity: i64 negatives use 0xd3.
        let c = CancelOrder {
            msg_type: "cancel_order",
            ts_ns: 0,
            order_id: -42_000_000_000,
        };
        let mut hand = Vec::new();
        encode_cancel_into(&mut hand, &c);
        let h: CancelShadow = rmp_serde::from_slice(&hand).expect("hand decode");
        assert_eq!(h.order_id, -42_000_000_000);
    }
}
