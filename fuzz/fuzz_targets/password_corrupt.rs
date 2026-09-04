#![no_main]

//! Drive **format=2 (password-protected) streams under the wrong
//! password, the right password, no password, and post-header
//! corruption** through every eager and streaming decode entry point.
//!
//! ## Why a separate target
//!
//! `password_streaming` feeds *raw* fuzz bytes to
//! `Decoder::new_with_password` — the header CRC rejects essentially
//! all of them, so the format=2 machinery past the header (the
//! per-frame `qm` re-prime of `spec/07` §3.5–§3.6 feeding Stage-A, the
//! Rice / LMS / Stage-B cascade running on *mis-primed* state) is
//! rarely reached. `corrupt_decode` reaches the deep decoder but only
//! for format=1 streams and only through the passwordless API. This
//! target starts from a valid **encoder-produced format=2 stream** and
//! explores the three format=2-specific situations:
//!
//! 1. **Wrong password.** The trailing per-frame CRC (`spec/01` §5.4)
//!    covers the *entropy-coded bytes*, not the PCM, so a wrong
//!    password decodes "successfully" to the wrong samples — every
//!    frame's Stage-A starts from a foreign `qm` vector and the LMS /
//!    Stage-B arithmetic runs over values the encoder never produced
//!    (wrapping territory). Contract: no panic, `Ok` output of the
//!    documented length, and eager == streaming (both paths use the
//!    same wrong priming, so they must agree bit-exactly).
//! 2. **Right password.** Eager, `frame_iter`, `decode_frame_at`,
//!    `decode_from_sample` all reproduce the encoder's input PCM
//!    bit-exactly (the round-trip pin under fuzz-chosen passwords,
//!    including the empty password of `spec/07` §9 item 2).
//! 3. **No password.** The passwordless `decode` / `Decoder::new` must
//!    refuse an intact format=2 header with `Error::PasswordRequired`
//!    (typed gate pin).
//!
//! Then the region past the 22-byte header is XOR-mutated from the
//! script (seek table, frame bodies, trailers) and 1–3 are repeated
//! with the panic-free contract only (a corrupted stream may reject
//! or decode to arbitrary samples; eager and streaming must still
//! agree whenever both succeed).
//!
//! ## Fuzz input layout
//!
//! ```text
//!   byte 0      : channels seed       → channels = (b0 % 6) + 1
//!   byte 1      : bit-depth selector  → bps = 16 + (b1 % 9), in 16..=24
//!   bytes 2-3   : sample_rate seed    → LE u16, masked to 64..=0x7FF
//!   bytes 4-5   : sample-count seed   → LE u16 per-channel samples,
//!                 capped at MAX_SAMPLES_PER_CHANNEL
//!   byte 6      : right-password length 0..=8 (`% 9`)
//!   bytes 7-14  : right-password bytes (first `len` used)
//!   byte 15     : wrong-password length 0..=8 (`% 9`)
//!   bytes 16-23 : wrong-password bytes (first `len` used)
//!   bytes 24..  : split in half — PCM seed, then the mutation script
//!                 of (LE u16 offset, u8 xor-mask) records applied to
//!                 the post-header region.
//! ```

use libfuzzer_sys::fuzz_target;

use oxideav_tta::{decode, decode_with_password, Decoder, Error};

const MAX_SAMPLES_PER_CHANNEL: usize = 6000;
const HEADER_LEN: usize = 22;
const MAX_FRAMES_FOR_STREAMING: usize = 4096;

fn build_pcm(seed: &[u8], slot_count: usize, byte_depth: usize) -> Vec<i32> {
    let mut samples = Vec::with_capacity(slot_count);
    for i in 0..slot_count {
        let s = if seed.is_empty() {
            ((i as i32) % 29) - 14
        } else {
            let base = (i * byte_depth) % seed.len();
            let b0 = seed[base] as u32;
            let b1 = seed[(base + 1) % seed.len()] as u32;
            if byte_depth == 2 {
                i16::from_le_bytes([b0 as u8, b1 as u8]) as i32
            } else {
                let b2 = seed[(base + 2) % seed.len()] as u32;
                let raw = b0 | (b1 << 8) | (b2 << 16);
                if raw & 0x0080_0000 != 0 {
                    (raw | 0xFF00_0000) as i32
                } else {
                    raw as i32
                }
            }
        };
        samples.push(s);
    }
    samples
}

