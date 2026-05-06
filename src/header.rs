//! TTA1 stream header + seek table parser.
//!
//! Parses the 22-byte stream header (magic + 18 bytes of meta-data + a
//! 32-bit IEEE-802.3 CRC) per `spec/01-bitstream-framing.md` §3, then
//! the seek table (`frame_count * 4` entry bytes + a 32-bit CRC) per
//! §4. Both CRCs are checked using the algorithm in `spec/01` §6 and
//! `crate::crc32`.
//!
//! ID3v2 prefix detection: a TTA1 file MAY begin with an ID3v2 tag
//! (`ID3` ASCII signature). Per spec §2 the prefix is independently
//! parsed and skipped before the TTA1 header lookup; this module
//! exposes [`skip_id3v2_prefix`] for that purpose.

use crate::crc32::Crc32;
use crate::error::{Error, Result};

/// On-disk fixed lengths in bytes.
const HEADER_LEN: usize = 22;
const MAGIC: &[u8; 4] = b"TTA1";

/// Workspace-policy ceiling on the 32-bit `sample_rate` field
/// (`spec/01` §3.3 — high bit reserved as a forward-compat flag).
const MAX_SAMPLE_RATE: u32 = 0x007F_FFFF;
/// libtta `MAX_NCH` (`reference/source/libtta/libtta.h:37`, cited via
/// `spec/01` §3 / `spec/04` §4).
const MAX_NCH: u16 = 6;

/// Parsed TTA1 stream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamHeader {
    /// Audio format ID. Always `1` for in-scope (format=1, integer PCM)
    /// streams; values other than `1` are rejected at parse time.
    pub format: u16,
    /// Number of audio channels, `1..=6`.
    pub channels: u16,
    /// Sample width in bits, `16..=24`.
    pub bits_per_sample: u16,
    /// Sample rate in Hz, `1..=0x7FFFFF` (per workspace policy ceiling).
    pub sample_rate: u32,
    /// Total per-channel sample count for the entire stream.
    pub total_samples: u32,
}

impl StreamHeader {
    /// `bytes_per_sample = (bits_per_sample + 7) / 8` per spec §3.2.
    /// Always `2` or `3` for valid in-scope streams.
    pub fn bytes_per_sample(&self) -> usize {
        self.bits_per_sample.div_ceil(8) as usize
    }

    /// Per-channel samples in a regular (non-last) frame:
    /// `floor(sample_rate * 256 / 245)` per spec §4.1.
    pub fn regular_frame_samples(&self) -> u32 {
        // 40-bit-wide intermediate per spec §4.1 to avoid overflow when
        // `sample_rate` exceeds 2^24 Hz.
        ((self.sample_rate as u64) * 256 / 245) as u32
    }

    /// `(frame_count, last_frame_samples)` derived from `total_samples`
    /// and the regular frame length per spec §4.1.
    pub fn frame_geometry(&self) -> (u32, u32) {
        let regular = self.regular_frame_samples();
        if regular == 0 || self.total_samples == 0 {
            return (0, 0);
        }
        let raw = self.total_samples % regular;
        if raw == 0 {
            (self.total_samples / regular, regular)
        } else {
            (self.total_samples / regular + 1, raw)
        }
    }
}

/// One frame's location in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameDescriptor {
    /// Byte offset in the original file where this frame's data block
    /// begins. The body runs from this offset to `file_offset +
    /// disk_size - 4`; the trailing 4 bytes are the per-frame CRC32.
    pub file_offset: u64,
    /// Total on-disk byte footprint of this frame, including the
    /// trailing 4-byte CRC, per spec §4.2.
    pub disk_size: u32,
    /// Per-channel sample count to reconstruct from this frame's body.
    pub sample_count: u32,
    /// `true` for the final frame in the stream.
    pub is_last: bool,
}

impl FrameDescriptor {
    /// Bytes of bit-packed entropy-coded body (excluding the trailing
    /// 4-byte CRC).
    pub fn body_size(&self) -> u32 {
        self.disk_size.saturating_sub(4)
    }
}

/// Detect and skip an ID3v2 prefix. Returns the byte offset of the
/// first post-tag byte; if no prefix is present, returns `0`.
///
/// Per spec/01 §2: signature `"ID3"`, then 2 version + 1 flags + 4
/// syncsafe length bytes; if flag bit `0x10` is set, an additional
/// 10-byte footer is included in the skip.
pub fn skip_id3v2_prefix(buf: &[u8]) -> Result<usize> {
    if buf.len() < 10 || &buf[..3] != b"ID3" {
        return Ok(0);
    }
    let flags = buf[5];
    let syncsafe = ((buf[6] as u32 & 0x7F) << 21)
        | ((buf[7] as u32 & 0x7F) << 14)
        | ((buf[8] as u32 & 0x7F) << 7)
        | (buf[9] as u32 & 0x7F);
    let mut total = 10usize + syncsafe as usize;
    if flags & 0x10 != 0 {
        total += 10;
    }
    if total > buf.len() {
        return Err(Error::Truncated);
    }
    Ok(total)
}

