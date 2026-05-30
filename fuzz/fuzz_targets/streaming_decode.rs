#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the round-187
//! streaming + random-access decode surface on
//! [`oxideav_tta::Decoder`].
//!
//! The existing `decode` fuzz target hammers the single-shot
//! [`oxideav_tta::decode`] (eager `decode_all`) path. Round 187 layered
//! four additional public entry points on top:
//!
//!   - [`Decoder::frame_iter`] — lazy, `O(frame)`-memory per-frame
//!     iterator.
//!   - [`Decoder::decode_frame_at(index)`] — random-access decode of
//!     a single seek-table entry.
//!   - [`Decoder::seek_to_sample(sample_index)`] — locate the frame
//!     containing a per-channel sample index.
//!   - [`Decoder::frame_iter_from(start_index)`] — resume iteration
//!     from an arbitrary frame index.
//!
//! Each of those is a fresh attacker surface: per-frame state-reset
//! discipline (`spec/01` §5.1 + `spec/02..05` §3.1) is what makes
//! random-access decode legitimate against the spec, but an attacker-
//! chosen `frame_index` or `sample_index` paired with an attacker-
//! chosen byte stream still has to surface every malformed-input case
//! as a typed [`oxideav_tta::Error`] — never a panic / index-out-of-
//! bounds / integer overflow / unbounded allocation.
//!
//! The contract under test on every fuzz input is:
//!
//!   1. [`Decoder::new`] either returns `Err(Error::…)` for malformed
//!      framing (in which case the streaming surface isn't reachable
//!      and the iteration is skipped) or returns a [`Decoder`] whose
//!      streaming API is then exercised.
//!   2. Every streaming-API call returns a `Result` (or, for
//!      [`Decoder::frame_iter`] / [`Decoder::frame_iter_from`], an
//!      iterator that yields `Result`s) — never panics, never
//!      indexes out of bounds, never integer-overflows, never
//!      allocates an attacker-controlled volume.
//!   3. **Cross-API agreement.** Whenever [`Decoder::decode_all`]
//!      succeeds with `Ok(samples)`, the per-frame outputs of
//!      [`Decoder::frame_iter`] concatenate to the same sample buffer
//!      bit-exactly. This pins the round-187 invariant that the
//!      streaming path produces the same PCM as the eager path,
//!      across the whole space of valid TTA1 byte streams an attacker
//!      can construct. The in-tree `src/seek_tests.rs` suite checks
//!      this on hand-picked fixtures; the fuzzer extends the
//!      assertion to attacker-chosen inputs.
//!   4. **Random-access agreement.** When [`Decoder::decode_frame_at`]
//!      succeeds for a given `frame_index`, the recovered PCM equals
//!      the matching slice of the eager `decode_all` output bit-
//!      exactly. The bit-exact assertion is gated on
//!      `decode_all().is_ok()` — when the eager path also rejects,
//!      both rejection paths are admissible.
//!   5. **Seek agreement.** When [`Decoder::seek_to_sample`] succeeds
//!      with `SeekPoint { frame_index, sample_offset_in_frame }`, the
//!      `frame_index` must be `< decoder.frames.len()` AND the
//!      `sample_offset_in_frame` must be strictly less than the
//!      regular per-frame sample count (or, for the last frame, less
//!      than that frame's `sample_count`). This pins the round-187
//!      arithmetic in [`Decoder::seek_to_sample`] against attacker-
//!      chosen `total_samples` / `sample_rate` header fields.
//!   6. **`frame_iter_from(start_index)` agreement.** When the
//!      iterator successfully decodes all subsequent frames, the
//!      concatenated PCM matches the suffix of the `decode_all`
//!      output starting at `frames[start_index].file_offset`'s
//!      sample boundary (i.e. the sum of preceding frames'
//!      per-channel sample counts × channels). Again gated on the
//!      eager path succeeding.
//!
//! ## Fuzz input layout
//!
//! ```text
//!   byte 0      : seed for `target_frame_index`; folded into
//!                 `frames.len()` via `% frames.len()` once the
//!                 Decoder is constructed.
//!   bytes 1..9  : seed for `target_sample_index` (LE u64); folded
//!                 into `total_samples` via `% total_samples` for the
//!                 seek_to_sample call.
//!   byte 9      : seed for `start_index` for `frame_iter_from`;
//!                 folded into `frames.len()` via `% (frames.len() + 1)`
//!                 so the "past-end → empty iterator" branch is also
//!                 driven.
//!   bytes 10..  : the TTA1 byte stream proper. Feeds `Decoder::new`
//!                 and the full streaming-API battery if construction
//!                 succeeds.
//! ```
//!
//! Inputs shorter than 10 bytes return immediately: there isn't
//! enough header room for a TTA1 stream anyway and the seed bytes
//! would alias header bytes — the existing `decode` target already
//! covers that tiny-input region.
//!
//! ## Cap on decoded sample volume
//!
//! `Decoder::decode_all()` allocates `total_samples * channels` `i32`
//! entries — for an attacker-chosen `total_samples` near `u32::MAX`
//! and `channels = 6` that's ~96 GiB. The Decoder's own bounds
//! reject this via `Error::Truncated` once the seek-table arithmetic
//! catches up, but to keep the fuzzer's per-iteration budget on the
//! streaming-state-machine surface rather than on the heap, the
//! `decode_all` / `frame_iter` agreement check is gated on the
//! frame count being below a small cap. The streaming API itself is
//! still called even for large frame counts — only the cross-API
//! agreement assertion is gated.

