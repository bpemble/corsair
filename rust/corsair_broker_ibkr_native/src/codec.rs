//! Wire codec — message framing on the IBKR API socket.
//!
//! Outbound (client → server):
//!   - Each field is encoded as a UTF-8 string + `\0` terminator.
//!   - Fields are concatenated; total payload is prefixed with a
//!     4-byte big-endian length (after-API-version messages only).
//!   - Initial handshake is a SPECIAL CASE — it's prefixed by
//!     "API\0" plus the length-prefixed version-range string.
//!
//! Inbound (server → client):
//!   - Each message is `[4-byte BE length][null-separated UTF-8 fields]`.
//!   - Last field has a trailing `\0` (the length includes it).
//!   - Strip the trailing empty after splitting on `\0`.
//!
//! Numeric encoding helpers:
//!   - `encode_int(i32)` → "12345"
//!   - `encode_f64(f)` → "1.5" or "Infinite" for math.inf
//!   - `encode_bool(b)` → "1" or "0"
//!   - Missing/unset → empty string ""

use bytes::{BufMut, BytesMut};
use std::io;

use crate::error::NativeError;

/// Maximum incoming message size we'll accept. IBKR doesn't define a
/// hard cap; this is a safety guard against runaway memory.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Encode a list of fields into a length-prefixed frame.
pub fn encode_fields(fields: &[&str]) -> Vec<u8> {
    let mut payload = Vec::new();
    for f in fields {
        payload.extend_from_slice(f.as_bytes());
        payload.push(0); // \0 terminator
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Encode a list of String fields. Convenience for callers that
/// already have owned Strings.
pub fn encode_owned(fields: &[String]) -> Vec<u8> {
    let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
    encode_fields(&refs)
}

/// Try to extract one complete frame from the front of `buf`.
/// Returns Ok(Some(fields)) when a frame was decoded, or Ok(None)
/// if more bytes are needed. Advances `buf` on successful decode.
///
/// Allocation note: an earlier version of this called
/// `String::from_utf8_lossy(payload).into_owned()` upfront — copying
/// the WHOLE payload before splitting. We now split the byte slice
/// directly and only allocate per-field, halving the alloc volume on
/// the hot path. UTF-8 lossy semantics are preserved (matches
/// ib_insync's `errors=backslashreplace` for resilience to weird
/// gateway state).
pub fn try_decode_frame(buf: &mut BytesMut) -> Result<Option<Vec<String>>, NativeError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&buf[..4]);
    let size = u32::from_be_bytes(len_bytes) as usize;
    if size > MAX_MESSAGE_BYTES {
        return Err(NativeError::Protocol(format!(
            "oversize message: {size} > {MAX_MESSAGE_BYTES}"
        )));
    }
    if buf.len() < 4 + size {
        return Ok(None);
    }
    let payload = &buf[4..4 + size];
    // Capacity hint: typical messages are 5-20 fields. Pre-allocating
    // 16 covers the common case without overallocating.
    let mut fields: Vec<String> = Vec::with_capacity(16);
    for field in payload.split(|&b| b == 0) {
        // Per-field UTF-8 conversion. `from_utf8_lossy` returns a
        // Cow that borrows when the bytes are valid UTF-8 (the common
        // case — IBKR fields are ASCII). into_owned() then only
        // allocates when validation produced replacement chars.
        // Otherwise it copies the bytes once.
        fields.push(String::from_utf8_lossy(field).into_owned());
    }
    // Last field is empty due to trailing \0; pop it.
    if fields.last().map(|f| f.is_empty()).unwrap_or(false) {
        fields.pop();
    }
    let _ = buf.split_to(4 + size);
    Ok(Some(fields))
}

/// Drain ALL complete frames from buf. Leaves a partial frame at the
/// tail.
pub fn try_decode_all(buf: &mut BytesMut) -> Result<Vec<Vec<String>>, NativeError> {
    let mut out = Vec::new();
    while let Some(fields) = try_decode_frame(buf)? {
        out.push(fields);
    }
    Ok(out)
}

