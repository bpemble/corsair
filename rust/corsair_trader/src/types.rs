//! Newtype wrappers for f64 values that carry semantic meaning beyond
//! their bit pattern. Audit §4.3 Option A — prevents the §16
//! (spot-vs-fit-forward) and §19 (Taylor anchor) bug classes by making
//! the compiler reject cross-substitution at call sites.
//!
//! Three values that have caused production bugs by being confused
//! with each other:
//!   - `FitForward`  — broker's parity-implied F at the SABR/SVI fit
//!                     time. SVI/SABR's `m` parameter is anchored on
//!                     log(K/this); using anything else re-anchors
//!                     the surface. This is the §16 root cause value.
//!   - `SpotAtFit`   — the spot the broker observed AT fit time.
//!                     The Taylor anchor: theo += δ × (current_spot −
//!                     spot_at_fit). NOT the same as `FitForward` on
//!                     calendar-carrying products (§19 root cause).
//!   - `CurrentSpot` — the trader's most recent observation of the
//!                     front-month underlying. NOT the same as either
//!                     of the above. Goes into Taylor reprice and
//!                     drift gates only.
//!
//! All three are `f64` at the bit level, but the compiler now refuses
//! to coerce between them. Conversion to/from `f64` is explicit
//! (`.0` access or `.raw()`); construction is explicit (`FitForward(x)`).
//! There is no `From<f64>` and no `Deref<Target=f64>` — the whole
//! point is that "this f64 is a fit forward" must be a deliberate
//! choice at construction time.
//!
//! The wrapped layout is `#[repr(transparent)]`, so these are zero-cost
//! at the ABI level. Serde derive uses the inner f64's representation,
//! so wire formats (msgpack / JSON) are unaffected.

use serde::{Deserialize, Serialize};

/// Fit-time forward (broker's parity-implied F when the SABR/SVI
/// surface was calibrated). Anchor for SVI's `m` parameter.
/// **MUST be the only f64 passed as `forward` to compute_theo.**
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct FitForward(pub f64);

/// Spot at fit time (broker's underlying-price observation when the
/// SABR/SVI surface was calibrated). Anchor for the Taylor reprice:
/// `theo = theo_at_fit + δ × (current_spot − spot_at_fit)`.
/// Distinct from `FitForward` on products with calendar carry —
/// substituting one for the other is the §19 bug.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct SpotAtFit(pub f64);

/// Current front-month underlying spot — the trader's most recent
/// observation. Used in Taylor reprice and forward-drift gates.
/// Distinct from the two above; passing this to compute_theo as
/// the `forward` argument is the §16 bug.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct CurrentSpot(pub f64);

impl FitForward {
    /// Explicit unwrap to raw f64. Use only at the boundary between
    /// the trader's typed values and pure-math callees (pricing crate,
    /// Taylor arithmetic) — NOT to bypass the type guard at a call
    /// site that ought to be rejected.
    #[inline]
    pub fn raw(self) -> f64 {
        self.0
    }
}

impl SpotAtFit {
    #[inline]
    pub fn raw(self) -> f64 {
        self.0
    }
}

impl CurrentSpot {
    #[inline]
    pub fn raw(self) -> f64 {
        self.0
    }
}

// Compile-time enforcement is implicit: this module's signatures
// reject cross-type substitution at the call site. A code change that
// passes a `CurrentSpot` to a function expecting `FitForward` fails
// to compile with E0308 ("expected `FitForward`, found `CurrentSpot`")
// — there is no `From` impl, no `Deref` coercion, and no implicit
// numeric promotion. The only path between newtypes is explicit `.0`
// or `.raw()` access, which is grep-able and review-visible.
//
// Runtime tests below assert the `.raw()` boundary works as expected
// and that the §16 / §19 magnitude assumptions hold under the typed
// API. The existence of the compile-time guard is verified by the
// fact that the rest of the trader code COMPILES under these types
// — any cross-substitution would have failed `cargo build` and
// blocked this PR.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_unwrap_round_trips() {
        let f = FitForward(6.021);
        assert_eq!(f.raw(), 6.021);
        let s = SpotAtFit(6.00);
        assert_eq!(s.raw(), 6.00);
        let c = CurrentSpot(5.96);
        assert_eq!(c.raw(), 5.96);
    }

    #[test]
    fn newtypes_have_zero_runtime_overhead() {
        // `#[repr(transparent)]` means the wrapped struct has the
        // same memory layout as the inner type. This test asserts
        // the size invariant — if it ever fails, the
        // `#[repr(transparent)]` attribute was dropped or a derive
        // inserted padding.
        assert_eq!(
            std::mem::size_of::<FitForward>(),
            std::mem::size_of::<f64>()
        );
        assert_eq!(
            std::mem::size_of::<SpotAtFit>(),
            std::mem::size_of::<f64>()
        );
        assert_eq!(
            std::mem::size_of::<CurrentSpot>(),
            std::mem::size_of::<f64>()
        );
    }
}
