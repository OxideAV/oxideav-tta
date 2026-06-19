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
//! - Bit-exact agreement with a reference-encoder-produced TTA1 byte
//!   stream. That requires either a reference-encoded fixture (forbidden
//!   input under the clean-room wall) or a checked-in conformance
//!   fixture (currently absent from the workspace).

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
fn scan_trailers_typed_accessors_match_parser_output() {
    // Walks `scan_trailers` against a real encoded TTA1 stream
    // augmented with both an APEv2 footer-only region and an
    // ID3v1 trailer, then lifts both detected ranges via the
    // round-261 typed `Id3v1Range` / `ApeV2Range` accessors and
    // confirms the typed views agree with the parser output bit
    // for bit per `spec/01` §7.
    let n = 256;
    let samples = sine(n, 1, 44_100, 440.0, 4_000);
    let mut tta = encode(&samples, 1, 16, 44_100).expect("encode should succeed");
    let eos = tta.len();

    // ── APEv2 footer-only region (50-byte body + 32-byte footer). ──
    let body_size = 50;
    tta.extend(std::iter::repeat(0xAAu8).take(body_size));
    tta.extend_from_slice(b"APETAGEX");
    tta.extend_from_slice(&2000u32.to_le_bytes());
    tta.extend_from_slice(&((body_size + 32) as u32).to_le_bytes());
    tta.extend_from_slice(&7u32.to_le_bytes()); // item_count
    tta.extend_from_slice(&0x2000_0000u32.to_le_bytes()); // flags (is_footer)
    tta.extend_from_slice(&[0u8; 8]); // reserved
    let ape_end = tta.len();

    // ── ID3v1 trailer (128 bytes, 'TAG' magic). ──
    tta.extend_from_slice(b"TAG");
    tta.extend(std::iter::repeat(0u8).take(125));
    let file_len = tta.len();

    let info = crate::scan_trailers(&tta).expect("trailer scan");

    // Raw fields still match the existing parser shape.
    assert_eq!(info.id3v1, Some((ape_end, 128)));
    assert_eq!(info.apev2, Some((eos, body_size + 32)));

    // Typed lift: ID3v1.
    let id3 = info.id3v1_typed(file_len).unwrap().expect("present");
    assert_eq!(id3.start(), ape_end);
    assert_eq!(id3.len(), 128);
    assert_eq!(id3.end(), file_len);
    assert!(id3.is_at_file_end(file_len));
    assert_eq!(id3.byte_range(), ape_end..file_len);
    assert_eq!(&tta[id3.byte_range()][..3], b"TAG");

    // Typed lift: APEv2.
    let ape = info.apev2_typed(file_len).unwrap().expect("present");
    assert_eq!(ape.start(), eos);
    assert_eq!(ape.len(), body_size + 32);
    assert_eq!(ape.end(), ape_end);
    assert!(!ape.is_at_file_end(file_len)); // ID3v1 trails it
    assert_eq!(ape.header_and_body_size(), body_size);
    // The footer magic lives at the end of the APE region.
    let footer_start = ape.end() - crate::ApeV2Range::FOOTER_SIZE;
    assert_eq!(&tta[footer_start..footer_start + 8], b"APETAGEX");

    // Combined window covers both trailers contiguously.
    let combined = info.combined_byte_range().unwrap();
    assert_eq!(combined, (eos, (body_size + 32) + 128));
    assert_eq!(combined.0 + combined.1, file_len);

    // Decode is still bit-exact end-to-end (trailers are out of TTA1
    // scope per spec/01 §7).
    let (_, decoded) = decode(&tta).expect("decode succeeds despite trailers");
    assert_eq!(decoded, samples);
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

// ---------------------------------------------------------------
// Round 204 — streaming + random-access decode surface on format=2
// (password-protected) streams. The round-187 surface (frame_iter,
// decode_frame_at, seek_to_sample, frame_iter_from) is now reachable
// for format=2 via the new public constructor
// `Decoder::new_with_password(bytes, password)`. The properties
// under test mirror the format=1 streaming suite above:
//
//   1. `Decoder::new_with_password` rejects a format=2 stream with the
//      wrong-password digest the same way the eager
//      `decode_with_password` does — no panic, no spurious
//      `PasswordRequired` rejection, and (per spec/07 §11 "no MAC")
//      the per-frame CRC32 still validates because the CRC is taken
//      over the encoded bitstream, not the post-Stage-A samples.
//      A wrong password therefore produces a successfully-decoded
//      stream of corrupt PCM (the spec-correct behaviour).
//   2. With the right password, the streaming `frame_iter` /
//      `decode_frame_at` / `frame_iter_from` paths produce
//      bit-identical PCM to the eager `decode_with_password` baseline
//      across a multi-frame format=2 stream. Random-access on a mid-
//      stream frame must equal the matching slice of the eager
//      decode (spec/02..05 §3.1 per-frame state reset + spec/07 §3.6
//      qm re-prime at every frame).
//   3. `Decoder::new_with_password` over a format=1 stream is a
//      transparent alias for `Decoder::new`: the unused digest is
//      dropped on construction (audit/07 §6.2-2) and the produced
//      PCM is bit-identical to the format=1 streaming path.
//   4. The constructor surfaces the same out-of-range error variants
//      as the format=1 surface (FrameIndexOutOfRange,
//      SampleIndexOutOfRange) for invalid random-access requests on
//      format=2 streams.
// ---------------------------------------------------------------

#[test]
fn new_with_password_format2_streaming_matches_eager_stereo_16bit() {
    // Long enough to span ≥ 2 frames at 44.1 kHz so the multi-frame
    // qm re-prime path is exercised by `frame_iter` (cf. round-5
    // multi-frame format=2 trace coverage closing audit/07 §6.2-5:
    // every frame init must reapply the digest priming).
    // `regular_frame_samples = floor(44_100 * 256 / 245) = 46_073`
    // per spec/01 §4.1, so 2.0 s × 44_100 = 88_200 → exactly two
    // frames (one regular + one tail).
    let n = (44_100.0 * 2.0) as usize;
    let samples = pseudo_noise(n, 2, 0x7FFF, 0x5EED_DEAD_C0DE_F00D);
    let password = b"correct horse battery staple";
    let tta = encode_with_password(&samples, 2, 16, 44_100, password).expect("encode format=2");

    let (info, eager) = decode_with_password(&tta, password).expect("eager decode_with_password");
    assert_eq!(info.format, 2);

    let dec =
        crate::Decoder::new_with_password(&tta, password).expect("Decoder::new_with_password");
    assert_eq!(dec.header.format, 2);
    assert!(
        dec.frames.len() >= 2,
        "test wants ≥ 2 frames so the multi-frame qm re-prime path \
         is exercised; got {}",
        dec.frames.len()
    );

    // (a) frame_iter concatenates to the eager baseline.
    let mut streaming = Vec::with_capacity(eager.len());
    for r in dec.frame_iter() {
        streaming.extend_from_slice(&r.expect("frame_iter decode"));
    }
    assert_eq!(
        streaming, eager,
        "frame_iter() PCM must equal decode_with_password() PCM bit-exactly \
         on format=2"
    );

    // (b) decode_frame_at on every frame matches the eager slice.
    let nch = info.channels as usize;
    let mut cursor = 0usize;
    for (i, fd) in dec.frames.iter().enumerate() {
        let n_per_ch = fd.sample_count as usize;
        let frame_pcm = dec.decode_frame_at(i).expect("decode_frame_at format=2");
        let expected = &eager[cursor..cursor + n_per_ch * nch];
        assert_eq!(
            frame_pcm, expected,
            "decode_frame_at({i}) on format=2 must match eager slice at cursor {cursor}"
        );
        cursor += n_per_ch * nch;
    }

    // (c) frame_iter_from(mid) produces the eager tail from that
    //     sample boundary.
    let start = 1usize.min(dec.frames.len().saturating_sub(1));
    let preceding: usize = dec.frames[..start]
        .iter()
        .map(|f| f.sample_count as usize)
        .sum();
    let mut tail = Vec::new();
    for r in dec.frame_iter_from(start) {
        tail.extend_from_slice(&r.expect("frame_iter_from decode"));
    }
    assert_eq!(
        tail,
        eager[preceding * nch..],
        "frame_iter_from(start) tail must match eager tail from the matching \
         sample boundary on format=2"
    );
}

#[test]
fn new_with_password_seek_to_sample_format2_lands_in_right_frame() {
    // ≥ 2 frames so the mid-stream + last-sample probes are not
    // trivially frame-0.
    let n = (44_100.0 * 2.5) as usize;
    let samples = pseudo_noise(n, 2, 0x7FFF, 0x1234_5678_9ABC_DEF0);
    let password = b"hunter2";
    let tta = encode_with_password(&samples, 2, 16, 44_100, password).expect("encode format=2");

    let dec =
        crate::Decoder::new_with_password(&tta, password).expect("Decoder::new_with_password");
    let regular = dec.header.regular_frame_samples() as u64;
    assert!(regular > 0);
    // Probe sample 0, mid-stream, last sample — same shape as the
    // format=1 `seek_to_sample_lands_in_right_frame` test above.
    for &target in &[0u64, (n as u64) / 2, (n as u64) - 1] {
        let sp = dec.seek_to_sample(target).expect("seek_to_sample");
        let frame_start = (sp.frame_index as u64) * regular;
        let frame_end = frame_start + dec.frames[sp.frame_index].sample_count as u64;
        assert!(
            target >= frame_start && target < frame_end,
            "format=2 target sample {target} fell outside frame {} [{frame_start}, {frame_end})",
            sp.frame_index
        );
        assert_eq!(sp.sample_offset_in_frame as u64, target - frame_start);
    }
}

#[test]
fn new_with_password_format2_seek_and_resume_bit_exact() {
    // Integration property mirroring the format=1
    // `frame_iter_streaming_seek_and_resume_bit_exact` test: seek to
    // sample S, decode via `frame_iter_from`, skip the in-frame
    // prefix, compare against the eager tail. ≥ 2 frames so the
    // sp.frame_index != 0 case actually fires.
    let n = (44_100.0 * 2.5) as usize;
    let samples = pseudo_noise(n, 2, 0x7FFF, 0xCAFE_F00D_BEEF_DEAD);
    let password = b"correct horse battery staple";
    let tta = encode_with_password(&samples, 2, 16, 44_100, password).expect("encode format=2");

    let (info, eager) = decode_with_password(&tta, password).expect("eager");
    let nch = info.channels as usize;

    let dec =
        crate::Decoder::new_with_password(&tta, password).expect("Decoder::new_with_password");
    let target_sample = (n as u64) * 3 / 4;
    let sp = dec.seek_to_sample(target_sample).expect("seek");

    let mut got: Vec<i32> = Vec::new();
    for (i_offset, r) in dec.frame_iter_from(sp.frame_index).enumerate() {
        let i = sp.frame_index + i_offset;
        let pcm = r.expect("decode frame");
        if i == sp.frame_index {
            let skip = sp.sample_offset_in_frame as usize * nch;
            got.extend_from_slice(&pcm[skip..]);
        } else {
            got.extend_from_slice(&pcm);
        }
    }
    let cursor = (target_sample as usize) * nch;
    let expected_tail = &eager[cursor..];
    assert_eq!(got.len(), expected_tail.len());
    assert_eq!(
        got, expected_tail,
        "format=2 streaming-from-seek must produce bit-identical PCM to the eager tail"
    );
}

#[test]
fn new_with_password_format1_stream_drops_unused_priming() {
    // A format=1 stream constructed via `Decoder::new_with_password`
    // must decode bit-identically to `Decoder::new`: the priming is
    // computed (for the open call) and then dropped per audit/07
    // §6.2-2 / spec/02 §3.1 (format=1 qm zero-init invariant).
    let samples = sine(8_192, 1, 44_100, 660.0, 12_000);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode format=1");

    let dec_plain = crate::Decoder::new(&tta).expect("Decoder::new");
    let dec_pw = crate::Decoder::new_with_password(&tta, b"unused-password")
        .expect("Decoder::new_with_password on format=1");
    assert_eq!(dec_plain.header.format, 1);
    assert_eq!(dec_pw.header.format, 1);
    assert_eq!(
        dec_plain.decode_all().unwrap(),
        dec_pw.decode_all().unwrap()
    );

    // The frame_iter path agrees with the eager decode_all on the
    // password-constructed format=1 decoder too.
    let mut streamed = Vec::new();
    for r in dec_pw.frame_iter() {
        streamed.extend_from_slice(&r.expect("frame"));
    }
    let eager_plain = dec_plain.decode_all().unwrap();
    assert_eq!(streamed, eager_plain);
}

#[test]
fn new_with_password_format2_wrong_password_decodes_but_corrupts() {
    // spec/07 §11: format=2 has no MAC. A wrong password produces
    // corrupt PCM but every per-frame CRC32 still validates (the CRC
    // is taken over the encoded bitstream, not over post-Stage-A
    // samples). The streaming constructor must therefore SUCCEED
    // under a wrong password — no spurious `PasswordRequired` /
    // `Crc32Mismatch` — and the resulting PCM shape must match the
    // header (channels × total_samples) even though the values
    // differ from the originals.
    let n = (44_100.0 * 0.2) as usize;
    let samples = pseudo_noise(n, 2, 0x7FFF, 0xABCD_EF01_2345_6789);
    let right = b"right-password-AbCdEf";
    let wrong = b"wrong-password-XyZ";
    let tta = encode_with_password(&samples, 2, 16, 44_100, right).expect("encode format=2");

    let dec_right =
        crate::Decoder::new_with_password(&tta, right).expect("right password constructs");
    let pcm_right: Vec<i32> = dec_right
        .frame_iter()
        .flat_map(|r| r.expect("frame"))
        .collect();
    // Correct round-trip with the right password.
    assert_eq!(pcm_right, samples);

    let dec_wrong = crate::Decoder::new_with_password(&tta, wrong)
        .expect("wrong password must still construct");
    let pcm_wrong: Vec<i32> = dec_wrong
        .frame_iter()
        .flat_map(|r| r.expect("frame must still decode under wrong password"))
        .collect();
    // Same shape (header is plaintext per spec/07 §2) but values
    // differ — exactly the spec/07 §11 "no MAC, garbage PCM"
    // behaviour. We do not require *any* particular divergence
    // pattern, only that the dimensions match and that wrong !=
    // right (the digest XOR-folds into qm at every frame, so for
    // non-trivial PCM the two outputs are essentially always
    // distinct).
    assert_eq!(pcm_wrong.len(), pcm_right.len());
    assert_ne!(
        pcm_wrong, pcm_right,
        "wrong-password decode should produce different PCM to the right-password decode \
         (spec/07 §11 no MAC, garbage-out)"
    );
}

#[test]
fn new_with_password_format2_out_of_range_index_errors() {
    let samples = sine(4_096, 1, 44_100, 440.0, 12_000);
    let password = b"x";
    let tta = encode_with_password(&samples, 1, 16, 44_100, password).expect("encode format=2");
    let dec =
        crate::Decoder::new_with_password(&tta, password).expect("Decoder::new_with_password");
    let last = dec.frames.len();
    assert_eq!(
        dec.decode_frame_at(last),
        Err(crate::Error::FrameIndexOutOfRange)
    );
    let total = dec.header.total_samples as u64;
    assert_eq!(
        dec.seek_to_sample(total),
        Err(crate::Error::SampleIndexOutOfRange)
    );
}

// ---------------------------------------------------------------
// Round 209 — player-API sugar:
//
//   Decoder::frame_iter_from_sample(sample_index)
//   Decoder::decode_from_sample(sample_index)
//
// Combine `seek_to_sample` + `frame_iter_from` + the in-frame prefix
// skip into a single call. The tests pin three invariants against the
// existing eager `decode_all` baseline:
//
//   1. `decode_from_sample(s)` equals `decode_all()[s * channels..]`
//      bit-exactly across the parameter cube (format=1 mono16,
//      stereo16, stereo24, 6ch16, plus format=2 stereo16).
//   2. `frame_iter_from_sample(s)` chained `.concat()` equals the
//      same tail, and yields the same per-frame structure as
//      `frame_iter_from(seek.frame_index)` with the inner skip
//      removed.
//   3. Rejection shape: `sample_index >= total_samples` returns
//      `SampleIndexOutOfRange` from both APIs. The boundary case
//      `total_samples - 1` succeeds and returns exactly `channels`
//      interleaved entries.
// ---------------------------------------------------------------

#[test]
fn decode_from_sample_matches_eager_tail_mono16_format1() {
    let samples = pseudo_noise(2 * 44_100, 1, 0x7FFF, 0x0C0F_FEE5);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;

    for &target in &[0u64, 100, 23_000, 70_000, (eager.len() / nch) as u64 - 1] {
        let got = dec.decode_from_sample(target).expect("decode_from_sample");
        let cursor = (target as usize) * nch;
        let expected = &eager[cursor..];
        assert_eq!(
            got.len(),
            expected.len(),
            "decode_from_sample({target}) length must match eager tail"
        );
        assert_eq!(
            got, expected,
            "decode_from_sample({target}) must equal eager decode tail bit-exactly"
        );
    }
}

#[test]
fn decode_from_sample_matches_eager_tail_stereo16_format1() {
    let samples = pseudo_noise(2 * 44_100, 2, 0x7FFF, 0xD00D_BEEF);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;
    let total = dec.header.total_samples as u64;

    for &target in &[0u64, total / 4, total / 2, total * 3 / 4, total - 1] {
        let got = dec.decode_from_sample(target).expect("decode_from_sample");
        let cursor = (target as usize) * nch;
        assert_eq!(got, eager[cursor..]);
    }
}

#[test]
fn decode_from_sample_matches_eager_tail_stereo24_format1() {
    let samples = pseudo_noise(44_100, 2, 0x7F_FFFF, 0xFADE_FEED);
    let tta = encode(&samples, 2, 24, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;
    let total = dec.header.total_samples as u64;

    for &target in &[1u64, total / 3, total - 1] {
        let got = dec.decode_from_sample(target).expect("decode_from_sample");
        let cursor = (target as usize) * nch;
        assert_eq!(got, eager[cursor..]);
    }
}

#[test]
fn decode_from_sample_matches_eager_tail_6ch16_format1() {
    let samples = pseudo_noise(20_000, 6, 0x7FFF, 0xBAAA_AAAD);
    let tta = encode(&samples, 6, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;
    let total = dec.header.total_samples as u64;

    for &target in &[0u64, total / 5, total - 1] {
        let got = dec.decode_from_sample(target).expect("decode_from_sample");
        let cursor = (target as usize) * nch;
        assert_eq!(got, eager[cursor..]);
    }
}

#[test]
fn decode_from_sample_matches_eager_tail_stereo16_format2() {
    let samples = pseudo_noise(44_100, 2, 0x7FFF, 0xACE_2026);
    let password = b"the-r209-target";
    let tta =
        encode_with_password(&samples, 2, 16, 44_100, password).expect("encode_with_password");
    let dec =
        crate::Decoder::new_with_password(&tta, password).expect("Decoder::new_with_password");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;
    let total = dec.header.total_samples as u64;

    for &target in &[0u64, total / 4, total / 2, total - 1] {
        let got = dec.decode_from_sample(target).expect("decode_from_sample");
        let cursor = (target as usize) * nch;
        assert_eq!(got, eager[cursor..]);
    }
}

#[test]
fn frame_iter_from_sample_concat_matches_eager_tail() {
    // The iterator path: collect every frame's PCM and verify the
    // concatenation equals the eager tail. Also pin that the
    // per-frame structure preserves what `frame_iter_from` would
    // have yielded (minus the in-frame skip).
    let samples = pseudo_noise(2 * 44_100, 2, 0x7FFF, 0x600D_F00D);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;

    let target_sample = (eager.len() / nch) as u64 * 3 / 4;
    let cursor = (target_sample as usize) * nch;
    let expected_tail = &eager[cursor..];

    let mut got: Vec<i32> = Vec::new();
    let mut emitted_frames = 0usize;
    for r in dec
        .frame_iter_from_sample(target_sample)
        .expect("frame_iter_from_sample")
    {
        let pcm = r.expect("frame decode");
        got.extend_from_slice(&pcm);
        emitted_frames += 1;
    }
    assert!(
        emitted_frames >= 1,
        "frame_iter_from_sample must yield at least one frame"
    );
    assert_eq!(got.len(), expected_tail.len());
    assert_eq!(got, expected_tail);

    // Cross-check: the inner `frame_iter_from(sp.frame_index)` with
    // the manual skip must produce the same bytes. This pins that
    // the new API is *exactly* sugar over the existing combinators —
    // no semantic drift.
    let sp = dec.seek_to_sample(target_sample).expect("seek");
    let mut by_hand: Vec<i32> = Vec::new();
    for (offset, r) in dec.frame_iter_from(sp.frame_index).enumerate() {
        let pcm = r.expect("manual decode");
        if offset == 0 {
            by_hand.extend_from_slice(&pcm[sp.sample_offset_in_frame as usize * nch..]);
        } else {
            by_hand.extend_from_slice(&pcm);
        }
    }
    assert_eq!(
        by_hand, got,
        "frame_iter_from_sample must equal the by-hand seek_to_sample + \
         frame_iter_from + skip composition"
    );
}

#[test]
fn frame_iter_from_sample_zero_equals_full_decode() {
    // Boundary: sample_index == 0 must be equivalent to a full
    // `frame_iter` decode with no leading skip applied.
    let samples = pseudo_noise(3 * 44_100 / 2, 2, 0x7FFF, 0xBAD_F00D);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");

    let mut got: Vec<i32> = Vec::new();
    for r in dec
        .frame_iter_from_sample(0)
        .expect("frame_iter_from_sample(0)")
    {
        got.extend_from_slice(&r.expect("decode"));
    }
    assert_eq!(got, eager);
}

#[test]
fn decode_from_sample_last_sample_returns_one_frame_of_one_sample() {
    // Boundary: sample_index = total_samples - 1 must succeed and
    // yield exactly `channels` interleaved entries (one per-channel
    // sample at the very end).
    let samples = pseudo_noise(44_100, 2, 0x7FFF, 0x00DE_ADBE_EF22);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let total = dec.header.total_samples as u64;
    let nch = dec.header.channels as usize;

    let got = dec
        .decode_from_sample(total - 1)
        .expect("decode_from_sample(total-1)");
    assert_eq!(got.len(), nch, "must return exactly `channels` entries");

    let eager = dec.decode_all().expect("decode_all");
    assert_eq!(&got[..], &eager[eager.len() - nch..]);
}

#[test]
fn decode_from_sample_rejects_out_of_range() {
    let samples = sine(4_096, 1, 44_100, 440.0, 12_000);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let total = dec.header.total_samples as u64;

    assert_eq!(
        dec.decode_from_sample(total),
        Err(crate::Error::SampleIndexOutOfRange)
    );
    assert_eq!(
        dec.decode_from_sample(total + 1),
        Err(crate::Error::SampleIndexOutOfRange)
    );
    assert!(dec.frame_iter_from_sample(total).is_err());
    assert!(dec.frame_iter_from_sample(u64::MAX).is_err());
}

#[test]
fn frame_iter_from_sample_format2_seek_and_resume_bit_exact() {
    // Format=2 (password-protected) equivalent of
    // `frame_iter_from_sample_concat_matches_eager_tail`. The
    // per-frame qm re-prime discipline of `spec/07` §3.5–§3.6 makes
    // mid-stream resume bit-exact against the eager
    // `decode_with_password` baseline.
    let samples = pseudo_noise(2 * 44_100, 2, 0x7FFF, 0xACE_F00D);
    let password = b"f2-frame-iter-from-sample";
    let tta = encode_with_password(&samples, 2, 16, 44_100, password).expect("encode format=2");
    let dec =
        crate::Decoder::new_with_password(&tta, password).expect("Decoder::new_with_password");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;

    let target_sample = (eager.len() / nch) as u64 / 2;
    let cursor = (target_sample as usize) * nch;
    let expected_tail = &eager[cursor..];

    let mut got: Vec<i32> = Vec::new();
    for r in dec
        .frame_iter_from_sample(target_sample)
        .expect("frame_iter_from_sample (format=2)")
    {
        got.extend_from_slice(&r.expect("frame decode"));
    }
    assert_eq!(got, expected_tail);
}

// ---------------------------------------------------------------
// Round 215 — Duration-keyed player-API surface.
//
// `Decoder::total_duration`, `Decoder::seek_to_time`,
// `Decoder::frame_iter_from_time`, `Decoder::decode_from_time` —
// integer-arithmetic Duration ↔ sample-index conversion atop the
// existing sample-keyed seek surface (`spec/01` §3.3 / §3.4 / §4.1).
// ---------------------------------------------------------------

#[test]
fn total_duration_matches_total_samples_over_sample_rate() {
    use core::time::Duration;
    // 2.5 s at 44.1 kHz → 110 250 samples. The integer-arithmetic
    // total_duration helper must reproduce 2.5 s exactly modulo the
    // sample-period quantisation.
    let samples = pseudo_noise(110_250, 2, 0x7FFF, 0xD0D0_BEEF);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");

    // 110 250 / 44 100 = 2.5 exactly → 2 s + 500 ms.
    assert_eq!(dec.total_duration(), Duration::from_millis(2_500));

    // Different shape: 1 s 1 sample (44 101 samples) at 44.1 kHz →
    // 1.000022... s. The helper carries the sub-sample remainder in
    // nanoseconds at integer precision.
    let samples2 = pseudo_noise(44_101, 1, 0x7FFF, 0xABCD_EF01);
    let tta2 = encode(&samples2, 1, 16, 44_100).expect("encode 44101");
    let dec2 = crate::Decoder::new(&tta2).expect("Decoder::new 44101");
    let expected_ns = 1_000_000_000u128 + (1_000_000_000u128 / 44_100u128);
    assert_eq!(dec2.total_duration().as_nanos(), expected_ns);
}

#[test]
fn header_total_duration_matches_decoder_total_duration() {
    // The header-level `StreamHeader::total_duration` shortcut and the
    // `TotalSamples::duration_at(sample_rate)` typed-accessor entry
    // point must agree bit-for-bit with the `Decoder::total_duration`
    // computation across the typical stream shapes — the integer
    // arithmetic on both sides is identical, so any divergence here
    // would be a regression on the typed-accessor projection.
    let cases: &[(u32, u16, u32)] = &[
        // (total_samples, channels, sample_rate)
        (44_100, 1, 44_100),     // exact 1 s mono 44.1k
        (110_250, 2, 44_100),    // 2.5 s stereo 44.1k
        (48_000 * 3, 1, 48_000), // 3 s mono 48k
        (44_101, 1, 44_100),     // 1 s + 1 sample → sub-second remainder
        (1, 1, 192_000),         // single sample at 192k
        (0, 1, 44_100),          // empty stream — both sides → ZERO
    ];
    for &(total_samples, channels, sample_rate) in cases {
        if total_samples == 0 {
            // Empty-stream path: skip the encode (the encoder would
            // produce a zero-frame stream which is structurally valid
            // but the existing encode entry rejects zero-sample input
            // via `InvalidSampleBuffer`). Instead, hand-construct the
            // header literal and exercise just the typed accessor.
            let h = crate::StreamHeader {
                format: 1,
                channels,
                bits_per_sample: 16,
                sample_rate,
                total_samples,
            };
            assert_eq!(h.total_duration(), core::time::Duration::ZERO);
            assert!(h.total_samples_typed().is_empty());
            continue;
        }
        let samples = pseudo_noise(
            total_samples as usize,
            channels,
            0x7FFF,
            0x1234_5678_u64.wrapping_mul(total_samples as u64),
        );
        let tta = encode(&samples, channels, 16, sample_rate).expect("encode");
        let dec = crate::Decoder::new(&tta).expect("Decoder::new");
        let header_duration = dec.header.total_duration();
        let typed_duration = dec
            .header
            .total_samples_typed()
            .duration_at(dec.header.sample_rate);
        assert_eq!(header_duration, dec.total_duration());
        assert_eq!(typed_duration, dec.total_duration());
        // Round-trip the raw field via the typed accessor.
        assert_eq!(
            dec.header.total_samples_typed().count(),
            dec.header.total_samples
        );
    }
}

#[test]
fn seek_to_time_zero_lands_at_first_sample() {
    use core::time::Duration;
    let samples = pseudo_noise(44_100, 1, 0x7FFF, 0xAACC_DDEE);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");

    let sp = dec.seek_to_time(Duration::ZERO).expect("seek");
    assert_eq!(sp.frame_index, 0);
    assert_eq!(sp.sample_offset_in_frame, 0);
}

#[test]
fn seek_to_time_matches_seek_to_sample_at_equivalent_time() {
    use core::time::Duration;
    // 2 s stereo at 44.1 kHz → spans 88 200 samples ≈ 1.92 frames
    // (regular_frame_samples = floor(44100 × 256 / 245) = 46 073), so
    // there are 2 frames; mid-stream lands inside frame 0 or 1.
    let samples = pseudo_noise(88_200, 2, 0x7FFF, 0xBEEF_F00D);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");

    for &(time_ms, expected_sample) in &[
        (0u64, 0u64),
        (500, 22_050),
        (1_000, 44_100),
        (1_500, 66_150),
    ] {
        let t = Duration::from_millis(time_ms);
        let from_time = dec.seek_to_time(t).expect("seek_to_time");
        let from_sample = dec.seek_to_sample(expected_sample).expect("seek_to_sample");
        assert_eq!(
            from_time, from_sample,
            "seek_to_time({time_ms} ms) must equal seek_to_sample({expected_sample})"
        );
    }
}

#[test]
fn seek_to_time_at_total_duration_rejects() {
    use core::time::Duration;
    let samples = pseudo_noise(44_100, 1, 0x7FFF, 0xCAFE_F00D);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");

    // Exactly total_duration: sample index = total_samples → out of
    // range (the last addressable sample is total_samples - 1).
    let td = dec.total_duration();
    assert_eq!(
        dec.seek_to_time(td),
        Err(crate::Error::SampleIndexOutOfRange)
    );
    // Past the end: also out of range, no panic.
    assert_eq!(
        dec.seek_to_time(td + Duration::from_secs(1)),
        Err(crate::Error::SampleIndexOutOfRange)
    );
    // Duration::MAX must not panic.
    assert_eq!(
        dec.seek_to_time(Duration::MAX),
        Err(crate::Error::SampleIndexOutOfRange)
    );
}

#[test]
fn decode_from_time_matches_decode_from_sample_bit_exact() {
    use core::time::Duration;
    // Multi-frame mono format=1: 2 s @ 44.1 kHz → 88 200 samples
    // across two regular frames.
    let samples = pseudo_noise(88_200, 1, 0x7FFF, 0x600D_C00C);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");

    for &time_ms in &[0u64, 250, 500, 750, 1_500] {
        let t = Duration::from_millis(time_ms);
        let from_time = dec.decode_from_time(t).expect("decode_from_time");
        let sample_index = time_ms * 44_100 / 1_000;
        let from_sample = dec
            .decode_from_sample(sample_index)
            .expect("decode_from_sample");
        assert_eq!(
            from_time, from_sample,
            "decode_from_time({time_ms} ms) must equal decode_from_sample({sample_index})"
        );
    }
}

#[test]
fn frame_iter_from_time_concat_matches_eager_tail() {
    use core::time::Duration;
    // 2 s stereo @ 44.1 kHz; mid-stream resume via frame_iter_from_time
    // must equal the eager tail bit-exactly.
    let samples = pseudo_noise(88_200, 2, 0x7FFF, 0xC0DE_C0DE);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;

    let t = Duration::from_millis(1_000); // 1 s in → sample 44 100.
    let cursor = 44_100usize * nch;
    let expected_tail = &eager[cursor..];

    let mut got: Vec<i32> = Vec::new();
    for r in dec.frame_iter_from_time(t).expect("frame_iter_from_time") {
        got.extend_from_slice(&r.expect("frame decode"));
    }
    assert_eq!(got, expected_tail);
}

#[test]
fn frame_iter_from_time_rejects_past_end() {
    let samples = pseudo_noise(44_100, 1, 0x7FFF, 0xACAB_F00D);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let td = dec.total_duration();
    assert!(dec.frame_iter_from_time(td).is_err());
    assert!(dec.decode_from_time(td).is_err());
}

#[test]
fn time_apis_format2_seek_and_resume_bit_exact() {
    use core::time::Duration;
    // Format=2 (password-protected) equivalent of
    // `decode_from_time_matches_decode_from_sample_bit_exact`: the
    // per-frame qm re-prime discipline (`spec/07` §3.5–§3.6) must
    // propagate through the duration-keyed sugar unchanged.
    let samples = pseudo_noise(2 * 44_100, 2, 0x7FFF, 0xACE_2150);
    let password = b"r215-duration-api";
    let tta = encode_with_password(&samples, 2, 16, 44_100, password).expect("encode format=2");
    let dec =
        crate::Decoder::new_with_password(&tta, password).expect("Decoder::new_with_password");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;

    let t = Duration::from_millis(1_000);
    let cursor = 44_100usize * nch;
    let expected_tail = &eager[cursor..];

    // Eager path equivalence.
    let from_time = dec.decode_from_time(t).expect("decode_from_time");
    assert_eq!(from_time.len(), expected_tail.len());
    assert_eq!(from_time, expected_tail);

    // Lazy path equivalence.
    let mut got: Vec<i32> = Vec::new();
    for r in dec.frame_iter_from_time(t).expect("frame_iter_from_time") {
        got.extend_from_slice(&r.expect("frame decode"));
    }
    assert_eq!(got, expected_tail);
}

#[test]
fn seek_to_time_sub_sample_period_resolves_to_same_sample() {
    use core::time::Duration;
    // Two timestamps lying within the same sample period at 48 kHz
    // must produce the same SeekPoint — the floor conversion
    // collapses sub-sample variability. A timestamp on the *next*
    // sample-boundary must advance by exactly one sample. The
    // 48 kHz period in floor-nanoseconds is `1_000_000_000 / 48_000
    // = 20833`, which carries a sub-nanosecond truncation residue,
    // so the boundary itself is the smallest time strictly greater
    // than `n × period_ns_exact`; the worked test uses ns-accurate
    // floor-then-add boundary arithmetic to avoid the residue.
    let samples = pseudo_noise(48_000, 1, 0x7FFF, 0xD0D0_F00D);
    let tta = encode(&samples, 1, 16, 48_000).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");

    // Pick a sample index we want to test the boundary around.
    let target_sample = 5_904u64;
    // The duration corresponding to `target_sample` under the
    // crate's floor convention — the seek_to_time at this value
    // must return `target_sample`, and at this value plus a small
    // positive nudge must still return `target_sample`.
    let target_sample_ns = (target_sample as u128) * 1_000_000_000u128 / 48_000u128;
    let at_boundary = Duration::from_nanos(target_sample_ns as u64);
    let at_boundary_sp = dec.seek_to_time(at_boundary).expect("seek boundary");
    let by_sample = dec.seek_to_sample(target_sample).expect("seek by sample");
    assert_eq!(at_boundary_sp, by_sample);

    // 1 ns later (still well inside the sample period) — same
    // sample.
    let one_ns_later = at_boundary + Duration::from_nanos(1);
    let one_ns_sp = dec.seek_to_time(one_ns_later).expect("seek +1 ns");
    assert_eq!(at_boundary_sp, one_ns_sp);

    // The boundary of `target_sample + 1` — must advance by one
    // sample.
    let next_sample_ns = ((target_sample + 1) as u128) * 1_000_000_000u128 / 48_000u128;
    // Add 1 ns to nudge past the truncation residue (the floor of
    // `(target_sample + 1) × period_exact` can sit a residue
    // *below* the true boundary, depending on the rate).
    let next_boundary = Duration::from_nanos(next_sample_ns as u64 + 1);
    let next_sp = dec.seek_to_time(next_boundary).expect("seek next boundary");
    let cross_sp = dec
        .seek_to_sample(target_sample + 1)
        .expect("seek by next sample");
    assert_eq!(next_sp, cross_sp);
    assert_ne!(next_sp, at_boundary_sp);
}

// ────────────────────────────────────────────────────────────────────
// Round 219 — half-open `[start, end)` sample/time-range player-API
// quartet on Decoder:
//
//   Decoder::decode_sample_range(start, end)
//   Decoder::frame_iter_sample_range(start, end)
//   Decoder::decode_time_range(start, end)
//   Decoder::frame_iter_time_range(start, end)
//
// Invariants to lock:
//
//   1. `decode_sample_range(start, end)` equals `decode_all()[start*nch
//      .. end*nch]` bit-exactly across the parameter cube (mono16 /
//      stereo16 / stereo24 / 6ch16 / format=2 stereo16).
//   2. `decode_sample_range(s, total_samples)` equals `decode_from_sample(s)`
//      — the half-open end at `total_samples` collapses to the
//      sample-keyed tail surface.
//   3. `decode_sample_range(0, total_samples)` equals `decode_all()`.
//   4. `decode_sample_range(s, s) == Ok(vec![])` for every `s` in
//      `[0, total_samples]` (including the boundary `s == total_samples`).
//   5. `frame_iter_sample_range(s, e)` chained `.concat()` equals
//      `decode_sample_range(s, e)`.
//   6. Trailing-frame trim: when `end` lands mid-frame, the final
//      yielded frame's interleaved entry count matches the in-frame
//      sample offset, not the full regular-frame width.
//   7. `decode_time_range(start, end)` matches `decode_sample_range`
//      at the duration_to_sample_index conversion of both endpoints.
//   8. Out-of-range rejection: `end > total_samples` →
//      `SampleIndexOutOfRange`; `start > end` →
//      `SampleIndexOutOfRange`.
//
// All tests use the crate's own production encoder to build a TTA1
// stream, decode eagerly to obtain the expected PCM, and compare the
// range-API output against the eager slice.
// ────────────────────────────────────────────────────────────────────

#[test]
fn decode_sample_range_matches_eager_slice_mono16_format1() {
    let samples = pseudo_noise(2 * 44_100, 1, 0x7FFF, 0x0219_C0DE_F00D_BEEF);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;
    let total = dec.header.total_samples as u64;

    for &(start, end) in &[
        (0u64, total),
        (0, total / 2),
        (total / 4, total * 3 / 4),
        (100, 200),
        (total - 1, total),
        (total, total), // empty range at the boundary
        (0, 0),         // empty range at the start
    ] {
        let got = dec
            .decode_sample_range(start, end)
            .expect("decode_sample_range");
        let expected = &eager[(start as usize) * nch..(end as usize) * nch];
        assert_eq!(
            got.len(),
            expected.len(),
            "decode_sample_range({start},{end}) length mismatch"
        );
        assert_eq!(
            got, expected,
            "decode_sample_range({start},{end}) must equal eager slice bit-exactly"
        );
    }
}

#[test]
fn decode_sample_range_matches_eager_slice_stereo16_format1() {
    let samples = pseudo_noise(2 * 44_100, 2, 0x7FFF, 0x0219_DEAD_BEEF_CAFE);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;
    let total = dec.header.total_samples as u64;

    for &(start, end) in &[
        (0u64, total),
        (total / 4, total / 2),
        (total / 2, total),
        (1, total - 1),
    ] {
        let got = dec
            .decode_sample_range(start, end)
            .expect("decode_sample_range");
        let expected = &eager[(start as usize) * nch..(end as usize) * nch];
        assert_eq!(got, expected);
    }
}

#[test]
fn decode_sample_range_matches_eager_slice_stereo24_format1() {
    let samples = pseudo_noise(44_100, 2, 0x7F_FFFF, 0x0219_FADE_BAAD_F00D);
    let tta = encode(&samples, 2, 24, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;
    let total = dec.header.total_samples as u64;

    for &(start, end) in &[(0u64, total), (100, total - 100), (total / 3, total / 2)] {
        let got = dec
            .decode_sample_range(start, end)
            .expect("decode_sample_range");
        let expected = &eager[(start as usize) * nch..(end as usize) * nch];
        assert_eq!(got, expected);
    }
}

#[test]
fn decode_sample_range_matches_eager_slice_6ch16_format1() {
    let samples = pseudo_noise(20_000, 6, 0x7FFF, 0x0219_BAAA_AAAD_FACE);
    let tta = encode(&samples, 6, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;
    let total = dec.header.total_samples as u64;

    for &(start, end) in &[
        (0u64, total),
        (500, total - 500),
        (total / 4, total * 3 / 4),
    ] {
        let got = dec
            .decode_sample_range(start, end)
            .expect("decode_sample_range");
        let expected = &eager[(start as usize) * nch..(end as usize) * nch];
        assert_eq!(got, expected);
    }
}

#[test]
fn decode_sample_range_matches_eager_slice_format2_password_stereo16() {
    let samples = pseudo_noise(44_100, 2, 0x7FFF, 0x0219_E2E2_2026);
    let password = b"the-r219-range-target";
    let tta =
        encode_with_password(&samples, 2, 16, 44_100, password).expect("encode_with_password");
    let dec =
        crate::Decoder::new_with_password(&tta, password).expect("Decoder::new_with_password");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;
    let total = dec.header.total_samples as u64;

    for &(start, end) in &[(0u64, total), (total / 4, total * 3 / 4), (1, total - 1)] {
        let got = dec
            .decode_sample_range(start, end)
            .expect("decode_sample_range");
        let expected = &eager[(start as usize) * nch..(end as usize) * nch];
        assert_eq!(got, expected, "format=2 range mismatch for ({start},{end})");
    }
}

#[test]
fn decode_sample_range_full_stream_equals_decode_all() {
    // The (0, total) range must reproduce decode_all() exactly across
    // a multi-frame stream — locks the "no frames missing at the
    // boundary" invariant for the trim path.
    let samples = pseudo_noise(3 * 44_100, 2, 0x7FFF, 0x0219_F011_5774_2001);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let total = dec.header.total_samples as u64;

    let full = dec
        .decode_sample_range(0, total)
        .expect("decode_sample_range(0, total)");
    assert_eq!(full, eager);
}

#[test]
fn decode_sample_range_to_total_equals_decode_from_sample() {
    // For every start, `decode_sample_range(start, total_samples)`
    // must equal `decode_from_sample(start)` — the half-open end at
    // `total_samples` collapses to the previously-shipped tail
    // surface.
    let samples = pseudo_noise(2 * 44_100, 2, 0x7FFF, 0x0219_77A1_7070_7070);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let total = dec.header.total_samples as u64;

    for &start in &[0u64, 1, total / 3, total / 2, total * 2 / 3, total - 1] {
        let range = dec
            .decode_sample_range(start, total)
            .expect("decode_sample_range(start, total)");
        let tail = dec
            .decode_from_sample(start)
            .expect("decode_from_sample(start)");
        assert_eq!(
            range, tail,
            "decode_sample_range({start}, total) must equal decode_from_sample({start})"
        );
    }
}

#[test]
fn decode_sample_range_empty_at_every_boundary() {
    // s == e returns Ok(vec![]) for every s in [0, total_samples],
    // including the upper boundary `s == total_samples` (which
    // `decode_from_sample` rejects, but the half-open range accepts
    // because it represents an empty selection at the very end).
    let samples = pseudo_noise(44_100, 2, 0x7FFF, 0x0219_E007_DA7A);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let total = dec.header.total_samples as u64;

    for &s in &[0u64, 1, total / 2, total - 1, total] {
        let got = dec
            .decode_sample_range(s, s)
            .expect("decode_sample_range(s, s)");
        assert!(
            got.is_empty(),
            "decode_sample_range({s}, {s}) must be empty"
        );
    }
}

#[test]
fn frame_iter_sample_range_concat_matches_decode_sample_range() {
    // The iterator path must produce, by concatenation, the same
    // PCM that decode_sample_range materialises eagerly.
    let samples = pseudo_noise(2 * 44_100, 2, 0x7FFF, 0x0219_C0AC_CA70);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let total = dec.header.total_samples as u64;

    for &(start, end) in &[
        (0u64, total),
        (100, total - 100),
        (total / 4, total * 3 / 4),
        (total / 3, total / 2),
    ] {
        let eager = dec
            .decode_sample_range(start, end)
            .expect("decode_sample_range");
        let lazy: Vec<i32> = dec
            .frame_iter_sample_range(start, end)
            .expect("frame_iter_sample_range")
            .flat_map(|r| r.expect("frame decode"))
            .collect();
        assert_eq!(lazy, eager, "lazy concat must equal eager range");
    }
}

#[test]
fn frame_iter_sample_range_trailing_trim_lands_at_boundary() {
    // When `end` falls mid-frame, the final yielded frame must be
    // trimmed so its interleaved entry count matches the in-frame
    // sample offset (not the full regular-frame width). Verifies
    // the trim is in-place and exact.
    let samples = pseudo_noise(3 * 44_100, 2, 0x7FFF, 0x0219_7A11_7E1A);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let nch = dec.header.channels as usize;
    let regular = dec.header.regular_frame_samples() as u64;
    // Pick `end` to land deep inside the second frame.
    let start = 100u64;
    let end = regular + 12_345;
    let frames: Vec<Vec<i32>> = dec
        .frame_iter_sample_range(start, end)
        .expect("frame_iter_sample_range")
        .map(|r| r.expect("frame decode"))
        .collect();
    // Should be exactly 2 yielded frames given start in frame 0 and
    // end deep in frame 1.
    assert_eq!(
        frames.len(),
        2,
        "expected 2 yielded frames (head-trim + tail-trim)"
    );
    // First frame: regular - start in-frame samples.
    let first_entries = (regular as usize - start as usize) * nch;
    assert_eq!(
        frames[0].len(),
        first_entries,
        "first frame must be head-trimmed to (regular - start) * channels"
    );
    // Second frame: 12_345 samples (= end - regular).
    let second_entries = (end as usize - regular as usize) * nch;
    assert_eq!(
        frames[1].len(),
        second_entries,
        "second frame must be tail-trimmed to (end - regular) * channels"
    );
    // Total entries match the request width.
    let total_entries: usize = frames.iter().map(|f| f.len()).sum();
    assert_eq!(total_entries, (end - start) as usize * nch);
}

#[test]
fn decode_time_range_matches_decode_sample_range_at_endpoints() {
    use core::time::Duration;
    // 2 s at 48 kHz — sample indices that are multiples of the rate
    // map to whole-second `Duration`s with no floor residue, so the
    // sample-keyed and duration-keyed range surfaces are guaranteed
    // to agree byte-for-byte at these boundaries.
    let samples = pseudo_noise(2 * 48_000, 2, 0x7FFF, 0x0219_71E3_AE0E);
    let tta = encode(&samples, 2, 16, 48_000).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let rate = dec.header.sample_rate as u64; // 48_000
    let total = dec.header.total_samples as u64; // 96_000
    assert_eq!(total, 2 * rate);

    // (start_sample, end_sample) pairs where both endpoints are
    // multiples of `rate` and therefore round-trip exactly through
    // `duration_to_sample_index` (`ns = s * 1e9 / rate` is exact when
    // `s` is a multiple of `rate`).
    for &(start_s, end_s) in &[
        (0u64, total),    // full stream
        (0u64, rate),     // first second
        (rate, total),    // second second
        (rate / 2, rate), // 0.5 s window — `rate/2` may not round-trip; nudged below
    ] {
        let start_t =
            Duration::from_nanos(((start_s as u128) * 1_000_000_000u128 / (rate as u128)) as u64);
        let end_t =
            Duration::from_nanos(((end_s as u128) * 1_000_000_000u128 / (rate as u128)) as u64);
        // Skip pairs where the round-trip is inexact (i.e. the
        // re-floored value disagrees with the original).
        let start_re = ((start_t.as_nanos() * rate as u128) / 1_000_000_000u128) as u64;
        let end_re = ((end_t.as_nanos() * rate as u128) / 1_000_000_000u128) as u64;
        if start_re != start_s || end_re != end_s {
            continue;
        }
        let time_got = dec
            .decode_time_range(start_t, end_t)
            .expect("decode_time_range");
        let sample_got = dec
            .decode_sample_range(start_s, end_s)
            .expect("decode_sample_range");
        assert_eq!(
            time_got, sample_got,
            "decode_time_range and decode_sample_range must agree at exact-round-trip boundaries ({start_s}, {end_s})"
        );
    }
}

#[test]
fn decode_time_range_full_duration_equals_decode_all() {
    use core::time::Duration;
    let samples = pseudo_noise(44_100, 2, 0x7FFF, 0x0219_F011_D02E);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    let dur = dec.total_duration();
    let got = dec
        .decode_time_range(Duration::ZERO, dur)
        .expect("decode_time_range(0, total_duration)");
    assert_eq!(got, eager);
}

#[test]
fn frame_iter_time_range_concat_matches_decode_time_range() {
    use core::time::Duration;
    let samples = pseudo_noise(2 * 44_100, 2, 0x7FFF, 0x0219_F127_1E1E);
    let tta = encode(&samples, 2, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");

    let start = Duration::from_millis(250);
    let end = Duration::from_millis(750);
    let eager = dec
        .decode_time_range(start, end)
        .expect("decode_time_range");
    let lazy: Vec<i32> = dec
        .frame_iter_time_range(start, end)
        .expect("frame_iter_time_range")
        .flat_map(|r| r.expect("frame decode"))
        .collect();
    assert_eq!(lazy, eager);
}

#[test]
fn decode_sample_range_rejects_start_greater_than_end() {
    let samples = pseudo_noise(44_100, 1, 0x7FFF, 0x0219_57A7_E2DE);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let total = dec.header.total_samples as u64;
    assert_eq!(
        dec.decode_sample_range(100, 50),
        Err(crate::Error::SampleIndexOutOfRange),
        "start > end must reject"
    );
    assert_eq!(
        dec.decode_sample_range(total, total - 1),
        Err(crate::Error::SampleIndexOutOfRange)
    );
    assert_eq!(
        dec.frame_iter_sample_range(100, 50)
            .err()
            .expect("must be Err"),
        crate::Error::SampleIndexOutOfRange
    );
}

#[test]
fn decode_sample_range_rejects_end_past_total_samples() {
    let samples = pseudo_noise(44_100, 1, 0x7FFF, 0x0219_E2D0_0070);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let total = dec.header.total_samples as u64;
    assert_eq!(
        dec.decode_sample_range(0, total + 1),
        Err(crate::Error::SampleIndexOutOfRange)
    );
    assert_eq!(
        dec.decode_sample_range(total - 1, total + 5),
        Err(crate::Error::SampleIndexOutOfRange)
    );
    // start == end == total + 1 still errors because end > total.
    assert_eq!(
        dec.decode_sample_range(total + 1, total + 1),
        Err(crate::Error::SampleIndexOutOfRange)
    );
}

#[test]
fn decode_time_range_rejects_end_past_total_duration() {
    use core::time::Duration;
    let samples = pseudo_noise(44_100, 1, 0x7FFF, 0x0219_7103_00AE);
    let tta = encode(&samples, 1, 16, 44_100).expect("encode");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let rate = dec.header.sample_rate as u128;
    let total = dec.header.total_samples as u128;
    // The smallest duration that floors to `total_samples + 1` per
    // `duration_to_sample_index`'s `floor(ns * rate / 1e9)` is the
    // ceiling of `(total + 1) * 1e9 / rate`. Add 1 ns to land
    // unambiguously past the boundary.
    let past_ns = (((total + 1) * 1_000_000_000u128).div_ceil(rate)) + 1;
    let past = Duration::from_nanos(past_ns as u64);
    assert_eq!(
        dec.decode_time_range(Duration::ZERO, past),
        Err(crate::Error::SampleIndexOutOfRange),
        "decode_time_range with end past total_duration must reject"
    );
    // Also pin `start > end` rejection on the duration-keyed surface.
    assert_eq!(
        dec.decode_time_range(Duration::from_secs(2), Duration::ZERO),
        Err(crate::Error::SampleIndexOutOfRange),
        "decode_time_range with start > end must reject"
    );
}

#[test]
fn decode_sample_range_format2_password_seek_and_clip_bit_exact() {
    // Format=2 (password-protected) range under the per-frame qm
    // re-prime discipline of spec/07 §3.5–§3.6 — locks that the
    // bounded segment is bit-exact against the eager baseline.
    let samples = pseudo_noise(2 * 44_100, 2, 0x7FFF, 0x0219_F2F2_5E60);
    let password = b"r219-segment-key";
    let tta =
        encode_with_password(&samples, 2, 16, 44_100, password).expect("encode_with_password");
    let dec =
        crate::Decoder::new_with_password(&tta, password).expect("Decoder::new_with_password");
    let eager = dec.decode_all().expect("decode_all");
    let nch = dec.header.channels as usize;
    let total = dec.header.total_samples as u64;
    let start = total / 5;
    let end = total * 4 / 5;
    let got = dec
        .decode_sample_range(start, end)
        .expect("decode_sample_range");
    let expected = &eager[(start as usize) * nch..(end as usize) * nch];
    assert_eq!(got, expected);
}

#[test]
fn frame_descriptor_typed_accessors_match_parsed_stream() {
    // End-to-end cross-API agreement on a real encoded multi-frame
    // stream: parse it via Decoder::new and walk every frame
    // descriptor confirming the typed accessors agree with the raw
    // fields they lift AND that the spec-derived regular-frame bound
    // (`spec/01` §4.1 — `floor(sample_rate * 256 / 245)`) holds for
    // every frame's sample count (every regular frame == regular
    // count; the last frame may be shorter).
    //
    // Three independent shapes pin different code paths:
    //   - mono 16-bit @ 44.1k, 2.5 s   => 3 frames (regular,regular,last)
    //   - stereo 16-bit @ 48k, 2 s     => 2 frames (exact-multiple => both regular)
    //   - mono 24-bit @ 44.1k, 1 s     => 1 frame (last == only)
    let cases: &[(u32, u16, u16, u32)] = &[
        // (total_samples, channels, bits_per_sample, sample_rate)
        (110_250, 1, 16, 44_100),
        (96_000, 2, 16, 48_000),
        (44_100, 1, 24, 44_100),
    ];
    for &(total_samples, channels, bits_per_sample, sample_rate) in cases {
        let samples = pseudo_noise(
            total_samples as usize,
            channels,
            (1i32 << (bits_per_sample - 1)) - 1,
            0xC0FF_EE00u64.wrapping_mul(total_samples as u64),
        );
        let tta = encode(&samples, channels, bits_per_sample, sample_rate).expect("encode");
        let dec = crate::Decoder::new(&tta).expect("Decoder::new");
        let regular = dec.header.regular_frame_samples();
        let (expected_frame_count, expected_last_samples) = dec.header.frame_geometry();
        assert_eq!(dec.frames.len() as u32, expected_frame_count);

        for (idx, fd) in dec.frames.iter().enumerate() {
            // disk_size lift round-trips.
            let len = fd.disk_size_typed().expect("disk_size_typed");
            assert_eq!(len.total_size(), fd.disk_size);
            assert_eq!(len.body_size(), fd.body_size());
            assert!(len.total_size() >= 4, "disk_size_typed enforces >= 4");

            // sample_count lift round-trips and respects the
            // regular-frame ceiling.
            let sc = fd.sample_count_typed().expect("sample_count_typed");
            assert_eq!(sc.count(), fd.sample_count);
            assert!(
                sc.is_within_regular_bound(regular),
                "frame {idx} sample_count {} exceeds regular bound {regular}",
                sc.count()
            );

            // Per spec/01 §4.1 / §5.5: every regular (non-last) frame
            // carries exactly `regular` samples; the last frame may be
            // shorter (and equals `expected_last_samples`).
            if (idx as u32) + 1 == expected_frame_count {
                assert_eq!(sc.count(), expected_last_samples);
            } else {
                assert_eq!(sc.count(), regular);
            }
        }
    }
}

#[test]
fn frame_geometry_typed_matches_parsed_stream() {
    // End-to-end cross-API agreement: parse a real multi-frame stream
    // and confirm the round-251 `FrameGeometry` typed projection on
    // `StreamHeader` agrees bit-for-bit with the decoder's actual
    // frame-table walk (per-frame sample counts, regular-vs-last
    // discrimination, seek-table on-disk size, total-samples
    // back-derivation).
    //
    // The same three-case parameter grid the round-246
    // `frame_descriptor_typed_accessors_match_parsed_stream` test
    // covers, so the geometry-side surface is pinned against the same
    // structurally-diverse shapes:
    //   - mono 16-bit @ 44.1k, 2.5 s   => 3 frames (regular, regular, last)
    //   - stereo 16-bit @ 48k, 2 s     => 2 frames (exact-multiple)
    //   - mono 24-bit @ 44.1k, 1 s     => 1 frame (last == only)
    let cases: &[(u32, u16, u16, u32)] = &[
        // (total_samples, channels, bits_per_sample, sample_rate)
        (110_250, 1, 16, 44_100),
        (96_000, 2, 16, 48_000),
        (44_100, 1, 24, 44_100),
    ];
    for &(total_samples, channels, bits_per_sample, sample_rate) in cases {
        let samples = pseudo_noise(
            total_samples as usize,
            channels,
            (1i32 << (bits_per_sample - 1)) - 1,
            0xFEED_C0DE_u64.wrapping_mul(total_samples as u64),
        );
        let tta = encode(&samples, channels, bits_per_sample, sample_rate).expect("encode");
        let dec = crate::Decoder::new(&tta).expect("Decoder::new");

        let g = dec.header.frame_geometry_typed();
        let (bare_count, bare_last) = dec.header.frame_geometry();
        // Typed projection's accessors agree with the bare-tuple
        // return + the `regular_frame_samples` derivation.
        assert_eq!(g.frame_count(), bare_count);
        assert_eq!(g.last_frame_samples(), bare_last);
        assert_eq!(
            g.regular_frame_samples(),
            dec.header.regular_frame_samples()
        );
        // Decoder's actual frame table has the same length.
        assert_eq!(dec.frames.len() as u32, g.frame_count());
        // Total-samples back-derivation round-trips through the typed
        // projection to the source header field.
        assert_eq!(g.total_samples(), dec.header.total_samples);
        // On-disk seek-table size matches `spec/01` §4.2's closed form.
        assert_eq!(g.seek_table_size_bytes(), bare_count as usize * 4 + 4);
        // exact-multiple gate: `total_samples mod regular == 0`
        // iff `last == regular` (the spec §4.1 exact-multiple branch).
        let expected_exact = total_samples % dec.header.regular_frame_samples() == 0;
        assert_eq!(g.is_exact_multiple(), expected_exact);

        // Per-frame sample lookup agrees with the parsed
        // `FrameDescriptor::sample_count` on every frame.
        for (idx, fd) in dec.frames.iter().enumerate() {
            let want = g
                .frame_samples_at(idx as u32)
                .expect("frame_samples_at in range");
            assert_eq!(want, fd.sample_count);
        }
        // One past the last frame is None.
        assert_eq!(g.frame_samples_at(g.frame_count()), None);
    }
}

#[test]
fn typed_stream_header_matches_parsed_stream() {
    // End-to-end cross-API agreement: encode a real stream, parse it
    // via Decoder::new, and confirm the round-262 aggregate
    // `StreamHeader::typed()` view agrees with (a) the raw fields it
    // lifts, (b) the derived projections the decoder itself computes
    // (`total_duration`, frame geometry), and (c) the spec/01 §3.4
    // PCM-buffer product rule against the actual input PCM size.
    //
    // The same three-case parameter grid the round-246 / round-251 /
    // round-254 cross-checks use, so the aggregate view is pinned
    // against the same structurally-diverse shapes:
    //   - mono 16-bit @ 44.1k, 2.5 s   => 3 frames (regular, regular, last)
    //   - stereo 16-bit @ 48k, 2 s     => 2 frames (exact-multiple)
    //   - mono 24-bit @ 44.1k, 1 s     => 1 frame (last == only)
    let cases: &[(u32, u16, u16, u32)] = &[
        // (total_samples, channels, bits_per_sample, sample_rate)
        (110_250, 1, 16, 44_100),
        (96_000, 2, 16, 48_000),
        (44_100, 1, 24, 44_100),
    ];
    for &(total_samples, channels, bits_per_sample, sample_rate) in cases {
        let samples = pseudo_noise(
            total_samples as usize,
            channels,
            (1i32 << (bits_per_sample - 1)) - 1,
            0xA66E_06A7_u64.wrapping_mul(total_samples as u64),
        );
        let tta = encode(&samples, channels, bits_per_sample, sample_rate).expect("encode");
        let dec = crate::Decoder::new(&tta).expect("Decoder::new");
        let h = dec.header;
        let t = h.typed().expect("typed() on parsed header");

        // (a) Every typed field agrees with the raw field it lifts.
        assert_eq!(t.format().as_raw(), h.format);
        assert_eq!(t.channels().count(), h.channels);
        assert_eq!(t.bits_per_sample().bits(), h.bits_per_sample);
        assert_eq!(t.sample_rate().hz(), h.sample_rate);
        assert_eq!(t.total_samples().count(), h.total_samples);
        assert!(!t.requires_password(), "format=1 stream");

        // (b) Derived projections agree with the decoder's own
        // computations: duration and frame geometry.
        assert_eq!(t.total_duration(), dec.total_duration());
        let g = t.frame_geometry();
        assert_eq!(g, h.frame_geometry_typed());
        assert_eq!(g.frame_count() as usize, dec.frames.len());
        assert_eq!(t.regular_frame_samples(), h.regular_frame_samples());

        // (c) spec/01 §3.4 product rule: the PCM byte budget equals
        // the interleaved input length times the byte depth.
        assert_eq!(
            t.pcm_byte_len(),
            (samples.len() as u64) * (t.byte_depth() as u64)
        );

        // Lossless round-trip back to the raw on-wire data model.
        assert_eq!(t.to_header(), h);
    }
}

#[test]
fn seek_point_typed_accessors_match_parsed_stream() {
    // End-to-end cross-API agreement: walk a real multi-frame stream's
    // worth of seek points (one per (frame_index, in_frame_offset)
    // pair the Decoder produces from `seek_to_sample`) and confirm
    // the typed sub-field accessors lift the same numbers the raw
    // fields hold, with the spec's invariants honoured at every step.
    //
    // The same three-shape parameter grid the round-246 / round-251
    // tests cover, so the seek-point surface is pinned against
    // structurally-diverse frame geometries:
    //   - mono 16-bit @ 44.1k, 2.5 s   => 3 frames
    //   - stereo 16-bit @ 48k, 2 s     => 2 frames (exact-multiple)
    //   - mono 24-bit @ 44.1k, 1 s     => 1 frame
    let cases: &[(u32, u16, u16, u32)] = &[
        // (total_samples, channels, bits_per_sample, sample_rate)
        (110_250, 1, 16, 44_100),
        (96_000, 2, 16, 48_000),
        (44_100, 1, 24, 44_100),
    ];
    for &(total_samples, channels, bits_per_sample, sample_rate) in cases {
        let samples = pseudo_noise(
            total_samples as usize,
            channels,
            (1i32 << (bits_per_sample - 1)) - 1,
            0xBADC_0FFE_u64.wrapping_mul(total_samples as u64),
        );
        let tta = encode(&samples, channels, bits_per_sample, sample_rate).expect("encode");
        let dec = crate::Decoder::new(&tta).expect("Decoder::new");
        let frame_count = dec.frames.len();
        let regular = dec.header.regular_frame_samples();

        // Sample probes pin every interesting boundary: the first
        // sample, the last sample, exact frame boundaries, and an
        // in-frame offset for each frame the stream actually carries.
        let mut probes: Vec<u64> = vec![0, total_samples as u64 - 1];
        for f in 0..frame_count {
            probes.push(f as u64 * regular as u64); // frame boundary
            if total_samples as u64 > f as u64 * regular as u64 + 17 {
                probes.push(f as u64 * regular as u64 + 17); // mid-frame
            }
        }
        probes.sort();
        probes.dedup();

        for sample_index in probes {
            let sp = dec.seek_to_sample(sample_index).expect("seek_to_sample");

            // Typed frame_index round-trips against the parsed frame
            // count and reports `is_last` consistent with the seek
            // table's last-frame discrimination.
            let fi = sp
                .frame_index_typed(frame_count)
                .expect("frame_index_typed");
            assert_eq!(fi.index(), sp.frame_index);
            assert_eq!(fi.is_last(frame_count), sp.frame_index + 1 == frame_count);

            // Typed sample_offset round-trips against the regular
            // per-frame sample count derived per spec/01 §4.1.
            let off = sp
                .sample_offset_typed(regular)
                .expect("sample_offset_typed");
            assert_eq!(off.offset(), sp.sample_offset_in_frame);
            assert_eq!(off.is_frame_boundary(), sp.sample_offset_in_frame == 0);

            // Interleaved-skip projection agrees with the existing
            // raw arithmetic that `frame_iter_from_sample` uses
            // internally (offset * channels).
            assert_eq!(
                off.interleaved_skip(channels),
                (sp.sample_offset_in_frame as usize) * (channels as usize)
            );
        }

        // Ad-hoc out-of-window literals reject at lift time with the
        // documented variants.
        let bad_fi = crate::SeekPoint {
            frame_index: frame_count,
            sample_offset_in_frame: 0,
        };
        assert_eq!(
            bad_fi.frame_index_typed(frame_count),
            Err(crate::Error::InvalidFrameIndex(frame_count))
        );
        let bad_off = crate::SeekPoint {
            frame_index: 0,
            sample_offset_in_frame: regular,
        };
        assert_eq!(
            bad_off.sample_offset_typed(regular),
            Err(crate::Error::InvalidInFrameSampleOffset(regular))
        );
    }
}

// ---------------------------------------------------------------------
// Bit-depth edge cases: 17..=23 bps (spec/01 §3.2).
//
// `spec/01-bitstream-framing.md` §3.2 derives `byte_depth = (bps + 7)
// / 8`. Every bit depth in `17..=23` shares `byte_depth == 3` with the
// canonical 24-bit case and therefore drives the LMS `shift`/`round`
// table index 2 (`shift = 10`, per `tables/lms-shift.csv`), exactly as
// 24-bit does. The whole codec pipeline (LMS, Stage-B, Rice,
// decorrelation) operates on signed `i32` and is bit-depth agnostic
// past the byte-depth-keyed `shift`; only the on-disk `bits_per_sample`
// field and the PCM packing width change. These tests pin that the
// non-multiple-of-8 bit depths round-trip bit-exactly through
// `encode` → `decode`, closing a coverage gap left by the prior
// suite's 16-/24-only fixtures.
//
// Sample magnitudes are kept inside the signed range each bit depth
// can carry (`±2^(bps-1) − 1`) so the encoder's input is a faithful
// PCM stream for that width; the codec never clamps, so an in-range
// input is the meaningful round-trip target.

/// Largest positive magnitude representable in `bps`-bit signed PCM.
fn signed_amp(bps: u16) -> i32 {
    (1i32 << (bps - 1)) - 1
}

#[test]
fn roundtrip_mono_17bit_sine() {
    let n = (44_100.0 * 0.05) as usize;
    let samples = sine(n, 1, 44_100, 440.0, signed_amp(17) / 2);
    assert_roundtrip(&samples, 1, 17, 44_100);
}

#[test]
fn roundtrip_mono_20bit_sine() {
    let n = (44_100.0 * 0.05) as usize;
    let samples = sine(n, 1, 44_100, 440.0, signed_amp(20) / 2);
    assert_roundtrip(&samples, 1, 20, 44_100);
}

#[test]
fn roundtrip_mono_23bit_sine() {
    let n = (44_100.0 * 0.05) as usize;
    let samples = sine(n, 1, 44_100, 440.0, signed_amp(23) / 2);
    assert_roundtrip(&samples, 1, 23, 44_100);
}

#[test]
fn roundtrip_stereo_18bit_pseudo_noise() {
    // 18-bit (byte_depth 3) stereo noise exercises the truncating-`/2`
    // decorrelation discriminator (spec/04 §6) at a non-24 width.
    let samples = pseudo_noise(
        2_048,
        2,
        signed_amp(18) | (signed_amp(18) >> 4),
        0xABCD_1234,
    );
    assert_roundtrip(&samples, 2, 18, 44_100);
}

#[test]
fn roundtrip_stereo_21bit_uncorrelated_sines() {
    let n_per_ch = (44_100.0 * 0.05) as usize;
    let amp_l = signed_amp(21) / 2;
    let amp_r = signed_amp(21) / 3;
    let mut samples = Vec::with_capacity(n_per_ch * 2);
    for s in 0..n_per_ch {
        let phase_l = 2.0 * std::f64::consts::PI * 440.0 * s as f64 / 44_100.0;
        let phase_r = 2.0 * std::f64::consts::PI * 660.0 * s as f64 / 44_100.0;
        samples.push((phase_l.sin() * amp_l as f64).round() as i32);
        samples.push((phase_r.sin() * amp_r as f64).round() as i32);
    }
    assert_roundtrip(&samples, 2, 21, 44_100);
}

#[test]
fn roundtrip_multi_frame_mono_19bit_44100() {
    // 2.5 s spans 3 frames at a 19-bit (byte_depth 3) width — pins the
    // per-frame state-reset discipline at a non-multiple-of-8 depth.
    let n = 110_250;
    let samples = sine(n, 1, 44_100, 440.0, signed_amp(19) / 2);
    assert_roundtrip(&samples, 1, 19, 44_100);
}

#[test]
fn roundtrip_full_scale_22bit_dc_and_impulse() {
    // Drive the predictor with a near-full-scale DC level plus a single
    // full-scale impulse so the residual swings the full 22-bit range,
    // confirming no overflow in the byte_depth-3 packing path.
    let amp = signed_amp(22);
    let mut samples = vec![amp / 4; 1024];
    samples[256] = amp;
    samples[768] = -amp;
    assert_roundtrip(&samples, 1, 22, 44_100);
}

// ---------------------------------------------------------------------
// Odd / intermediate channel-count cascade (spec/04 §4.3).
//
// `spec/04-decorrelation.md` §4.3 states there is **no special case**
// for odd channel counts: the inverse cascade is "a single chain walk
// with one anchor at `N-1`" for every `N >= 2`, and anti-pattern §9.4
// flags any parity-conditional code path as a divergence source. The
// prior suite covered `nch ∈ {1, 2, 6}` but never `{3, 4, 5}`, leaving
// the odd-N (`3`, `5`) and the even-but-non-6 (`4`) cascade walks
// unexercised. These round-trips drive each intermediate channel count
// with independent per-channel content (so the forward differences are
// non-trivial on every channel pair), closing that gap.

/// Build an `nch`-channel interleaved PCM buffer where each channel
/// carries an independent sine of a distinct frequency/amplitude, so
/// the decorrelation forward differences are non-zero on every pair.
fn multi_sine(n_per_ch: usize, nch: u16, bps: u16) -> Vec<i32> {
    let base_amp = signed_amp(bps) / 3;
    let mut out = Vec::with_capacity(n_per_ch * nch as usize);
    for s in 0..n_per_ch {
        for ch in 0..nch {
            let freq = 200.0 * (1.0 + 0.17 * ch as f64);
            let phase = 2.0 * std::f64::consts::PI * freq * s as f64 / 44_100.0;
            let amp = base_amp as f64 * (1.0 - 0.08 * ch as f64);
            out.push((phase.sin() * amp).round() as i32);
        }
    }
    out
}

#[test]
fn roundtrip_three_channel_16bit() {
    // N=3: the canonical odd-N cascade (anchor at ch2, walk to ch0).
    let samples = multi_sine(1_024, 3, 16);
    assert_roundtrip(&samples, 3, 16, 44_100);
}

#[test]
fn roundtrip_four_channel_16bit() {
    // N=4: even but not 6 — two intermediate forward-difference channels.
    let samples = multi_sine(1_024, 4, 16);
    assert_roundtrip(&samples, 4, 16, 44_100);
}

#[test]
fn roundtrip_five_channel_16bit() {
    // N=5: the other odd-N cascade.
    let samples = multi_sine(1_024, 5, 16);
    assert_roundtrip(&samples, 5, 16, 44_100);
}

#[test]
fn roundtrip_three_channel_24bit_pseudo_noise() {
    // N=3 at 24-bit with uncorrelated per-channel noise exercises the
    // odd-N cascade together with the truncating-`/2` sign discipline
    // (spec/04 §6) on a wide dynamic range. Distinct seeds per channel
    // keep the forward differences large and signed.
    let n_per_ch = 1_536;
    let mut samples = Vec::with_capacity(n_per_ch * 3);
    let mask = signed_amp(24);
    let per_ch: Vec<Vec<i32>> = (0..3u64)
        .map(|c| pseudo_noise(n_per_ch, 1, mask, 0x5EED_0000 + c))
        .collect();
    for s in 0..n_per_ch {
        for ch in &per_ch {
            samples.push(ch[s]);
        }
    }
    assert_roundtrip(&samples, 3, 24, 44_100);
}

#[test]
fn roundtrip_five_channel_multi_frame_44100() {
    // N=5 across 3 frames pins the odd-N cascade together with the
    // per-frame predictor/Rice reset for an intermediate channel count.
    let samples = multi_sine(110_250, 5, 16);
    assert_roundtrip(&samples, 5, 16, 44_100);
}

// ---------------------------------------------------------------------
// Encoder-produced seek table is valid for the decoder's random-access
// API across the new shapes (milestone: drive encode + seek toward
// decode parity).
//
// The encoder writes one seek-table entry per frame (`spec/01` §4.2,
// each entry = on-disk frame size including the trailing CRC). These
// tests confirm the entries the *encoder* produces let the *decoder*'s
// `decode_frame_at` / `seek_to_sample` / `frame_iter` random-access
// paths land bit-exactly, for the intermediate channel counts and
// non-multiple-of-8 bit depths added above — i.e. the seek table is
// correct for shapes the prior seek suite (mono/stereo at 16/24) never
// covered.

/// Assert that every random-access decode path agrees with the eager
/// `decode_all` for an encoder-produced multi-frame stream.
#[track_caller]
fn assert_random_access_parity(samples: &[i32], channels: u16, bps: u16, sample_rate: u32) {
    let tta = encode(samples, channels, bps, sample_rate).expect("encode should succeed");
    let dec = crate::Decoder::new(&tta).expect("Decoder::new");
    let eager = dec.decode_all().expect("decode_all");
    assert_eq!(eager, samples, "eager decode must match the encoder input");

    let n_frames = dec.frames.len();
    assert!(n_frames >= 2, "fixture must be multi-frame for this test");

    // frame_iter concatenation equals eager.
    let mut streamed = Vec::new();
    for r in dec.frame_iter() {
        streamed.extend_from_slice(&r.expect("frame_iter decode"));
    }
    assert_eq!(streamed, eager, "frame_iter must equal decode_all");

    // decode_frame_at on every frame, concatenated, equals eager.
    let mut by_index = Vec::new();
    for i in 0..n_frames {
        by_index.extend_from_slice(&dec.decode_frame_at(i).expect("decode_frame_at"));
    }
    assert_eq!(
        by_index, eager,
        "decode_frame_at concat must equal decode_all"
    );

    // seek_to_sample at a few interior sample positions lands on a frame
    // whose decoded PCM is the matching slice of the eager output.
    let nch = channels as usize;
    let regular = ((sample_rate as u64) * 256 / 245) as usize;
    let total = samples.len() / nch;
    for &target in &[0usize, regular, regular + 1, regular * 2, total - 1] {
        if target >= total {
            continue;
        }
        let sp = dec.seek_to_sample(target as u64).expect("seek_to_sample");
        let frame_start = sp.frame_index * regular;
        let frame_pcm = dec
            .decode_frame_at(sp.frame_index)
            .expect("decode_frame_at after seek");
        // The frame containing `target` must reproduce the eager slice.
        let expected = &eager[frame_start * nch..frame_start * nch + frame_pcm.len()];
        assert_eq!(
            frame_pcm, expected,
            "seek_to_sample({target}) frame {} mismatch",
            sp.frame_index
        );
        // And the target sample sits inside this frame.
        assert!(
            (sp.sample_offset_in_frame as usize) < frame_pcm.len() / nch,
            "target {target} offset out of frame bounds"
        );
    }
}

#[test]
fn encoder_seek_table_random_access_three_channel_multi_frame() {
    let samples = multi_sine(110_250, 3, 16);
    assert_random_access_parity(&samples, 3, 16, 44_100);
}

#[test]
fn encoder_seek_table_random_access_19bit_mono_multi_frame() {
    let n = 110_250;
    let samples = sine(n, 1, 44_100, 440.0, signed_amp(19) / 2);
    assert_random_access_parity(&samples, 1, 19, 44_100);
}

#[test]
fn encoder_seek_table_random_access_five_channel_multi_frame() {
    let samples = multi_sine(110_250, 5, 16);
    assert_random_access_parity(&samples, 5, 16, 44_100);
}
