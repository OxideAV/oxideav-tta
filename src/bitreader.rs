//! LSB-first bit reader for TTA1 frame bodies.
//!
//! Per `spec/05-rice.md` §2.1: the frame body's bit stream is read
//! least-significant-bit first within each byte. Bytes are consumed in
//! file order. The bit cache holds up to 32 bits in its low positions;
//! its high bits are zero.
//!
//! This reader also folds every byte it consumes into a running CRC32
//! register so the caller can verify the per-frame trailing CRC at
//! end-of-frame.

use crate::crc32::Crc32;
use crate::error::{Error, Result};

/// Reader for one frame body.
pub struct BitReader<'a> {
    body: &'a [u8],
    pos: usize,
    bcache: u32,
    bcount: u32,
    crc: Crc32,
}

impl<'a> BitReader<'a> {
    /// Construct a fresh reader for the body bytes (excluding the
    /// trailing CRC). The CRC register starts at `0xFFFFFFFF`.
    pub fn new(body: &'a [u8]) -> Self {
        Self {
            body,
            pos: 0,
            bcache: 0,
            bcount: 0,
            crc: Crc32::new(),
        }
    }

    /// Bytes consumed from the body so far.
    pub fn bytes_consumed(&self) -> usize {
        self.pos
    }

    /// Snapshot the current CRC32 register (folding-only — does not
    /// flush the bit cache, since the CRC operates on bytes).
    pub fn crc_state(&self) -> Crc32 {
        self.crc
    }

    /// Read one byte from the body, fold it into the CRC register,
    /// and return it.
    fn read_byte(&mut self) -> Result<u8> {
        let b = *self.body.get(self.pos).ok_or(Error::Truncated)?;
        self.pos += 1;
        self.crc.update_byte(b);
        Ok(b)
    }

    /// Ensure the cache has at least `min_bits` (`<= 32`) valid bits.
    fn refill_to(&mut self, min_bits: u32) -> Result<()> {
        debug_assert!(min_bits <= 32);
        while self.bcount < min_bits {
            let b = self.read_byte()?;
            self.bcache |= (b as u32) << self.bcount;
            self.bcount += 8;
        }
        Ok(())
    }

    /// Read `k` bits LSB-first; returns the value in the low `k` bits.
    /// `k == 0` returns `0`.
    pub fn read_bits(&mut self, k: u32) -> Result<u32> {
        if k == 0 {
            return Ok(0);
        }
        debug_assert!(k <= 32);
        self.refill_to(k)?;
        let mask = if k == 32 { u32::MAX } else { (1u32 << k) - 1 };
        let v = self.bcache & mask;
        if k == 32 {
            self.bcache = 0;
        } else {
            self.bcache >>= k;
        }
        self.bcount -= k;
        Ok(v)
    }

    /// Count the number of leading `1` bits before a terminating `0`,
    /// consuming both the `1`s and the terminator. Implements the
    /// fast path described in `spec/05` §2.3 for runs of `0xFF`
    /// bytes.
    pub fn read_unary(&mut self) -> Result<u32> {
        let mut count = 0u32;

        // Fast path: if the cache is currently entirely 1-bits (or
        // empty), drop it whole and consume `0xFF` bytes 8 bits at a
        // time until a non-`0xFF` byte arrives.
        let cache_mask = if self.bcount >= 32 {
            u32::MAX
        } else if self.bcount == 0 {
            0
        } else {
            (1u32 << self.bcount) - 1
        };
        if self.bcount == 0 || self.bcache == cache_mask {
            count += self.bcount;
            self.bcache = 0;
            self.bcount = 0;
            loop {
                let b = self.read_byte()?;
                if b == 0xFF {
                    count += 8;
                } else {
                    self.bcache = b as u32;
                    self.bcount = 8;
                    break;
                }
            }
        }

        // Per-bit count over the now-non-saturated cache.
        self.refill_to(1)?;
        while self.bcache & 1 != 0 {
            count += 1;
            self.bcache >>= 1;
            self.bcount -= 1;
            if self.bcount == 0 {
                self.refill_to(1)?;
            }
        }
        // Consume the terminating `0` bit.
        self.bcache >>= 1;
        self.bcount -= 1;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_bits_lsb_first() {
        // 0b01010101 — LSB-first reads should yield 1,0,1,0,1,0,1,0
        // for 1-bit reads.
        let bytes = [0b0101_0101];
        let mut r = BitReader::new(&bytes);
        for &expected in &[1u32, 0, 1, 0, 1, 0, 1, 0] {
            assert_eq!(r.read_bits(1).unwrap(), expected);
        }
    }

    #[test]
    fn read_bits_multibyte() {
        // 0xCD 0xAB => bits LSB-first across bytes: 1,0,1,1,0,0,1,1
        // (low byte) then 1,1,0,1,0,1,0,1 (high byte).
        let bytes = [0xCD, 0xAB];
        let mut r = BitReader::new(&bytes);
        // Read 16 bits at once; expect the LE u16 = 0xABCD.
        assert_eq!(r.read_bits(16).unwrap(), 0xABCD);
    }

    #[test]
    fn unary_basic() {
        // LSB-first reading of 0b00011011: bit 0 = 1, bit 1 = 1, bit 2
        // = 0 (terminator). Unary value = 2; remaining 5 cache bits
        // are 0b00011 = 3.
        let bytes = [0b0001_1011];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_unary().unwrap(), 2);
        assert_eq!(r.read_bits(5).unwrap(), 0b0_0011);
    }

    #[test]
    fn unary_fast_path_runs_through_ff_bytes() {
        // Two 0xFF bytes (8 + 8 = 16 ones) followed by 0b0111_1111
        // — LSB-first that byte is 1,1,1,1,1,1,1,0 → 7 more ones then
        // the terminator. Total unary = 16 + 7 = 23.
        let bytes = [0xFF, 0xFF, 0b0111_1111];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_unary().unwrap(), 23);
    }

    #[test]
    fn unary_terminator_in_first_byte() {
        // 0b00000000 => zero leading ones; the bit-0 is the
        // terminator.
        let bytes = [0u8];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_unary().unwrap(), 0);
        // 7 bits left in cache, all zero.
        assert_eq!(r.read_bits(7).unwrap(), 0);
    }
}
