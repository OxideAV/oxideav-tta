#![no_main]

//! Decode a **valid encoder-produced TTA1 stream that has then been
//! byte-corrupted past its header** through every public decode entry
//! point, asserting panic-freedom.
//!
//! ## Why a separate target
//!
//! The `decode` / `streaming_decode` / `demuxer` targets feed *raw*
//! fuzz bytes into `oxideav_tta::decode` / `Decoder::new`. That is the
//! right shape for the attacker-facing "arbitrary file on disk" risk,
//! but it has a structural blind spot: the 22-byte stream header ends
//! in an IEEE-802.3 CRC32 over its preceding 18 bytes (`spec/01` §3.5),
//! and `Decoder::new` rejects any stream whose header CRC does not
//! verify *before it ever touches a frame body*. A libfuzzer mutator
//! working on raw bytes essentially never stumbles onto a 4-byte CRC
//! that matches the 18 header bytes it sits behind, so those targets
//! overwhelmingly exercise the **header-rejection** path and only
//! rarely reach the decoder's deep machinery — the seek-table walk, the
//! per-frame trailing-CRC check (`spec/01` §4.3 / §5), the adaptive
//! Rice decoder (`spec/05`), the Stage-A sign-LMS and Stage-B
//! predictors (`spec/02`/`spec/03`), and inverse channel decorrelation
//! (`spec/04`).
//!
//! This target closes that gap. It first synthesises a *structurally
//! valid* stream from a small structured prefix of the fuzz input by
//! running the in-crate **encoder** (so the header CRC, seek table, and
//! per-frame trailing CRCs are all internally consistent), then applies
//! fuzz-driven byte mutations to the region **after** the 22-byte
//! header. The header therefore still parses, `Decoder::new` proceeds
//! past the header gate, and the mutations land on the seek table, the
//! seek-table CRC, the frame bodies, and the per-frame trailers — i.e.
//! exactly the deep decode surface the raw-byte targets cannot
//! routinely reach. The corrupted stream is then driven through:
//!
//!   1. [`oxideav_tta::decode`] — eager single-shot decode.
//!   2. [`oxideav_tta::Decoder::new`] + `decode_all` /
//!      `frame_iter` / `decode_frame_at` / `seek_to_sample` — the
//!      streaming + random-access surface.
//!   3. [`oxideav_tta::scan_trailers`] — the out-of-stream metadata
//!      scanner.
//!
//! ## Contract under test
//!
//! Panic-freedom on every input. A corrupted stream is *expected* to
//! be rejected (`Error::Crc32Mismatch { region }` for a flipped
//! seek-table / frame / header byte, `Error::Truncated`, …) or — when
//! the mutation happens to leave a self-consistent stream — to decode
//! to *some* `Vec<i32>`; the
//! only invariant is that none of the calls panic, index out of
//! bounds, integer-overflow (debug / ASAN), or allocate an
//! attacker-controlled volume. No bit-exactness is asserted: once the
//! body bytes are mutated the recovered samples are by construction not
//! the originals, so there is nothing to compare against. The value is
//! purely the deep-path panic-freedom coverage.
//!
//! ## Fuzz input layout
//!
//! ```text
//!   byte 0      : channels seed       → channels = (b0 % 6) + 1, in 1..=6
//!   byte 1      : bit-depth selector  → bps = 16 + (b1 % 9), in 16..=24
//!   bytes 2-5   : sample_rate seed    → LE u32, masked to 0x007F_FFFF | 1
//!   bytes 6-7   : sample-count seed    → LE u16, number of per-channel
//!                 samples (capped at MAX_SAMPLES_PER_CHANNEL) so the
//!                 stream spans one or more frames
//!   bytes 8..   : split in half — the first half seeds the encoder's
//!                 input PCM (so the synthesised stream is non-trivial),
//!                 the second half is the mutation script: pairs of
//!                 (LE u16 offset, u8 xor-mask) applied to the
//!                 post-header byte region.
//! ```
//!
//! An input shorter than 8 bytes returns immediately (no header room).

use libfuzzer_sys::fuzz_target;

use oxideav_tta::Decoder;

/// Per-channel sample cap. Keeps the encoder's working buffers and the
/// fuzzer's per-iteration budget bounded while still spanning multiple
/// frames at the default ~1.044 s frame regularity for typical rates.
const MAX_SAMPLES_PER_CHANNEL: usize = 2048;

