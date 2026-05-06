//! Test-only TTA1 encoder used for self-roundtrip fixtures.
//!
//! The Implementer round 1 deliverable is decoder-only; no production
//! encoder is exposed. This module implements the *minimal* logic
//! needed to manufacture valid format=1 TTA streams that the decoder
//! can be fed end-to-end (header + seek table + frames + per-frame
//! CRC). It is gated behind `#[cfg(test)]` and is not part of the
//! crate's public surface.
//!
//! The encoder mirrors the decoder structurally, applying each
//! transform's inverse:
//!
//! 1. Forward channel decorrelation per `spec/04` §3.1 / §4.1.
//! 2. Encoder Stage-B: subtract the predictor, save the
//!    pre-subtraction PCM as the new `prev` (`spec/03` §4.3).
//! 3. Encoder Stage-A: subtract the LMS prediction; the rest of the
//!    state update mirrors the decoder.
//! 4. Adaptive Rice encode: zigzag the residual, split into
//!    `(unary, binary_tail)` per the same `(k0, k1)` selectors, write
//!    the bit stream LSB-first.
//! 5. Pad the bit cache to the next byte boundary; emit the trailing
//!    CRC32.

#![cfg(test)]

use crate::lms::LmsState;
use crate::rice::{zigzag, RiceState};
use crate::stage_b::StageBState;
use crate::tables;

/// Build a complete TTA1 file from interleaved `i32` PCM samples.
///
/// `samples` length must equal `total_samples * channels`. `bps` must
/// be 16 or 24. The returned `Vec<u8>` is a self-consistent TTA1 file
/// that the decoder accepts.
pub fn encode_to_tta1(
    samples: &[i32],
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
) -> Vec<u8> {
    encode_to_tta1_inner(samples, channels, bits_per_sample, sample_rate, 1, None)
}

/// Format=2 encoder for self-roundtrip tests: primes Stage-A's
/// `qm[0..7]` with the password digest at every per-channel frame
/// init (per `spec/07` §3.5) and writes `format = 2` in the header.
pub fn encode_to_tta1_format2(
    samples: &[i32],
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
    password: &[u8],
) -> Vec<u8> {
    let priming = crate::password::derive_qm_priming(password);
    encode_to_tta1_inner(
        samples,
        channels,
        bits_per_sample,
        sample_rate,
        2,
        Some(priming),
    )
}

fn encode_to_tta1_inner(
    samples: &[i32],
    channels: u16,
    bits_per_sample: u16,
    sample_rate: u32,
    format: u16,
    qm_priming: Option<[i32; 8]>,
) -> Vec<u8> {
    assert!((1..=6).contains(&channels));
    assert!((16..=24).contains(&bits_per_sample));
    assert!(sample_rate > 0 && sample_rate <= 0x007F_FFFF);
    let nch = channels as usize;
    assert!(
        samples.len() % nch == 0,
        "sample buffer length must be a multiple of channel count"
    );
    let total_samples = (samples.len() / nch) as u32;
    let bytes_per_sample = bits_per_sample.div_ceil(8) as usize;

    // Compute frame geometry per spec §4.1.
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
    // Header (22 bytes).
    file.extend_from_slice(b"TTA1");
    file.extend_from_slice(&format.to_le_bytes());
    file.extend_from_slice(&channels.to_le_bytes());
    file.extend_from_slice(&bits_per_sample.to_le_bytes());
    file.extend_from_slice(&sample_rate.to_le_bytes());
    file.extend_from_slice(&total_samples.to_le_bytes());
    let header_crc = crate::crc32::crc32(&file);
    file.extend_from_slice(&header_crc.to_le_bytes());

    // Seek table.
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

    file
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
            let p_b = cs.stage_b.prev.wrapping_mul(31) >> 5;
            let s_a_in = s_b_in.wrapping_sub(p_b);
            cs.stage_b.prev = s_b_in;
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

fn lms_step_encode(state: &mut LmsState, s_a_in: i32) -> i32 {
    if state.error > 0 {
        for i in 0..8 {
            state.qm[i] = state.qm[i].wrapping_add(state.dx[i]);
        }
    } else if state.error < 0 {
        for i in 0..8 {
            state.qm[i] = state.qm[i].wrapping_sub(state.dx[i]);
        }
    }
    let mut sum: i32 = state.round;
    for i in 0..8 {
        sum = sum.wrapping_add(state.dl[i].wrapping_mul(state.qm[i]));
    }
    let p_a = sum >> state.shift;
    for i in 0..4 {
        state.dx[i] = state.dx[i + 1];
        state.dl[i] = state.dl[i + 1];
    }
    let dl_pre = [state.dl[4], state.dl[5], state.dl[6], state.dl[7]];
    let dx_mags = tables::lms_dx_magnitudes();
    for k in 0..4 {
        let mag = dx_mags[k];
        state.dx[4 + k] = if dl_pre[k] < 0 { -mag } else { mag };
    }
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

fn rice_encode_one(writer: &mut BitWriter, state: &mut RiceState, e: i32) {
    let value = zigzag(e);
    let bias_k0 = if state.k0 >= 32 {
        0u32
    } else {
        1u32 << state.k0
    };
    if value < bias_k0 {
        // Low mode.
        let k = state.k0;
        let unary = 0;
        let binary_tail = value;
        writer.put_unary(unary);
        writer.put_bits(binary_tail, k);
        state.sum0 = state.sum0.wrapping_add(value).wrapping_sub(state.sum0 >> 4);
        if state.k0 > 0 && state.sum0 < shl_saturating(state.k0 + 4) {
            state.k0 -= 1;
        } else if state.sum0 > shl_saturating(state.k0 + 5) {
            state.k0 += 1;
        }
    } else {
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
        state.sum1 = state
            .sum1
            .wrapping_add(pre_bias)
            .wrapping_sub(state.sum1 >> 4);
        if state.k1 > 0 && state.sum1 < shl_saturating(state.k1 + 4) {
            state.k1 -= 1;
        } else if state.sum1 > shl_saturating(state.k1 + 5) {
            state.k1 += 1;
        }
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
