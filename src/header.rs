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
/// `MAX_NCH` per `spec/01` §3 / `spec/04` §4.
const MAX_NCH: u16 = 6;

/// Typed enumeration of the `format` field in the TTA1 stream header
/// (`spec/01` §3.1). Only `Format::Simple` (= 1) and
/// `Format::Encrypted` (= 2) are accepted at parse time; the parser
/// rejects any other on-wire value with [`Error::UnsupportedFormat`]
/// before any [`StreamHeader`] is constructed. The enum therefore
/// covers exactly the set of values that can be observed in a
/// successfully-parsed header.
///
/// The typed enum is non-exhaustive — should the spec ever extend the
/// in-scope value set (DOC §3.1 reserves `3` for IEEE-754 float, which
/// is not implemented in any in-scope round), the addition would be a
/// non-breaking variant rather than an API break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// `format == 1` per `spec/01` §3.1 — integer PCM, no encryption.
    Simple,
    /// `format == 2` per `spec/01` §3.1 — password-derived qm priming
    /// per `spec/07` §3.5–§3.6.
    Encrypted,
}

impl Format {
    /// Try to lift a raw on-wire `format` byte into the typed enum.
    /// Returns [`Error::UnsupportedFormat`] for any value outside the
    /// `{1, 2}` accepted set (`spec/01` §3.1).
    pub fn from_raw(value: u16) -> Result<Self> {
        match value {
            1 => Ok(Format::Simple),
            2 => Ok(Format::Encrypted),
            other => Err(Error::UnsupportedFormat(other)),
        }
    }

    /// Round-trip back to the on-wire `u16` value (`1` or `2`).
    pub fn as_raw(&self) -> u16 {
        match self {
            Format::Simple => 1,
            Format::Encrypted => 2,
        }
    }

    /// `true` for `Format::Encrypted` (`format == 2`). Convenience for
    /// callers that branch on the password-priming discipline of
    /// `spec/07` §3 without naming the variant.
    pub fn requires_password(&self) -> bool {
        matches!(self, Format::Encrypted)
    }
}

/// Typed wrapper around the `bits_per_sample` field, validated to the
/// in-scope range `16..=24` per `spec/01` §3.2. Construction via
/// [`BitsPerSample::from_raw`] is the only entry path; the raw `u16`
/// inside [`StreamHeader::bits_per_sample`] is kept for backward
/// compatibility with existing callers but a successfully-parsed
/// header always corresponds to a constructible [`BitsPerSample`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BitsPerSample(u16);

impl BitsPerSample {
    /// Lift a raw `u16` into the validated typed accessor.
    /// Returns [`Error::UnsupportedBitDepth`] for values outside
    /// `16..=24` per `spec/01` §3.2.
    pub fn from_raw(value: u16) -> Result<Self> {
        if (16..=24).contains(&value) {
            Ok(BitsPerSample(value))
        } else {
            Err(Error::UnsupportedBitDepth(value))
        }
    }

    /// Underlying `u16` width (16..=24).
    pub fn bits(&self) -> u16 {
        self.0
    }

    /// `byte_depth = (bits + 7) / 8` per `spec/01` §3.2.
    /// Always `2` (bps = 16) or `3` (bps = 17..=24).
    pub fn byte_depth(&self) -> usize {
        self.0.div_ceil(8) as usize
    }
}

/// Typed wrapper around the `channels` field, validated to the in-
/// scope range `1..=6` per `spec/01` §3 (`MAX_NCH = 6`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelCount(u16);

impl ChannelCount {
    /// Lift a raw `u16` into the validated typed accessor.
    /// Returns [`Error::UnsupportedChannelCount`] for values outside
    /// `1..=6` per `spec/01` §3.
    pub fn from_raw(value: u16) -> Result<Self> {
        if (1..=MAX_NCH).contains(&value) {
            Ok(ChannelCount(value))
        } else {
            Err(Error::UnsupportedChannelCount(value))
        }
    }

    /// Underlying channel count (1..=6).
    pub fn count(&self) -> u16 {
        self.0
    }

    /// `true` when the stream carries more than one channel (`>= 2`).
    /// Convenience for callers that branch on the decorrelation
    /// cascade gate of `spec/04` §3.
    pub fn is_multichannel(&self) -> bool {
        self.0 >= 2
    }
}

/// Typed wrapper around the `sample_rate` field, validated to the
/// workspace-policy range `1..=0x7FFFFF` per `spec/01` §3.3 (the high
/// bit is reserved as a forward-compatibility flag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SampleRate(u32);

