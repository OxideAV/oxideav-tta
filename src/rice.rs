//! Adaptive Rice entropy decoder per `spec/05-rice.md`.
//!
//! Per-channel state `(k0, k1, sum0, sum1)` is reset to the constants
//! `(10, 10, 0x4000, 0x4000)` at every frame boundary. The decoder
//! consumes one Rice value per call: a unary prefix, then a `k`-bit
//! binary tail (`k = k0` if prefix `u == 0`, else `k = k1`), then
//! reassembles the unsigned magnitude (with a depth-1 escape bias
//! `1 << k0` in the high-mode branch), updates the trackers per the
//! IIR-feedback law of spec §5.2 with the 2x window thresholds of §5.3,
//! and finally zigzag-decodes the magnitude into a signed residual per
//! §3.5.

use crate::bitreader::BitReader;
use crate::error::Result;

/// Per-channel adaptive-Rice state. See spec §4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiceState {
    pub k0: u32,
    pub k1: u32,
    pub sum0: u32,
    pub sum1: u32,
}

impl RiceState {
    /// Frame-entry constants (spec §4.2).
    pub const fn frame_init() -> Self {
        Self {
            k0: 10,
            k1: 10,
            sum0: 0x4000,
            sum1: 0x4000,
        }
    }
}

impl Default for RiceState {
    fn default() -> Self {
        Self::frame_init()
    }
}

/// `1 << shift`, saturating at `1 << 31` for `shift >= 31` so the
/// threshold computations of spec §5.3 do not panic on `k >= 26`.
#[inline]
fn shl_saturating(shift: u32) -> u32 {
    if shift >= 31 {
        0x8000_0000
    } else {
        1u32 << shift
    }
}

/// Decode one Rice value from `reader` and return the signed residual.
/// Updates `state` in place per spec §5.
pub fn decode_one(reader: &mut BitReader<'_>, state: &mut RiceState) -> Result<i32> {
    let u = reader.read_unary()?;
    let (mode_high, k_for_tail, prefix_value) = if u == 0 {
        (false, state.k0, 0u32)
    } else {
        (true, state.k1, u - 1)
    };

    let binary_tail = reader.read_bits(k_for_tail)?;
    let mut value = prefix_value
        .wrapping_shl(k_for_tail)
        .wrapping_add(binary_tail);

    // STEP A — high-mode k1/sum1 update happens on the PRE-bias value.
    if mode_high {
        state.sum1 = state.sum1.wrapping_add(value).wrapping_sub(state.sum1 >> 4);
        if state.k1 > 0 && state.sum1 < shl_saturating(state.k1 + 4) {
            state.k1 -= 1;
        } else if state.sum1 > shl_saturating(state.k1 + 5) {
            state.k1 += 1;
        }
        // Add the depth-1 escape bias using the CURRENT k0 (spec §3.4,
        // §5.4) — k0 has not yet been touched by STEP B.
        value = value.wrapping_add(shl_saturating(state.k0));
    }

    // STEP B — k0/sum0 update on the POST-bias value.
    state.sum0 = state.sum0.wrapping_add(value).wrapping_sub(state.sum0 >> 4);
    if state.k0 > 0 && state.sum0 < shl_saturating(state.k0 + 4) {
        state.k0 -= 1;
    } else if state.sum0 > shl_saturating(state.k0 + 5) {
        state.k0 += 1;
    }

    // STEP C — TTA-zigzag de-zigzag (spec §3.5).
    Ok(dezigzag(value))
}

/// TTA-flavoured zigzag (odd → positive, even → non-positive).
#[inline]
pub fn dezigzag(value: u32) -> i32 {
    if value & 1 == 1 {
        // (value + 1) >> 1 -> positive
        ((value.wrapping_add(1)) >> 1) as i32
    } else {
        // -(value >> 1) -> non-positive
        -((value >> 1) as i32)
    }
}

/// Inverse of `dezigzag` — maps a signed residual to its unsigned
/// zigzag magnitude. Used by the test-only encoder (`crate::encoder`).
#[cfg(test)]
#[inline]
pub fn zigzag(e: i32) -> u32 {
    if e > 0 {
        (e as u32).wrapping_mul(2).wrapping_sub(1)
    } else {
        (-(e as i64) as u32).wrapping_mul(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_roundtrip_small_values() {
        for e in -10..=10i32 {
            assert_eq!(dezigzag(zigzag(e)), e, "roundtrip failed at e={e}");
        }
    }

    /// Spec §3.5 table — magnitude 0 → 0, magnitude 1 → +1, magnitude
    /// 2 → -1, magnitude 3 → +2, magnitude 4 → -2.
    #[test]
    fn zigzag_specific_values() {
        assert_eq!(dezigzag(0), 0);
        assert_eq!(dezigzag(1), 1);
        assert_eq!(dezigzag(2), -1);
        assert_eq!(dezigzag(3), 2);
        assert_eq!(dezigzag(4), -2);
    }

    /// Reproduce spec §7.1: starting from the frame-entry trackers
    /// `(10, 10, 0x4000, 0x4000)`, decoding the very first sample of
    /// the canonical 440 Hz mono fixture (`raw_unary=0`,
    /// `residual_signed=0`, encoded bit stream `0b00000000_000` — 11
    /// zero bits) leaves state `(9, 10, 15360, 16384)` and returns
    /// `e = 0`.
    #[test]
    fn step_zero_matches_spec_7_1() {
        // 11 zero bits (1 unary terminator + 10-bit tail of zero) =
        // two zero bytes' worth of low bits.
        let body = [0u8, 0u8];
        let mut reader = BitReader::new(&body);
        let mut state = RiceState::frame_init();
        let e = decode_one(&mut reader, &mut state).unwrap();
        assert_eq!(e, 0);
        assert_eq!(state.k0, 9);
        assert_eq!(state.k1, 10);
        assert_eq!(state.sum0, 15_360);
        assert_eq!(state.sum1, 16_384);
    }
}
