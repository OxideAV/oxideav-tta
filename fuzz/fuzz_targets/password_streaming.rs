#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the **format=2
//! (password-protected) streaming + random-access decode surface** on
//! [`oxideav_tta::Decoder`], constructed via
//! [`Decoder::new_with_password`].
//!
//! The existing `decode` target hammers the eager single-shot
//! [`oxideav_tta::decode`] / [`oxideav_tta::decode_with_password`]
//! entry points, and the round-190 `streaming_decode` target hammers
//! the lazy streaming surface — but only through [`Decoder::new`],
//! which is the **format=1** constructor (`spec/02` §3.1 zero-init qm
//! priming). The format=2 streaming path
//! ([`Decoder::new_with_password`] + the streaming / random-access
//! battery) re-primes Stage-A's `qm[0..7]` from a CRC-64 digest of the
//! password at *every* per-channel frame init (`spec/07` §3.5–§3.6).
//! That re-prime is a fresh attacker surface: an attacker-chosen
//! password paired with an attacker-chosen byte stream and an
//! attacker-chosen frame / sample index still has to surface every
//! malformed-input case as a typed [`oxideav_tta::Error`] — never a
//! panic / index-out-of-bounds / integer overflow / unbounded
//! allocation — AND the eager and streaming password paths must agree
//! bit-exactly on the recovered PCM.
//!
//! ## What this target pins that the others do not
//!
//! 1. **`Decoder::new_with_password` framing.** The format-gate lift
//!    (`spec/01` §3 `format`-field validation with the
//!    `PasswordRequired` gate dropped because a password is supplied),
//!    the format=1-with-password `clear_priming` tolerance
//!    (audit/07 §6.2-2), and the format=2 seek-table parse all run
//!    against attacker bytes.
//! 2. **Eager-vs-streaming agreement for format=2.** Whenever the
//!    eager [`oxideav_tta::decode_with_password`] succeeds, the
//!    password-aware [`Decoder::frame_iter`] concatenation must equal
//!    it bit-exactly. This is the `spec/07` §3.6 "re-prime qm[] at
//!    every frame init" rule observed through the lazy path: if the
//!    streaming constructor failed to thread the priming through to
//!    every frame's Stage-A reset, the two paths would diverge on any
//!    multi-frame format=2 stream.
//! 3. **Random-access agreement for format=2.**
//!    [`Decoder::decode_frame_at`] and [`Decoder::frame_iter_from`]
//!    against attacker-chosen indices must match the eager output's
//!    corresponding slice / suffix — pinning that a single
//!    seek-table-addressed frame is primed identically whether reached
//!    by sequential walk or random access.
//! 4. **`seek_to_sample` arithmetic under format=2 geometry.** The
//!    returned `SeekPoint` must stay in range for the password stream's
//!    seek table exactly as for format=1.
//!
//! ## Fuzz input layout
//!
//! ```text
//!   byte 0      : password length `pw_len` in 0..=8 (folded via
//!                 `% 9`); a `pw_len == 0` empty password drives the
//!                 `spec/07` §9 item 2 all-zero-digest edge.
//!   bytes 1..9  : the 8 candidate password bytes; the first `pw_len`
//!                 are taken as the password.
//!   byte 9      : seed for `target_frame_index` (folded into
//!                 `frames.len()`).
//!   bytes 10..18: seed for `target_sample_index` (LE u64, folded into
//!                 `total_samples`).
//!   byte 18     : seed for `start_index` for `frame_iter_from`
//!                 (folded into `frames.len() + 1` so the past-end
//!                 empty-iterator branch is driven).
//!   bytes 19..  : the TTA1 byte stream proper. Feeds
//!                 `Decoder::new_with_password` and the full
//!                 password-aware streaming battery.
//! ```
//!
//! Inputs shorter than 19 bytes return immediately: there isn't enough
//! header room for a TTA1 stream after the seed prefix, and the tiny-
//! input region is already covered by the `decode` target.
//!
//! ## Cap on decoded sample volume
//!
//! As with `streaming_decode`, the cross-API bit-exact assertions are
//! gated on the frame count staying below a small cap so the fuzzer
//! doesn't spend its per-iteration budget allocating O(GiB) sample
//! buffers when a malformed `total_samples` slips past the cheap
//! `Truncated` rejection. The streaming-API panic-free contract is
//! still exercised on every input regardless of the cap.

use libfuzzer_sys::fuzz_target;

use oxideav_tta::Decoder;

/// Skip the cross-API bit-exact agreement checks above this frame
/// count. The panic-free contract is still exercised unconditionally;
/// this cap only gates the `decode_with_password() == concat(...)`
/// assertions.
const MAX_FRAMES_FOR_CROSSCHECK: usize = 4096;