impl SampleRate {
    /// Lift a raw `u32` into the validated typed accessor.
    /// Returns [`Error::UnsupportedSampleRate`] for `0` or any value
    /// above the `0x7FFFFF` policy ceiling per `spec/01` §3.3.
    pub fn from_raw(value: u32) -> Result<Self> {
        if value == 0 || value > MAX_SAMPLE_RATE {
            Err(Error::UnsupportedSampleRate(value))
        } else {
            Ok(SampleRate(value))
        }
    }

    /// Underlying sample rate in Hz (1..=0x7FFFFF).
    pub fn hz(&self) -> u32 {
        self.0
    }

    /// Per-channel samples in a regular (non-last) frame:
    /// `floor(sample_rate * 256 / 245)` per `spec/01` §4.1. Computed
    /// with a 64-bit-wide intermediate to avoid the 32-bit overflow
    /// that would occur near `sample_rate = 2^24` Hz per `spec/01`
    /// §4.1's "at least 40-bit-wide intermediate" rule.
    pub fn regular_frame_samples(&self) -> u32 {
        ((self.0 as u64) * 256 / 245) as u32
    }
}

/// Typed wrapper around the `total_samples` field per `spec/01` §3.4 —
/// the per-channel sample count of the audio payload (the same number
/// a WAV `fact` chunk would record for a non-PCM transcode).
///
/// The entire `u32` value space is structurally legal per spec §3.4
/// ("a `total_samples` of zero is structurally valid; no frames
/// follow"), so [`TotalSamples::from_raw`] is infallible. The newtype
/// exists for two reasons:
///
/// 1. **Symmetry with the round-240 typed sub-field accessors.** Once
///    [`Format`] / [`BitsPerSample`] / [`ChannelCount`] / [`SampleRate`]
///    exist alongside [`StreamHeader`]'s raw fields, callers that want
///    to thread the parsed payload-size separately from the rest of the
///    header benefit from the same self-documenting wrapper rather than
///    a bare `u32`.
///
/// 2. **Duration computation per `spec/01` §3.4.** A caller with a
///    `(total_samples, sample_rate)` pair can compute the stream's
///    playback length in `core::time::Duration` directly via
///    [`TotalSamples::duration_at`], without going through a full
///    [`crate::Decoder`] construction. The arithmetic matches the
///    sample-keyed → `Duration` conversion used by
///    [`crate::Decoder::total_duration`] / `seek_to_time` so any caller
///    deriving one outside the decoder and one inside agrees bit-for-bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TotalSamples(u32);

impl TotalSamples {
    /// Lift a raw `u32` into the typed accessor. Every `u32` value is
    /// in scope per `spec/01` §3.4 (zero is structurally valid; the
    /// upper bound is just the field width), so this never fails — the
    /// `from_raw` shape is retained for symmetry with the other typed
    /// accessors and to give a single discoverable entry point.
    pub fn from_raw(value: u32) -> Self {
        TotalSamples(value)
    }

    /// Underlying per-channel sample count (the on-wire `total_samples`
    /// value verbatim).
    pub fn count(&self) -> u32 {
        self.0
    }

