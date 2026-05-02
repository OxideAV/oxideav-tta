//! CRC32 used by every layer of the TTA file format.
//!
//! Polynomial `0xEDB88320` (the reflected IEEE-802.3 form), initial
//! value `0xFFFFFFFF`, output XORed with `0xFFFFFFFF`. This matches
//! `AV_CRC_32_IEEE_LE`. Each layer (header / seek-table / frame body)
//! computes the CRC over its preceding bytes and compares against the
//! 4-byte little-endian trailer.

const POLY: u32 = 0xEDB8_8320;

/// Lazily-built byte-wise CRC32 lookup table.
fn table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            }
            *slot = c;
        }
        t
    })
}

/// Compute the TTA CRC32 of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    let t = table();
    let mut c: u32 = 0xFFFF_FFFF;
    for &b in data {
        c = (c >> 8) ^ t[((c ^ b as u32) & 0xFF) as usize];
    }
    c ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_matches_init_inversion() {
        // CRC32 of empty input is the standard 0x00000000 (init ^ ~init).
        assert_eq!(crc32(&[]), 0);
    }

    #[test]
    fn known_vector_check_message() {
        // ASCII "123456789" -> 0xCBF43926, the well-known IEEE-802.3 LE
        // CRC32 check value.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