/// Parse and CRC-verify the 22-byte stream header at `buf[..22]`.
///
/// Returns the parsed header on success, the byte offset advanced past
/// the header (always `22`), or an [`Error`] if the magic, CRC, or any
/// validated field is malformed/out-of-scope.
pub fn parse_stream_header(buf: &[u8]) -> Result<(StreamHeader, usize)> {
    if buf.len() < HEADER_LEN {
        return Err(Error::Truncated);
    }
    let magic = &buf[0..4];
    if magic != MAGIC {
        return Err(Error::InvalidMagic);
    }

    // CRC over the 18 leading bytes (magic + meta-data); the on-disk
    // 4 bytes at offset 18..22 must equal the recomputed value.
    let body = &buf[..18];
    let stored_crc = u32::from_le_bytes(buf[18..22].try_into().unwrap());
    let computed = crate::crc32::crc32(body);
    if stored_crc != computed {
        return Err(Error::Crc32Mismatch { region: "header" });
    }

    let format = u16::from_le_bytes(buf[4..6].try_into().unwrap());
    let channels = u16::from_le_bytes(buf[6..8].try_into().unwrap());
    let bits_per_sample = u16::from_le_bytes(buf[8..10].try_into().unwrap());
    let sample_rate = u32::from_le_bytes(buf[10..14].try_into().unwrap());
    let total_samples = u32::from_le_bytes(buf[14..18].try_into().unwrap());

    if format != 1 {
        return Err(Error::UnsupportedFormat(format));
    }
    if channels == 0 || channels > MAX_NCH {
        return Err(Error::UnsupportedChannelCount(channels));
    }
    if !(16..=24).contains(&bits_per_sample) {
        return Err(Error::UnsupportedBitDepth(bits_per_sample));
    }
    if sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
        return Err(Error::UnsupportedSampleRate(sample_rate));
    }

    Ok((
        StreamHeader {
            format,
            channels,
            bits_per_sample,
            sample_rate,
            total_samples,
        },
        HEADER_LEN,
    ))
}

/// Parse the seek table immediately following the stream header.
///
/// `buf` is positioned at the seek-table start (the byte after the
/// stream header's CRC). Returns the constructed list of
/// [`FrameDescriptor`]s and the number of bytes consumed by the seek
/// table itself (`4 * frame_count + 4`). The base file offset is the
/// caller's responsibility (it equals the post-ID3v2 offset of the
/// stream header plus 22 plus the seek table size).
///
/// Per spec §4.3, a seek-table CRC failure does NOT abort the decode
/// in libtta; this implementation flags the failure to the caller via
/// [`SeekTable::crc_ok`] but still returns a usable list, leaving the
/// caller to decide whether to continue.
#[derive(Debug, Clone)]
pub struct SeekTable {
    pub frames: Vec<FrameDescriptor>,
    /// `true` if the seek-table CRC matched.
    pub crc_ok: bool,
}

