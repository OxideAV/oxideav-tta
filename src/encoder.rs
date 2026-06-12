//! TTA1 encoder — production module.
//!
//! Mirrors the decoder pipeline applied in reverse. For each per-step
//! tuple `(channel, sample)` the encoder:
//!
//! 1. Performs forward channel decorrelation per `spec/04` §3.1 / §4.1.
//! 2. Subtracts the Stage-B prediction (`(prev * 31) >> 5`) from the
//!    decorrelated sample. The pre-subtraction PCM is saved as the new
//!    `prev` so the decoder, applied to the residual stream, observes
//!    the symmetric state evolution (`spec/03` §4.3).
//! 3. Runs Stage-A's 8-tap sign-LMS in the same state order the decoder
//!    uses, then subtracts the prediction to obtain the Rice residual
//!    (`spec/02` §4.2 — STEPs 1..5 run identically; the only difference
//!    from the decoder is that the closing PCM reconstruction uses the
//!    encoder's input sample directly instead of `e + p_A`).
//! 4. Zigzags the residual and emits the adaptive-Rice prefix + tail
//!    per `spec/05` §3..§5, updating the `(k0, k1, sum0, sum1)`
//!    trackers in lock-step with the decoder (`spec/05` §5.2 / §5.3).
//! 5. Per-frame: pads the bit cache to the next byte boundary and
//!    appends the trailing IEEE-802.3 CRC32 over the body (`spec/01`
//!    §5.3 / §5.4).
//! 6. Per-file: emits the 22-byte stream header (`spec/01` §3), then
//!    the seek table (`spec/01` §4), then the frame blobs in order.
//!
//! Format=1 is the default. [`encode_with_password`] flips
//! `header.format = 2` and primes Stage-A's `qm[0..7]` with the CRC-64
//! digest of the password at every per-channel frame init (`spec/07`
//! §3.5), producing a stream that round-trips bit-exactly through
//! [`crate::decode_with_password`].

use crate::error::{Error, Result};
use crate::lms::LmsState;
use crate::rice::{zigzag, RiceState};
use crate::stage_b::StageBState;

/// Maximum supported channels per `spec/01` §3 (mirrors the decoder's
/// `MAX_NCH`).
const MAX_NCH: u16 = 6;
/// Workspace policy ceiling on `sample_rate` per `spec/01` §3.3 (high
/// bit reserved as a forward-compat flag).
const MAX_SAMPLE_RATE: u32 = 0x007F_FFFF;

/// Encode interleaved `i32` PCM samples into a complete TTA1 format=1
/// byte stream.
///
/// `samples` is interleaved in channel-then-sample order
/// (`c0_s0, c1_s0, ..., c0_s1, c1_s1, ...`); its length MUST equal
/// `total_samples * channels`.
///
/// The returned bytes are a self-contained TTA1 file: 22-byte stream
/// header (with verified CRC32), seek table (with verified CRC32),
/// then frame blobs back-to-back. Round-trips bit-exactly through
/// [`crate::decode`].
///
/// # Errors
///
/// Rejects out-of-scope inputs with [`Error::UnsupportedChannelCount`]
/// (`channels` outside `1..=6`),
/// [`Error::UnsupportedBitDepth`] (`bits_per_sample` outside `16..=24`),
/// [`Error::UnsupportedSampleRate`] (zero or above `0x007F_FFFF`), and
/// [`Error::InvalidSampleBuffer`] when `samples.len()` is not a
/// multiple of `channels`.
pub fn encode(
    samples: &[i32],
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
) -> Result<Vec<u8>> {
    encode_inner(samples, channels, bits_per_sample, sample_rate, 1, None)
}

/// Encode interleaved `i32` PCM samples into a complete TTA1 format=2
/// (password-derived qm priming) byte stream per `spec/07`.
///
/// The password is hashed with ECMA-182 CRC-64 to derive the eight-
/// byte vector used to prime Stage-A's `qm[0..7]` at every per-channel
/// frame init. The resulting stream is bit-exactly decodable by
/// [`crate::decode_with_password`] with the same password.
///
/// Empty passwords are accepted: the digest is all-zero and the
/// resulting bitstream is byte-identical to [`encode`]'s format=1
/// output except for the header `format` field (per `spec/07` §9
/// item 2).
///
/// # Errors
///
/// Same input validation as [`encode`].
pub fn encode_with_password(
    samples: &[i32],
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
    password: &[u8],
) -> Result<Vec<u8>> {
    let priming = crate::password::derive_qm_priming(password);
    encode_inner(
        samples,
        channels,
        bits_per_sample,
        sample_rate,
        2,
        Some(priming),
    )
}

