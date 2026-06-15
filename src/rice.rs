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

/// Upper bound on the adaptive Rice parameter `k` (spec §5.3).
///
/// The spec notes the increment branch has **no** semantic cap — `k`
/// can in principle grow without bound — but in the reference encoder
/// it stays in `[0, 31]` as a numerical artifact of the `bit_shift`
/// table clamping past index 31. A valid stream never drives `k`
/// above ~16, but a hostile/corrupt bitstream can chain enough
/// high-mode escapes to push `k1` (or `k0`) past 31. Without a cap,
/// the subsequent `read_bits(k)` would request more than 32 bits —
/// which trips `BitReader::read_bits`'s `k <= 32` invariant (a
/// debug-build panic; a garbage shift in release). Clamping `k` at 31
/// on increment mirrors the reference's observed `[0, 31]` range and
/// keeps every binary-tail read within `read_bits`'s contract without
/// altering the decode of any valid stream.
const MAX_K: u32 = 31;

/// Decode one Rice value from `reader` and return the signed residual.
/// Updates `state` in place per spec §5.
#[allow(dead_code)] // direct callers vanish under `--features trace`.
pub fn decode_one(reader: &mut BitReader<'_>, state: &mut RiceState) -> Result<i32> {
    Ok(decode_one_traced(reader, state)?.residual_signed)
}