fuzz_target!(|data: &[u8]| {
    if data.len() < 19 {
        return;
    }

    // ── Seed extraction ───────────────────────────────────────────
    let pw_len = (data[0] as usize) % 9; // 0..=8
    let password = &data[1..1 + pw_len];
    let target_frame_seed = data[9] as usize;
    let target_sample_seed = u64::from_le_bytes([
        data[10], data[11], data[12], data[13], data[14], data[15], data[16], data[17],
    ]);
    let start_index_seed = data[18] as usize;
    let tta_bytes = &data[19..];

    // ── 1. Construct the password-aware Decoder. Malformed framing
    //       returns Err and the streaming surface isn't reachable —
    //       the panic-free contract there is satisfied by returning. ─
    let dec = match Decoder::new_with_password(tta_bytes, password) {
        Ok(d) => d,
        Err(_) => return,
    };

    let frame_count = dec.frames.len();
    let total_samples = dec.total_samples();

    // ── 2. Eager decode_with_password vs lazy frame_iter agreement ─
    //
    // The eager reference is the public `decode_with_password`, which
    // exercises the same priming derivation independently of the
    // Decoder we hold. Both must agree on success AND on rejection.
    let eager = oxideav_tta::decode_with_password(tta_bytes, password).map(|(_, pcm)| pcm);
    let lazy_concat: Result<Vec<i32>, _> = dec
        .frame_iter()
        .collect::<Result<Vec<Vec<i32>>, _>>()
        .map(|frames| frames.into_iter().flatten().collect());

    if frame_count <= MAX_FRAMES_FOR_CROSSCHECK {
        match (&eager, &lazy_concat) {
            (Ok(eager_samples), Ok(lazy_samples)) => {
                assert_eq!(
                    eager_samples,
                    lazy_samples,
                    "decode_with_password() vs frame_iter() PCM disagreement: \
                     channels={} bps={} format={} frames={} pw_len={}",
                    dec.header.channels,
                    dec.header.bits_per_sample,
                    dec.header.format,
                    frame_count,
                    pw_len,
                );
            }
            (Ok(_), Err(_)) => panic!(
                "decode_with_password succeeded but frame_iter failed: \
                 channels={} bps={} format={} frames={} pw_len={}",
                dec.header.channels,
                dec.header.bits_per_sample,
                dec.header.format,
                frame_count,
                pw_len,
            ),
            (Err(_), Ok(_)) => panic!(
                "frame_iter succeeded but decode_with_password failed: \
                 channels={} bps={} format={} frames={} pw_len={}",
                dec.header.channels,
                dec.header.bits_per_sample,
                dec.header.format,
                frame_count,
                pw_len,
            ),
            (Err(_), Err(_)) => {
                // Both password paths agree on rejection — admissible.
            }
        }
    }

    // ── 3. decode_frame_at(target_frame_index) agreement ──────────
    if frame_count > 0 {
        let target_frame_index = target_frame_seed % frame_count;
        let frame_pcm = dec.decode_frame_at(target_frame_index);

        if frame_count <= MAX_FRAMES_FOR_CROSSCHECK {
            if let (Ok(frame_samples), Ok(eager_samples)) = (&frame_pcm, &eager) {
                let nch = dec.header.channels as usize;
                let preceding: usize = dec.frames[..target_frame_index]
                    .iter()
                    .map(|f| f.sample_count as usize)
                    .sum();
                let this_frame_samples = dec.frames[target_frame_index].sample_count as usize;
                let start = preceding * nch;
                let end = start + this_frame_samples * nch;
                // Defensive bound: a corrupt seek table whose geometry
                // disagrees with the eager output must not surface as a
                // fuzz crash — the eager-vs-lazy cross-check above
                // already pins the canonical PCM.
                if end <= eager_samples.len() {
                    assert_eq!(
                        frame_samples,
                        &eager_samples[start..end],
                        "decode_frame_at({target_frame_index}) PCM disagrees with eager slice: \
                         channels={} bps={} frames={frame_count} pw_len={pw_len}",
                        dec.header.channels,
                        dec.header.bits_per_sample,
                    );
                }
            }
        }
    } else {
        // Empty stream still exercises the OOB branch panic-free.
        let _ = dec.decode_frame_at(0);
    }

    // ── 4. seek_to_sample(target_sample_index) agreement ──────────
    let target_sample_index = if total_samples == 0 {
        target_sample_seed
    } else {
        target_sample_seed % (total_samples as u64)
    };
    match dec.seek_to_sample(target_sample_index) {
        Ok(sp) => {
            assert!(
                sp.frame_index < frame_count,
                "seek_to_sample({target_sample_index}) returned frame_index={} \
                 with frames.len()={}",
                sp.frame_index,
                frame_count,
            );
            let this_frame_samples = dec.frames[sp.frame_index].sample_count;
            assert!(
                sp.sample_offset_in_frame < this_frame_samples,
                "seek_to_sample({target_sample_index}) returned offset {} \
                 >= frame {sample_count} for frame {}",
                sp.sample_offset_in_frame,
                sp.frame_index,
                sample_count = this_frame_samples,
            );
        }
        Err(_) => {
            // Typed rejection (SampleIndexOutOfRange or similar) is
            // contractually correct.
        }
    }

    // ── 5. frame_iter_from(start_index) agreement ─────────────────
    let start_index = start_index_seed % (frame_count + 1);
    let from_concat: Result<Vec<i32>, _> = dec
        .frame_iter_from(start_index)
        .collect::<Result<Vec<Vec<i32>>, _>>()
        .map(|frames| frames.into_iter().flatten().collect());

    if frame_count <= MAX_FRAMES_FOR_CROSSCHECK {
        if let (Ok(from_samples), Ok(eager_samples)) = (&from_concat, &eager) {
            let nch = dec.header.channels as usize;
            let preceding: usize = dec.frames[..start_index]
                .iter()
                .map(|f| f.sample_count as usize)
                .sum();
            let start = preceding * nch;
            if start <= eager_samples.len() {
                assert_eq!(
                    from_samples,
                    &eager_samples[start..],
                    "frame_iter_from({start_index}) suffix disagrees with eager: \
                     channels={} bps={} frames={frame_count} pw_len={pw_len}",
                    dec.header.channels,
                    dec.header.bits_per_sample,
                );
            }
        }
    }
});
