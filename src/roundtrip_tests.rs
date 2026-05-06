//! Full encode-decode self-roundtrip tests.
//!
//! No reference TTA fixtures are checked into the workspace (the
//! `audit/reference-tapes/**` and `reference/inputs/**` trees are
//! gitignored), so round 1 verification is performed via the
//! crate-internal `#[cfg(test)]` encoder, which mirrors the decoder's
//! state machines. The test that we're verifying is:
//!
//! - Encoder + decoder agree on every bit of the framing layer (header
//!   CRC, seek-table CRC, per-frame CRC).
//! - Encoder + decoder agree on every transform's inverse: Rice
//!   trackers stay in lock-step; Stage-A LMS state stays in lock-step;
//!   Stage-B `prev` register stays in lock-step; channel decorrelation
//!   roundtrips (including the truncating-`/2` discriminator on
//!   odd-negative cases).
//!
//! What this does NOT verify (deferred to Auditor / round 2):
//!
//! - Bit-exact agreement with libtta's encoded output. That requires
//!   either a libtta-encoded fixture (forbidden input under the wall)
//!   or a checked-in reference fixture (currently absent from the
//!   workspace).
//! - The trace contract in `spec/06-trace-contract.md` (no debug
//!   instrumentation is wired in this round).

use crate::encoder::encode_to_tta1;
use crate::{decode, pack_pcm};

/// Generate a short integer-PCM sine wave for `n_samples` per channel.
fn sine(n_samples: usize, channels: u16, sample_rate: u32, freq_hz: f64, amp_i32: i32) -> Vec<i32> {
    let mut out = Vec::with_capacity(n_samples * channels as usize);
    for s in 0..n_samples {
        let phase = 2.0 * std::f64::consts::PI * freq_hz * s as f64 / sample_rate as f64;
        let v = ((phase.sin()) * amp_i32 as f64).round() as i32;
        for _ in 0..channels {
            out.push(v);
        }
    }
    out
}

/// Pseudo-random integer-PCM via a small xorshift; avoids std::collections
/// imports and is deterministic.
fn pseudo_noise(n_samples: usize, channels: u16, amp_mask: i32, seed: u64) -> Vec<i32> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    let mut out = Vec::with_capacity(n_samples * channels as usize);
    for _ in 0..n_samples * channels as usize {
        // xorshift64.
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let raw = s as i64;
        let v = ((raw & 0xFFFF_FFFF) as i32) & amp_mask;
        // Center around zero by sign-extending the low bits.
        let half = (amp_mask >> 1).wrapping_add(1);
        let centered = v.wrapping_sub(half);
        out.push(centered);
    }
    out
}

/// Tiny "DC + impulse" pattern that exercises the predictor warmup.
fn dc_with_impulse(n_samples: usize, channels: u16, dc: i32, impulse: i32) -> Vec<i32> {
    let mut out = Vec::with_capacity(n_samples * channels as usize);
    for s in 0..n_samples {
        let v = if s == n_samples / 4 { impulse } else { dc };
        for _ in 0..channels {
            out.push(v);
        }
    }
    out
}

/// Encode → decode roundtrip. Asserts the decoded samples equal the
/// originals exactly.
#[track_caller]
fn assert_roundtrip(samples: &[i32], channels: u16, bits_per_sample: u16, sample_rate: u32) {
    let tta = encode_to_tta1(samples, channels, bits_per_sample, sample_rate);
    let (info, decoded) = decode(&tta).expect("decode should succeed");
    assert_eq!(info.format, 1);
    assert_eq!(info.channels, channels);
    assert_eq!(info.bits_per_sample, bits_per_sample);
    assert_eq!(info.sample_rate, sample_rate);
    assert_eq!(
        info.total_samples as usize,
        samples.len() / channels as usize
    );
    assert_eq!(
        decoded.len(),
        samples.len(),
        "decoded sample count mismatch"
    );
    if decoded != samples {
        // Find the first divergence to make CI failure useful.
        for (i, (&got, &want)) in decoded.iter().zip(samples.iter()).enumerate() {
            assert_eq!(
                got, want,
                "first divergence at sample index {i}: got {got}, want {want}"
            );
        }
    }
    assert_eq!(decoded, samples);
}

#[test]
fn roundtrip_mono_16bit_silence() {
    let samples = vec![0i32; 1024];
    assert_roundtrip(&samples, 1, 16, 44_100);
}

#[test]
fn roundtrip_mono_16bit_sine_short() {
    // 0.05 s of a 440 Hz sine — well within a single frame.
    let n = (44_100.0 * 0.05) as usize;
    let samples = sine(n, 1, 44_100, 440.0, 16_000);
    assert_roundtrip(&samples, 1, 16, 44_100);
}

#[test]
fn roundtrip_mono_24bit_sine_short() {
    let n = (44_100.0 * 0.05) as usize;
    let samples = sine(n, 1, 44_100, 440.0, 1_000_000);
    assert_roundtrip(&samples, 1, 24, 44_100);
}

#[test]
fn roundtrip_stereo_16bit_correlated_sine() {
    let n = (44_100.0 * 0.05) as usize;
    let samples = sine(n, 2, 44_100, 440.0, 12_000);
    assert_roundtrip(&samples, 2, 16, 44_100);
}

