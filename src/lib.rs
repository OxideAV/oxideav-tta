//! Pure-Rust True Audio (TTA) lossless audio codec.
//!
//! **Round 1 — clean-room implementation.** This crate decodes TTA1
//! format=1 (integer PCM) streams in pure safe Rust against the
//! strict-isolation clean-room workspace at
//! `docs/audio/tta-cleanroom/`. Format=2 (encrypted) and format=3
//! (IEEE float) are out of scope; the encoder is reserved for round 2.
//!
//! The decoder pipeline mirrors `spec/02..05`:
//!
//! 1. Adaptive Rice entropy decode (`spec/05`) — produces one signed
//!    residual per channel-step.
//! 2. Stage-A 8-tap sign-LMS predictor (`spec/02`) — adds an adaptive
//!    prediction; updates `(dl, dx, qm, error)` in lock-step.
//! 3. Stage-B fixed-order recursive predictor (`spec/03`) — adds
//!    `(prev * 31) >> 5` and stores the result back as the new `prev`.
//! 4. Pairwise inverse channel decorrelation (`spec/04`) — for
//!    `nch >= 2`, walks the channel buffer from the highest index
//!    downward.
//!
//! All four stages reset their per-channel state at every
//! `FRAME_BEGIN`. The framing layer (`spec/01`) verifies the header,
//! seek-table, and per-frame CRC32s using the standard IEEE-802.3
//! polynomial.
//!
//! ## Public API
//!
//! - [`decode`] — single-shot decode of a complete TTA1 byte buffer
//!   into interleaved `i32` samples and the parsed [`StreamInfo`].
//! - [`pack_pcm`] — convenience packer that converts the `i32` output
//!   into the appropriate `i16` / 24-bit / `i32` little-endian byte
//!   stream per `spec/01` §3.2.
//! - [`Decoder`] — lower-level frame-by-frame interface for streaming
//!   consumers.
//! - [`Error`] — crate-local error type.
//!
//! All identifiers are documented; see each module for the spec
//! cross-reference. The crate `forbid`s `unsafe`.

#![forbid(unsafe_code)]

mod bitreader;
mod crc32;
mod decoder;
mod decorr;
#[cfg(test)]
mod encoder;
mod error;
mod header;
mod lms;
mod rice;
mod stage_b;
mod tables;

pub use crate::decoder::{decode_frame, Decoder};
pub use crate::error::{Error, Result};
pub use crate::header::{FrameDescriptor, StreamHeader};

/// Re-exported alias for the parsed stream header. [`StreamInfo`] is
/// the same type as [`StreamHeader`] under a more public-friendly
/// name; both are kept available so existing callers (none yet) and
/// new readers can pick the name that fits their context.
pub type StreamInfo = StreamHeader;

/// Decode an entire TTA1 byte buffer in one call.
///
/// Returns `(StreamInfo, samples)` where `samples` is interleaved
/// `i32` PCM in channel-then-sample order
/// (`c0_s0, c1_s0, ..., c0_s1, c1_s1, ...`). The output count is
/// `info.total_samples * info.channels`.
///
/// All header / seek-table / per-frame CRCs are verified; any failure
/// returns the appropriate [`Error`] variant.
pub fn decode(bytes: &[u8]) -> Result<(StreamInfo, Vec<i32>)> {
    let dec = Decoder::new(bytes)?;
    let header = dec.header;
    let pcm = dec.decode_all()?;
    Ok((header, pcm))
}

/// Pack interleaved `i32` PCM samples into the appropriate
/// little-endian byte stream for the stream's bit depth, per
/// `spec/01-bitstream-framing.md` §3.2.
///
/// - bps=16 → 2 bytes per sample, signed `i16` LE.
/// - bps=24 → 3 bytes per sample, signed 24-bit two's-complement LE.
///
/// Unsupported bit depths panic — callers that decoded successfully
/// already have a validated bit depth.
pub fn pack_pcm(samples: &[i32], bits_per_sample: u16) -> Vec<u8> {
    let bytes_per_sample = bits_per_sample.div_ceil(8) as usize;
    let mut out = Vec::with_capacity(samples.len() * bytes_per_sample);
    match bytes_per_sample {
        2 => {
            for &s in samples {
                let v = s as i16;
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        3 => {
            for &s in samples {
                let v = s & 0x00FF_FFFF;
                out.push((v & 0xFF) as u8);
                out.push(((v >> 8) & 0xFF) as u8);
                out.push(((v >> 16) & 0xFF) as u8);
            }
        }
        _ => panic!("oxideav-tta::pack_pcm: unsupported bits_per_sample {bits_per_sample}"),
    }
    out
}

#[cfg(test)]
mod roundtrip_tests;