fn encode_inner(
    samples: &[i32],
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
    format: u16,
    qm_priming: Option<[i32; 8]>,
) -> Result<Vec<u8>> {
    if channels == 0 || channels > MAX_NCH {
        return Err(Error::UnsupportedChannelCount(channels));
    }
    if !(16..=24).contains(&bits_per_sample) {
        return Err(Error::UnsupportedBitDepth(bits_per_sample));
    }
    if sample_rate == 0 || sample_rate > MAX_SAMPLE_RATE {
        return Err(Error::UnsupportedSampleRate(sample_rate));
    }
    let nch = channels as usize;
    if samples.len() % nch != 0 {
        return Err(Error::InvalidSampleBuffer);
    }
    let total_samples = (samples.len() / nch) as u32;
    let bytes_per_sample = bits_per_sample.div_ceil(8) as usize;

    // Frame geometry per spec/01 §4.1.
    let regular: u32 = ((sample_rate as u64) * 256 / 245) as u32;
    let (frame_count, last_samples) = if total_samples == 0 {
        (0u32, 0u32)
    } else {
        let raw = total_samples % regular;
        if raw == 0 {
            (total_samples / regular, regular)
        } else {
            (total_samples / regular + 1, raw)
        }
    };

    let mut frame_blobs: Vec<Vec<u8>> = Vec::with_capacity(frame_count as usize);
    let mut sample_cursor = 0usize;
    for i in 0..frame_count {
        let is_last = i + 1 == frame_count;
        let n_samples = if is_last { last_samples } else { regular } as usize;
        let frame_pcm = &samples[sample_cursor..sample_cursor + n_samples * nch];
        sample_cursor += n_samples * nch;
        let blob = encode_one_frame(frame_pcm, nch, bytes_per_sample, qm_priming.as_ref());
        frame_blobs.push(blob);
    }

    let mut file = Vec::new();
    // Stream header (22 bytes, spec/01 §3).
    file.extend_from_slice(b"TTA1");
    file.extend_from_slice(&format.to_le_bytes());
    file.extend_from_slice(&channels.to_le_bytes());
    file.extend_from_slice(&bits_per_sample.to_le_bytes());
    file.extend_from_slice(&sample_rate.to_le_bytes());
    file.extend_from_slice(&total_samples.to_le_bytes());
    let header_crc = crate::crc32::crc32(&file);
    file.extend_from_slice(&header_crc.to_le_bytes());

    // Seek table (spec/01 §4).
    let seek_start = file.len();
    for blob in &frame_blobs {
        let entry = blob.len() as u32;
        file.extend_from_slice(&entry.to_le_bytes());
    }
    let seek_crc = crate::crc32::crc32(&file[seek_start..]);
    file.extend_from_slice(&seek_crc.to_le_bytes());

    // Frame data.
    for blob in &frame_blobs {
        file.extend_from_slice(blob);
    }

    Ok(file)
}

fn encode_one_frame(
    pcm: &[i32],
    nch: usize,
    bytes_per_sample: usize,
    qm_priming: Option<&[i32; 8]>,
) -> Vec<u8> {
    let samples_per_channel = pcm.len() / nch;
    let mut writer = BitWriter::new();
    let mut chans: Vec<EncoderChannelState> = (0..nch)
        .map(|_| {
            let mut lms = LmsState::frame_init(bytes_per_sample);
            if let Some(prime) = qm_priming {
                lms.qm = *prime;
            }
            EncoderChannelState {
                rice: RiceState::frame_init(),
                lms,
                stage_b: StageBState::frame_init(),
            }
        })
        .collect();

    let mut scratch = vec![0i32; nch];
    for s in 0..samples_per_channel {
        let base = s * nch;
        scratch.copy_from_slice(&pcm[base..base + nch]);
        crate::decorr::forward(&mut scratch);
        for ch in 0..nch {
            let cs = &mut chans[ch];
            let s_b_in = scratch[ch];
            // Stage-B (encoder): subtract `(prev * 31) >> 5`. Save the
            // pre-subtraction PCM as the new `prev` so the decoder's
            // `s_B = s_A + p_B` recovers the same value with the same
            // state evolution.
            let p_b = cs.stage_b.prev.wrapping_mul(31) >> 5;
            let s_a_in = s_b_in.wrapping_sub(p_b);
            cs.stage_b.prev = s_b_in;
            // Stage-A (encoder): the LMS step's state update mirrors
            // the decoder; the residual we emit is `s_A_in - p_A`.
            let e = lms_step_encode(&mut cs.lms, s_a_in);
            rice_encode_one(&mut writer, &mut cs.rice, e);
        }
    }

    let body_bytes = writer.finish_byte_aligned();
    let crc = crate::crc32::crc32(&body_bytes);
    let mut blob = body_bytes;
    blob.extend_from_slice(&crc.to_le_bytes());
    blob
}