/// Skip the random-access cross-calls above this frame count so a
/// mutation that inflates an apparent frame count can't push the
/// fuzzer into O(GiB) allocation territory. The eager `decode` /
/// `Decoder::new` panic-free contract is still exercised on every
/// input.
const MAX_FRAMES_FOR_STREAMING: usize = 4096;

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }

    // ── Header fields from the structured prefix ───────────────────
    let channels = ((data[0] as u16) % 6) + 1;
    let bits_per_sample: u16 = 16 + (data[1] % 9) as u16; // 16..=24
    let raw_rate = u32::from_le_bytes([data[2], data[3], data[4], data[5]]);
    let sample_rate = (raw_rate & 0x007F_FFFF) | 1;
    let nch = channels as usize;

    let samples_per_channel =
        (u16::from_le_bytes([data[6], data[7]]) as usize).min(MAX_SAMPLES_PER_CHANNEL);
    if samples_per_channel == 0 {
        // Zero-length stream has no frame bodies to corrupt; the
        // empty-stream path is already covered by the `decode` target.
        return;
    }

    // ── Split the tail into PCM seed + mutation script ─────────────
    let tail = &data[8..];
    let split = tail.len() / 2;
    let pcm_seed = &tail[..split];
    let mut_script = &tail[split..];

    // ── Build the encoder input PCM (interleaved i32) ──────────────
    // Derive each sample from `byte_depth`-wide sign-extended seed
    // bytes so every value lands inside the declared depth's signed
    // storage range and the encoder accepts it. The seed is cycled so
    // a short input still fills the whole buffer (with a deterministic,
    // fuzz-driven pattern rather than all-zero).
    let byte_depth = (bits_per_sample as usize).div_ceil(8); // 2 or 3
    let slot_count = samples_per_channel * nch;
    let mut samples = Vec::with_capacity(slot_count);
    for i in 0..slot_count {
        let s = if pcm_seed.is_empty() {
            // No seed bytes at all: a deterministic low-amplitude ramp
            // keeps the frame bodies non-trivial (non-constant residuals
            // exercise the Rice high-mode escape).
            ((i as i32) % 17) - 8
        } else {
            let base = (i * byte_depth) % pcm_seed.len();
            match byte_depth {
                2 => {
                    let b0 = pcm_seed[base];
                    let b1 = pcm_seed[(base + 1) % pcm_seed.len()];
                    i16::from_le_bytes([b0, b1]) as i32
                }
                3 => {
                    let b0 = pcm_seed[base] as u32;
                    let b1 = pcm_seed[(base + 1) % pcm_seed.len()] as u32;
                    let b2 = pcm_seed[(base + 2) % pcm_seed.len()] as u32;
                    let raw = b0 | (b1 << 8) | (b2 << 16);
                    if raw & 0x0080_0000 != 0 {
                        (raw | 0xFF00_0000) as i32
                    } else {
                        raw as i32
                    }
                }
                _ => 0,
            }
        };
        samples.push(s);
    }

    // ── Encode a structurally valid stream ─────────────────────────
    let mut bytes = match oxideav_tta::encode(&samples, channels, bits_per_sample, sample_rate) {
        Ok(b) => b,
        Err(_) => return, // typed rejection — nothing to corrupt.
    };

    // The 22-byte stream header (`spec/01` §3) must stay intact so
    // `Decoder::new` proceeds past the header CRC gate; mutate only the
    // region at or after offset 22 (the seek table onward).
    const HEADER_LEN: usize = 22;
    if bytes.len() <= HEADER_LEN {
        return;
    }
    let body = &mut bytes[HEADER_LEN..];

    // ── Apply the fuzz-driven mutation script ──────────────────────
    // Each 3-byte record is (LE u16 offset, u8 xor-mask). The offset is
    // folded into the body length so it always lands in range; the xor
    // mask flips attacker-chosen bits. XOR (rather than overwrite)
    // keeps a zero mask a no-op so the fuzzer can also probe the
    // "valid stream, no corruption" baseline.
    for rec in mut_script.chunks_exact(3) {
        let off = (u16::from_le_bytes([rec[0], rec[1]]) as usize) % body.len();
        body[off] ^= rec[2];
    }

    // ── 1. Eager single-shot decode. ───────────────────────────────
    let _ = oxideav_tta::decode(&bytes);

    // ── 2. Streaming + random-access surface. ──────────────────────
    if let Ok(dec) = Decoder::new(&bytes) {
        let frame_count = dec.frames.len();
        let total_samples = dec.total_samples();

        let _ = dec.decode_all();

        if frame_count > 0 && frame_count <= MAX_FRAMES_FOR_STREAMING {
            // frame_iter — drain it; a corrupt frame body surfaces as a
            // yielded Err, never a panic.
            for r in dec.frame_iter() {
                if r.is_err() {
                    break;
                }
            }

            // Random-access decode of a fuzz-chosen frame.
            let fi = (data[0] as usize) % frame_count;
            let _ = dec.decode_frame_at(fi);

            // Seek to a fuzz-chosen per-channel sample index.
            let si = if total_samples == 0 {
                0
            } else {
                (u64::from_le_bytes([data[2], data[3], data[4], data[5], data[6], data[7], 0, 0]))
                    % (total_samples as u64)
            };
            let _ = dec.seek_to_sample(si);
        }
    }

    // ── 3. Trailer scan over the corrupted stream. ─────────────────
    let _ = oxideav_tta::scan_trailers(&bytes);
});