    /// `true` for a structurally-empty stream (`total_samples == 0`).
    /// Per `spec/01` §3.4 this is a valid TTA1 file with zero
    /// frames following the seek table — the codec construction is
    /// well-defined; no PCM is produced.
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Playback duration at `sample_rate` Hz, using nanosecond-grain
    /// integer arithmetic so the result is exact and overflow-free for
    /// the entire in-scope envelope (`sample_rate ≤ 0x7FFFFF`,
    /// `total_samples ≤ u32::MAX`).
    ///
    /// The formula is `total_samples / sample_rate` seconds plus
    /// `floor((total_samples mod sample_rate) * 1_000_000_000 /
    /// sample_rate)` nanoseconds, matching the
    /// [`crate::Decoder::total_duration`] internal arithmetic so a
    /// caller that derives one outside the decoder agrees bit-for-bit
    /// with the decoder-internal computation.
    ///
    /// `sample_rate == 0` returns [`core::time::Duration::ZERO`] (no
    /// division by zero; not a structurally legal stream — `sample_rate
    /// == 0` is rejected by [`parse_stream_header`] — but the accessor
    /// stays total).
    pub fn duration_at(&self, sample_rate: u32) -> core::time::Duration {
        if sample_rate == 0 {
            return core::time::Duration::ZERO;
        }
        let n = self.0 as u64;
        let r = sample_rate as u64;
        let secs = n / r;
        let remainder = n % r;
        // Sub-second component in nanoseconds: floor(remainder * 1e9 /
        // sample_rate). Widened to u128 so the multiplication cannot
        // overflow — `remainder < sample_rate ≤ 0x7FFFFF` and the
        // product stays well under `u128::MAX`. The widening is a
        // defensive cost-free belt-and-braces against a future
        // sample-rate-ceiling lift.
        let ns = ((remainder as u128) * 1_000_000_000u128 / (r as u128)) as u64;
        core::time::Duration::new(secs, ns as u32)
    }
}

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

    /// Lifts the raw `format` field into the typed [`Format`] enum.
    ///
    /// A successfully-parsed header is guaranteed to carry a value in
    /// the accepted set `{1, 2}` per `spec/01` §3.1, so this method
    /// returns `Ok` for every value reachable from a parsed
    /// [`StreamHeader`]. It is exposed as a `Result` rather than an
    /// infallible accessor so that ad-hoc [`StreamHeader`] structs
    /// constructed by callers (e.g. for round-trip testing) get the
    /// same validation discipline rather than panicking.
    pub fn format_typed(&self) -> Result<Format> {
        Format::from_raw(self.format)
    }

    /// Lifts the raw `bits_per_sample` field into the typed
    /// [`BitsPerSample`] accessor (validates `16..=24` per `spec/01`
    /// §3.2). Same `Result` discipline as [`Self::format_typed`].
    pub fn bits_per_sample_typed(&self) -> Result<BitsPerSample> {
        BitsPerSample::from_raw(self.bits_per_sample)
    }

    /// Lifts the raw `channels` field into the typed [`ChannelCount`]
    /// accessor (validates `1..=6` per `spec/01` §3). Same `Result`
    /// discipline as [`Self::format_typed`].
    pub fn channel_count_typed(&self) -> Result<ChannelCount> {
        ChannelCount::from_raw(self.channels)
    }

    /// Lifts the raw `sample_rate` field into the typed [`SampleRate`]
    /// accessor (validates `1..=0x7FFFFF` per `spec/01` §3.3). Same
    /// `Result` discipline as [`Self::format_typed`].
    pub fn sample_rate_typed(&self) -> Result<SampleRate> {
        SampleRate::from_raw(self.sample_rate)
    }

    /// Lifts the raw `total_samples` field into the typed
    /// [`TotalSamples`] accessor per `spec/01` §3.4.
    ///
    /// Every `u32` value is structurally legal per the spec (zero is
    /// permitted; the upper bound is just the field width), so this is
    /// an infallible projection. It is provided alongside the other
    /// `*_typed` accessors so callers reach for `header.total_samples_typed()`
    /// rather than the bare `header.total_samples` field whenever they
    /// want to thread the payload size + its derived duration through
    /// a player API without going through a full [`crate::Decoder`].
    pub fn total_samples_typed(&self) -> TotalSamples {
        TotalSamples::from_raw(self.total_samples)
    }

    /// Convenience: playback duration at the parsed `sample_rate` using
    /// the same nanosecond-grain integer arithmetic as
    /// [`crate::Decoder::total_duration`]. Returns
    /// [`core::time::Duration::ZERO`] for `sample_rate == 0` (rejected
    /// by [`parse_stream_header`]; the accessor stays total for ad-hoc
    /// `StreamHeader` literals used in encode tests).
    ///
    /// Equivalent to `self.total_samples_typed().duration_at(self.sample_rate)`.
    pub fn total_duration(&self) -> core::time::Duration {
        self.total_samples_typed().duration_at(self.sample_rate)
    }
}

/// Typed wrapper around a [`FrameDescriptor`]'s `disk_size` field —
/// the total on-disk byte footprint of one frame's data block per
/// `spec/01` §4.2 (the seek-table entry value verbatim, including the
/// trailing 4-byte per-frame CRC of `spec/01` §5.1).
///
/// Validated against `>= 4`: every on-disk frame block is `body || u32
/// CRC`, so the minimum legal entry is exactly 4 bytes (an empty body
/// followed by the four CRC bytes). The per-frame decoder enforces the
/// same lower bound at `decode_frame` entry — the typed accessor
/// surfaces that invariant at the seek-table layer so a caller that
/// constructs a [`FrameDescriptor`] literal (e.g. an ad-hoc encoder
/// fixture) gets the same `Error::InvalidFrameByteLength` discipline
/// the decoder hot path produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameByteLength(u32);

impl FrameByteLength {
    /// Lift a raw `u32` into the typed accessor. Returns
    /// [`Error::InvalidFrameByteLength`] for any value below `4` —
    /// the minimum size required to hold the trailing per-frame CRC
    /// per `spec/01` §5.1. Every value `>= 4` is structurally legal
    /// per spec §4.2 (the entropy-coded body's byte budget is whatever
    /// the encoder produced; the spec imposes no upper bound).
    pub fn from_raw(value: u32) -> Result<Self> {
        if value < 4 {
            Err(Error::InvalidFrameByteLength(value))
        } else {
            Ok(FrameByteLength(value))
        }
    }

    /// Total on-disk byte footprint of the frame, including the
    /// trailing 4-byte CRC (`spec/01` §4.2 — the seek-table entry's
    /// value verbatim).
    pub fn total_size(&self) -> u32 {
        self.0
    }

