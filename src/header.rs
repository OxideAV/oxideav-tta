//! Parse the 22-byte `TTA1` stream header (carried as decoder
//! `extradata`) and derive frame layout.
//!
//! ```text
//! 0x00  "TTA1"          4 B   ASCII signature
//! 0x04  format          2 B   LE u16 — 1 = simple, 2 = encrypted (rejected)
//! 0x06  channels        2 B   LE u16 — 1..=8 supported
//! 0x08  bits_per_sample 2 B   LE u16 — 8 / 16 / 24
//! 0x0A  sample_rate     4 B   LE u32 — must be ≤ 0x7FFFFF
//! 0x0E  total_samples   4 B   LE u32 — per-channel sample count
//! 0x12  header CRC32    4 B   LE u32 — over bytes 0..=17
//! ```
//!
//! Frame size is derived from the sample rate alone:
//! `frame_size = floor(sample_rate * 256 / 245)` (samples per channel).

use oxideav_core::{Error, Result};

use crate::crc::crc32;

pub const HEADER_LEN: usize = 22;
pub const SIGNATURE: &[u8; 4] = b"TTA1";

/// Maximum sample-rate accepted; matches the 23-bit cap noted in the
/// spec to keep `sample_rate * 256` from overflowing 32 bits.
pub const MAX_SAMPLE_RATE: u32 = 0x007F_FFFF;

/// Parsed `TTA1` stream header.
#[derive(Clone, Debug)]
pub struct TtaHeader {
    pub format: u16,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub sample_rate: u32,
    pub total_samples: u32,
}

impl TtaHeader {
    /// Parse a 22-byte header buffer and verify its trailing CRC32.
    ///
    /// Trailing data beyond byte 22 is ignored — the typical caller
    /// passes the entire `extradata` blob in.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_LEN {
            return Err(Error::invalid(format!(
                "TTA header: need {HEADER_LEN} bytes, got {}",
                buf.len()
            )));
        }
        if &buf[0..4] != SIGNATURE {
            return Err(Error::invalid("TTA header: bad signature (expected TTA1)"));
        }
        let format = u16::from_le_bytes([buf[4], buf[5]]);
        let channels = u16::from_le_bytes([buf[6], buf[7]]);
        let bits_per_sample = u16::from_le_bytes([buf[8], buf[9]]);
        let sample_rate = u32::from_le_bytes([buf[10], buf[11], buf[12], buf[13]]);
        let total_samples = u32::from_le_bytes([buf[14], buf[15], buf[16], buf[17]]);
        let claimed = u32::from_le_bytes([buf[18], buf[19], buf[20], buf[21]]);
        let computed = crc32(&buf[0..18]);
        if computed != claimed {
            return Err(Error::invalid(format!(
                "TTA header: CRC32 mismatch (got {computed:#010x}, want {claimed:#010x})"
            )));
        }

        // Spec: format 1 (simple, unencrypted) is the only one we
        // implement; the encoder never emits format 2.
        if format != 1 {
            return Err(Error::unsupported(format!(
                "TTA header: format {format} not supported (only simple/1)"
            )));
        }
        if !(1..=8).contains(&channels) {
            return Err(Error::unsupported(format!(
                "TTA header: {channels} channels not supported (1..=8)"
            )));
        }
        if !matches!(bits_per_sample, 8 | 16 | 24) {
            return Err(Error::unsupported(format!(
                "TTA header: bits_per_sample {bits_per_sample} (only 8/16/24)"
            )));
        }
        if sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
            return Err(Error::invalid(format!(
                "TTA header: sample_rate {sample_rate} out of range"
            )));
        }
        if total_samples == 0 {
            return Err(Error::invalid("TTA header: total_samples is zero"));
        }
        Ok(Self {
            format,
            channels,
            bits_per_sample,
            sample_rate,
            total_samples,
        })
    }

    /// Per-channel samples in a (full) frame.
    ///
    /// Derived from sample-rate alone via `floor(sr * 256 / 245)`.
    pub fn frame_size(&self) -> u32 {
        // Multiplied as u64 to avoid wrap if sample_rate is near MAX.
        ((self.sample_rate as u64) * 256 / 245) as u32
    }

    /// Number of frames in the stream.
    pub fn total_frames(&self) -> u32 {
        let f = self.frame_size().max(1);
        // ceil(total_samples / frame_size).
        self.total_samples.div_ceil(f)
    }

    /// Per-channel sample count in the *last* frame (which is the only
    /// short one). Returns `frame_size` when the total is an exact
    /// multiple of `frame_size`.
    pub fn last_frame_size(&self) -> u32 {
        let f = self.frame_size();
        let r = self.total_samples % f;
        if r == 0 {
            f
        } else {
            r
        }
    }

    /// Bit depth as a byte count (1, 2 or 3).
    pub fn bps_bytes(&self) -> u32 {
        self.bits_per_sample.div_ceil(8) as u32
    }
}