#[test]
fn roundtrip_stereo_16bit_uncorrelated_sines() {
    let n_per_ch = (44_100.0 * 0.05) as usize;
    let mut samples = Vec::with_capacity(n_per_ch * 2);
    for s in 0..n_per_ch {
        let phase_l = 2.0 * std::f64::consts::PI * 440.0 * s as f64 / 44_100.0;
        let phase_r = 2.0 * std::f64::consts::PI * 660.0 * s as f64 / 44_100.0;
        samples.push((phase_l.sin() * 12_000.0).round() as i32);
        samples.push((phase_r.sin() * 8_000.0).round() as i32);
    }
    assert_roundtrip(&samples, 2, 16, 44_100);
}

#[test]
fn roundtrip_stereo_16bit_pseudo_noise() {
    // Noise exercises the truncating-`/2` discriminator (odd-negative
    // dec_in[0] cases) in the inverse decorrelation cascade per
    // spec/04 §6 / §7.1.
    let samples = pseudo_noise(2_048, 2, 0x7FFF, 0x1234_5678);
    assert_roundtrip(&samples, 2, 16, 44_100);
}

#[test]
fn roundtrip_six_channel_16bit() {
    // 6 channels exercises the N>2 inverse-decorrelation cascade
    // (spec/04 §4.2). Use independent low-frequency sines per
    // channel for clear discrimination.
    let n_per_ch = 1_024;
    let mut samples = Vec::with_capacity(n_per_ch * 6);
    for s in 0..n_per_ch {
        for ch in 0..6 {
            let freq = 220.0 * (1.0 + 0.1 * ch as f64);
            let phase = 2.0 * std::f64::consts::PI * freq * s as f64 / 44_100.0;
            let amp = 8_000.0 - 500.0 * ch as f64;
            samples.push((phase.sin() * amp).round() as i32);
        }
    }
    assert_roundtrip(&samples, 6, 16, 44_100);
}

#[test]
fn roundtrip_dc_with_impulse_mono() {
    let samples = dc_with_impulse(512, 1, 256, 12_000);
    assert_roundtrip(&samples, 1, 16, 44_100);
}

#[test]
fn roundtrip_multi_frame_mono_44100() {
    // 2.5 s at 44.1 kHz spans 3 frames (regular_frame_samples =
    // 46080; last frame = 110250 - 92160 = 18090). Exercises the
    // per-frame state-reset discipline of every spec.
    let n = 110_250;
    let samples = sine(n, 1, 44_100, 440.0, 14_000);
    assert_roundtrip(&samples, 1, 16, 44_100);
}

#[test]
fn pcm_pack_round_trip_16bit() {
    let samples: Vec<i32> = vec![0, 1, -1, 32_767, -32_768, 100, -100];
    let packed = pack_pcm(&samples, 16);
    assert_eq!(packed.len(), samples.len() * 2);
    // Verify the LE i16 bytes round-trip.
    for (i, chunk) in packed.chunks(2).enumerate() {
        let v = i16::from_le_bytes([chunk[0], chunk[1]]);
        assert_eq!(v as i32, samples[i]);
    }
}

#[test]
fn pcm_pack_round_trip_24bit() {
    let samples: Vec<i32> = vec![0, 1, -1, 8_388_607, -8_388_608, 0x123456, -0x123456];
    let packed = pack_pcm(&samples, 24);
    assert_eq!(packed.len(), samples.len() * 3);
    // Verify by parsing back as signed 24-bit LE.
    for (i, chunk) in packed.chunks(3).enumerate() {
        let raw = (chunk[0] as i32) | ((chunk[1] as i32) << 8) | ((chunk[2] as i32) << 16);
        let signed = if raw & 0x0080_0000 != 0 {
            raw | (-1i32 << 24)
        } else {
            raw
        };
        assert_eq!(signed, samples[i]);
    }
}

#[test]
fn header_validation_rejects_unsupported_format() {
    // Build a header that claims format=2 (encrypted) and verify the
    // decoder rejects it cleanly.
    let mut buf = Vec::new();
    buf.extend_from_slice(b"TTA1");
    buf.extend_from_slice(&2u16.to_le_bytes()); // format
    buf.extend_from_slice(&1u16.to_le_bytes()); // channels
    buf.extend_from_slice(&16u16.to_le_bytes()); // bps
    buf.extend_from_slice(&44_100u32.to_le_bytes());
    buf.extend_from_slice(&100u32.to_le_bytes());
    let crc = crate::crc32::crc32(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    assert!(matches!(
        decode(&buf),
        Err(crate::Error::UnsupportedFormat(2))
    ));
}

#[test]
fn corrupted_frame_crc_detected() {
    let n = 256;
    let samples = sine(n, 1, 44_100, 440.0, 4_000);
    let mut tta = encode_to_tta1(&samples, 1, 16, 44_100);
    // Flip the very last byte (part of the trailing per-frame CRC).
    let last = tta.len() - 1;
    tta[last] ^= 0x01;
    let result = decode(&tta);
    assert!(matches!(
        result,
        Err(crate::Error::Crc32Mismatch { region: "frame" })
    ));
}
