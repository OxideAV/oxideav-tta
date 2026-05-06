//! IEEE-802.3 CRC32, LSB-first / reflected-polynomial form.
//!
//! Per `spec/01-bitstream-framing.md` §6, all three TTA1 CRCs (header,
//! seek table, per-frame trailer) use the same algorithm:
//!
//! - Reflected polynomial `0xEDB88320` (forward-form `0x04C11DB7`).
//! - Initial register value `0xFFFFFFFF`.
//! - Output XOR `0xFFFFFFFF`.
//! - LSB-first input/output bit order within each byte.
//! - Little-endian on-wire byte order.
//!
//! This is identical in algorithmic specification to gzip / PNG / ZIP
//! CRC-32 ("CRC-32-IEEE"). The Sarwate byte-update step
//! `crc = TABLE[(crc ^ b) & 0xFF] ^ (crc >> 8)` is the canonical form.

/// Reflected IEEE-802.3 CRC32 polynomial.
const POLY: u32 = 0xEDB8_8320;

/// Build the 256-entry Sarwate lookup table at compile time.
const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
}

const TABLE: [u32; 256] = build_table();

/// Streaming CRC32 register. Initial state is `0xFFFFFFFF`; output
/// is the register XOR `0xFFFFFFFF`.
#[derive(Clone, Copy, Debug)]
pub struct Crc32 {
    state: u32,
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    /// Reset to the initial state (`0xFFFFFFFF`).
    pub const fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    /// Fold one byte into the running register.
    pub fn update_byte(&mut self, byte: u8) {
        let idx = ((self.state ^ byte as u32) & 0xFF) as usize;
        self.state = TABLE[idx] ^ (self.state >> 8);
    }

    /// Fold a byte slice into the running register.
    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.update_byte(b);
        }
    }

    /// Return the final CRC value (register XOR `0xFFFFFFFF`).
    pub fn finalize(self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }
}

/// Convenience: compute the CRC32 of a byte slice in one shot.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut h = Crc32::new();
    h.update(bytes);
    h.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC32 of the empty input is `0` (= `!0xFFFFFFFF`).
    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32(&[]), 0);
    }

    /// "123456789" is the standard CRC-32 test vector — expected value
    /// `0xCBF43926`. (Same as gzip / zlib / PNG / Ethernet.)
    #[test]
    fn check_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