/// Encoder-side Stage-A step. Performs the same STEP 1..5 update as
/// `LmsState::step_traced` (`spec/02` §4.2) but takes the PCM sample
/// `s_a_in` as input and returns the residual `e = s_a_in - p_A`.
///
/// The `dl[4..7]` regeneration at STEP 5 uses `s_a_in` directly (the
/// encoder knows the true sample), which keeps the decoder's
/// `s_A = e + p_A` consistent on the symmetric replay.
fn lms_step_encode(state: &mut LmsState, s_a_in: i32) -> i32 {
    // STEP 1 — sign-LMS qm update gated on the previous step's
    // residual. Branch-free `qm[i] += sign(error) * dx[i]`, mirroring
    // `LmsState::step` (see the rationale there): identical wrapping
    // result, no per-sample data-dependent branch.
    let sgn = (state.error > 0) as i32 - (state.error < 0) as i32;
    for i in 0..8 {
        state.qm[i] = state.qm[i].wrapping_add(sgn.wrapping_mul(state.dx[i]));
    }
    // STEP 2 — prediction.
    let mut sum: i32 = state.round;
    for i in 0..8 {
        sum = sum.wrapping_add(state.dl[i].wrapping_mul(state.qm[i]));
    }
    let p_a = sum >> state.shift;
    // STEP 3 — head→tail shift of dx[0..3] and dl[0..3].
    for i in 0..4 {
        state.dx[i] = state.dx[i + 1];
        state.dl[i] = state.dl[i + 1];
    }
    let dl_pre = [state.dl[4], state.dl[5], state.dl[6], state.dl[7]];
    // STEP 4 — regenerate dx[4..7]. Uses the magnitudes cached on the
    // state at frame init (see `LmsState::dx_mags`) so the per-sample
    // loop skips the lazy-table synchronisation check.
    let mags = state.dx_mags;
    for ((d, mag), dlp) in state.dx[4..].iter_mut().zip(mags).zip(dl_pre) {
        *d = if dlp < 0 { -mag } else { mag };
    }
    // STEP 5 — residual feedback + dl[4..7] regeneration. The encoder
    // uses `s_a_in` directly (= `e + p_A`); this matches what the
    // decoder writes on its symmetric step.
    let e = s_a_in.wrapping_sub(p_a);
    state.error = e;
    let s_a = s_a_in;
    state.dl[7] = s_a;
    state.dl[6] = s_a.wrapping_sub(dl_pre[3]);
    state.dl[5] = s_a.wrapping_sub(dl_pre[2]).wrapping_sub(dl_pre[3]);
    state.dl[4] = s_a
        .wrapping_sub(dl_pre[1])
        .wrapping_sub(dl_pre[2])
        .wrapping_sub(dl_pre[3]);
    e
}

struct EncoderChannelState {
    rice: RiceState,
    lms: LmsState,
    stage_b: StageBState,
}

/// LSB-first bit writer used by the entropy encoder. Mirrors the
/// reader's bit-order discipline (`crate::bitreader`) so the encoded
/// bytes are decoded back losslessly.
struct BitWriter {
    bytes: Vec<u8>,
    cache: u64,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            cache: 0,
            nbits: 0,
        }
    }

    fn put_bits(&mut self, value: u32, k: u32) {
        if k == 0 {
            return;
        }
        debug_assert!(k <= 32);
        let mask = if k == 32 { u32::MAX } else { (1u32 << k) - 1 };
        let v = (value & mask) as u64;
        self.cache |= v << self.nbits;
        self.nbits += k;
        while self.nbits >= 8 {
            self.bytes.push((self.cache & 0xFF) as u8);
            self.cache >>= 8;
            self.nbits -= 8;
        }
    }

    fn put_unary(&mut self, u: u32) {
        let mut remaining = u;
        while remaining >= 32 {
            self.put_bits(u32::MAX, 32);
            remaining -= 32;
        }
        if remaining > 0 {
            self.put_bits((1u32 << remaining) - 1, remaining);
        }
        self.put_bits(0, 1);
    }

    fn finish_byte_aligned(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.bytes.push((self.cache & 0xFF) as u8);
        }
        self.bytes
    }
}

