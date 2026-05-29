//! Full encode-decode self-roundtrip tests.
//!
//! No reference TTA fixtures are checked into the workspace (the
//! `audit/reference-tapes/**` and `reference/inputs/**` trees are
//! gitignored), so verification is performed via the crate's own
//! production [`crate::encode`] / [`crate::encode_with_password`]
//! entry points, which mirror the decoder's state machines exactly.
//! The tests verify:
//!
//! - Encoder + decoder agree on every bit of the framing layer (header
//!   CRC, seek-table CRC, per-frame CRC).
//! - Encoder + decoder agree on every transform's inverse: Rice
//!   trackers stay in lock-step; Stage-A LMS state stays in lock-step;
//!   Stage-B `prev` register stays in lock-step; channel decorrelation
//!   roundtrips (including the truncating-`/2` discriminator on
//!   odd-negative cases).
//!
//! What this does NOT verify (deferred to Auditor):
//!
//! - Bit-exact agreement with libtta's encoded output. That requires
//!   either a libtta-encoded fixture (forbidden input under the wall)
//!   or a checked-in reference fixture (currently absent from the
//!   workspace).

use crate::{decode, decode_with_password, encode, encode_with_password, pack_pcm};

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
    let tta =
        encode(samples, channels, bits_per_sample, sample_rate).expect("encode should succeed");
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

/// Verify the spec/06 trace tape's structural properties on a tiny
/// self-encoded mono fixture: every line parses with `\t`/`=`, the
/// first event is `FILE_HEADER`, the last is `FRAME_END`, the count
/// of `STAGE_B_PREDICT` lines equals `nch * total_samples`, the
/// count of `PCM_OUT` lines equals `total_samples`, and `DECORR_PRE`
/// is zero on mono (spec/06 §11).
#[cfg(feature = "trace")]
#[test]
fn trace_tape_structural_self_check_mono() {
    // Use the per-thread override so concurrent tests do not race
    // against each other on the shared `OXIDEAV_TTA_TRACE_FILE` env
    // var. The override is thread-local; production users still
    // hit the env-var contract per spec/06 §2.
    let tmp = std::env::temp_dir().join("oxideav-tta-trace-mono.tsv");
    if tmp.exists() {
        std::fs::remove_file(&tmp).unwrap();
    }
    crate::trace::set_thread_trace_path(Some(tmp.clone()));

    let n = 256;
    let samples = sine(n, 1, 44_100, 440.0, 8_000);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode should succeed");
    let (_info, _decoded) = decode(&tta).expect("decode should succeed");

    crate::trace::set_thread_trace_path(None);

    let tape = std::fs::read_to_string(&tmp).expect("tape was written");
    let lines: Vec<&str> = tape.lines().collect();
    assert!(!lines.is_empty(), "tape must be non-empty");
    assert!(
        lines[0].starts_with("ev=FILE_HEADER\t"),
        "first line must be FILE_HEADER, got: {}",
        lines[0]
    );
    assert!(
        lines.last().unwrap().starts_with("ev=FRAME_END\t"),
        "last line must be FRAME_END"
    );
    // Every line: split on `\t`, every non-first record split on `=`.
    for (i, line) in lines.iter().enumerate() {
        let mut parts = line.split('\t');
        let head = parts.next().expect("each line carries an ev=...");
        assert!(
            head.starts_with("ev="),
            "line {i} does not start with ev=: {line}"
        );
        for p in parts {
            assert!(
                p.contains('='),
                "line {i} has a non `key=value` record `{p}`"
            );
        }
    }
    let count = |needle: &str| {
        lines
            .iter()
            .filter(|l| l.starts_with(&format!("ev={needle}\t")))
            .count()
    };
    // total_samples = 256, nch = 1.
    assert_eq!(
        count("STAGE_B_PREDICT"),
        n,
        "STAGE_B_PREDICT count must equal nch * total_samples = {n}"
    );
    assert_eq!(
        count("PCM_OUT"),
        n,
        "PCM_OUT count must equal total_samples = {n}"
    );
    assert_eq!(
        count("DECORR_PRE"),
        0,
        "DECORR_PRE must be empty on a mono fixture (spec/06 §11)"
    );
    assert_eq!(count("DECORR_POST"), 0);
    assert_eq!(count("FRAME_BEGIN"), 1);
    assert_eq!(count("FRAME_END"), 1);
    assert_eq!(count("FILE_HEADER"), 1);
    assert_eq!(count("HEADER_CRC"), 1);
    assert_eq!(count("SEEK_TABLE_BEGIN"), 1);
    assert_eq!(count("SEEK_TABLE_END"), 1);
    assert_eq!(count("SEEK_ENTRY"), 1, "single-frame fixture → 1 entry");
    assert_eq!(count("LMS_INIT"), 1);
    assert_eq!(count("RICE_K_INIT"), 1);

    let _ = std::fs::remove_file(&tmp);
}

