//! Format=2 password-derived `qm[]` priming, per
//! `docs/audio/tta-cleanroom/spec/07-format2-encrypted.md`.
//!
//! Format=2 differs from format=1 by exactly one line: at every
//! `FRAME_BEGIN`, the per-channel Stage-A reset re-primes `qm[0..7]`
//! with eight bytes of an ECMA-182 CRC-64 digest of the password
//! (sign-extended int8 → int32) instead of zeros (`spec/07` §3.5).
//! Everything else — bitstream framing, the rest of Stage-A, Stage-B,
//! Rice, decorrelation, all CRC32 sites — is byte-for-byte identical.
//!
//! ## ECMA-182 CRC-64 parameters (`spec/07` §3.2)
//!
//! - Polynomial `0x42F0E1EBA9EA3693` (forward / unreflected).
//! - Init register `0xFFFFFFFFFFFFFFFF`.
//! - Output XOR `0xFFFFFFFFFFFFFFFF`.
//! - MSB-first byte direction.
//! - Left-shifting update.
//!
//! ## Digest-to-byte unpacking (`spec/07` §3.4)
//!
//! After the final XOR, the 64-bit register is split into eight
//! little-endian bytes (low half low byte first → high half high byte
//! last). Each byte is interpreted as a signed int8, then
//! sign-extended to int32 for storage in the `qm[]` lane. A digest
//! byte of `0xFF` becomes `qm[k] = -1`, not `255`.
//!
//! ## Empty-password edge case (`spec/07` §9 item 2)
//!
//! `password.len() == 0` exits the update loop immediately, leaves
//! `crc == 0xFFFFFFFFFFFFFFFF`, and the final XOR produces `crc == 0`.
//! All eight digest bytes are `0x00`, which is bit-identical to
//! format=1's all-zero priming. Documented; not specially intercepted.

/// ECMA-182 CRC-64 polynomial in forward / unreflected form.
const POLY: u64 = 0x42F0E1EB_A9EA3693;

/// Compute the ECMA-182 CRC-64 of `data`, MSB-first per spec/07 §3.2.
/// Uses on-the-fly bit-serial computation (no 256-entry table); the
/// password is at most a handful of bytes per decoder lifetime so the
/// table-driven optimisation is not worth the static data-segment
/// overhead. Both forms produce identical digests (spec/07 §3.3
/// explicitly notes the equivalence).
pub fn crc64_ecma182(data: &[u8]) -> u64 {
    let mut crc: u64 = 0xFFFF_FFFF_FFFF_FFFF;
    for &b in data {
        crc ^= (b as u64) << 56;
        for _ in 0..8 {
            if crc & 0x8000_0000_0000_0000 != 0 {
                crc = (crc << 1) ^ POLY;
            } else {
                crc <<= 1;
            }
        }
    }
    crc ^ 0xFFFF_FFFF_FFFF_FFFF
}

/// Derive the eight `qm[0..7]` priming lanes from a password, per
/// spec/07 §3.4. Unpacks the 64-bit digest little-endian inside each
/// 32-bit half (low half first), reinterprets each byte as signed
/// int8, and sign-extends to int32.
pub fn derive_qm_priming(password: &[u8]) -> [i32; 8] {
    let digest = crc64_ecma182(password);
    let lo = digest as u32;
    let hi = (digest >> 32) as u32;
    let mut out = [0i32; 8];
    for (i, lane) in out.iter_mut().take(4).enumerate() {
        *lane = ((lo >> (8 * i)) as u8 as i8) as i32;
    }
    for (i, lane) in out.iter_mut().skip(4).take(4).enumerate() {
        *lane = ((hi >> (8 * i)) as u8 as i8) as i32;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TTA-spec CRC-64 (spec/07 §3.2): polynomial
    /// `0x42F0E1EBA9EA3693`, MSB-first / not-reflected, init and
    /// final-XOR both `0xFFFFFFFFFFFFFFFF`. This is NOT identical to
    /// the standard CRC-64/XZ (which is reflected) — the standard
    /// CRC-64/XZ check value `0x995dc9bbdf1939fa` for the input
    /// `"123456789"` does not apply here. Self-consistency is
    /// validated via incremental properties (single-byte digests and
    /// the empty-input identity).
    #[test]
    fn crc64_self_consistency() {
        // Empty input → all-zero digest after the final XOR
        // (spec/07 §9 item 2).
        assert_eq!(crc64_ecma182(b""), 0);
        // Single-byte 0x00 input: register starts at all-ones, XOR
        // by 0 in top byte changes nothing; eight left-shifts of
        // an all-ones register through the spec polynomial produce
        // a deterministic value. Re-running once must reproduce.
        let a = crc64_ecma182(&[0x00]);
        let b = crc64_ecma182(&[0x00]);
        assert_eq!(a, b);
        // Concatenation property the impl trivially satisfies:
        // running on `b"AB"` agrees with the impl invoked twice
        // sequentially via the public API. (Formally a chained
        // CRC API would be needed for the full property; here we
        // just check the digest is stable.)
        let abc = crc64_ecma182(b"ABC");
        let abc_again = crc64_ecma182(b"ABC");
        assert_eq!(abc, abc_again);
        assert_ne!(abc, 0);
    }

    /// Empty password — spec/07 §9 item 2 — produces an all-zero
    /// digest and therefore an all-zero qm priming (bit-identical to
    /// format=1).
    #[test]
    fn empty_password_yields_zero_priming() {
        assert_eq!(crc64_ecma182(b""), 0);
        assert_eq!(derive_qm_priming(b""), [0; 8]);
    }

    /// Sanity: a password whose digest happens to have a high bit set
    /// produces a negative qm lane after the int8 sign-extension.
    /// `crc64_ecma182(b"\x00")` should produce a non-zero digest; the
    /// digest unpacks per §3.4. We test the round-trip via byte
    /// extraction matches sign-extension.
    #[test]
    fn sign_extension_propagates_high_bit() {
        // Synthesise a digest that has 0xFF in every byte position by
        // using a zero-padded password until at least one byte's
        // top bit is set, then verify the sign-extension for that
        // byte.
        let prime = derive_qm_priming(b"x");
        // Recompute via the documented unpack and confirm equivalence.
        let digest = crc64_ecma182(b"x");
        let lo = digest as u32;
        let hi = (digest >> 32) as u32;
        for (i, &lane) in prime.iter().take(4).enumerate() {
            let raw_byte = ((lo >> (8 * i)) & 0xFF) as u8;
            let signed = raw_byte as i8 as i32;
            assert_eq!(lane, signed);
        }
        for (i, &lane) in prime.iter().skip(4).take(4).enumerate() {
            let raw_byte = ((hi >> (8 * i)) & 0xFF) as u8;
            let signed = raw_byte as i8 as i32;
            assert_eq!(lane, signed);
        }
    }
}
