#![no_main]

//! Drive arbitrary fuzz-supplied bytes through the round-209 /
//! round-215 / round-219 player-API surface on
//! [`oxideav_tta::Decoder`] — the sample-keyed `decode_from_sample` /
//! `frame_iter_from_sample` pair and the half-open `[start, end)`
//! range quartet `decode_sample_range` / `frame_iter_sample_range` /
//! `decode_time_range` / `frame_iter_time_range`.
//!
//! The existing `streaming_decode` target (round 190) covers the
//! round-187 streaming + random-access surface (`frame_iter`,
//! `decode_frame_at`, `seek_to_sample`, `frame_iter_from`). This
//! target extends that contract to the sample / duration sugar
//! layered on top in r209 / r215 / r219:
//!
//!   - [`Decoder::decode_from_sample(sample_index)`] — eager tail
//!     decode from a per-channel sample boundary.
//!   - [`Decoder::frame_iter_from_sample(sample_index)`] — lazy
//!     analogue.
//!   - [`Decoder::decode_sample_range(start, end)`] — eager
//!     half-open `[start, end)` range.
//!   - [`Decoder::frame_iter_sample_range(start, end)`] — lazy
//!     analogue.
//!   - [`Decoder::decode_time_range(start, end)`] /
//!     [`Decoder::frame_iter_time_range(start, end)`] — the
//!     duration-keyed equivalents that pre-floor both endpoints via
//!     `floor(time_ns * sample_rate / 1e9)`.
//!
//! Each of those is a fresh attacker surface: arbitrary `(start,
//! end)` boundaries paired with an attacker-chosen byte stream must
//! surface every malformed-input case as a typed
//! [`oxideav_tta::Error`] — never a panic, never an index-out-of-
//! bounds, never an integer overflow, never an attacker-controlled
//! allocation.
//!
//! The contract under test on every fuzz input is:
//!
//!   1. [`Decoder::new`] either returns `Err(Error::…)` for malformed
//!      framing (in which case the range surface isn't reachable and
//!      iteration is skipped) or returns a [`Decoder`] whose range API
//!      is then exercised against the attacker-chosen seeds.
//!   2. Every range / sample / time call returns a `Result` (or an
//!      iterator that yields `Result`s) — never panics, never
//!      indexes out of bounds, never integer-overflows, never
//!      allocates an attacker-controlled volume.
//!   3. **`decode_from_sample` agreement.** When both
//!      [`Decoder::decode_all`] and [`Decoder::decode_from_sample`]
//!      succeed for the fuzzer-chosen `start`, the latter's PCM
//!      equals the suffix of `decode_all()` starting at
//!      `start * channels` interleaved entries. Gated on
//!      `decode_all().is_ok()` and the frame-count cap to keep the
//!      per-iteration budget on the state machine rather than the
//!      heap.
//!   4. **`decode_sample_range` agreement.** When both
//!      [`Decoder::decode_all`] and
//!      [`Decoder::decode_sample_range(start, end)`] succeed, the
//!      latter's PCM equals
//!      `decode_all()[start * channels .. end * channels]` bit-
//!      exactly. The half-open convention permits `end ==
//!      total_samples` (full tail) and `start == end` (empty `Vec`).
//!   5. **Lazy / eager equivalence on the range quartet.** When
//!      [`Decoder::frame_iter_sample_range(start, end)`] succeeds,
//!      the concatenation of every yielded `Vec<i32>` equals the
//!      eager [`Decoder::decode_sample_range(start, end)`] output.
//!      Same for the duration-keyed pair (both forward through the
//!      same `floor(time_ns * sample_rate / 1e9)` conversion so the
//!      lazy and eager surfaces converge to the same underlying
//!      sample-keyed call).
//!   6. **Boundary collapses on the sample-keyed surface.**
//!      `decode_sample_range(0, total)` equals `decode_all()`;
//!      `decode_sample_range(s, s)` returns `Ok(vec![])` for
//!      `s ∈ [0, total]`; `decode_sample_range(s, total)` equals
//!      `decode_from_sample(s)`. The duration-keyed analogue
//!      `decode_time_range(Duration::ZERO, total_duration())` is
//!      **not** asserted equal to `decode_all`: the duration
//!      round-trip `samples → Duration → samples` is lossy by one
//!      sample for `(total_samples, sample_rate)` pairs whose
//!      product `total_samples * 1e9 / sample_rate` doesn't have an
//!      exact integer-nanosecond representation. The
//!      `decode_time_range_full_duration_equals_decode_all`
//!      hand-fixture in `roundtrip_tests` is rate-aligned by
//!      construction so its round-trip is exact; the fuzzer's
//!      attacker-chosen `total_samples` can land on an
//!      off-by-one-sample case.
//!   7. **Typed rejection shape.** `start > end` and `end >
//!      total_samples` must surface
//!      [`oxideav_tta::Error::SampleIndexOutOfRange`] on both the
//!      sample- and duration-keyed surfaces.
//!
//! ## Fuzz input layout
//!
//! ```text
//!   bytes 0..8  : seed for `start_sample` (LE u64); folded into
//!                 `total_samples + 1` via `% (total_samples + 1)`
//!                 once the Decoder is constructed (the `+ 1` admits
//!                 the boundary `start == total` case that returns
//!                 `Ok(vec![])`).
//!   bytes 8..16 : seed for `end_sample` (LE u64); folded into
//!                 `total_samples + 1` similarly so the boundary
//!                 case `end == total` is driven.
//!   byte 16     : range-mode bias byte. Bits 0..2 select one of
//!                 four sub-cases (clamped start≤end, swapped start>
//!                 end, `s == e == 0`, `s == e == total`) so the
//!                 invariant + rejection branches are both reached.
//!                 The remaining bits are unused.
//!   bytes 17..  : the TTA1 byte stream proper. Feeds `Decoder::new`
//!                 and the full range API battery if construction
//!                 succeeds.
//! ```
//!
//! Inputs shorter than 17 bytes return immediately: the existing
//! `decode` target already covers tiny inputs.
//!
//! ## Cap on decoded sample volume
//!
//! `Decoder::decode_all()` allocates `total_samples * channels` `i32`
//! entries — for an attacker-chosen `total_samples` near `u32::MAX`
//! and `channels = 6` that's ~96 GiB. The Decoder's own bounds reject
//! this via [`oxideav_tta::Error::Truncated`] once the seek-table
//! arithmetic catches up, but to keep the fuzzer's per-iteration
//! budget on the range-state-machine surface rather than on the
//! heap, the eager-vs-lazy + agreement checks are gated on the frame
//! count being below a small cap. The range API itself is still
//! called even for large frame counts — only the cross-API agreement
//! assertion is gated.