    /// Bytes of bit-packed entropy-coded body (= `total_size - 4` per
    /// `spec/01` §5.1, where the four trailing bytes are the per-frame
    /// IEEE-802.3 CRC32). Always strictly less than [`Self::total_size`]
    /// (subtraction is safe because `total_size >= 4` by construction).
    pub fn body_size(&self) -> u32 {
        self.0 - 4
    }
}

/// Typed wrapper around a [`FrameDescriptor`]'s `sample_count` field —
/// the per-channel sample count to reconstruct from this frame's body
/// per `spec/01` §5.5.
///
/// Validated against `>= 1`: every frame descriptor produced by the
/// parser describes at least one sample per `spec/01` §4.1 / §5.5.
/// The empty-stream case (`total_samples = 0` per spec §3.4) produces
/// zero frame descriptors instead — so a descriptor carrying
/// `sample_count = 0` is structurally impossible per spec and is
/// rejected by the typed accessor with [`Error::InvalidFrameSampleCount`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameSampleCount(u32);

impl FrameSampleCount {
    /// Lift a raw `u32` into the typed accessor. Returns
    /// [`Error::InvalidFrameSampleCount`] for `0` per `spec/01` §4.1 /
    /// §5.5. Every non-zero value `u32` is in scope (the upper bound is
    /// just the field width; the documented practical ceiling is the
    /// `floor(sample_rate * 256 / 245)` regular-frame count of `spec/01`
    /// §4.1 — checked by [`Self::is_within_regular_bound`]).
    pub fn from_raw(value: u32) -> Result<Self> {
        if value == 0 {
            Err(Error::InvalidFrameSampleCount(value))
        } else {
            Ok(FrameSampleCount(value))
        }
    }

    /// Per-channel sample count carried by this frame (`>= 1`).
    pub fn count(&self) -> u32 {
        self.0
    }

