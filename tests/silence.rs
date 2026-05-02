//! Round-trip the all-zero PCM signal through ffmpeg's TTA encoder
//! and assert bit-exact recovery. This test is the load-bearing
//! "decoder works" check: silence is the simplest input on which the
//! Rice coder's adaptation, the predictor cascade, and the per-frame
//! plumbing all participate and have well-defined fixed points.

use std::process::{Command, Stdio};

use oxideav_core::SampleFormat;
use oxideav_tta::container::parse_file;
use oxideav_tta::decoder::decode_with_sample_count;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn mono16_silence_lossless() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not on PATH; skipping silence roundtrip");
        return;
    }
    let wav = std::env::temp_dir().join(format!("oxav-tta-silence-{}.wav", std::process::id()));
    let tta = std::env::temp_dir().join(format!("oxav-tta-silence-{}.tta", std::process::id()));
    let _ = std::fs::remove_file(&wav);
    let _ = std::fs::remove_file(&tta);

    // 0.5 s of digital silence at 44.1 kHz / 16-bit mono.
    let s1 = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=mono",
            "-t",
            "0.5",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&wav)
        .status()
        .expect("ffmpeg WAV gen");
    assert!(s1.success());
    let s2 = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&wav)
        .args(["-c:a", "tta"])
        .arg(&tta)
        .status()
        .expect("ffmpeg TTA encode");
    assert!(s2.success());

    let wav_b = std::fs::read(&wav).expect("read wav");
    let tta_b = std::fs::read(&tta).expect("read tta");
    let _ = std::fs::remove_file(&wav);
    let _ = std::fs::remove_file(&tta);

    // Pull the WAV's raw PCM block.
    let mut pcm = vec![];
    let mut i = 12;
    while i + 8 <= wav_b.len() {
        let sz =
            u32::from_le_bytes([wav_b[i + 4], wav_b[i + 5], wav_b[i + 6], wav_b[i + 7]]) as usize;
        if &wav_b[i..i + 4] == b"data" {
            pcm = wav_b[i + 8..i + 8 + sz].to_vec();
            break;
        }
        i += 8 + sz + (sz & 1);
    }
    assert!(!pcm.is_empty());
    // Source is digital silence — every WAV PCM byte is zero.
    assert!(pcm.iter().all(|&b| b == 0), "expected all-zero source PCM");

    // Decode with this crate.
    let parsed = parse_file(&tta_b).expect("parse TTA");
    let total_frames = parsed.frames.len();
    let full = parsed.header.frame_size() as usize;
    let last = parsed.header.last_frame_size() as usize;
    let mut decoded = vec![];
    for (idx, fr) in parsed.frames.iter().enumerate() {
        let body_full = &tta_b[fr.offset..fr.offset + fr.size];
        let body = &body_full[..body_full.len() - 4];
        let count = if idx + 1 == total_frames { last } else { full };
        let chans = decode_with_sample_count(body, &parsed.header, count)
            .unwrap_or_else(|e| panic!("frame {idx} decode: {e:?}"));
        for s in &chans[0] {
            let _ = SampleFormat::S16;
            decoded.extend_from_slice(&(*s as i16).to_le_bytes());
        }
    }
    assert_eq!(decoded.len(), pcm.len(), "PCM length");
    assert_eq!(decoded, pcm, "decoded silence is bit-exact");
}
