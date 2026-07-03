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

/// Slice-by-8 table set. `TABLES[0]` is the Sarwate table; each
/// further level answers "what does the register look like after this
/// byte value followed by `j` zero bytes" via the standard recurrence
/// `TABLES[j][i] = (TABLES[j-1][i] >> 8) ^ TABLES[0][TABLES[j-1][i] & 0xFF]`.
/// Folding 8 input bytes then combining the 8 per-byte lookups with
/// XOR is algebraically identical to 8 sequential Sarwate steps (CRC
/// is linear over GF(2)), but breaks the per-byte serial dependency
/// chain into 8 independent loads per iteration. Round-386 profiling:
/// the whole-body per-frame CRC pass (`spec/01` §5.4) went from ~1
/// table-latency per byte to ~1 per 8 bytes, which is what made
/// hoisting the CRC out of the bit reader a net win.
const TABLES: [[u32; 256]; 8] = build_tables();

const fn build_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    tables[0] = build_table();
    let mut j = 1;
    while j < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = tables[j - 1][i];
            tables[j][i] = (prev >> 8) ^ tables[0][(prev & 0xFF) as usize];
            i += 1;
        }
        j += 1;
    }
    tables
}

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
    ///
    /// Uses slice-by-8 for the bulk (see [`TABLES`]) and Sarwate for
    /// the sub-8-byte tail; the result is byte-for-byte identical to
    /// repeated [`Self::update_byte`].
    pub fn update(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        let mut crc = self.state;
        for chunk in &mut chunks {
            let lo = u32::from_le_bytes(chunk[0..4].try_into().unwrap()) ^ crc;
            let hi = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
            crc = TABLES[7][(lo & 0xFF) as usize]
                ^ TABLES[6][((lo >> 8) & 0xFF) as usize]
                ^ TABLES[5][((lo >> 16) & 0xFF) as usize]
                ^ TABLES[4][(lo >> 24) as usize]
                ^ TABLES[3][(hi & 0xFF) as usize]
                ^ TABLES[2][((hi >> 8) & 0xFF) as usize]
                ^ TABLES[1][((hi >> 16) & 0xFF) as usize]
                ^ TABLES[0][(hi >> 24) as usize];
        }
        self.state = crc;
        for &b in chunks.remainder() {
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

    /// The slice-by-8 bulk path must agree with the per-byte Sarwate
    /// step at every length (0..=64 covers empty, sub-8 tails, exact
    /// multiples, and mixed bulk+tail splits) and at every alignment
    /// of the internal 8-byte chunking.
    #[test]
    fn slice_by_8_matches_per_byte_at_every_length() {
        // Deterministic non-trivial byte pattern.
        let data: Vec<u8> = (0..64u32)
            .map(|i| (i.wrapping_mul(151).wrapping_add(i >> 3) & 0xFF) as u8)
            .collect();
        for len in 0..=data.len() {
            let mut bulk = Crc32::new();
            bulk.update(&data[..len]);
            let mut byby = Crc32::new();
            for &b in &data[..len] {
                byby.update_byte(b);
            }
            assert_eq!(
                bulk.finalize(),
                byby.finalize(),
                "slice-by-8 diverged from Sarwate at len={len}"
            );
        }
    }

    /// Split-point independence: folding a buffer in two `update`
    /// calls at any split must equal one whole-buffer call (the
    /// streaming contract the frame decoder relies on).
    #[test]
    fn streaming_split_independence() {
        let data: Vec<u8> = (0..40u32).map(|i| (i * 37 % 251) as u8).collect();
        let whole = crc32(&data);
        for split in 0..=data.len() {
            let mut h = Crc32::new();
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(h.finalize(), whole, "split at {split} diverged");
        }
    }
}