    /// `true` when the count is `<=` the regular per-frame sample count
    /// derived from the stream's `sample_rate` per `spec/01` §4.1 /
    /// §5.5. The regular-frame ceiling caps every frame's per-channel
    /// sample count; only the last frame may be shorter and never
    /// longer. Convenience for callers that want to sanity-check an
    /// ad-hoc [`FrameDescriptor`] literal against the spec's frame-
    /// geometry rule without re-deriving the regular count themselves.
    pub fn is_within_regular_bound(&self, regular_frame_samples: u32) -> bool {
        self.0 <= regular_frame_samples
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

    /// Lifts the raw `disk_size` field into the typed
    /// [`FrameByteLength`] accessor per `spec/01` §4.2 / §5.1
    /// (validates `>= 4` so the trailing CRC fits).
    ///
    /// A successfully-parsed descriptor is guaranteed to satisfy the
    /// `>= 4` bound because the per-frame decoder rejects any
    /// shorter entry at `decode_frame` entry; the accessor returns a
    /// `Result` rather than an infallible projection so an ad-hoc
    /// [`FrameDescriptor`] literal constructed by a caller (e.g. an
    /// encode-side fixture) gets the same
    /// [`Error::InvalidFrameByteLength`] discipline.
    pub fn disk_size_typed(&self) -> Result<FrameByteLength> {
        FrameByteLength::from_raw(self.disk_size)
    }

    /// Lifts the raw `sample_count` field into the typed
    /// [`FrameSampleCount`] accessor per `spec/01` §4.1 / §5.5
    /// (validates `>= 1`).
    ///
    /// A successfully-parsed descriptor is guaranteed to satisfy the
    /// `>= 1` bound because every parser-produced descriptor describes
    /// at least one sample (the empty-stream case produces zero
    /// descriptors instead). Same `Result` discipline as
    /// [`Self::disk_size_typed`] for the ad-hoc-literal path.
    pub fn sample_count_typed(&self) -> Result<FrameSampleCount> {
        FrameSampleCount::from_raw(self.sample_count)
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

/// Parse and CRC-verify the 22-byte stream header at `buf[..22]`,
/// rejecting any `format` other than `1` (the round-1 contract).
///
/// Returns the parsed header on success, the byte offset advanced past
/// the header (always `22`), or an [`Error`] if the magic, CRC, or any
/// validated field is malformed/out-of-scope.
#[allow(dead_code)] // exercised by header tests; round-2 hot path uses parse_stream_header_any_format.
pub fn parse_stream_header(buf: &[u8]) -> Result<(StreamHeader, usize)> {
    let (header, n, _crc) = parse_stream_header_with_crc(buf)?;
    if header.format != 1 {
        return Err(Error::UnsupportedFormat(header.format));
    }
    Ok((header, n))
}

/// Parse and CRC-verify the 22-byte stream header at `buf[..22]`,
/// accepting both `format == 1` (integer PCM) and `format == 2`
/// (password-derived qm priming, spec/07). The caller is responsible
/// for refusing format=2 when no password is supplied. Other formats
/// (3 IEEE float, ...) are still rejected.
///
/// This wrapper drops the computed CRC for callers that don't need it
/// in their trace output; [`parse_stream_header_with_crc`] is the
/// underlying entry that surfaces the freshly-computed IEEE-802.3
/// CRC32 alongside the header for downstream `HEADER_CRC` trace
/// emission per spec/06 §5.1 (closes audit/07 §6.2-3).
pub fn parse_stream_header_any_format(buf: &[u8]) -> Result<(StreamHeader, usize)> {
    let (h, n, _crc) = parse_stream_header_with_crc(buf)?;
    Ok((h, n))
}

/// Like [`parse_stream_header_any_format`] but also returns the
/// freshly-computed IEEE-802.3 CRC32 over the 18 header-body bytes
/// (`spec/01` §3.5).
///
/// Callers that emit a spec/06 `HEADER_CRC` trace event use this
/// entry point so the `computed_crc` field carries the real value
/// rather than a placeholder zero. Functionally equivalent to the
/// non-`_with_crc` variant — the parser would have rejected the
/// header at the CRC check if the value did not match the on-disk
/// CRC bytes.
pub fn parse_stream_header_with_crc(buf: &[u8]) -> Result<(StreamHeader, usize, u32)> {
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

    if format != 1 && format != 2 {
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
        computed,
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
/// Per spec §4.3, a seek-table CRC failure is non-fatal; this
/// implementation flags the failure to the caller via
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
    fn format_typed_round_trip() {
        // Simple (1) and Encrypted (2) round-trip cleanly.
        let s = Format::from_raw(1).unwrap();
        assert_eq!(s, Format::Simple);
        assert_eq!(s.as_raw(), 1);
        assert!(!s.requires_password());
        let e = Format::from_raw(2).unwrap();
        assert_eq!(e, Format::Encrypted);
        assert_eq!(e.as_raw(), 2);
        assert!(e.requires_password());
        // Any other value is rejected.
        assert!(matches!(
            Format::from_raw(0),
            Err(Error::UnsupportedFormat(0))
        ));
        assert!(matches!(
            Format::from_raw(3),
            Err(Error::UnsupportedFormat(3))
        ));
        assert!(matches!(
            Format::from_raw(255),
            Err(Error::UnsupportedFormat(255))
        ));
    }

    #[test]
    fn bits_per_sample_typed_boundary() {
        let b16 = BitsPerSample::from_raw(16).unwrap();
        assert_eq!(b16.bits(), 16);
        assert_eq!(b16.byte_depth(), 2);
        let b17 = BitsPerSample::from_raw(17).unwrap();
        assert_eq!(b17.byte_depth(), 3);
        let b23 = BitsPerSample::from_raw(23).unwrap();
        assert_eq!(b23.byte_depth(), 3);
        let b24 = BitsPerSample::from_raw(24).unwrap();
        assert_eq!(b24.bits(), 24);
        assert_eq!(b24.byte_depth(), 3);
        // Out of range.
        assert!(matches!(
            BitsPerSample::from_raw(8),
            Err(Error::UnsupportedBitDepth(8))
        ));
        assert!(matches!(
            BitsPerSample::from_raw(15),
            Err(Error::UnsupportedBitDepth(15))
        ));
        assert!(matches!(
            BitsPerSample::from_raw(25),
            Err(Error::UnsupportedBitDepth(25))
        ));
        assert!(matches!(
            BitsPerSample::from_raw(32),
            Err(Error::UnsupportedBitDepth(32))
        ));
    }

    #[test]
    fn channel_count_typed_boundary() {
        let mono = ChannelCount::from_raw(1).unwrap();
        assert_eq!(mono.count(), 1);
        assert!(!mono.is_multichannel());
        let stereo = ChannelCount::from_raw(2).unwrap();
        assert_eq!(stereo.count(), 2);
        assert!(stereo.is_multichannel());
        let six = ChannelCount::from_raw(6).unwrap();
        assert_eq!(six.count(), 6);
        assert!(six.is_multichannel());
        // Out of range.
        assert!(matches!(
            ChannelCount::from_raw(0),
            Err(Error::UnsupportedChannelCount(0))
        ));
        assert!(matches!(
            ChannelCount::from_raw(7),
            Err(Error::UnsupportedChannelCount(7))
        ));
        assert!(matches!(
            ChannelCount::from_raw(255),
            Err(Error::UnsupportedChannelCount(255))
        ));
    }

    #[test]
    fn sample_rate_typed_boundary() {
        let sr = SampleRate::from_raw(44_100).unwrap();
        assert_eq!(sr.hz(), 44_100);
        assert_eq!(sr.regular_frame_samples(), 46_080);
        // Boundary: max accepted value.
        let max = SampleRate::from_raw(MAX_SAMPLE_RATE).unwrap();
        assert_eq!(max.hz(), MAX_SAMPLE_RATE);
        // The regular_frame_samples computation must not overflow at
        // the max input (canary against a future regression that drops
        // the `(... as u64) * 256 / 245` widening).
        let expected = ((MAX_SAMPLE_RATE as u64) * 256 / 245) as u32;
        assert_eq!(max.regular_frame_samples(), expected);
        // Out of range.
        assert!(matches!(
            SampleRate::from_raw(0),
            Err(Error::UnsupportedSampleRate(0))
        ));
        assert!(matches!(
            SampleRate::from_raw(MAX_SAMPLE_RATE + 1),
            Err(Error::UnsupportedSampleRate(_))
        ));
        assert!(matches!(
            SampleRate::from_raw(u32::MAX),
            Err(Error::UnsupportedSampleRate(_))
        ));
    }

    #[test]
    fn total_samples_typed_boundary() {
        // Zero is structurally valid per spec §3.4.
        let z = TotalSamples::from_raw(0);
        assert_eq!(z.count(), 0);
        assert!(z.is_empty());
        // Max u32 value remains in scope (the upper bound is just the
        // field width per spec §3.4).
        let mx = TotalSamples::from_raw(u32::MAX);
        assert_eq!(mx.count(), u32::MAX);
        assert!(!mx.is_empty());
        // Mid-range value round-trips cleanly.
        let mid = TotalSamples::from_raw(44_100);
        assert_eq!(mid.count(), 44_100);
        assert!(!mid.is_empty());
    }

    #[test]
    fn total_samples_typed_duration_at_44100() {
        // 1 second at 44.1k.
        let one_sec = TotalSamples::from_raw(44_100);
        assert_eq!(
            one_sec.duration_at(44_100),
            core::time::Duration::from_secs(1)
        );
        // Zero samples → zero duration.
        let zero = TotalSamples::from_raw(0);
        assert_eq!(zero.duration_at(44_100), core::time::Duration::ZERO);
        // Zero rate → zero duration (no division by zero).
        assert_eq!(
            TotalSamples::from_raw(44_100).duration_at(0),
            core::time::Duration::ZERO
        );
        // Sub-second precision: 22_050 samples at 44.1k = 0.5s = 500ms
        // = 500_000_000 ns.
        let half = TotalSamples::from_raw(22_050);
        let d = half.duration_at(44_100);
        assert_eq!(d.as_secs(), 0);
        assert_eq!(d.subsec_nanos(), 500_000_000);
    }

    #[test]
    fn total_samples_typed_duration_at_envelope_upper_bound() {
        // Envelope: total_samples = u32::MAX, sample_rate = 0x7FFFFF.
        // The arithmetic must not overflow and must produce a sensible
        // duration. The expected value: secs = u32::MAX / 0x7FFFFF =
        // 511 (integer), remainder = u32::MAX mod 0x7FFFFF =
        // 0x7FFFFF * 512 - 1 - 0x7FFFFF * 511 = 0x7FFFFE, so the
        // nanoseconds component is floor(0x7FFFFE * 1e9 / 0x7FFFFF) =
        // 999_999_880.
        let mx = TotalSamples::from_raw(u32::MAX);
        let d = mx.duration_at(MAX_SAMPLE_RATE);
        let expected_secs = (u32::MAX as u64) / (MAX_SAMPLE_RATE as u64);
        let expected_remainder = (u32::MAX as u64) % (MAX_SAMPLE_RATE as u64);
        let expected_ns =
            ((expected_remainder as u128) * 1_000_000_000u128 / (MAX_SAMPLE_RATE as u128)) as u64;
        assert_eq!(d.as_secs(), expected_secs);
        assert_eq!(d.subsec_nanos(), expected_ns as u32);
    }

    #[test]
    fn stream_header_total_samples_typed_round_trip() {
        // The lifting method returns the same value the raw field
        // carries; the convenience `total_duration` agrees with the
        // typed accessor's `duration_at(self.sample_rate)`.
        let buf = build_header_bytes(1, 2, 16, 48_000, 96_000);
        let (h, _) = parse_stream_header(&buf).unwrap();
        let typed = h.total_samples_typed();
        assert_eq!(typed.count(), h.total_samples);
        assert_eq!(typed.count(), 96_000);
        assert!(!typed.is_empty());
        // 96_000 samples at 48 kHz = 2 seconds exactly.
        let d = h.total_duration();
        assert_eq!(d, core::time::Duration::from_secs(2));
        assert_eq!(d, typed.duration_at(h.sample_rate));
    }

    #[test]
    fn stream_header_total_samples_typed_zero_payload() {
        // `total_samples = 0` is structurally valid per spec §3.4 —
        // both the accessor and the parser accept it; the duration is
        // zero.
        let buf = build_header_bytes(1, 1, 16, 44_100, 0);
        let (h, _) = parse_stream_header(&buf).unwrap();
        assert_eq!(h.total_samples_typed().count(), 0);
        assert!(h.total_samples_typed().is_empty());
        assert_eq!(h.total_duration(), core::time::Duration::ZERO);
        // Frame geometry is also (0, 0) for an empty stream.
        assert_eq!(h.frame_geometry(), (0, 0));
    }

    #[test]
    fn stream_header_typed_accessors_match_raw() {
        // Round-trip: a successfully-parsed header has every typed
        // accessor agreeing with the raw field it lifts.
        let buf = build_header_bytes(1, 2, 16, 44_100, 88_200);
        let (h, _) = parse_stream_header(&buf).unwrap();
        assert_eq!(h.format_typed().unwrap(), Format::Simple);
        assert_eq!(h.format_typed().unwrap().as_raw(), h.format);
        let bps = h.bits_per_sample_typed().unwrap();
        assert_eq!(bps.bits(), h.bits_per_sample);
        assert_eq!(bps.byte_depth(), h.bytes_per_sample());
        let ch = h.channel_count_typed().unwrap();
        assert_eq!(ch.count(), h.channels);
        assert!(ch.is_multichannel());
        let sr = h.sample_rate_typed().unwrap();
        assert_eq!(sr.hz(), h.sample_rate);
        assert_eq!(sr.regular_frame_samples(), h.regular_frame_samples());

        // A constructed-by-hand header with a now-rejected raw value
        // round-trips back through the typed accessor as the same
        // error variant the parser would have produced (e.g. caller
        // doing ad-hoc validation pre-encode).
        let bogus = StreamHeader {
            format: 1,
            channels: 0,
            bits_per_sample: 16,
            sample_rate: 44_100,
            total_samples: 0,
        };
        assert!(matches!(
            bogus.channel_count_typed(),
            Err(Error::UnsupportedChannelCount(0))
        ));
    }

    #[test]
    fn frame_byte_length_typed_boundary() {
        // 4 is the minimum legal value (empty body + 4-byte CRC).
        let m = FrameByteLength::from_raw(4).unwrap();
        assert_eq!(m.total_size(), 4);
        assert_eq!(m.body_size(), 0);
        // Mid-range value round-trips cleanly.
        let mid = FrameByteLength::from_raw(22_189).unwrap();
        assert_eq!(mid.total_size(), 22_189);
        assert_eq!(mid.body_size(), 22_185);
        // u32::MAX is in scope per spec §4.2 (no upper bound on entry
        // size); the body_size derivation must not overflow.
        let mx = FrameByteLength::from_raw(u32::MAX).unwrap();
        assert_eq!(mx.total_size(), u32::MAX);
        assert_eq!(mx.body_size(), u32::MAX - 4);
        // Values below 4 are rejected.
        assert!(matches!(
            FrameByteLength::from_raw(0),
            Err(Error::InvalidFrameByteLength(0))
        ));
        assert!(matches!(
            FrameByteLength::from_raw(1),
            Err(Error::InvalidFrameByteLength(1))
        ));
        assert!(matches!(
            FrameByteLength::from_raw(3),
            Err(Error::InvalidFrameByteLength(3))
        ));
    }

    #[test]
    fn frame_sample_count_typed_boundary() {
        // 1 is the minimum legal value per spec/01 §4.1 / §5.5.
        let one = FrameSampleCount::from_raw(1).unwrap();
        assert_eq!(one.count(), 1);
        // Mid-range value round-trips cleanly.
        let mid = FrameSampleCount::from_raw(46_080).unwrap();
        assert_eq!(mid.count(), 46_080);
        // u32::MAX is in scope (the upper bound is the field width).
        let mx = FrameSampleCount::from_raw(u32::MAX).unwrap();
        assert_eq!(mx.count(), u32::MAX);
        // Zero is rejected.
        assert!(matches!(
            FrameSampleCount::from_raw(0),
            Err(Error::InvalidFrameSampleCount(0))
        ));
    }

    #[test]
    fn frame_sample_count_typed_regular_bound() {
        // At 44.1 kHz the regular per-frame count is
        // floor(44_100 * 256 / 245) = 46_080. Any frame's
        // per-channel count must be <= that to be a legal regular
        // frame; the last frame is the only one allowed to be shorter.
        let regular = 46_080u32;
        // Below or at the cap: in-bound.
        assert!(FrameSampleCount::from_raw(1)
            .unwrap()
            .is_within_regular_bound(regular));
        assert!(FrameSampleCount::from_raw(regular)
            .unwrap()
            .is_within_regular_bound(regular));
        // Above the cap: out of bound (cannot be a legal frame at
        // this sample rate).
        assert!(!FrameSampleCount::from_raw(regular + 1)
            .unwrap()
            .is_within_regular_bound(regular));
        assert!(!FrameSampleCount::from_raw(u32::MAX)
            .unwrap()
            .is_within_regular_bound(regular));
    }

    #[test]
    fn frame_descriptor_typed_accessors_match_raw() {
        // A FrameDescriptor produced by parse_seek_table on the
        // 1-frame canonical fixture (`sine-440Hz-1ch-16bit-44100-1s`)
        // carries disk_size = 22_189 (per spec/01 §8.1 cross-validation)
        // and sample_count = 44_100. Synthesise it directly and check
        // every typed accessor agrees with the raw field it lifts.
        let fd = FrameDescriptor {
            file_offset: 30,
            disk_size: 22_189,
            sample_count: 44_100,
            is_last: true,
        };
        let len = fd.disk_size_typed().unwrap();
        assert_eq!(len.total_size(), fd.disk_size);
        assert_eq!(len.body_size(), fd.body_size());
        let sc = fd.sample_count_typed().unwrap();
        assert_eq!(sc.count(), fd.sample_count);
        // At sample_rate = 44_100, regular = 46_080; the descriptor's
        // 44_100 samples fit (it's the last frame of a 1-frame stream
        // that happens to be shorter than regular).
        assert!(sc.is_within_regular_bound(46_080));

        // A constructed-by-hand descriptor with a now-rejected raw
        // disk_size round-trips back through the typed accessor as the
        // same error variant the decoder hot path would have produced.
        let bogus_len = FrameDescriptor {
            file_offset: 0,
            disk_size: 3,
            sample_count: 1,
            is_last: true,
        };
        assert!(matches!(
            bogus_len.disk_size_typed(),
            Err(Error::InvalidFrameByteLength(3))
        ));

        // A constructed-by-hand descriptor with sample_count = 0
        // similarly surfaces the structurally-impossible value.
        let bogus_sc = FrameDescriptor {
            file_offset: 0,
            disk_size: 4,
            sample_count: 0,
            is_last: true,
        };
        assert!(matches!(
            bogus_sc.sample_count_typed(),
            Err(Error::InvalidFrameSampleCount(0))
        ));
    }

    #[test]
    fn frame_descriptor_typed_accessors_round_trip_via_parse_seek_table() {
        // End-to-end: parse a real seek table and confirm every parsed
        // descriptor's typed accessors agree with the raw fields, and
        // every descriptor's sample_count is within the regular-frame
        // bound (every regular frame == regular_count; the last frame
        // may be shorter).
        // Build a header for a 2.5 s @ 44.1 kHz mono 16-bit stream:
        // total_samples = 110_250, regular = 46_080 => 3 frames
        // (46_080, 46_080, 18_090).
        let buf = build_header_bytes(1, 1, 16, 44_100, 110_250);
        let (h, _) = parse_stream_header(&buf).unwrap();
        let regular = h.regular_frame_samples();
        let (frame_count, last_samples) = h.frame_geometry();
        assert_eq!(frame_count, 3);
        assert_eq!(last_samples, 18_090);

        // Synthesise a minimal seek table: three frame entries each
        // with disk_size = 50 (an arbitrary >= 4 value) plus a CRC.
        let mut sk_buf = Vec::with_capacity(16);
        let entries = [50u32, 50, 50];
        for e in &entries {
            sk_buf.extend_from_slice(&e.to_le_bytes());
        }
        let sk_crc = crate::crc32::crc32(&sk_buf);
        sk_buf.extend_from_slice(&sk_crc.to_le_bytes());
        let (seek, _) = parse_seek_table(&sk_buf, &h, 30).unwrap();
        assert!(seek.crc_ok);
        assert_eq!(seek.frames.len(), 3);

        for fd in &seek.frames {
            // disk_size lift.
            let len = fd.disk_size_typed().unwrap();
            assert_eq!(len.total_size(), fd.disk_size);
            assert_eq!(len.body_size(), fd.body_size());
            // sample_count lift + regular-bound gate.
            let sc = fd.sample_count_typed().unwrap();
            assert_eq!(sc.count(), fd.sample_count);
            assert!(sc.is_within_regular_bound(regular));
        }

        // The last descriptor's sample_count == last_samples (18_090);
        // the other two carry regular (46_080).
        assert_eq!(seek.frames[0].sample_count, regular);
        assert_eq!(seek.frames[1].sample_count, regular);
        assert_eq!(seek.frames[2].sample_count, last_samples);
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