/// Parse a complete on-disk `TTA1` file: header + seek-table +
/// per-frame size list. Verifies both the header CRC and the
/// seek-table CRC. Returns the parsed header plus a vector of
/// per-frame byte offsets and sizes (each size *includes* the
/// trailing per-frame CRC32).
pub fn parse_file(buf: &[u8]) -> Result<ParsedFile> {
    let header = TtaHeader::parse(buf)?;
    let total_frames = header.total_frames() as usize;
    let seek_table_off = HEADER_LEN;
    let seek_array_bytes = total_frames * 4;
    let seek_crc_off = seek_table_off + seek_array_bytes;
    if buf.len() < seek_crc_off + 4 {
        return Err(Error::invalid(
            "TTA file: truncated before end of seek table CRC",
        ));
    }
    let mut sizes = Vec::with_capacity(total_frames);
    for i in 0..total_frames {
        let p = seek_table_off + i * 4;
        let s = u32::from_le_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
        sizes.push(s);
    }
    let claimed = u32::from_le_bytes([
        buf[seek_crc_off],
        buf[seek_crc_off + 1],
        buf[seek_crc_off + 2],
        buf[seek_crc_off + 3],
    ]);
    let computed = crc32(&buf[seek_table_off..seek_crc_off]);
    if computed != claimed {
        return Err(Error::invalid(format!(
            "TTA file: seek-table CRC mismatch (got {computed:#010x}, want {claimed:#010x})"
        )));
    }
    let mut frames = Vec::with_capacity(total_frames);
    let mut off = seek_crc_off + 4;
    for &size in &sizes {
        let size_us = size as usize;
        if off + size_us > buf.len() {
            return Err(Error::invalid("TTA file: frame extends past end of buffer"));
        }
        frames.push(FrameRef {
            offset: off,
            size: size_us,
        });
        off += size_us;
    }
    Ok(ParsedFile { header, frames })
}

/// Result of [`parse_file`].
#[derive(Clone, Debug)]
pub struct ParsedFile {
    pub header: TtaHeader,
    pub frames: Vec<FrameRef>,
}

/// One frame's location inside a TTA file. `size` includes the
/// trailing 4-byte CRC32.
#[derive(Clone, Copy, Debug)]
pub struct FrameRef {
    pub offset: usize,
    pub size: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_header(
        channels: u16,
        bps: u16,
        sample_rate: u32,
        total_samples: u32,
    ) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0..4].copy_from_slice(SIGNATURE);
        h[4..6].copy_from_slice(&1u16.to_le_bytes());
        h[6..8].copy_from_slice(&channels.to_le_bytes());
        h[8..10].copy_from_slice(&bps.to_le_bytes());
        h[10..14].copy_from_slice(&sample_rate.to_le_bytes());
        h[14..18].copy_from_slice(&total_samples.to_le_bytes());
        let crc = crc32(&h[0..18]);
        h[18..22].copy_from_slice(&crc.to_le_bytes());
        h
    }

    #[test]
    fn parses_synthetic_mono16() {
        let h = build_header(1, 16, 44100, 88_200);
        let parsed = TtaHeader::parse(&h).unwrap();
        assert_eq!(parsed.channels, 1);
        assert_eq!(parsed.bits_per_sample, 16);
        assert_eq!(parsed.sample_rate, 44100);
        assert_eq!(parsed.total_samples, 88_200);
        assert_eq!(parsed.frame_size(), 46_080);
        assert_eq!(parsed.total_frames(), 2);
        assert_eq!(parsed.last_frame_size(), 88_200 - 46_080);
    }

    #[test]
    fn rejects_bad_crc() {
        let mut h = build_header(2, 16, 48_000, 12_000);
        h[10] ^= 0x01; // flip a sample-rate bit; CRC no longer matches.
        let err = TtaHeader::parse(&h).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("CRC32 mismatch"), "got {msg}");
    }

    #[test]
    fn frame_size_table_matches_doc() {
        // From §3.3 of the spec doc.
        for (sr, expected) in [
            (22_050, 23_040),
            (44_100, 46_080),
            (48_000, 50_155),
            (96_000, 100_310),
        ] {
            let h = build_header(1, 16, sr, 1);
            let p = TtaHeader::parse(&h).unwrap();
            assert_eq!(p.frame_size(), expected, "sample rate {sr}");
        }
    }
}