/// Same as [`decode_one`] but returns the side-channel values used by
/// the spec/06 trace emitter. `RiceTrace.raw_unary` is the unary
/// prefix count, `mode` is `false` for low-mode and `true` for
/// high-mode, `k_used` is the `k` applied to the binary tail (= `k0`
/// for low-mode, `k1` for high-mode), captured **before** any
/// adaptive update.
pub fn decode_one_traced(reader: &mut BitReader<'_>, state: &mut RiceState) -> Result<RiceTrace> {
    let u = reader.read_unary()?;
    let (mode_high, k_for_tail, prefix_value) = if u == 0 {
        (false, state.k0, 0u32)
    } else {
        (true, state.k1, u - 1)
    };
    let k_used = k_for_tail;

    let binary_tail = reader.read_bits(k_for_tail)?;
    let mut value = prefix_value
        .wrapping_shl(k_for_tail)
        .wrapping_add(binary_tail);

    // STEP A — high-mode k1/sum1 update happens on the PRE-bias value.
    if mode_high {
        state.sum1 = state.sum1.wrapping_add(value).wrapping_sub(state.sum1 >> 4);
        if state.k1 > 0 && state.sum1 < shl_saturating(state.k1 + 4) {
            state.k1 -= 1;
        } else if state.k1 < MAX_K && state.sum1 > shl_saturating(state.k1 + 5) {
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
    } else if state.k0 < MAX_K && state.sum0 > shl_saturating(state.k0 + 5) {
        state.k0 += 1;
    }

    // STEP C — TTA-zigzag de-zigzag (spec §3.5).
    let residual_signed = dezigzag(value);
    Ok(RiceTrace {
        raw_unary: u,
        mode_high,
        k_used,
        residual_signed,
        k0_post: state.k0,
        k1_post: state.k1,
        sum0_post: state.sum0,
        sum1_post: state.sum1,
    })
}

/// Side-channel return of [`decode_one_traced`] — the values
/// populating spec/06's `RICE_DECODE` and `RICE_K_UPDATE` events.
#[allow(dead_code)] // many fields only referenced by the trace emitter (cfg-gated).
pub struct RiceTrace {
    pub raw_unary: u32,
    pub mode_high: bool,
    pub k_used: u32,
    pub residual_signed: i32,
    pub k0_post: u32,
    pub k1_post: u32,
    pub sum0_post: u32,
    pub sum1_post: u32,
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
/// zigzag magnitude per `spec/05` §3.5. Used by [`crate::encoder`].
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

    /// Regression: a hostile bitstream that keeps `sum1` above the
    /// increment threshold must NOT drive `k1` past `MAX_K` (= 31).
    /// Before the cap, `k1` could climb to 32+, after which the next
    /// high-mode tail read called `BitReader::read_bits(k1)` with
    /// `k1 > 32`, tripping the reader's `k <= 32` invariant (a
    /// debug-build panic, garbage shift in release). Found by the
    /// `fuzz/fuzz_targets/decode.rs` harness (round 124). Here we seed
    /// `k1` at the cap and verify the increment branch leaves it
    /// pinned at 31 rather than overflowing, and that `decode_one`
    /// returns `Ok` rather than panicking.
    #[test]
    fn k1_increment_saturates_at_max_k() {
        // A single high-mode step: unary prefix `u = 1` (one `1` bit
        // then a `0` terminator = LSB-first `0b01` in bit positions
        // 0,1), followed by a 31-bit tail. `sum1` is seeded huge so
        // the increment branch fires; `k1` is already at the cap.
        // Byte 0 low bits: bit0=1 (unary), bit1=0 (terminator), the
        // remaining 6 bits + bytes 1..=4 supply the 31-bit tail
        // (33 bits total → 5 bytes; pad to 8 for headroom).
        let body = [0x01u8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let mut reader = BitReader::new(&body);
        let mut state = RiceState {
            k0: MAX_K,
            k1: MAX_K,
            sum0: 0xFFFF_FFFF,
            sum1: 0xFFFF_FFFF,
        };
        // Must not panic, and must not request more than 32 bits.
        let _ = decode_one(&mut reader, &mut state).expect("decode must not error on full body");
        assert!(state.k0 <= MAX_K, "k0 exceeded MAX_K: {}", state.k0);
        assert!(state.k1 <= MAX_K, "k1 exceeded MAX_K: {}", state.k1);
    }

    /// Spec §5.3 decrement still works at the cap: with `sum` small,
    /// a step at `k = MAX_K` decrements rather than staying pinned.
    #[test]
    fn k_decrements_normally_from_cap() {
        let body = [0u8; 8];
        let mut reader = BitReader::new(&body);
        let mut state = RiceState {
            k0: MAX_K,
            k1: MAX_K,
            sum0: 0,
            sum1: 0,
        };
        // Low-mode step (unary prefix 0): k0 should decrement.
        let _ = decode_one(&mut reader, &mut state).expect("decode");
        assert_eq!(state.k0, MAX_K - 1);
    }

    /// Append one Rice codeword to a growing LSB-first bit buffer per
    /// spec §6: `u` unary `1` bits, then a `0` terminator, then a
    /// `k`-bit binary tail LSB-first (`k = k0` for `u == 0` low mode,
    /// else `k1`). This is the encode-side bit layout the decoder
    /// inverts; it lets each test below name `(u, k, tail)` directly
    /// and build the exact body the hand-verification in §7 consumes.
    fn push_codeword(bits: &mut Vec<u8>, u: u32, k: u32, tail: u32) {
        for _ in 0..u {
            bits.push(1);
        }
        bits.push(0); // unary terminator
        for i in 0..k {
            bits.push(((tail >> i) & 1) as u8);
        }
    }

    /// Pack an LSB-first bit list into bytes (bit 0 → LSB of byte 0).
    fn pack_lsb_first(bits: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; bits.len().div_ceil(8)];
        for (i, &b) in bits.iter().enumerate() {
            if b != 0 {
                out[i / 8] |= 1 << (i % 8);
            }
        }
        out
    }

    /// Reproduce spec §7.2 — the first non-trivial sample, exercising
    /// the depth-1 escape bias taken from `k0_pre = 9` (NOT `k1`,
    /// anti-pattern §9.2) and the STEP-A-before-STEP-B ordering
    /// (anti-pattern §9.3). Pre-state `(k0=9, k1=10, sum0=15360,
    /// sum1=16384)`; `u=2`, `k1=10`, binary_tail=515; the spec tape
    /// records post-state `(10, 10, 16451, 16899)` and residual 1026.
    #[test]
    fn step_one_matches_spec_7_2() {
        // u=2 high-mode codeword with a k1=10-bit tail = 515.
        let mut bits = Vec::new();
        push_codeword(&mut bits, 2, 10, 515);
        let body = pack_lsb_first(&bits);
        let mut reader = BitReader::new(&body);
        let mut state = RiceState {
            k0: 9,
            k1: 10,
            sum0: 15_360,
            sum1: 16_384,
        };
        let e = decode_one(&mut reader, &mut state).unwrap();
        assert_eq!(e, 1026, "residual mismatch vs spec §7.2");
        assert_eq!(state.k0, 10);
        assert_eq!(state.k1, 10);
        assert_eq!(state.sum0, 16_451);
        assert_eq!(state.sum1, 16_899);
    }

    /// Reproduce spec §7.4 — the first step with `k0 != k1`, the
    /// canonical witness that the escape bias uses `k0` (= 1024) while
    /// the high-mode tail width uses `k1` (= 9). Pre-state `(k0=10,
    /// k1=9, sum0=26219, sum1=16229)`; `u=2`, `k1=9`, binary_tail=129;
    /// the tape records post-state `(10, 9, 26246, 15856)` and residual
    /// 833. A `1 << k1` bias (anti-pattern §9.2) would yield the wrong
    /// `value`, residual, and both sums here.
    #[test]
    fn step_seventeen_matches_spec_7_4() {
        let mut bits = Vec::new();
        push_codeword(&mut bits, 2, 9, 129);
        let body = pack_lsb_first(&bits);
        let mut reader = BitReader::new(&body);
        let mut state = RiceState {
            k0: 10,
            k1: 9,
            sum0: 26_219,
            sum1: 16_229,
        };
        let e = decode_one(&mut reader, &mut state).unwrap();
        assert_eq!(e, 833, "residual mismatch vs spec §7.4");
        assert_eq!(state.k0, 10);
        assert_eq!(state.k1, 9);
        assert_eq!(state.sum0, 26_246);
        assert_eq!(state.sum1, 15_856);
    }

    /// Reproduce spec §7.5 — the first negative residual, exercising
    /// the even-magnitude → negative sign branch of the zigzag decode
    /// (§3.5) and confirming the low-mode (`u=0`) path leaves `sum1`
    /// untouched while `sum0` decays and `k0` decrements. Pre-state
    /// `(k0=10, k1=9, sum0=17094, sum1=12279)`; `u=0`, `k0=10`,
    /// binary_tail=38; the tape records post-state `(9, 9, 16064,
    /// 12279)` and residual -19.
    #[test]
    fn step_thirtythree_matches_spec_7_5() {
        let mut bits = Vec::new();
        push_codeword(&mut bits, 0, 10, 38);
        let body = pack_lsb_first(&bits);
        let mut reader = BitReader::new(&body);
        let mut state = RiceState {
            k0: 10,
            k1: 9,
            sum0: 17_094,
            sum1: 12_279,
        };
        let e = decode_one(&mut reader, &mut state).unwrap();
        assert_eq!(e, -19, "residual mismatch vs spec §7.5");
        assert_eq!(state.k0, 9);
        assert_eq!(state.k1, 9);
        assert_eq!(state.sum0, 16_064);
        // Low mode never touches sum1 (§7.6 quiet-regime note).
        assert_eq!(state.sum1, 12_279);
    }
}