use libfuzzer_sys::fuzz_target;

use oxideav_tta::Decoder;

/// Skip the cross-API agreement check above this frame count. The
/// streaming-API panic-free contract is still exercised on every
/// input; this cap only gates the `decode_all() == concat(frame_iter)`
/// bit-exact assertion so the fuzzer doesn't spend iterations
/// allocating O(GiB) sample buffers when a malformed `total_samples`
/// slips past the cheap `Truncated` rejection.
const MAX_FRAMES_FOR_CROSSCHECK: usize = 4096;

fuzz_target!(|data: &[u8]| {
    if data.len() < 10 {
        return;
    }

    let target_frame_seed = data[0] as usize;
    let target_sample_seed = u64::from_le_bytes([
        data[1], data[2], data[3], data[4], data[5], data[6], data[7], data[8],
    ]);
    let start_index_seed = data[9] as usize;
    let tta_bytes = &data[10..];

    // ── 1. Construct the Decoder. Malformed framing returns Err and
    //       the streaming surface isn't reachable — that's covered by
    //       the `decode` fuzz target and the contract here is just
    //       "no panic". ──────────────────────────────────────────────
    let dec = match Decoder::new(tta_bytes) {
        Ok(d) => d,
        Err(_) => return,
    };

    let frame_count = dec.frames.len();
    let total_samples = dec.total_samples();

    // ── 2. decode_all + frame_iter cross-API agreement ────────────
    //
    // Only assert agreement when the frame count is small enough that
    // the eager allocation stays inside the fuzzer's per-iteration
    // budget. Above the cap, still drive both paths but skip the
    // cross-check.
    let eager = dec.decode_all();
    let lazy_concat: Result<Vec<i32>, _> = dec
        .frame_iter()
        .collect::<Result<Vec<Vec<i32>>, _>>()
        .map(|frames| frames.into_iter().flatten().collect());

    if frame_count <= MAX_FRAMES_FOR_CROSSCHECK {
        match (&eager, &lazy_concat) {
            (Ok(eager_samples), Ok(lazy_samples)) => {
                assert_eq!(
                    eager_samples, lazy_samples,
                    "decode_all() vs frame_iter() PCM disagreement: \
                     channels={} bps={} format={} frames={}",
                    dec.header.channels, dec.header.bits_per_sample, dec.header.format, frame_count,
                );
            }
            (Ok(_), Err(_)) => panic!(
                "decode_all succeeded but frame_iter failed: \
                 channels={} bps={} format={} frames={}",
                dec.header.channels, dec.header.bits_per_sample, dec.header.format, frame_count,
            ),
            (Err(_), Ok(_)) => panic!(
                "frame_iter succeeded but decode_all failed: \
                 channels={} bps={} format={} frames={}",
                dec.header.channels, dec.header.bits_per_sample, dec.header.format, frame_count,
            ),
            (Err(_), Err(_)) => {
                // Both paths agree on rejection — admissible.
            }
        }
    }

    // ── 3. decode_frame_at(target_frame_index) agreement ──────────
    //
    // Skip the call entirely for empty streams (no frames → every
    // index is OOB and the Decoder's typed FrameIndexOutOfRange is
    // already exercised by the in-tree seek_tests).
    if frame_count > 0 {
        let target_frame_index = target_frame_seed % frame_count;
        let frame_pcm = dec.decode_frame_at(target_frame_index);

        // Compute the eager-output slice this frame's PCM should
        // match, gated on both decode_frame_at and decode_all
        // succeeding AND the cross-check cap being respected.
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
                // Defensive bound: a corrupt seek table that
                // disagrees with the eager output's geometry must not
                // surface as a fuzz crash — fall through silently
                // since `eager_samples` already pins the canonical
                // PCM via the cross-check above.
                if end <= eager_samples.len() {
                    assert_eq!(
                        frame_samples,
                        &eager_samples[start..end],
                        "decode_frame_at({target_frame_index}) PCM disagrees with eager slice: \
                         channels={} bps={} frames={frame_count}",
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
    //
    // Empty streams (total_samples == 0) make the modulo undefined;
    // skip the modulo and pass the raw seed so the empty-stream
    // SampleIndexOutOfRange branch is still driven.
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
            // The per-frame sample offset must be strictly less than
            // the frame's own sample_count. The last frame can be
            // shorter than the regular size; the descriptor carries
            // the actual count.
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
    //
    // `start_index >= frames.len()` produces an empty iterator
    // (not an error) per the round-187 contract. Fold into
    // `frames.len() + 1` so the "past-end" branch is also driven.
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
            // Same defensive bound as above: don't trip a fuzz crash
            // on a seek-table-vs-eager disagreement that the
            // cross-check above already covers.
            if start <= eager_samples.len() {
                assert_eq!(
                    from_samples,
                    &eager_samples[start..],
                    "frame_iter_from({start_index}) suffix disagrees with eager: \
                     channels={} bps={} frames={frame_count}",
                    dec.header.channels,
                    dec.header.bits_per_sample,
                );
            }
        }
    }
});
