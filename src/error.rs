//! Crate-local error type.

/// Errors produced by the TTA1 decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A read or seek did not produce the requested number of bytes.
    Truncated,
    /// The stream-header magic was not the four bytes `'T','T','A','1'`.
    InvalidMagic,
    /// The 32-bit CRC32 stored alongside a header / seek table / frame
    /// did not match the recomputed value over the covered bytes.
    Crc32Mismatch {
        /// Which CRC region failed: `"header"`, `"seek_table"`, or
        /// `"frame"`.
        region: &'static str,
    },
    /// The stream header carried a `format` value other than `1`. Format
    /// 2 (encrypted) and format 3 (IEEE float) are out of scope for the
    /// round-1 deliverable.
    UnsupportedFormat(u16),
    /// `bits_per_sample` was outside the in-scope range `16..=24`. The
    /// decoder rejects 8-bit and 32-bit streams per `spec/01` §3.
    UnsupportedBitDepth(u16),
    /// `channels` was zero or above the `MAX_NCH = 6` cap (`spec/01` §3).
    UnsupportedChannelCount(u16),
    /// `sample_rate` exceeded the workspace-policy ceiling of `0x7FFFFF`
    /// Hz (the reserved-high-bit boundary documented in `spec/01` §3.3).
    UnsupportedSampleRate(u32),
    /// The header carried `format == 2` (encrypted) but no password
    /// was supplied to the decoder. Surfaces the spec-defined
    /// password-required failure per `spec/07` §7. Use
    /// [`crate::decode_with_password`] to supply one.
    PasswordRequired,
    /// The interleaved PCM buffer handed to [`crate::encode`] /
    /// [`crate::encode_with_password`] had a length that was not a
    /// multiple of the requested channel count. Length must equal
    /// `total_samples * channels`.
    InvalidSampleBuffer,
    /// A frame index passed to
    /// [`crate::Decoder::decode_frame_at`] was outside
    /// `0..frames.len()`.
    FrameIndexOutOfRange,
    /// A per-channel sample index passed to
    /// [`crate::Decoder::seek_to_sample`] was at or above the
    /// stream's `total_samples`.
    SampleIndexOutOfRange,
    /// A random-access seek was requested on a stream whose seek-table
    /// CRC32 (`spec/01` §4.3) did not validate. Per spec §4.3, "It is
    /// possible to decode a TTA file with a corrupted seek table, but
    /// in 'unseekable' mode only": the byte offsets in the table cannot
    /// be trusted to point at frame boundaries, so random-access seeks
    /// are refused with this recoverable error while linear decode
    /// ([`crate::Decoder::decode_all`] / [`crate::Decoder::frame_iter`])
    /// continues. Callers can test [`crate::Decoder::is_seekable`]
    /// before issuing a seek to avoid the error.
    SeekTableUnreliable,
    /// A seek-table entry / [`crate::FrameDescriptor`] carried a
    /// `disk_size` smaller than 4 bytes, leaving no room for the
    /// trailing per-frame CRC32 required by `spec/01` §5.1 (each
    /// on-disk frame block is `body || u32 CRC`, so the minimum
    /// legal entry is exactly 4 bytes — an empty body followed by
    /// the four CRC bytes).
    InvalidFrameByteLength(u32),
    /// A seek-table entry / [`crate::FrameDescriptor`] carried a
    /// per-channel `sample_count` of zero. Per `spec/01` §4.1 / §5.5
    /// every frame descriptor produced by the parser describes at
    /// least one sample (the empty-stream `total_samples = 0` case
    /// produces zero frame descriptors instead), so the typed
    /// accessor rejects the structurally-impossible zero value at
    /// lift time rather than silently propagating it.
    InvalidFrameSampleCount(u32),
    /// A [`crate::SeekPoint`]'s `frame_index` was outside the
    /// `0..frame_count` window for the stream it claims to belong to.
    /// Surfaces at typed-accessor lift time on an ad-hoc
    /// [`crate::SeekPoint`] literal; the parser-produced seek points
    /// from [`crate::Decoder::seek_to_sample`] /
    /// [`crate::Decoder::seek_to_time`] are guaranteed to satisfy the
    /// bound at construction.
    InvalidFrameIndex(usize),
    /// A [`crate::SeekPoint`]'s `sample_offset_in_frame` was at or
    /// above the regular per-frame sample count derived per `spec/01`
    /// §4.1. Per spec §4.1 / §5.5 every in-frame offset is strictly
    /// less than the regular per-frame count (the modulo arithmetic
    /// in `seek_to_sample` makes that a structural invariant); a
    /// hand-crafted [`crate::SeekPoint`] that violates the gate is
    /// rejected at typed-accessor lift time.
    InvalidInFrameSampleOffset(u32),
    /// A hand-constructed [`crate::Id3v1Range`] failed the `spec/01`
    /// §7 ID3v1 invariants: the length is not exactly 128, the byte
    /// range is not anchored at `(file_len - 128, file_len)` (an
    /// ID3v1 trailer is fixed-length and lives at the very end of
    /// the file), or the `(start, len)` arithmetic overflows or
    /// addresses bytes past the file end. The `(start, len)` pair
    /// is reported as-supplied so the caller can correlate the
    /// rejection back to the input. Surfaces only at typed-accessor
    /// lift time on an ad-hoc literal; the `scan_trailers` parser
    /// guarantees the invariant at construction.
    InvalidId3v1Range(usize, usize),
    /// A hand-constructed [`crate::ApeV2Range`] failed the `spec/01`
    /// §7 APEv2 invariants: the length is below the 32-byte footer
    /// minimum (an APEv2 region is at least a 32-byte footer per the
    /// published APE tags header spec), or the `(start, len)`
    /// arithmetic overflows or addresses bytes past the file end.
    /// The `(start, len)` pair is reported as-supplied. Surfaces only
    /// at typed-accessor lift time on an ad-hoc literal; the
    /// `scan_trailers` parser guarantees the invariant at construction.
    InvalidApeV2Range(usize, usize),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Truncated => f.write_str("oxideav-tta: stream truncated"),
            Error::InvalidMagic => f.write_str("oxideav-tta: invalid TTA1 magic"),
            Error::Crc32Mismatch { region } => {
                write!(f, "oxideav-tta: CRC32 mismatch in {region}")
            }
            Error::UnsupportedFormat(v) => {
                write!(f, "oxideav-tta: unsupported format ID {v}")
            }
            Error::UnsupportedBitDepth(v) => {
                write!(f, "oxideav-tta: unsupported bits_per_sample {v}")
            }
            Error::UnsupportedChannelCount(v) => {
                write!(f, "oxideav-tta: unsupported channel count {v}")
            }
            Error::UnsupportedSampleRate(v) => {
                write!(
                    f,
                    "oxideav-tta: sample rate {v} exceeds policy ceiling 0x7FFFFF"
                )
            }
            Error::PasswordRequired => {
                f.write_str("oxideav-tta: format=2 (encrypted) stream requires a password")
            }
            Error::InvalidSampleBuffer => f.write_str(
                "oxideav-tta: interleaved PCM length is not a multiple of channel count",
            ),
            Error::FrameIndexOutOfRange => f.write_str("oxideav-tta: frame index out of range"),
            Error::SampleIndexOutOfRange => {
                f.write_str("oxideav-tta: sample index at or above total_samples")
            }
            Error::SeekTableUnreliable => f.write_str(
                "oxideav-tta: seek-table CRC32 failed; stream is decodable in linear mode only",
            ),
            Error::InvalidFrameByteLength(v) => {
                write!(
                    f,
                    "oxideav-tta: frame disk_size {v} is less than the 4-byte trailing CRC minimum"
                )
            }
            Error::InvalidFrameSampleCount(v) => {
                write!(
                    f,
                    "oxideav-tta: frame sample_count {v} is below the 1-sample-per-frame minimum"
                )
            }
            Error::InvalidFrameIndex(v) => {
                write!(
                    f,
                    "oxideav-tta: seek-point frame_index {v} is outside the 0..frame_count window"
                )
            }
            Error::InvalidInFrameSampleOffset(v) => {
                write!(
                    f,
                    "oxideav-tta: seek-point sample_offset_in_frame {v} is at or above the regular per-frame sample count"
                )
            }
            Error::InvalidId3v1Range(start, len) => {
                write!(
                    f,
                    "oxideav-tta: ID3v1 trailer range (start={start}, len={len}) does not satisfy the spec/01 §7 invariants (len must be exactly 128 and the range must be anchored at file end)"
                )
            }
            Error::InvalidApeV2Range(start, len) => {
                write!(
                    f,
                    "oxideav-tta: APEv2 trailer range (start={start}, len={len}) does not satisfy the spec/01 §7 invariants (len must be at least the 32-byte footer minimum and the range must lie within the file)"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

/// Crate-local Result alias.
pub type Result<T> = core::result::Result<T, Error>;
