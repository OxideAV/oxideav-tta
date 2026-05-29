//! Pure-Rust True Audio (TTA) lossless audio codec.
//!
//! **Round 5 — clean-room implementation, encoder + decoder.** This
//! crate decodes and encodes TTA1 format=1 (integer PCM) and format=2
//! (password-derived qm priming; `spec/07`) streams in pure safe Rust
//! against the strict-isolation clean-room workspace at
//! `docs/audio/tta-cleanroom/`. Round 2 added the spec/06 trace
//! contract (debug build) and the `oxideav-core` framework
//! integration; round 3 promoted the test-only encoder to a public
//! API that round-trips bit-exactly through the decoder; round 4
//! added ID3v1 / APEv2 trailer detection (`spec/01` §7); round 5
//! closed `audit/07` §6.2-2 / §6.2-3 / §6.2-5 — `HEADER_CRC` now
//! carries the real computed CRC, `decode_with_password` no longer
//! double-parses format=1 streams, and multi-frame format=2 trace
//! tests put a wire-level seal on `spec/07` §3.6's
//! "re-prime qm[] at every frame init" rule.
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
//! ## Cargo features
//!
//! - **`registry`** (default): wire the crate into `oxideav-core`'s
//!   codec / container registries. Disable for standalone builds that
//!   want the decoder without the framework dependency.
//! - **`trace`** (off by default): activate the `spec/06`
//!   debug-build trace emitter. With the feature on AND
//!   `OXIDEAV_TTA_TRACE_FILE=<path>` set, [`Decoder::decode_all`]
//!   writes one TSV event line per state transition to that path,
//!   compatible with `tools/tta-diff/`. With the feature off this is
//!   compile-time stripped to zero overhead.
//!
//! ## Public API
//!
//! - [`decode`] — single-shot decode of a complete TTA1 byte buffer
//!   into interleaved `i32` samples and the parsed [`StreamInfo`].
//! - [`decode_with_password`] — same but for format=2 streams; the
//!   password derives the eight-byte digest used to prime Stage-A's
//!   `qm[]` per `spec/07` §3.
//! - [`encode`] — single-shot encode of interleaved `i32` PCM into a
//!   complete TTA1 format=1 byte stream (round-trips bit-exactly
//!   through [`decode`]).
//! - [`encode_with_password`] — format=2 encoder; the password seeds
//!   Stage-A's `qm[]` priming at every per-channel frame init per
//!   `spec/07` §3.5.
//! - [`pack_pcm`] — convenience packer that converts the `i32` output
//!   into the appropriate `i16` / 24-bit / `i32` little-endian byte
//!   stream per `spec/01` §3.2.
//! - [`Decoder`] — lower-level frame-by-frame interface for streaming
//!   consumers. Offers [`Decoder::decode_all`] (eager, full-stream),
//!   [`Decoder::frame_iter`] (lazy, one frame at a time — `O(frame)`
//!   memory regardless of stream length), [`Decoder::decode_frame_at`]
//!   (random-access by frame index), and the
//!   [`Decoder::seek_to_sample`] / [`Decoder::frame_iter_from`]
//!   pair for resume-from-sample seeking via the seek table.
//! - [`Error`] — crate-local error type.
//!
//! All identifiers are documented; see each module for the spec
//! cross-reference. The crate `forbid`s `unsafe`.

#![forbid(unsafe_code)]

mod bitreader;
mod crc32;
mod decoder;
mod decorr;
mod encoder;
mod error;
mod header;
mod lms;
mod password;
#[cfg(feature = "registry")]
mod registry;
mod rice;
mod stage_b;
mod tables;
#[cfg(feature = "trace")]
mod trace;
mod trailers;