// ─── Initial handshake encoding ───────────────────────────────────

/// Produce the "API\0" prefix + length-prefixed version-range
/// string. Sent ONCE as the very first thing on the socket.
///
/// IBKR's API protocol negotiates server version on connect. We
/// announce a range; server picks the highest it supports.
pub fn encode_handshake(min_version: i32, max_version: i32) -> Vec<u8> {
    let version_str = format!("v{min_version}..{max_version}");
    let mut out = BytesMut::new();
    out.put_slice(b"API\0");
    out.put_u32(version_str.len() as u32);
    out.put_slice(version_str.as_bytes());
    out.to_vec()
}

// ─── Field encoding helpers ───────────────────────────────────────
//
// String-returning variants for the legacy slow-path encoder. The hot
// path (place_template::place_order_fast) uses the `write_*` family
// below which writes directly into a Vec<u8> with zero allocation per
// field, via itoa (~5x faster than i32::to_string) and ryu (~6x
// faster than f64::to_string).

pub fn encode_int(v: i32) -> String {
    let mut buf = itoa::Buffer::new();
    buf.format(v).to_string()
}

pub fn encode_int64(v: i64) -> String {
    let mut buf = itoa::Buffer::new();
    buf.format(v).to_string()
}

pub fn encode_f64(v: f64) -> String {
    if v.is_infinite() {
        "Infinite".into()
    } else {
        let mut buf = ryu::Buffer::new();
        buf.format(v).to_string()
    }
}

/// String form of `write_qty` for the slow-path encoder. Emits "1"
/// for integral values (matching ib_insync's str(int)) and ryu float
/// otherwise.
pub fn encode_qty(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() && v.abs() < 1e18 {
        let mut buf = itoa::Buffer::new();
        buf.format(v as i64).to_string()
    } else {
        encode_f64(v)
    }
}

/// Write an i32 directly to `buf` (no String allocation).
#[inline]
pub fn write_int(buf: &mut Vec<u8>, v: i32) {
    let mut itoa_buf = itoa::Buffer::new();
    buf.extend_from_slice(itoa_buf.format(v).as_bytes());
}

/// Write an i64 directly to `buf`.
#[inline]
pub fn write_int64(buf: &mut Vec<u8>, v: i64) {
    let mut itoa_buf = itoa::Buffer::new();
    buf.extend_from_slice(itoa_buf.format(v).as_bytes());
}

/// Write an f64 directly to `buf`. Mirrors `encode_f64` — emits
/// "Infinite" for `f64::INFINITY`, ryu shortest-round-trip otherwise.
#[inline]
pub fn write_f64(buf: &mut Vec<u8>, v: f64) {
    if v.is_infinite() {
        buf.extend_from_slice(b"Infinite");
    } else {
        let mut ryu_buf = ryu::Buffer::new();
        buf.extend_from_slice(ryu_buf.format(v).as_bytes());
    }
}

/// Write a quantity to `buf`. ib_insync emits integral quantities as
/// "1" (Python int) but floats as "1.0" (Python float). For total
/// quantity on options/futures (always whole numbers in our use case),
/// the int form matches ib_insync's reference encoding byte-for-byte.
/// Falls back to float (`write_f64`) for non-integral values, e.g.
/// fractional crypto orders.
#[inline]
pub fn write_qty(buf: &mut Vec<u8>, v: f64) {
    if v.fract() == 0.0 && v.is_finite() && v.abs() < 1e18 {
        let mut itoa_buf = itoa::Buffer::new();
        buf.extend_from_slice(itoa_buf.format(v as i64).as_bytes());
    } else {
        write_f64(buf, v);
    }
}

pub fn encode_bool(v: bool) -> String {
    (if v { "1" } else { "0" }).into()
}

pub fn encode_unset() -> String {
    String::new()
}

// ─── Helpers for read-side parsing ────────────────────────────────

pub fn parse_int(s: &str) -> Result<i32, NativeError> {
    s.parse::<i32>()
        .map_err(|e| NativeError::Malformed(format!("expected int, got {s:?}: {e}")))
}