pub fn parse_seek_table(
    buf: &[u8],
    header: &StreamHeader,
    frame_data_start: u64,
) -> Result<(SeekTable, usize)> {
    let (frame_count, last_samples) = header.frame_geometry();
    let entries_bytes = (frame_count as usize) * 4;
    let total_bytes = entries_bytes + 4;
    if buf.len() < total_bytes {
        return Err(Error::Truncated);
    }

    let mut crc_state = Crc32::new();
    crc_state.update(&buf[..entries_bytes]);
    let computed = crc_state.finalize();
    let stored = u32::from_le_bytes(buf[entries_bytes..entries_bytes + 4].try_into().unwrap());
    let crc_ok = computed == stored;

    let regular_samples = header.regular_frame_samples();
    let mut frames = Vec::with_capacity(frame_count as usize);
    let mut offset = frame_data_start;
    for i in 0..(frame_count as usize) {
        let off = i * 4;
        let disk_size = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        let is_last = i + 1 == frame_count as usize;
        let sample_count = if is_last {
            last_samples
        } else {
            regular_samples
        };
        frames.push(FrameDescriptor {
            file_offset: offset,
            disk_size,
            sample_count,
            is_last,
        });
        offset = offset.saturating_add(disk_size as u64);
    }

    Ok((SeekTable { frames, crc_ok }, total_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc32::crc32;

    fn build_header_bytes(format: u16, nch: u16, bps: u16, sr: u32, total: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(22);
        buf.extend_from_slice(b"TTA1");
        buf.extend_from_slice(&format.to_le_bytes());
        buf.extend_from_slice(&nch.to_le_bytes());
        buf.extend_from_slice(&bps.to_le_bytes());
        buf.extend_from_slice(&sr.to_le_bytes());
        buf.extend_from_slice(&total.to_le_bytes());
        let crc = crc32(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        buf
    }

    #[test]
    fn parse_minimal_header() {
        let buf = build_header_bytes(1, 1, 16, 44_100, 44_100);
        let (h, n) = parse_stream_header(&buf).unwrap();
        assert_eq!(n, 22);
        assert_eq!(h.format, 1);
        assert_eq!(h.channels, 1);
        assert_eq!(h.bits_per_sample, 16);
        assert_eq!(h.sample_rate, 44_100);
        assert_eq!(h.total_samples, 44_100);
        assert_eq!(h.bytes_per_sample(), 2);
        assert_eq!(h.regular_frame_samples(), 46_080);
    }

    #[test]
    fn invalid_magic_rejected() {
        let mut buf = build_header_bytes(1, 1, 16, 44_100, 44_100);
        buf[0] = b'X';
        // CRC will now fail too, but magic check fires first.
        assert!(matches!(
            parse_stream_header(&buf),
            Err(Error::InvalidMagic)
        ));
    }

    #[test]
    fn header_crc_mismatch_rejected() {
        let mut buf = build_header_bytes(1, 1, 16, 44_100, 44_100);
        // Flip a CRC byte.
        buf[18] ^= 0x01;
        assert!(matches!(
            parse_stream_header(&buf),
            Err(Error::Crc32Mismatch { region: "header" })
        ));
    }

    #[test]
    fn unsupported_format_rejected() {
        let buf = build_header_bytes(2, 1, 16, 44_100, 44_100);
        assert!(matches!(
            parse_stream_header(&buf),
            Err(Error::UnsupportedFormat(2))
        ));
    }

    #[test]
    fn unsupported_bps_rejected() {
        let buf = build_header_bytes(1, 1, 8, 44_100, 44_100);
        assert!(matches!(
            parse_stream_header(&buf),
            Err(Error::UnsupportedBitDepth(8))
        ));
        let buf = build_header_bytes(1, 1, 32, 44_100, 44_100);
        assert!(matches!(
            parse_stream_header(&buf),
            Err(Error::UnsupportedBitDepth(32))
        ));
    }

    #[test]
    fn channel_bounds() {
        assert!(matches!(
            parse_stream_header(&build_header_bytes(1, 0, 16, 44_100, 44_100)),
            Err(Error::UnsupportedChannelCount(0))
        ));
        assert!(matches!(
            parse_stream_header(&build_header_bytes(1, 7, 16, 44_100, 44_100)),
            Err(Error::UnsupportedChannelCount(7))
        ));
    }

    #[test]
    fn frame_geometry_examples() {
        // 1s at 44.1k fits in one frame (regular = 46080).
        let h = StreamHeader {
            format: 1,
            channels: 1,
            bits_per_sample: 16,
            sample_rate: 44_100,
            total_samples: 44_100,
        };
        assert_eq!(h.regular_frame_samples(), 46_080);
        assert_eq!(h.frame_geometry(), (1, 44_100));

        // Multi-frame: 2.5s = 110250 samples => frames 0,1 = 46080,
        // last = 110250 - 92160 = 18090.
        let h = StreamHeader {
            total_samples: 110_250,
            ..h
        };
        assert_eq!(h.frame_geometry(), (3, 18_090));

        // Exact multiple => last frame is regular-length.
        let h = StreamHeader {
            total_samples: 92_160,
            ..h
        };
        assert_eq!(h.frame_geometry(), (2, 46_080));
    }

    #[test]
    fn id3v2_prefix_skip() {
        // No prefix.
        assert_eq!(skip_id3v2_prefix(b"TTA1...").unwrap(), 0);
        // Synthesise a 27-byte ID3v2 header (10 bytes prefix + 17
        // payload). syncsafe = 17.
        let mut buf = Vec::new();
        buf.extend_from_slice(b"ID3");
        buf.push(4); // major
        buf.push(0); // minor
        buf.push(0); // flags
        buf.extend_from_slice(&[0, 0, 0, 17]);
        buf.extend(std::iter::repeat(0u8).take(17));
        assert_eq!(skip_id3v2_prefix(&buf).unwrap(), 27);

        // Footer flag adds 10 more bytes.
        let mut buf2 = buf.clone();
        buf2[5] = 0x10;
        // Need to ensure the buffer is large enough; pad to 37.
        buf2.extend(std::iter::repeat(0u8).take(10));
        assert_eq!(skip_id3v2_prefix(&buf2).unwrap(), 37);
    }
}