#[cfg(feature = "trace")]
#[test]
fn trace_tape_decorr_events_only_on_multichannel() {
    let tmp = std::env::temp_dir().join("oxideav-tta-trace-stereo.tsv");
    if tmp.exists() {
        std::fs::remove_file(&tmp).unwrap();
    }
    crate::trace::set_thread_trace_path(Some(tmp.clone()));

    let n = 128;
    let samples = sine(n, 2, 44_100, 440.0, 6_000);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode should succeed");
    let (_info, _decoded) = decode(&tta).expect("decode should succeed");

    crate::trace::set_thread_trace_path(None);

    let tape = std::fs::read_to_string(&tmp).expect("tape was written");
    let count = |needle: &str| {
        tape.lines()
            .filter(|l| l.starts_with(&format!("ev={needle}\t")))
            .count()
    };
    assert_eq!(count("DECORR_PRE"), n);
    assert_eq!(count("DECORR_POST"), n);
    assert_eq!(count("PCM_OUT"), n);
    assert_eq!(count("STAGE_B_PREDICT"), 2 * n);
    assert_eq!(count("LMS_INIT"), 2);
    assert_eq!(count("RICE_K_INIT"), 2);

    let _ = std::fs::remove_file(&tmp);
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
fn header_validation_rejects_format_2_without_password() {
    // Build a header that claims format=2 (encrypted) and verify the
    // password-less `decode` entry point surfaces PasswordRequired
    // (the spec-defined password-required failure per spec/07 §7).
    let mut buf = Vec::new();
    buf.extend_from_slice(b"TTA1");
    buf.extend_from_slice(&2u16.to_le_bytes()); // format
    buf.extend_from_slice(&1u16.to_le_bytes()); // channels
    buf.extend_from_slice(&16u16.to_le_bytes()); // bps
    buf.extend_from_slice(&44_100u32.to_le_bytes());
    buf.extend_from_slice(&100u32.to_le_bytes());
    let crc = crate::crc32::crc32(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    assert!(matches!(decode(&buf), Err(crate::Error::PasswordRequired)));
}

#[test]
fn header_validation_rejects_unsupported_format() {
    // Format=3 (IEEE float) is still out of scope.
    let mut buf = Vec::new();
    buf.extend_from_slice(b"TTA1");
    buf.extend_from_slice(&3u16.to_le_bytes()); // format
    buf.extend_from_slice(&1u16.to_le_bytes()); // channels
    buf.extend_from_slice(&16u16.to_le_bytes()); // bps
    buf.extend_from_slice(&44_100u32.to_le_bytes());
    buf.extend_from_slice(&100u32.to_le_bytes());
    let crc = crate::crc32::crc32(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    assert!(matches!(
        decode(&buf),
        Err(crate::Error::UnsupportedFormat(3))
    ));
}

#[test]
fn roundtrip_format2_password_protected() {
    // Format=2 (spec/07): the encoder primes Stage-A's qm[] with the
    // password digest at every per-channel frame init; the
    // password-aware decoder applies the same priming on read. Round-
    // trip must be sample-exact.
    let n = (44_100.0 * 0.05) as usize;
    let samples = sine(n, 1, 44_100, 440.0, 12_000);
    let password = b"correct horse battery staple";
    let tta =
        encode_with_password(&samples, 1, 16, 44_100, password).expect("encode should succeed");
    let (info, decoded) =
        decode_with_password(&tta, password).expect("password-aware decode should succeed");
    assert_eq!(info.format, 2);
    assert_eq!(info.channels, 1);
    assert_eq!(decoded, samples);
}

#[test]
fn format2_without_password_fails_clean() {
    // The plain `decode` path must surface PasswordRequired for
    // format=2 streams.
    let n = 1024;
    let samples = vec![0i32; n];
    let tta =
        encode_with_password(&samples, 1, 16, 44_100, b"hunter2").expect("encode should succeed");
    assert!(matches!(decode(&tta), Err(crate::Error::PasswordRequired)));
}

#[test]
fn format2_wrong_password_corrupts_decode() {
    // Wrong password produces wrong qm priming, which produces wrong
    // PCM. The frame CRC32 still matches (format=2 doesn't
    // authenticate the password — it's lightweight obfuscation per
    // spec/07 §11), so the decode succeeds with corrupt audio. We
    // assert at least one sample diverges from the reference.
    let n = 1024;
    let samples = sine(n, 1, 44_100, 440.0, 8_000);
    let tta =
        encode_with_password(&samples, 1, 16, 44_100, b"correct").expect("encode should succeed");
    let (_, decoded) = decode_with_password(&tta, b"wrong").expect("decode succeeds");
    assert!(
        decoded != samples,
        "wrong password should corrupt PCM (format=2 has no MAC)"
    );
}

#[test]
fn scan_trailers_finds_id3v1_appended_to_real_tta_file() {
    // Build a real TTA1 file via the production encoder, then append
    // a synthetic 128-byte ID3v1 trailer. `scan_trailers` should
    // pick it up without disturbing the decode path.
    let n = 256;
    let samples = sine(n, 1, 44_100, 440.0, 4_000);
    let mut tta = encode(&samples, 1, 16, 44_100).expect("encode should succeed");
    let eos = tta.len();
    tta.extend_from_slice(b"TAG");
    tta.extend(std::iter::repeat(0u8).take(125));
    assert_eq!(tta.len(), eos + 128);

    // Decode still succeeds end-to-end (the trailer sits past the
    // last frame's CRC, outside the TTA1-level scope per spec/01 §7).
    let (_, decoded) = decode(&tta).expect("decode succeeds despite trailing tag");
    assert_eq!(decoded, samples);

    let info = crate::scan_trailers(&tta).expect("trailer scan");
    assert_eq!(info.id3v1, Some((eos, 128)));
    assert_eq!(info.apev2, None);
}

#[test]
fn scan_trailers_finds_apev2_footer_only() {
    let n = 256;
    let samples = sine(n, 1, 44_100, 440.0, 4_000);
    let mut tta = encode(&samples, 1, 16, 44_100).expect("encode should succeed");
    let eos = tta.len();
    // Build an APEv2 footer-only region with a 50-byte body.
    let body_size = 50;
    tta.extend(std::iter::repeat(0xAAu8).take(body_size));
    tta.extend_from_slice(b"APETAGEX");
    tta.extend_from_slice(&2000u32.to_le_bytes());
    tta.extend_from_slice(&((body_size + 32) as u32).to_le_bytes());
    tta.extend_from_slice(&1u32.to_le_bytes()); // item_count
    tta.extend_from_slice(&0x2000_0000u32.to_le_bytes()); // flags (is_footer)
    tta.extend_from_slice(&[0u8; 8]); // reserved

    let info = crate::scan_trailers(&tta).expect("trailer scan");
    assert_eq!(info.id3v1, None);
    assert_eq!(info.apev2, Some((eos, body_size + 32)));
}

#[test]
fn scan_trailers_finds_both_with_ape_immediately_before_id3v1() {
    let n = 256;
    let samples = sine(n, 1, 44_100, 440.0, 4_000);
    let mut tta = encode(&samples, 1, 16, 44_100).expect("encode should succeed");
    let eos = tta.len();
    let body_size = 20;
    tta.extend(std::iter::repeat(0xAAu8).take(body_size));
    tta.extend_from_slice(b"APETAGEX");
    tta.extend_from_slice(&2000u32.to_le_bytes());
    tta.extend_from_slice(&((body_size + 32) as u32).to_le_bytes());
    tta.extend_from_slice(&1u32.to_le_bytes());
    tta.extend_from_slice(&0x2000_0000u32.to_le_bytes());
    tta.extend_from_slice(&[0u8; 8]);
    let ape_end = tta.len();
    tta.extend_from_slice(b"TAG");
    tta.extend(std::iter::repeat(0u8).take(125));

    let info = crate::scan_trailers(&tta).expect("trailer scan");
    assert_eq!(info.id3v1, Some((ape_end, 128)));
    assert_eq!(info.apev2, Some((eos, body_size + 32)));
}

#[test]
fn scan_trailers_returns_empty_on_clean_tta_file() {
    let n = 256;
    let samples = sine(n, 1, 44_100, 440.0, 4_000);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode should succeed");
    let info = crate::scan_trailers(&tta).expect("trailer scan");
    assert!(info.is_empty(), "no trailers expected on a fresh encode");
}

#[test]
fn corrupted_frame_crc_detected() {
    let n = 256;
    let samples = sine(n, 1, 44_100, 440.0, 4_000);
    let mut tta = encode(&samples, 1, 16, 44_100).expect("encode should succeed");
    // Flip the very last byte (part of the trailing per-frame CRC).
    let last = tta.len() - 1;
    tta[last] ^= 0x01;
    let result = decode(&tta);
    assert!(matches!(
        result,
        Err(crate::Error::Crc32Mismatch { region: "frame" })
    ));
}

#[test]
fn roundtrip_format2_multi_frame_44100_mono() {
    // 2.5 s at 44.1 kHz spans 3 frames (regular_frame_samples = 46080;
    // last frame = 110250 - 92160 = 18090). Spec/07 §3.6 requires the
    // password-derived qm priming to be re-applied at EVERY frame init,
    // not just frame 0. The audit/07 §6.2-5 follow-up flagged that
    // single-frame coverage cannot tell the spec-correct behaviour
    // (re-prime per frame) apart from a buggy "prime once at frame 0
    // only" implementation; multi-frame round-trip exposes the bug
    // because LMS state is no longer all-zero on frame 1 entry, so
    // an unprimed qm would diverge from the encoder's primed-qm
    // residuals on the first sample of frame 1.
    let n = 110_250;
    let samples = sine(n, 1, 44_100, 440.0, 14_000);
    let password = b"multi-frame format2 verification";
    let tta = encode_with_password(&samples, 1, 16, 44_100, password)
        .expect("multi-frame format=2 encode should succeed");
    let (info, decoded) =
        decode_with_password(&tta, password).expect("password-aware decode should succeed");
    assert_eq!(info.format, 2);
    assert_eq!(info.channels, 1);
    assert_eq!(info.total_samples as usize, n);
    let regular = (44_100u64 * 256 / 245) as u32;
    let (frame_count, _) = info.frame_geometry();
    assert!(
        frame_count >= 3,
        "fixture must span ≥ 3 frames; regular={regular}, frame_count={frame_count}"
    );
    assert_eq!(decoded, samples);
}

#[test]
fn roundtrip_format2_multi_frame_44100_stereo() {
    // Multi-frame stereo: same per-frame qm-reset rule (spec/07 §3.6)
    // plus the per-frame Stage-A state reset for both channels per
    // spec/02 §3.1. Two channels share the SAME priming digest per
    // spec/07 §3.5.
    let n_per_ch = 110_250;
    let mut samples = Vec::with_capacity(n_per_ch * 2);
    for s in 0..n_per_ch {
        let phase_l = 2.0 * std::f64::consts::PI * 440.0 * s as f64 / 44_100.0;
        let phase_r = 2.0 * std::f64::consts::PI * 660.0 * s as f64 / 44_100.0;
        samples.push((phase_l.sin() * 12_000.0).round() as i32);
        samples.push((phase_r.sin() * 9_000.0).round() as i32);
    }
    let password = b"stereo multi-frame";
    let tta = encode_with_password(&samples, 2, 16, 44_100, password)
        .expect("multi-frame stereo format=2 encode should succeed");
    let (info, decoded) =
        decode_with_password(&tta, password).expect("stereo decode should succeed");
    assert_eq!(info.format, 2);
    assert_eq!(info.channels, 2);
    let (frame_count, _) = info.frame_geometry();
    assert!(frame_count >= 3);
    assert_eq!(decoded, samples);
}

#[test]
fn decode_with_password_format1_succeeds_with_clear_priming() {
    // Regression test for audit/07 §6.2-2: when format=1 bytes are
    // handed to decode_with_password, the priming must be cleared
    // (qm zero-init at every frame per spec/02 §3.1) without the
    // header / seek-table being re-parsed. We can't directly observe
    // the absence of a re-parse from outside, but we can confirm the
    // decoded PCM equals the plain-decode result; if the priming had
    // bled through it would produce a different sample stream because
    // spec/02 §4.2 STEP 1's sign-LMS gate would fire on a non-zero
    // qm[] from sample 0 of frame 0.
    let n = 1_024;
    let samples = sine(n, 1, 44_100, 440.0, 8_000);
    let tta = encode(&samples, 1, 16, 44_100).expect("format=1 encode should succeed");
    let (info_plain, decoded_plain) = decode(&tta).expect("plain decode");
    let (info_pwd, decoded_pwd) =
        decode_with_password(&tta, b"any password").expect("password-aware decode of format=1");
    assert_eq!(info_plain.format, 1);
    assert_eq!(info_pwd.format, 1);
    assert_eq!(decoded_plain, samples);
    assert_eq!(decoded_pwd, samples);
    assert_eq!(decoded_plain, decoded_pwd);
}

#[cfg(feature = "trace")]
#[test]
fn trace_tape_header_crc_carries_real_value() {
    // audit/07 §6.2-3 regression: the HEADER_CRC event's `computed_crc`
    // field must carry the freshly-parsed IEEE-802.3 CRC32 over the 18
    // header-body bytes (`spec/01` §3.5), NOT a placeholder zero. We
    // synthesize a fixture, extract its on-disk header CRC (bytes 18..22
    // little-endian), and assert the tape's `HEADER_CRC computed_crc`
    // hex field matches.
    let tmp = std::env::temp_dir().join("oxideav-tta-trace-header-crc.tsv");
    if tmp.exists() {
        std::fs::remove_file(&tmp).unwrap();
    }
    crate::trace::set_thread_trace_path(Some(tmp.clone()));

    let n = 256;
    let samples = sine(n, 1, 44_100, 440.0, 8_000);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let on_disk_crc = u32::from_le_bytes(tta[18..22].try_into().unwrap());

    let (_info, _decoded) = decode(&tta).expect("decode");
    crate::trace::set_thread_trace_path(None);

    let tape = std::fs::read_to_string(&tmp).expect("tape was written");
    let header_crc_line = tape
        .lines()
        .find(|l| l.starts_with("ev=HEADER_CRC\t"))
        .expect("tape carries a HEADER_CRC line");
    // The hex field is encoded as `computed_crc=0xXXXXXXXX`.
    let needle = format!("computed_crc=0x{:08x}", on_disk_crc);
    assert!(
        header_crc_line.contains(&needle),
        "HEADER_CRC line `{header_crc_line}` must contain `{needle}` \
         (on-disk header CRC; placeholder-zero would be 0x00000000)"
    );
    assert!(
        on_disk_crc != 0,
        "fixture's on-disk header CRC must be non-zero so the assertion above \
         actually proves the placeholder bug is gone"
    );
    assert!(header_crc_line.contains("crc_ok=1"));
    let _ = std::fs::remove_file(&tmp);
}

#[cfg(feature = "trace")]
#[test]
fn trace_tape_format2_qm_priming_reapplied_every_frame() {
    // audit/07 §6.2-5 follow-up: spec/07 §3.6 says the password-derived
    // qm priming reapplies at EVERY frame init, not just frame 0. A
    // multi-frame format=2 trace tape lets us inspect the `LMS_PRE`
    // event at `step_idx=0` of each frame and confirm the same eight
    // qm bytes (= digest sign-extended int8 → int32) appear there
    // regardless of frame index.
    let tmp = std::env::temp_dir().join("oxideav-tta-trace-fmt2-multi.tsv");
    if tmp.exists() {
        std::fs::remove_file(&tmp).unwrap();
    }
    crate::trace::set_thread_trace_path(Some(tmp.clone()));

    // 2.5 s = 110 250 samples → 3 frames at 44.1 kHz.
    let n = 110_250;
    let samples = sine(n, 1, 44_100, 440.0, 10_000);
    let password = b"trace multi-frame format2";
    let tta = encode_with_password(&samples, 1, 16, 44_100, password).expect("encode");
    let (_info, _decoded) = decode_with_password(&tta, password).expect("decode");
    crate::trace::set_thread_trace_path(None);

    // Derive the expected qm priming bytes the same way `password.rs`
    // does: ECMA-182 CRC-64 of `password`, little-endian byte unpacking,
    // sign-extended int8 → int32 (spec/07 §3.4 / §3.5).
    let expected_qm = crate::password::derive_qm_priming(password);

    let tape = std::fs::read_to_string(&tmp).expect("tape was written");
    // Locate every `LMS_PRE` event at `step_idx=0 channel=0`. There
    // must be one per frame in a mono fixture, and every one must
    // carry the same eight `qm_pre` values.
    let mut frame_qm_pres: Vec<Vec<i32>> = Vec::new();
    for line in tape.lines() {
        if !line.starts_with("ev=LMS_PRE\t") {
            continue;
        }
        // Parse the field-record `key=value\t...` pairs.
        let mut step_idx: Option<u32> = None;
        let mut channel: Option<u32> = None;
        let mut qm_pre: Option<Vec<i32>> = None;
        for rec in line.split('\t').skip(1) {
            let (k, v) = rec.split_once('=').expect("malformed key=value");
            match k {
                "step_idx" => step_idx = Some(v.parse().unwrap()),
                "channel" => channel = Some(v.parse().unwrap()),
                "qm_pre" => {
                    qm_pre = Some(v.split(',').map(|s| s.parse::<i32>().unwrap()).collect())
                }
                _ => {}
            }
        }
        if step_idx == Some(0) && channel == Some(0) {
            frame_qm_pres.push(qm_pre.expect("LMS_PRE always carries qm_pre"));
        }
    }
    assert!(
        frame_qm_pres.len() >= 3,
        "multi-frame fixture must produce ≥ 3 LMS_PRE step_idx=0 events; got {}",
        frame_qm_pres.len()
    );
    for (i, qm) in frame_qm_pres.iter().enumerate() {
        assert_eq!(qm.len(), 8, "qm_pre array length");
        let qm_arr: [i32; 8] = qm
            .as_slice()
            .try_into()
            .expect("qm_pre carries exactly 8 i32 fields");
        assert_eq!(
            qm_arr, expected_qm,
            "frame {i} must enter Stage-A with the digest-primed qm[] \
             (spec/07 §3.6 reapplies the priming at EVERY frame init); \
             got qm={qm:?}, expected={expected_qm:?}"
        );
    }
    let _ = std::fs::remove_file(&tmp);
}

#[cfg(feature = "trace")]
#[test]
fn trace_tape_format2_qm_priming_reapplied_every_frame_stereo() {
    // Stereo multi-frame variant: spec/07 §3.5 says all `nch` channels
    // share the SAME `dec_data` priming. Spec/06 §7.3 says
    // `step_idx = 0..nch*expected_samples-1` and `channel = step_idx
    // mod nch`, so channel 0's first sample lives at `step_idx=0` and
    // channel 1's at `step_idx=1`. We scan the first nch=2 LMS_PRE
    // events of every frame and assert both carry the digest-derived
    // qm_pre.
    let tmp = std::env::temp_dir().join("oxideav-tta-trace-fmt2-multi-stereo.tsv");
    if tmp.exists() {
        std::fs::remove_file(&tmp).unwrap();
    }
    crate::trace::set_thread_trace_path(Some(tmp.clone()));

    let n_per_ch = 110_250;
    let mut samples = Vec::with_capacity(n_per_ch * 2);
    for s in 0..n_per_ch {
        let phase_l = 2.0 * std::f64::consts::PI * 440.0 * s as f64 / 44_100.0;
        let phase_r = 2.0 * std::f64::consts::PI * 660.0 * s as f64 / 44_100.0;
        samples.push((phase_l.sin() * 11_000.0).round() as i32);
        samples.push((phase_r.sin() * 8_000.0).round() as i32);
    }
    let password = b"stereo trace fmt2";
    let tta = encode_with_password(&samples, 2, 16, 44_100, password).expect("encode");
    let (_info, _decoded) = decode_with_password(&tta, password).expect("decode");
    crate::trace::set_thread_trace_path(None);

    let expected_qm = crate::password::derive_qm_priming(password);

    let tape = std::fs::read_to_string(&tmp).expect("tape was written");
    // Group qm_pre observations at step_idx ∈ {0, 1} (= first sample
    // slot of nch=2) by (frame_idx, channel).
    let mut by_frame_ch: std::collections::BTreeMap<(u32, u32), [i32; 8]> =
        std::collections::BTreeMap::new();
    for line in tape.lines() {
        if !line.starts_with("ev=LMS_PRE\t") {
            continue;
        }
        let mut frame_idx: Option<u32> = None;
        let mut step_idx: Option<u32> = None;
        let mut channel: Option<u32> = None;
        let mut qm_pre: Option<Vec<i32>> = None;
        for rec in line.split('\t').skip(1) {
            let (k, v) = rec.split_once('=').expect("malformed key=value");
            match k {
                "frame_idx" => frame_idx = Some(v.parse().unwrap()),
                "step_idx" => step_idx = Some(v.parse().unwrap()),
                "channel" => channel = Some(v.parse().unwrap()),
                "qm_pre" => {
                    qm_pre = Some(v.split(',').map(|s| s.parse::<i32>().unwrap()).collect())
                }
                _ => {}
            }
        }
        // First sample of each frame: step_idx=0 (channel 0) and
        // step_idx=1 (channel 1) per spec/06 §7.3.
        let s = step_idx.unwrap();
        if s == 0 || s == 1 {
            let arr: [i32; 8] = qm_pre.unwrap().as_slice().try_into().unwrap();
            by_frame_ch.insert((frame_idx.unwrap(), channel.unwrap()), arr);
        }
    }
    // Expect ≥ 3 frames × 2 channels = 6 entries.
    assert!(
        by_frame_ch.len() >= 6,
        "expected ≥ 6 (frame,channel) entries for ≥ 3 frames × 2 channels; got {}",
        by_frame_ch.len()
    );
    for ((frame_idx, channel), qm) in &by_frame_ch {
        assert_eq!(
            *qm, expected_qm,
            "frame {frame_idx} channel {channel}: qm[] must be the digest priming \
             (spec/07 §3.5 — same digest for ALL nch channels, spec/07 §3.6 — \
             every frame init re-primes); got qm={qm:?}, expected={expected_qm:?}"
        );
    }
    let _ = std::fs::remove_file(&tmp);
}

// ---------------------------------------------------------------
// Streaming + random-access decode API (spec/01 §"seek table" +
// spec/02..05 §3.1 per-frame state reset). The properties under
// test:
//
//   1. `decode_frame_at(i)` returns bit-identical PCM to the
//      corresponding slice of `decode_all()`. This is the
//      "per-frame state-reset" property in observable form: if
//      any stage carried state across frames, random-access would
//      diverge.
//   2. `frame_iter()` produces the same concatenation as
//      `decode_all()`.
//   3. `seek_to_sample(s)` lands on a `frame_index` whose
//      `(file_offset_in_per_channel_samples, +sample_count)`
//      window contains `s`, and `sample_offset_in_frame` matches
//      `s` exactly.
//   4. `frame_iter()` reports a correct `size_hint` and an
//      ExactSizeIterator length matching `Decoder::frames.len()`.
// ---------------------------------------------------------------

#[test]
fn frame_iter_matches_decode_all_stereo_16bit() {
    let n = (44_100.0 * 0.4) as usize; // big enough to span multiple frames
    let samples = pseudo_noise(n, 2, 0x7FFF, 0xDEAD_BEEF);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");

    let eager = decode(&tta).expect("eager decode").1;

    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let mut streaming = Vec::with_capacity(eager.len());
    for r in dec.frame_iter() {
        streaming.extend_from_slice(&r.expect("frame_iter decode"));
    }
    assert_eq!(
        streaming, eager,
        "streaming frame_iter must produce bit-identical PCM to decode_all"
    );
}

#[test]
fn decode_frame_at_matches_decode_all_mono_24bit() {
    let n = (44_100.0 * 0.4) as usize;
    let samples = pseudo_noise(n, 1, 0x7F_FFFF, 0xC0FF_EE12);
    let tta = encode(&samples, 1, 24, 44_100).expect("encode");

    let eager = decode(&tta).expect("eager decode").1;

    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let mut cursor = 0usize;
    for (i, fd) in dec.frames.iter().enumerate() {
        let n_per_ch = fd.sample_count as usize;
        let frame_pcm = dec.decode_frame_at(i).expect("decode_frame_at");
        let expected = &eager[cursor..cursor + n_per_ch * dec.header.channels as usize];
        assert_eq!(
            frame_pcm, expected,
            "decode_frame_at({i}) must equal the slice of decode_all at sample cursor {cursor}; \
             if it does not, the per-frame state reset (spec/02..05 §3.1) is being violated"
        );
        cursor += n_per_ch * dec.header.channels as usize;
    }
    assert_eq!(cursor, eager.len(), "frame loop must cover every sample");
}

#[test]
fn decode_frame_at_rejects_out_of_range_index() {
    let samples = sine(64, 1, 44_100, 440.0, 12_000);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let last = dec.frames.len();
    assert_eq!(
        dec.decode_frame_at(last),
        Err(crate::Error::FrameIndexOutOfRange)
    );
}

#[test]
fn frame_iter_exact_size_matches_frames_len() {
    let n = (44_100.0 * 0.6) as usize;
    let samples = sine(n, 2, 44_100, 440.0, 8_000);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let it = dec.frame_iter();
    let expected = dec.frames.len();
    assert_eq!(
        it.len(),
        expected,
        "ExactSizeIterator::len() must match frames.len()"
    );
    let (low, high) = it.size_hint();
    assert_eq!(low, expected);
    assert_eq!(high, Some(expected));
}

#[test]
fn seek_to_sample_lands_in_right_frame() {
    let n = (44_100.0 * 0.6) as usize;
    let samples = sine(n, 1, 44_100, 660.0, 12_000);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let regular = dec.header.regular_frame_samples() as u64;
    assert!(regular > 0, "regular_frame_samples must be > 0");
    // Probe sample 0, mid-stream, last sample.
    for &target in &[0u64, (n as u64) / 2, (n as u64) - 1] {
        let sp = dec
            .seek_to_sample(target)
            .unwrap_or_else(|e| panic!("seek_to_sample({target}) failed: {e:?}"));
        let frame_start = (sp.frame_index as u64) * regular;
        let frame_end = frame_start + dec.frames[sp.frame_index].sample_count as u64;
        assert!(
            target >= frame_start && target < frame_end,
            "target sample {target} fell outside frame {} [{frame_start}, {frame_end})",
            sp.frame_index
        );
        assert_eq!(
            sp.sample_offset_in_frame as u64,
            target - frame_start,
            "sample_offset_in_frame should equal target - frame_start"
        );
    }
}

#[test]
fn frame_iter_from_past_end_is_empty() {
    let samples = sine(128, 1, 44_100, 440.0, 12_000);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let past = dec.frames.len() + 5;
    let it = dec.frame_iter_from(past);
    assert_eq!(it.len(), 0);
    let collected: Vec<_> = dec.frame_iter_from(past).collect();
    assert!(collected.is_empty());
}

#[test]
fn seek_to_sample_rejects_at_or_past_total_samples() {
    let samples = sine(128, 1, 44_100, 440.0, 12_000);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let total = dec.header.total_samples as u64;
    assert_eq!(
        dec.seek_to_sample(total),
        Err(crate::Error::SampleIndexOutOfRange)
    );
    assert_eq!(
        dec.seek_to_sample(total + 100),
        Err(crate::Error::SampleIndexOutOfRange)
    );
}

#[test]
fn frame_iter_streaming_seek_and_resume_bit_exact() {
    // The integration property: seek to sample S, decode the
    // containing frame plus all subsequent frames via the lazy
    // iterator, skip the in-frame prefix, and the resulting
    // interleaved samples must be the eager decode's tail from S.
    let n = (44_100.0 * 0.5) as usize;
    let samples = pseudo_noise(n, 2, 0x7FFF, 0xFEED_FACE);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");

    let eager = decode(&tta).expect("eager").1;
    let nch = 2usize;

    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let target_sample = (n as u64) * 3 / 4; // ~75% in
    let sp = dec.seek_to_sample(target_sample).expect("seek");

    // Use `frame_iter_from` so the skipped prefix is not decoded —
    // that is the whole point of the seek-resume API.
    let mut got: Vec<i32> = Vec::new();
    let mut emitted_frames = 0usize;
    for (i_offset, r) in dec.frame_iter_from(sp.frame_index).enumerate() {
        let i = sp.frame_index + i_offset;
        let pcm = r.expect("decode frame");
        if i == sp.frame_index {
            let skip = sp.sample_offset_in_frame as usize * nch;
            got.extend_from_slice(&pcm[skip..]);
        } else {
            got.extend_from_slice(&pcm);
        }
        emitted_frames += 1;
    }
    assert!(
        emitted_frames >= 1,
        "should have decoded at least one frame from the seek point"
    );

    let cursor = (target_sample as usize) * nch;
    let expected_tail = &eager[cursor..];
    assert_eq!(
        got.len(),
        expected_tail.len(),
        "streaming-from-seek tail length must match eager tail"
    );
    assert_eq!(
        got, expected_tail,
        "streaming-from-seek must produce bit-identical PCM to the eager decode's tail"
    );
}