/// Eager-vs-streaming differential under one password. Returns the
/// eager PCM when it decoded.
fn drive_password(bytes: &[u8], password: &[u8], probe: u8) -> Option<Vec<i32>> {
    let eager = decode_with_password(bytes, password)
        .ok()
        .map(|(_, pcm)| pcm);

    let Ok(dec) = Decoder::new_with_password(bytes, password) else {
        // The eager path parses the same header + seek table; the two
        // constructors must agree on acceptance.
        assert!(
            eager.is_none(),
            "eager accepted what the streaming constructor refused"
        );
        return None;
    };
    let nch = dec.header.channels as usize;
    let fc = dec.frames.len();
    let total = dec.total_samples() as u64;

    let all = dec.decode_all().ok();
    assert_eq!(
        all, eager,
        "decode_all must agree with decode_with_password"
    );

    if fc > 0 && fc <= MAX_FRAMES_FOR_STREAMING {
        let mut lazy = Vec::new();
        let mut lazy_ok = true;
        for r in dec.frame_iter() {
            match r {
                Ok(pcm) => lazy.extend_from_slice(&pcm),
                Err(_) => {
                    lazy_ok = false;
                    break;
                }
            }
        }
        if let Some(e) = &eager {
            assert!(lazy_ok, "frame_iter failed where the eager path succeeded");
            assert_eq!(
                &lazy, e,
                "frame_iter concatenation must equal the eager output"
            );
        }

        let fi = (probe as usize) % fc;
        match dec.decode_frame_at(fi) {
            Ok(pcm) => {
                assert_eq!(pcm.len(), dec.frames[fi].sample_count as usize * nch);
                if let Some(e) = &eager {
                    let start = dec.frames[..fi]
                        .iter()
                        .map(|f| f.sample_count as usize * nch)
                        .sum::<usize>();
                    assert_eq!(&e[start..start + pcm.len()], &pcm[..]);
                }
            }
            Err(_) => assert!(
                eager.is_none(),
                "random access failed on an eagerly-decodable stream"
            ),
        }

        if total > 0 && dec.is_seekable() {
            let si = (probe as u64 * 613) % total;
            let p = dec.seek_to_sample(si).expect("seekable stream seeks");
            assert!(p.frame_index < fc);
            if let (Some(e), Ok(suffix)) = (&eager, dec.decode_from_sample(si)) {
                assert_eq!(&suffix[..], &e[si as usize * nch..]);
            }
        }
    }
    eager
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 24 {
        return;
    }

    let channels = ((data[0] as u16) % 6) + 1;
    let bits_per_sample: u16 = 16 + (data[1] % 9) as u16;
    let sample_rate = (u16::from_le_bytes([data[2], data[3]]) as u32 & 0x7FF).max(64);
    let samples_per_channel =
        (u16::from_le_bytes([data[4], data[5]]) as usize).min(MAX_SAMPLES_PER_CHANNEL);
    let right_len = (data[6] % 9) as usize;
    let right_pw = &data[7..7 + right_len];
    let wrong_len = (data[15] % 9) as usize;
    let wrong_pw = &data[16..16 + wrong_len];
    if samples_per_channel == 0 {
        return;
    }
    let nch = channels as usize;
    let byte_depth = (bits_per_sample as usize).div_ceil(8);

    let tail = &data[24..];
    let split = tail.len() / 2;
    let pcm_seed = &tail[..split];
    let mut_script = &tail[split..];

    let pcm = build_pcm(pcm_seed, samples_per_channel * nch, byte_depth);
    let Ok(mut bytes) =
        oxideav_tta::encode_with_password(&pcm, channels, bits_per_sample, sample_rate, right_pw)
    else {
        return;
    };
    let probe = data[6] ^ data[15];

    // ── Intact stream ──────────────────────────────────────────────
    // 3. No password: the typed gate.
    assert!(matches!(decode(&bytes), Err(Error::PasswordRequired)));
    assert!(matches!(Decoder::new(&bytes), Err(Error::PasswordRequired)));

    // 2. Right password: bit-exact round trip on every path.
    let right = drive_password(&bytes, right_pw, probe).expect("right password decodes");
    assert_eq!(
        right, pcm,
        "right password must reproduce the encoder input"
    );

    // 1. Wrong password: same shape, same eager/streaming agreement.
    // (A "wrong" password that happens to hash to the same digest —
    // e.g. both empty — simply reproduces the input; that is fine.)
    let wrong = drive_password(&bytes, wrong_pw, probe).expect("wrong password still decodes");
    assert_eq!(
        wrong.len(),
        pcm.len(),
        "wrong-password output keeps the stream length"
    );

    // ── Corrupted stream (header intact) ───────────────────────────
    if bytes.len() <= HEADER_LEN {
        return;
    }
    let body = &mut bytes[HEADER_LEN..];
    for rec in mut_script.as_chunks::<3>().0 {
        let off = (u16::from_le_bytes([rec[0], rec[1]]) as usize) % body.len();
        body[off] ^= rec[2];
    }
    assert!(matches!(decode(&bytes), Err(Error::PasswordRequired)));
    let _ = drive_password(&bytes, right_pw, probe);
    let _ = drive_password(&bytes, wrong_pw, probe);
    let _ = oxideav_tta::scan_trailers(&bytes);
});
