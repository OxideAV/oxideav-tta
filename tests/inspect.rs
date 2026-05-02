//! Hand inspection of an ffmpeg-encoded sample TTA stream. Always
//! prints the first 16 decoded samples and their WAV-source counterparts
//! when ffmpeg is available; the assertions only fire for the first
//! sample (which must be exactly recovered with all-zero predictor
//! state).

use std::process::{Command, Stdio};

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
fn inspect_mono16_first_samples() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not on PATH; skipping inspection");
        return;
    }
    let wav = std::env::temp_dir().join(format!("oxav-tta-inspect-{}.wav", std::process::id()));
    let tta = std::env::temp_dir().join(format!("oxav-tta-inspect-{}.tta", std::process::id()));
    let _ = std::fs::remove_file(&wav);
    let _ = std::fs::remove_file(&tta);
    let s1 = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=0.01:beep_factor=0",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&wav)
        .status()
        .unwrap();
    assert!(s1.success());
    let s2 = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&wav)
        .args(["-c:a", "tta"])
        .arg(&tta)
        .status()
        .unwrap();
    assert!(s2.success());

    let wav_b = std::fs::read(&wav).unwrap();
    let tta_b = std::fs::read(&tta).unwrap();
    let _ = std::fs::remove_file(&wav);
    let _ = std::fs::remove_file(&tta);

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

    let parsed = oxideav_tta::container::parse_file(&tta_b).unwrap();
    let fr0 = &parsed.frames[0];
    let body_full = &tta_b[fr0.offset..fr0.offset + fr0.size];
    let body = &body_full[..body_full.len() - 4];
    let chans = oxideav_tta::decoder::decode_with_sample_count(
        body,
        &parsed.header,
        parsed.header.last_frame_size() as usize,
    )
    .unwrap();

    eprintln!("num decoded samples: {}", chans[0].len());
    for k in 0..32.min(chans[0].len()) {
        let dec = chans[0][k];
        let exp = i16::from_le_bytes([pcm[2 * k], pcm[2 * k + 1]]) as i32;
        eprintln!(
            "[{k}] decoded={dec:>6}  expected={exp:>6}  diff={:>6}",
            dec - exp
        );
    }

    // First sample should always match (all-zero state).
    let exp0 = i16::from_le_bytes([pcm[0], pcm[1]]) as i32;
    assert_eq!(chans[0][0], exp0, "sample 0 must match");
}