pub use crate::decoder::{decode_frame, Decoder, FrameIter, SeekPoint};
pub use crate::encoder::{encode, encode_with_password};
pub use crate::error::{Error, Result};
pub use crate::header::{FrameDescriptor, StreamHeader};
pub use crate::trailers::{detect_trailers, TrailerInfo};

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
///
/// Format=2 (encrypted) streams return [`Error::PasswordRequired`];
/// use [`decode_with_password`] to supply the password.
pub fn decode(bytes: &[u8]) -> Result<(StreamInfo, Vec<i32>)> {
    let dec = Decoder::new(bytes)?;
    let header = dec.header;
    let pcm = dec.decode_all()?;
    Ok((header, pcm))
}

/// Decode a format=2 (password-protected) TTA1 byte buffer.
///
/// The password is hashed with ECMA-182 CRC-64 to derive the eight-
/// byte qm priming vector applied at every per-channel Stage-A reset
/// (`spec/07` §3). Format=1 streams accept the same call — the
/// priming is computed but unused (qm always zero-init for format=1
/// per `spec/02` §3.1).
pub fn decode_with_password(bytes: &[u8], password: &[u8]) -> Result<(StreamInfo, Vec<i32>)> {
    let priming = crate::password::derive_qm_priming(password);
    let mut dec = Decoder::new_with_priming(bytes, Some(priming))?;
    // Format=1 with password supplied: the digest is computed but
    // must not mutate state. Clear the priming on the existing
    // decoder so format=1's invariant (qm zero-init at every frame
    // per spec/02 §3.1) is preserved without re-parsing the header
    // and seek table (closes audit/07 §6.2-2).
    if dec.header.format == 1 {
        dec.clear_priming();
    }
    let header = dec.header;
    let pcm = dec.decode_all()?;
    Ok((header, pcm))
}

/// Scan a TTA1 byte buffer for optional ID3v1 / APEv2 trailers per
/// `spec/01` §7.
///
/// Walks the (optional) ID3v2 prefix + stream header + seek table to
/// compute the byte offset of the last frame's end, then defers to
/// [`detect_trailers`] for the actual signature scan. Returns the
/// detected [`TrailerInfo`] (possibly empty); errors only when the
/// framing itself is malformed (which would also fail
/// [`decode`]/[`Decoder::new`]).
pub fn scan_trailers(bytes: &[u8]) -> Result<TrailerInfo> {
    // The Decoder constructor accepts both format=1 and format=2
    // headers; we don't need to actually decode anything to compute
    // the end-of-stream offset, so use `new_with_priming(_, None)`
    // and ignore the PasswordRequired guard by reading the header
    // directly when format == 2.
    let id3_skip = crate::header::skip_id3v2_prefix(bytes)?;
    let after_id3 = &bytes[id3_skip..];
    let (header, hdr_len) = crate::header::parse_stream_header_any_format(after_id3)?;
    let (frame_count, _) = header.frame_geometry();
    let seek_table_len = (frame_count as usize) * 4 + 4;
    let frame_data_start = (id3_skip + hdr_len + seek_table_len) as u64;
    let seek_table_input = &after_id3[hdr_len..];
    let (seek_table, _) =
        crate::header::parse_seek_table(seek_table_input, &header, frame_data_start)?;
    let eos = if let Some(last) = seek_table.frames.last() {
        (last.file_offset as usize).saturating_add(last.disk_size as usize)
    } else {
        // Zero-frame stream — the file ends at the seek-table CRC.
        id3_skip + hdr_len + seek_table_len
    };
    Ok(detect_trailers(bytes, eos))
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

// Framework integration: when the `registry` feature is on, expose
// the canonical `register(ctx)` function and let the macro-generated
// `__oxideav_entry` hook into `oxideav-meta::register_all`. Standalone
// (no-`oxideav-core`) builds drop both.
#[cfg(feature = "registry")]
pub use crate::registry::{register, register_codecs, register_containers, CODEC_ID_STR};

#[cfg(feature = "registry")]
oxideav_core::register!("oxideav-tta", register);

#[cfg(test)]
mod roundtrip_tests;

#[cfg(all(test, feature = "registry"))]
mod seek_tests;
