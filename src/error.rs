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
    /// `bits_per_sample` was outside the libtta-2.3-supported range
    /// `16..=24`. The decoder rejects 8-bit and 32-bit streams the same
    /// way the format author's reference does.
    UnsupportedBitDepth(u16),
    /// `channels` was zero or above the libtta `MAX_NCH = 6` cap.
    UnsupportedChannelCount(u16),
    /// `sample_rate` exceeded the workspace-policy ceiling of `0x7FFFFF`
    /// Hz (the reserved-high-bit boundary documented in `spec/01` §3.3).
    UnsupportedSampleRate(u32),
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
        }
    }
}

impl std::error::Error for Error {}

/// Crate-local Result alias.
pub type Result<T> = core::result::Result<T, Error>;