pub fn parse_int64(s: &str) -> Result<i64, NativeError> {
    s.parse::<i64>()
        .map_err(|e| NativeError::Malformed(format!("expected int64, got {s:?}: {e}")))
}

pub fn parse_f64(s: &str) -> Result<f64, NativeError> {
    if s.is_empty() {
        return Ok(0.0);
    }
    if s == "Infinite" {
        return Ok(f64::INFINITY);
    }
    s.parse::<f64>()
        .map_err(|e| NativeError::Malformed(format!("expected f64, got {s:?}: {e}")))
}

pub fn parse_bool(s: &str) -> bool {
    s == "1" || s.eq_ignore_ascii_case("true")
}

// ─── tokio AsyncWrite helper (used by client) ─────────────────────

/// Write a frame to the wire. Helper around `AsyncWriteExt::write_all`
/// that produces a single send call per frame.
pub async fn write_frame<W>(w: &mut W, frame: &[u8]) -> io::Result<()>
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    w.write_all(frame).await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_fields_pads_with_nulls() {
        let frame = encode_fields(&["1", "2", "3"]);
        // length prefix is 6 bytes (3 fields × 2 bytes each: digit + \0)
        assert_eq!(&frame[..4], &[0, 0, 0, 6]);
        // payload should be "1\02\03\0"
        assert_eq!(&frame[4..], b"1\x002\x003\x00");
    }

    #[test]
    fn encode_empty_field_is_just_terminator() {
        let frame = encode_fields(&["", ""]);
        assert_eq!(&frame[..4], &[0, 0, 0, 2]);
        assert_eq!(&frame[4..], b"\x00\x00");
    }

    #[test]
    fn round_trip_one_message() {
        let frame = encode_fields(&["test", "42", "1.5"]);
        let mut buf = BytesMut::from(frame.as_slice());
        let fields = try_decode_frame(&mut buf).unwrap().unwrap();
        assert_eq!(fields, vec!["test", "42", "1.5"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_frame_returns_none() {
        let frame = encode_fields(&["test"]);
        // Cut off the last byte
        let mut buf = BytesMut::from(&frame[..frame.len() - 1]);
        let r = try_decode_frame(&mut buf).unwrap();
        assert!(r.is_none());
        assert_eq!(buf.len(), frame.len() - 1, "partial buf untouched");
    }

    #[test]
    fn try_decode_all_returns_multiple() {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&encode_fields(&["a"]));
        buf.extend_from_slice(&encode_fields(&["b", "c"]));
        buf.extend_from_slice(&encode_fields(&["d", "e", "f"]));
        let frames = try_decode_all(&mut buf).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0], vec!["a"]);
        assert_eq!(frames[1], vec!["b", "c"]);
        assert_eq!(frames[2], vec!["d", "e", "f"]);
    }

    #[test]
    fn handshake_format() {
        let h = encode_handshake(100, 176);
        assert_eq!(&h[..4], b"API\x00");
        // version string "v100..176" is 9 bytes
        assert_eq!(&h[4..8], &[0, 0, 0, 9]);
        assert_eq!(&h[8..], b"v100..176");
    }

    #[test]
    fn oversize_frame_rejected() {
        // Forge a header claiming 32 MB (above MAX).
        let mut buf = BytesMut::with_capacity(4);
        buf.extend_from_slice(&(64u32 * 1024 * 1024).to_be_bytes());
        let r = try_decode_frame(&mut buf);
        assert!(matches!(r, Err(NativeError::Protocol(_))));
    }

    #[test]
    fn parse_helpers() {
        assert_eq!(parse_int("123").unwrap(), 123);
        assert_eq!(parse_f64("1.5").unwrap(), 1.5);
        assert!(parse_f64("Infinite").unwrap().is_infinite());
        assert_eq!(parse_f64("").unwrap(), 0.0);
        assert!(parse_bool("1"));
        assert!(!parse_bool("0"));
        assert_eq!(encode_f64(f64::INFINITY), "Infinite");
        assert_eq!(encode_bool(true), "1");
    }
}