use libfuzzer_sys::fuzz_target;

use oxideav_tta::Decoder;

/// Skip the cross-API agreement check above this frame count. The
/// range / sample / time API panic-free contract is still exercised on
/// every input; this cap only gates the `decode_all` vs
/// `decode_sample_range` bit-exact assertion so the fuzzer does not
/// spend iterations allocating O(GiB) sample buffers when a malformed
/// `total_samples` slips past the cheap `Truncated` rejection.
const MAX_FRAMES_FOR_CROSSCHECK: usize = 4096;

fuzz_target!(|data: &[u8]| {
    if data.len() < 17 {
        return;
    }

    let start_seed = u64::from_le_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]);
    let end_seed = u64::from_le_bytes([
        data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
    ]);
    let mode_byte = data[16];
    let tta_bytes = &data[17..];

    // ── 1. Construct the Decoder. Malformed framing returns Err and
    //       the range surface isn't reachable. ────────────────────────
    let dec = match Decoder::new(tta_bytes) {
        Ok(d) => d,
        Err(_) => return,
    };

    let frame_count = dec.frames.len();
    let total_samples = dec.total_samples() as u64;
    let channels = dec.header.channels as usize;

    // The `+ 1` admits the half-open boundary `s == total` which is
    // a valid range endpoint per the round-219 contract.
    let modulus = total_samples.saturating_add(1);
    let raw_a = if modulus == 0 {
        start_seed
    } else {
        start_seed % modulus
    };
    let raw_b = if modulus == 0 {
        end_seed
    } else {
        end_seed % modulus
    };

    // Range-mode selector: keep four sub-cases so the rejection
    // branches and the empty-range branch are both driven by
    // attacker-chosen seeds.
    let (mut start, mut end) = match mode_byte & 0b11 {
        // Canonical: clamp start ≤ end. Most coverage spends on this
        // branch since it exercises the bit-exact agreement path.
        0 => (raw_a.min(raw_b), raw_a.max(raw_b)),
        // Swapped: `start > end` must surface SampleIndexOutOfRange.
        // Only force the swap when the two seeds are distinct; if
        // they're equal it falls through to the empty-range branch.
        1 => {
            if raw_a == raw_b {
                (raw_a, raw_b)
            } else {
                (raw_a.max(raw_b), raw_a.min(raw_b))
            }
        }
        // Empty range at the leading boundary (`(0, 0)`).
        2 => (0, 0),
        // Empty range at the trailing boundary (`(total, total)`).
        _ => (total_samples, total_samples),
    };

    // For a zero-sample stream (total_samples == 0) the canonical
    // branch yields `(0, 0)` which is the only valid call shape; the
    // empty-range branches degenerate to the same point. Leave
    // `start`/`end` as-is — the call must still be panic-free.
    if total_samples == 0 {
        // No further normalisation needed; the API must accept the
        // empty (0, 0) case and reject (s, e > 0) per the contract.
        // Keep the values flowing into the calls below.
        let _ = (&mut start, &mut end);
    }

    // ── 2. eager full decode for cross-API agreement gate ─────────
    let eager = if frame_count <= MAX_FRAMES_FOR_CROSSCHECK {
        Some(dec.decode_all())
    } else {
        None
    };

    // ── 3. decode_from_sample agreement ──────────────────────────
    //
    // `decode_from_sample(s)` rejects `s == total_samples` (and
    // beyond) per the round-209 contract; drive both the in-range
    // and the out-of-range cases.
    let from_sample_call = dec.decode_from_sample(start);
    let from_sample_iter_call = dec.frame_iter_from_sample(start);

    if let (Some(Ok(eager_samples)), Ok(from_samples)) = (eager.as_ref(), &from_sample_call) {
        if start < total_samples {
            let prefix_entries = (start as usize).saturating_mul(channels);
            if prefix_entries <= eager_samples.len() {
                assert_eq!(
                    from_samples,
                    &eager_samples[prefix_entries..],
                    "decode_from_sample({start}) tail disagrees with eager: \
                     channels={channels} bps={} frames={frame_count}",
                    dec.header.bits_per_sample,
                );
            }
        }
    }

    // Lazy / eager equivalence on the sample-keyed pair.
    if let (Ok(eager_from), Ok(iter)) = (&from_sample_call, from_sample_iter_call) {
        let lazy_concat: Result<Vec<i32>, _> = iter
            .collect::<Result<Vec<Vec<i32>>, _>>()
            .map(|frames| frames.into_iter().flatten().collect());
        if let Ok(lazy_samples) = lazy_concat {
            assert_eq!(
                eager_from, &lazy_samples,
                "frame_iter_from_sample({start}) concat disagrees with \
                 decode_from_sample({start}): channels={channels} \
                 bps={} frames={frame_count}",
                dec.header.bits_per_sample,
            );
        }
    }

    // ── 4. decode_sample_range agreement ─────────────────────────
    //
    // The half-open `[start, end)` contract:
    //   - `start > end` → SampleIndexOutOfRange (rejection branch).
    //   - `end > total_samples` → SampleIndexOutOfRange.
    //   - `start == end ∈ [0, total]` → Ok(vec![]).
    //   - otherwise → suffix of decode_all from `start` to `end`.
    let range_call = dec.decode_sample_range(start, end);
    let range_iter_call = dec.frame_iter_sample_range(start, end);

    // Validate the rejection-shape branches by construction.
    if start > end {
        assert!(
            range_call.is_err(),
            "decode_sample_range({start}, {end}) accepted start > end"
        );
        assert!(
            range_iter_call.is_err(),
            "frame_iter_sample_range({start}, {end}) accepted start > end"
        );
    }
    if end > total_samples {
        assert!(
            range_call.is_err(),
            "decode_sample_range({start}, {end}) accepted end > total_samples={total_samples}"
        );
    }

    if let (Some(Ok(eager_samples)), Ok(range_samples)) = (eager.as_ref(), &range_call) {
        // Empty range short-circuit: must be exactly an empty Vec.
        if start == end {
            assert!(
                range_samples.is_empty(),
                "decode_sample_range({start}, {end}) returned non-empty for empty range \
                 (len={}, frames={frame_count})",
                range_samples.len(),
            );
        } else if start < end && end <= total_samples {
            let lo = (start as usize).saturating_mul(channels);
            let hi = (end as usize).saturating_mul(channels);
            if hi <= eager_samples.len() && lo <= hi {
                assert_eq!(
                    range_samples,
                    &eager_samples[lo..hi],
                    "decode_sample_range({start}, {end}) slice disagrees with eager: \
                     channels={channels} bps={} frames={frame_count}",
                    dec.header.bits_per_sample,
                );
            }
        }
    }

    // Lazy / eager equivalence on the range pair.
    if let (Ok(range_eager), Ok(iter)) = (&range_call, range_iter_call) {
        let lazy_concat: Result<Vec<i32>, _> = iter
            .collect::<Result<Vec<Vec<i32>>, _>>()
            .map(|frames| frames.into_iter().flatten().collect());
        if let Ok(lazy_samples) = lazy_concat {
            assert_eq!(
                range_eager, &lazy_samples,
                "frame_iter_sample_range({start}, {end}) concat disagrees with \
                 decode_sample_range({start}, {end}): channels={channels} \
                 bps={} frames={frame_count}",
                dec.header.bits_per_sample,
            );
        }
    }

    // ── 5. Boundary collapses on the eager call. ─────────────────
    //
    // `(0, total)` ⇔ `decode_all`; `(s, total)` ⇔
    // `decode_from_sample(s)` when `s < total`. Gated on the eager
    // path succeeding so a malformed stream that fails decode_all
    // does not produce spurious assertions here.
    if let Some(Ok(eager_samples)) = eager.as_ref() {
        if let Ok(full_range) = dec.decode_sample_range(0, total_samples) {
            assert_eq!(
                &full_range, eager_samples,
                "decode_sample_range(0, total) != decode_all: \
                 channels={channels} bps={} frames={frame_count}",
                dec.header.bits_per_sample,
            );
        }
    }

    if start < total_samples {
        if let (Ok(tail_range), Ok(tail_from)) = (
            dec.decode_sample_range(start, total_samples),
            dec.decode_from_sample(start),
        ) {
            assert_eq!(
                tail_range, tail_from,
                "decode_sample_range({start}, total) != decode_from_sample({start}): \
                 channels={channels} bps={} frames={frame_count}",
                dec.header.bits_per_sample,
            );
        }
    }

    // ── 6. Duration-keyed surface. ──────────────────────────────
    //
    // The contract here is panic-free for attacker-chosen Durations
    // plus the lazy/eager concat equivalence on the
    // duration-keyed pair. The bit-exact agreement with
    // `decode_all` is **not** asserted at the `(Duration::ZERO,
    // total_duration())` boundary because the duration round-trip
    // `samples → Duration → samples` is lossy by one sample for
    // some `(total_samples, sample_rate)` pairs whose product
    // `total_samples * 1e9 / sample_rate` doesn't have an exact
    // integer-nanosecond representation. The conversion is
    // `floor(time_ns * sample_rate / 1e9)` (`spec/01` §3.3 /
    // §3.4) and the floor occasionally drops the last sample.
    // The roundtrip_tests::decode_time_range_full_duration_equals_decode_all
    // hand-fixture is rate-aligned by construction (44 100 samples
    // at 44 100 Hz) so the round-trip is exact there; the fuzzer's
    // attacker-chosen `total_samples` can land on an
    // off-by-one-sample case.
    let total_d = dec.total_duration();

    // The two duration-keyed paths must agree with each other
    // (lazy == eager) on the same `(start_d, end_d)` boundary even
    // when the boundary is the lossy `(0, total_duration)` pair —
    // both forward through the same `duration_to_sample_index`
    // arithmetic so they must converge to the same sample-keyed
    // call and therefore the same PCM.
    if let (Ok(time_eager), Ok(time_iter)) = (
        dec.decode_time_range(core::time::Duration::ZERO, total_d),
        dec.frame_iter_time_range(core::time::Duration::ZERO, total_d),
    ) {
        let lazy_concat: Result<Vec<i32>, _> = time_iter
            .collect::<Result<Vec<Vec<i32>>, _>>()
            .map(|frames| frames.into_iter().flatten().collect());
        if let Ok(lazy_samples) = lazy_concat {
            assert_eq!(
                time_eager, lazy_samples,
                "frame_iter_time_range(0, total_duration) concat disagrees with eager: \
                 channels={channels} bps={} frames={frame_count}",
                dec.header.bits_per_sample,
            );
        }
    }

    // `start > end` on the duration-keyed surface must reject too.
    // Construct a clearly-out-of-order pair: `(total_duration,
    // Duration::ZERO)` whenever total_duration > 0. For zero-sample
    // streams `total_duration == Duration::ZERO`, both endpoints
    // collapse to the same point and the `start == end` empty
    // contract takes over instead.
    if !total_d.is_zero() {
        assert!(
            dec.decode_time_range(total_d, core::time::Duration::ZERO)
                .is_err(),
            "decode_time_range(total, 0) accepted start > end (total_d={total_d:?})"
        );
        assert!(
            dec.frame_iter_time_range(total_d, core::time::Duration::ZERO)
                .is_err(),
            "frame_iter_time_range(total, 0) accepted start > end (total_d={total_d:?})"
        );
    }
});
