//! Pure-Rust **True Audio (TTA)** lossless audio decoder.
//!
//! TTA is a stateless lossless codec: every frame is decoded
//! independently from a 22-byte stream-level header (channels,
//! bit-depth, sample-rate, total-sample-count) and the frame body, which
//! is just an LSB-first Rice-coded residual stream followed by a 32-bit
//! CRC. Per-frame, the codec runs:
//!
//! 1. **Rice entropy decoder** with two adaptive `k` parameters (k0,
//!    k1) and an escape threshold of `1 << k0`.
//! 2. **8-tap sign-LMS adaptive filter** (Stage A): the per-channel
//!    integer prediction added to the residual.
//! 3. **Fixed-order integer predictor** (Stage B): a single-tap
//!    `((prev × ((1 << k) - 1)) >> k)` term, with a bit-depth-specific
//!    `k` of `4` for 8-bit and `5` for 16/24-bit.
//! 4. **Pairwise inter-channel decorrelation** for stereo and beyond:
//!    encoder pairs `(c0, c1)` as `(c1 - c0, (c0 + c1) / 2)`; the
//!    decoder unwinds in reverse.
//!
//! Frame size is derived from the sample-rate alone:
//! `frame_size = floor(sample_rate * 256 / 245)`. The last frame is the
//! only short one. All state (Rice trackers, filter weights, predictor)
//! is reset at frame entry, so frames are random-access.
//!
//! Three CRC32 layers protect the file (`AV_CRC_32_IEEE_LE`, polynomial
//! `0xEDB88320`, init `0xFFFFFFFF`, output XORed with `0xFFFFFFFF`):
//!
//! - Header CRC over bytes 0..18.
//! - Seek-table CRC over the size array.
//! - Per-frame CRC over the entropy stream body.
//!
//! See `docs/audio/tta/tta-trace-reverse-engineering.md` for the
//! clean-room behavioural spec this crate was written to.

#![allow(clippy::needless_range_loop)]

pub mod codec;
pub mod container;
pub mod crc;
pub mod decoder;
pub mod header;

use oxideav_core::CodecRegistry;

/// Stable codec id string this crate registers under.
pub const CODEC_ID_STR: &str = "tta";

/// Register the TTA decoder with `reg`. After this call the registry
/// can construct a decoder for `CodecId::new("tta")`.
pub fn register_codecs(reg: &mut CodecRegistry) {
    codec::register(reg);
}
