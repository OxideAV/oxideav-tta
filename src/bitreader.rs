//! LSB-first bit reader for TTA1 frame bodies.
//!
//! Per `spec/05-rice.md` §2.1: the frame body's bit stream is read
//! least-significant-bit first within each byte. Bytes are consumed in
//! file order. The bit cache holds up to 63 valid bits in its low
//! positions.
//!
//! Round 386 (bench+profile): the cache widened from `u32`/one-byte
//! refill to `u64`/eight-byte bulk refill, and the unary scan moved
//! from a per-bit loop to a single `trailing_ones` count. The refill
//! uses the standard duplicate-byte trick: when 8 whole bytes are
//! available, one unaligned little-endian `u64` load is OR-shifted
//! into the cache and `pos` advances only by the number of *whole*
//! bytes that fit (`(63 - bcount) / 8`); the partially-fitting top
//! byte's low bits land above the valid region and are harmlessly
//! re-OR'd — bit-identical — when that byte is properly loaded later.
//! Bits at positions `>= bcount` are therefore not guaranteed zero;
//! every consumer masks to the valid width.
//!
//! CRC32 verification is the caller's job: the per-frame trailing CRC
//! covers every *body byte* (`spec/01` §5.4) — including any padding
//! bytes past the last consumed residual bit — so the whole-body
//! one-shot [`crate::crc32::crc32`] over the same slice this reader
//! walks is exactly the value the old per-consumed-byte folding
//! produced. Hoisting the fold out of the refill path keeps the CRC
//! table-lookup dependency chain off the entropy decoder's hot loop.

use crate::error::{Error, Result};

/// Reader for one frame body.
pub struct BitReader<'a> {
    body: &'a [u8],
    pos: usize,
    bcache: u64,
    bcount: u32,
}

impl<'a> BitReader<'a> {
    /// Construct a fresh reader for the body bytes (excluding the
    /// trailing CRC).
    pub fn new(body: &'a [u8]) -> Self {
        Self {
            body,
            pos: 0,
            bcache: 0,
            bcount: 0,
        }
    }

    /// Bytes consumed from the body so far (= bytes drawn into the
    /// cache; cached-but-unread bits count as consumed). Production
    /// code stopped needing this once the per-frame CRC moved to a
    /// whole-body one-shot pass; the rice chained-stream test still
    /// asserts it to pin full-body consumption.
    #[cfg(test)]
    pub fn bytes_consumed(&self) -> usize {
        self.pos
    }

    /// Top up the cache to at least 56 valid bits (or to end-of-body).
    ///
    /// Fast path: one unaligned 8-byte little-endian load, advancing
    /// `pos` by the whole bytes that fit below bit 63. Tail path
    /// (fewer than 8 bytes left): byte-at-a-time. Never errors —
    /// callers detect exhaustion via `bcount`.
    #[inline]
    fn refill(&mut self) {
        if self.bcount >= 56 {
            return;
        }
        if let Some(chunk) = self.body.get(self.pos..self.pos + 8) {
            let w = u64::from_le_bytes(chunk.try_into().unwrap());
            self.bcache |= w << self.bcount;
            self.pos += ((63 - self.bcount) >> 3) as usize;
            self.bcount |= 56;
        } else {
            while self.bcount < 56 && self.pos < self.body.len() {
                self.bcache |= (self.body[self.pos] as u64) << self.bcount;
                self.pos += 1;
                self.bcount += 8;
            }
        }
    }

    /// Read `k` bits LSB-first; returns the value in the low `k` bits.
    /// `k == 0` returns `0`.
    #[inline]
    pub fn read_bits(&mut self, k: u32) -> Result<u32> {
        if k == 0 {
            return Ok(0);
        }
        debug_assert!(k <= 32);
        if self.bcount < k {
            self.refill();
            if self.bcount < k {
                return Err(Error::Truncated);
            }
        }
        // k <= 32 < 64, so the mask shift cannot overflow.
        let v = (self.bcache & ((1u64 << k) - 1)) as u32;
        self.bcache >>= k;
        self.bcount -= k;
        Ok(v)
    }

    /// Count the number of leading `1` bits before a terminating `0`,
    /// consuming both the `1`s and the terminator. The `0xFF`-run fast
    /// path of `spec/05` §2.3 falls out naturally: each loop iteration
    /// swallows up to 63 one-bits in a single `trailing_ones`.
    #[inline]
    pub fn read_unary(&mut self) -> Result<u32> {
        let mut count = 0u32;
        loop {
            if self.bcount == 0 {
                self.refill();
                if self.bcount == 0 {
                    return Err(Error::Truncated);
                }
            }
            // Bits >= bcount may be stale duplicates (see module doc),
            // so cap the run at the valid width.
            let ones = self.bcache.trailing_ones().min(self.bcount);
            if ones < self.bcount {
                // Terminating `0` found inside the valid bits: consume
                // the run and the terminator. `ones + 1 <= bcount <= 63`,
                // so the shift is in range.
                count += ones;
                self.bcache >>= ones + 1;
                self.bcount -= ones + 1;
                return Ok(count);
            }
            // Every valid bit is a `1` — swallow them all and go
            // around for a refill.
            count += self.bcount;
            self.bcache = 0;
            self.bcount = 0;
        }
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