/// Encoder-side adaptive Rice step. Mirrors `crate::rice::decode_one`
/// symmetrically: same low-/high-mode split (low when
/// `value < (1 << k0)`), same depth-1 escape bias on the high path,
/// same `(k0, k1, sum0, sum1)` IIR-feedback update (`spec/05` §5.2 +
/// §5.3 thresholds).
fn rice_encode_one(writer: &mut BitWriter, state: &mut RiceState, e: i32) {
    let value = zigzag(e);
    let bias_k0 = if state.k0 >= 32 {
        0u32
    } else {
        1u32 << state.k0
    };
    if value < bias_k0 {
        // Low mode: unary prefix = 0, binary tail at k0.
        let k = state.k0;
        let unary = 0;
        let binary_tail = value;
        writer.put_unary(unary);
        writer.put_bits(binary_tail, k);
        // STEP B equivalent — sum0 / k0 update only.
        state.sum0 = state.sum0.wrapping_add(value).wrapping_sub(state.sum0 >> 4);
        if state.k0 > 0 && state.sum0 < shl_saturating(state.k0 + 4) {
            state.k0 -= 1;
        } else if state.sum0 > shl_saturating(state.k0 + 5) {
            state.k0 += 1;
        }
    } else {
        // High mode: subtract the bias before computing prefix / tail.
        let pre_bias = value - bias_k0;
        let k1 = state.k1;
        let prefix_value = if k1 >= 32 { 0 } else { pre_bias >> k1 };
        let binary_tail = if k1 >= 32 {
            pre_bias
        } else if k1 == 0 {
            0
        } else {
            pre_bias & ((1u32 << k1) - 1)
        };
        let unary = prefix_value + 1;
        writer.put_unary(unary);
        writer.put_bits(binary_tail, k1);
        // STEP A equivalent — sum1 / k1 update on the pre-bias value.
        state.sum1 = state
            .sum1
            .wrapping_add(pre_bias)
            .wrapping_sub(state.sum1 >> 4);
        if state.k1 > 0 && state.sum1 < shl_saturating(state.k1 + 4) {
            state.k1 -= 1;
        } else if state.sum1 > shl_saturating(state.k1 + 5) {
            state.k1 += 1;
        }
        // STEP B equivalent — sum0 / k0 update on the post-bias value
        // (using the *current* k0, which has not yet been mutated).
        let bias_after_a = if state.k0 >= 32 {
            0u32
        } else {
            1u32 << state.k0
        };
        let value_after_bias = pre_bias.wrapping_add(bias_after_a);
        state.sum0 = state
            .sum0
            .wrapping_add(value_after_bias)
            .wrapping_sub(state.sum0 >> 4);
        if state.k0 > 0 && state.sum0 < shl_saturating(state.k0 + 4) {
            state.k0 -= 1;
        } else if state.sum0 > shl_saturating(state.k0 + 5) {
            state.k0 += 1;
        }
    }
}

#[inline]
fn shl_saturating(shift: u32) -> u32 {
    if shift >= 31 {
        0x8000_0000
    } else {
        1u32 << shift
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rice_encode_decode_roundtrip() {
        let residuals: Vec<i32> = vec![
            0, 1026, 1038, 1074, 1099, 1086, 1078, 873, -19, 0, 0, -42, 5, -7, 12, -3, 1234, -1234,
            100, -100,
        ];
        let mut writer = BitWriter::new();
        let mut state = RiceState::frame_init();
        let mut k_states_after = Vec::new();
        for &e in &residuals {
            rice_encode_one(&mut writer, &mut state, e);
            k_states_after.push(state);
        }
        let body = writer.finish_byte_aligned();

        let mut reader = crate::bitreader::BitReader::new(&body);
        let mut state = RiceState::frame_init();
        for (i, &expected_e) in residuals.iter().enumerate() {
            let e = crate::rice::decode_one(&mut reader, &mut state).unwrap();
            assert_eq!(e, expected_e, "residual mismatch at i={i}");
            assert_eq!(state, k_states_after[i], "tracker state mismatch at i={i}");
        }
    }
}
