# oxideav-tta

Pure-Rust True Audio (TTA) lossless audio codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Round 5 (+ r187 streaming surface, r204 format=2 streaming reach,
r209 sample-keyed player-API sugar, r215 duration-keyed player-API
sugar, r219 half-open sample/time range quartet, r261 typed
`TrailerInfo` sub-field accessors, r262 aggregate `TypedStreamHeader`
validated view) — clean-room encoder + decoder +
framework integration + trace tape + format=2 + ID3v1/APEv2 trailer
detection + multi-frame format=2 trace coverage + format=2
streaming/random-access surface + `decode_from_sample` /
`frame_iter_from_sample` + `decode_from_time` / `frame_iter_from_time` /
`seek_to_time` / `total_duration` + `decode_sample_range` /
`frame_iter_sample_range` / `decode_time_range` /
`frame_iter_time_range` + typed `Id3v1Range` / `ApeV2Range` on
`TrailerInfo::id3v1_typed` / `apev2_typed` /
`combined_byte_range` per `spec/01` §7 + `StreamHeader::typed()` aggregate
view per `spec/01` §3.** Both encodes and decodes TTA1 format=1
(integer PCM) and format=2 (password-derived qm priming, `spec/07`)
streams in pure safe Rust against the strict-isolation clean-room
workspace at
[`docs/audio/tta-cleanroom/`](https://github.com/OxideAV/docs/tree/master/audio/tta-cleanroom).
Encoder output round-trips bit-exactly through the decoder.

Round 187 layers a streaming + random-access decode API on top of
the existing eager path: `Decoder::frame_iter` (lazy, `O(frame)`
memory), `Decoder::decode_frame_at(index)` (random-access by
seek-table index), `Decoder::seek_to_sample(sample_index)` (locate
the frame containing a per-channel sample), and
`Decoder::frame_iter_from(start_index)` (resume from the seek
point without decoding the skipped prefix). The bit-exact
agreement with `decode_all` is locked by tests for both
single-frame and full streaming-from-seek paths — the per-frame
state-reset discipline of `spec/01` §5.1 + `spec/02..05` §3.1 is
what makes random-access decode legitimate against the spec.

Round 204 extends that surface to format=2 (password-protected,
`spec/07`) streams via the new public
`Decoder::new_with_password(bytes, password)` constructor. With it,
the same `frame_iter` / `decode_frame_at` / `seek_to_sample` /
`frame_iter_from` API is reachable across format=2 streams under
identical per-frame qm re-prime discipline (`spec/07` §3.5–§3.6) —
six new tests in `roundtrip_tests` lock bit-exact agreement with the
eager `decode_with_password` baseline on multi-frame stereo16, the
seek-and-resume integration path on format=2, the format=1
fall-through (priming computed then dropped per audit/07 §6.2-2), the
spec/07 §11 "wrong-password decodes but corrupts" semantic, and the
`FrameIndexOutOfRange` / `SampleIndexOutOfRange` rejection shape.

Round 215 layers a duration-keyed convenience quartet on top of the
sample-keyed round-209 player surface:
`Decoder::total_duration()` returns the stream's per-channel playback
length as a `core::time::Duration` (computed from `total_samples` and
`sample_rate` via integer arithmetic at nanosecond granularity, no
floating-point intermediates), and `Decoder::seek_to_time(d)` /
`Decoder::frame_iter_from_time(d)` / `Decoder::decode_from_time(d)`
mirror the existing sample-keyed seek surface against a clock
`Duration` from stream start. The `Duration → sample_index`
conversion is `floor(time_ns × sample_rate / 1e9)` widened to `u128`
so the multiplication is overflow-free for the full
`(sample_rate ≤ 0x7FFFFF, Duration ≤ Duration::MAX)` envelope (per
`spec/01` §3.3 cap); the floor is monotone non-decreasing and never
overshoots the true sample boundary. Out-of-range times surface
`Error::SampleIndexOutOfRange` (`time >= total_duration` →
`sample_index >= total_samples`), `Duration::MAX` does not panic.
Nine new tests in `roundtrip_tests` lock the invariants:
total-duration arithmetic at sample-aligned and one-sample-past
endpoints, `seek_to_time(Duration::ZERO)` landing at sample 0,
`seek_to_time` equivalence with `seek_to_sample` at sample-rate-aligned
boundaries, `seek_to_time` rejection at and past `total_duration`,
`decode_from_time` bit-exact equivalence with `decode_from_sample`
across multi-frame format=1, `frame_iter_from_time` lazy
concatenation matching the eager tail, `frame_iter_from_time` /
`decode_from_time` rejecting past-end, format=2 (password-protected)
duration-keyed seek-and-resume bit-exact agreement with eager
`decode_with_password`, and the sub-sample-period boundary discipline
(two timestamps within the same sample period collapse to one
SeekPoint; the next sample-boundary advances by exactly one sample).
Eight extra unit tests in `decoder::duration_helpers_tests` walk the
arithmetic primitive at the rate × sample-index cube. Total
123 lib + 9 integration after r215 (r209's `110 lib` was an
under-count of the actual surface; the r215 additions land cleanly
on top).

Round 209 layers a player-API convenience pair on top of the same
streaming surface: `Decoder::decode_from_sample(sample_index)`
materialises the suffix of `decode_all` starting at the requested
per-channel sample boundary, and
`Decoder::frame_iter_from_sample(sample_index)` returns a
trace-silent `SampleSkipIter` that yields the same suffix one frame
at a time. Both calls combine the previously-manual
`seek_to_sample(s) → frame_iter_from(sp.frame_index) → drain the
sp.sample_offset_in_frame × channels prefix` ritual into a single
entry point. Ten new tests in `roundtrip_tests` pin bit-exact
agreement with `decode_all`'s tail across the parameter cube (mono16
/ stereo16 / stereo24 / 6ch16 in format=1, stereo16 in format=2),
the inner equivalence to the by-hand composition, the
`sample_index = 0` round-trip, the `total_samples - 1` boundary
returning exactly `channels` entries, and the
`SampleIndexOutOfRange` rejection shape on both APIs.

Round 219 extends the r209 / r215 player-API surface from "seek and
play the tail" to "seek and play a bounded segment" via a half-open
`[start, end)` range quartet on `Decoder`. The eager
`Decoder::decode_sample_range(start, end)` returns the interleaved
`i32` PCM for per-channel samples `start..end`; the lazy
`Decoder::frame_iter_sample_range(start, end)` returns a new
`SampleRangeIter` whose concatenation equals the eager call. The
duration-keyed `Decoder::decode_time_range(start, end)` and
`Decoder::frame_iter_time_range(start, end)` pre-floor both endpoints
via the same `floor(time_ns × sample_rate / 1e9)` arithmetic the
round-215 sugar already uses. The trailing frame is trimmed in-place
via `Vec::truncate`, so the returned PCM is exactly
`(end - start) × channels` interleaved entries and frames past `end`
are never decoded. The half-open convention permits
`end == total_samples` (equivalent to `decode_from_sample(start)`)
and `start == end` (returns `Ok(vec![])` without touching the
bitstream); `start > end` and `end > total_samples` surface
`Error::SampleIndexOutOfRange`. Format=2 (password-protected) reach
is automatic via `Decoder::new_with_password` — the per-frame qm
re-prime discipline of `spec/07` §3.5–§3.6 propagates through the
new surface unchanged. Sixteen new tests in `roundtrip_tests` pin
bit-exact agreement with `decode_all`'s slice across the parameter
cube (mono16 / stereo16 / stereo24 / 6ch16 in format=1, stereo16 in
format=2), the boundary collapses (`(0, total) ⇔ decode_all`,
`(s, total) ⇔ decode_from_sample(s)`, `(s, s) ⇔ Ok(vec![])` for
`s ∈ [0, total_samples]`), the lazy/eager concatenation equivalence,
the trailing-frame trim landing at the exact mid-frame boundary,
the time-keyed surface agreeing with the sample-keyed surface at
exact-round-trip rate-aligned boundaries, and the
`SampleIndexOutOfRange` rejection shape on `start > end` and
`end > total_samples` for both the sample-keyed and the
duration-keyed surfaces. Total 140 lib + 9 integration after r219
(with `--all-features`); 135 lib + 9 integration on the default
feature set.

The fresh orphan `master` is the starting point; the previous
implementation, retired alongside the OxideAV docs audit dated
2026-05-06 (see
[AUDIT-2026-05-06.md](https://github.com/OxideAV/docs/blob/master/AUDIT-2026-05-06.md)),
is preserved on the `old` branch for reference but is **not** used as
input to this rebuild.

## What round 1 covers

- **Bitstream framing** (`spec/01`): TTA1 header parse, optional ID3v2
  prefix skip, seek table consumer, frame iterator, IEEE-802.3 CRC32
  verification at all three sites (header, seek table, per-frame
  trailer).
- **Adaptive Rice entropy decoder** (`spec/05`): LSB-first bit reader
  with the unary fast path; per-channel `(k0, k1, sum0, sum1)`
  trackers reset to `(10, 10, 0x4000, 0x4000)` per frame; depth-1
  escape bias `(1 << k0)`; zigzag de-zigzag (odd → positive, even →
  negative).
- **Stage-A 8-tap sign-LMS predictor** (`spec/02`): `(dl[8], dx[8],
  qm[8], error)` per channel, all reset to zero per frame; per-bps
  `shift`/`round` loaded from `tables/lms-shift.csv` via
  `include_str!`; `dx[4..7]` magnitudes loaded from
  `tables/lms-dx-magnitudes.csv`; spec §4.2 five-step update sequence.
- **Stage-B fixed-order recursive predictor** (`spec/03`): single
  `prev` register per channel, reset to zero per frame; `(prev * 31)
  >> 5` with arithmetic right shift (no rounding addend).
- **Inverse channel decorrelation** (`spec/04`): mono passthrough;
  stereo and N>2 cascade walking from highest channel index downward;
  C signed truncating `/2` (NOT `>>1`) per spec §6.
- **PCM packing**: signed 16-bit LE and signed 24-bit LE output via
  `pack_pcm` per spec §3.2.

## What round 2 adds on top

- **`spec/06` debug trace emitter**: gated behind a `trace` Cargo
  feature (off by default) and the `OXIDEAV_TTA_TRACE_FILE` env var
  per spec/06 §2. With both on, the decoder writes one TSV event
  line per state transition (18-event vocabulary) suitable for
  lockstep diff against a reference-instrumented tape via
  `docs/audio/tta-cleanroom/tools/tta-diff/`. Zero overhead when
  the feature is off.
- **`oxideav-core` framework integration**: default-on `registry`
  Cargo feature wires a `Decoder` impl (codec id `"tta"`), a raw-
  `.tta` `Demuxer` (probe / `tta` extension), and the
  `register(ctx)` entry point that `oxideav-meta::register_all`
  picks up via `oxideav_core::register!`. Standalone consumers can
  build with `default-features = false` to drop the `oxideav-core`
  dep.
- **Frame-boundary streaming demuxer + O(1) seek**: the registered
  `.tta` demuxer parses the TTA1 seek table at open and emits one
  packet per audio frame (each wrapped as a self-contained mini-
  TTA1 file so the decoder can consume it without coordination).
  `Demuxer::seek_to(pts)` is O(1): the target frame is
  `min(pts / regular_frame_samples, n_frames - 1)` per `spec/01`
  §4.1, with `regular_frame_samples = floor(sample_rate * 256 / 245)`.
  Sub-frame pts requests snap to the containing frame's first
  sample, negative pts clamp to 0, and past-end pts clamp to the
  last frame's first sample. Decoder state self-resets at every
  frame boundary by construction (`spec/02-05`), so the demuxer
  does not need to coordinate LMS / Stage-B / Rice resets.
- **Format=2 (password-derived qm priming)** per `spec/07`:
  `decode_with_password(bytes, password)` derives an ECMA-182
  CRC-64 digest of the password and primes Stage-A's `qm[0..7]`
  with the eight digest bytes (sign-extended int8 → int32) at every
  per-channel frame init. Plain `decode()` returns
  `Error::PasswordRequired` for format=2 streams.

## What round 3 adds on top

- **Production encoder** (`crate::encode`, `crate::encode_with_password`):
  symmetric inverse of the decoder pipeline. Forward channel
  decorrelation (`spec/04` §3.1), Stage-B prediction subtraction
  (`spec/03` §4.3), Stage-A LMS step with residual feedback
  (`spec/02` §4.2), zigzag + adaptive Rice with the lock-stepped
  `(k0, k1, sum0, sum1)` trackers (`spec/05` §5.2 / §5.3), per-frame
  byte alignment + IEEE-802.3 CRC32 (`spec/01` §5.3 / §5.4), then
  header + seek table assembly (`spec/01` §3 / §4). Self-roundtrip is
  bit-exact across every fixture in the existing test suite
  (16-bit / 24-bit, 1..=6 channels, format=1 and format=2,
  silence / sine / pseudo-noise / DC+impulse / multi-frame).
- **Framework `Encoder` impl** wired through the `registry` feature:
  the same `CodecInfo::new("tta")` registration that already carried
  the decoder factory now also carries `encoder(make_encoder)`, so
  `CodecRegistry::first_encoder(&params)` returns a working TTA
  encoder. The adapter accepts interleaved S16/S24 audio frames,
  buffers the PCM, and emits one self-contained TTA1 file as a
  keyframe packet on `flush()`.

## What round 4 adds on top

- **ID3v1 / APEv2 trailer detection** per `spec/01` §7: a new
  [`scan_trailers`] / [`detect_trailers`] pair that walks the
  optional ID3v2 prefix + stream header + seek table to compute the
  end-of-stream byte boundary, then signature-scans the post-stream
  region for the ID3v1 `'TAG'` magic (fixed 128-byte trailer at file
  end) and / or the APEv2 `'APETAGEX'` footer magic (32-byte footer
  with embedded `tag_size` + optional 32-byte header sentinel). The
  scanner returns absolute `(start, len)` byte ranges and never
  reads bytes inside the TTA1 frame region — out-of-stream metadata
  is host-app territory per spec §7. Bogus APE `tag_size` values
  that would overrun the post-stream region are rejected silently
  (the trailer is treated as "not present" rather than as a parse
  error; spec §7 specifies host-app passthrough for trailers).

## What round 5 adds on top

- **Multi-frame format=2 trace coverage** closing
  `docs/audio/tta-cleanroom/audit/07` §6.2-5. The pre-existing
  format=2 tests covered a single frame only, which couldn't tell
  the spec-correct "re-prime qm[] at every frame init" behaviour
  (`spec/07` §3.5 / §3.6) apart from a hypothetical "prime once at
  frame 0" implementation. The round-5 tests exercise format=2
  across 3+ frames in both mono and stereo at 44.1 kHz and verify,
  under the `trace` feature, that the `LMS_PRE` event's `qm_pre[]`
  carries the ECMA-182 CRC-64 digest bytes at step_idx ∈
  {0..nch-1} of **every** frame — putting a wire-level seal on
  spec/07 §3.6.
- **`HEADER_CRC` `computed_crc` carries the real value**, not the
  placeholder zero flagged by `audit/07` §6.2-3. The decoder now
  threads the freshly-computed IEEE-802.3 CRC32 of the 18
  header-body bytes through to the trace emitter, so a
  conformance-tape exact-match against a libtta-instrumented
  reference no longer needs an `--ignore-fields computed_crc`
  exemption on the `HEADER_CRC` line.
- **`decode_with_password` no longer re-parses format=1 streams**
  (audit/07 §6.2-2). The previous code constructed two `Decoder`s
  back-to-back when a password was supplied against a format=1
  file; the new path constructs one, calls a crate-internal
  `clear_priming` to drop the unused digest, and decodes from
  there. The format=1 `qm` zero-init invariant (`spec/02` §3.1)
  is preserved; the redundant header / seek-table parse is gone.

Still out of scope (no current asks): format=3 (IEEE float) and
bit-exact lockstep against libtta-encoded reference fixtures (needs
a sanctioned fixture in `audit/reference-tapes/`).

## Why clean-room

libtta is the canonical TTA reference (Aleksander Djuric / Pavel
Zhilin, en.true-audio.com, LGPL-2.1). oxideav cannot ship LGPL code,
so every line of this crate is written without reading libtta or any
FFmpeg-derived TTA source. The clean-room workspace at
`docs/audio/tta-cleanroom/` is the wall: the Implementer reads only
`spec/`, `tables/`, and `reference/docs/`.

## Verification

The `audit/reference-tapes/**` and `reference/inputs/**` trees are
gitignored, so verification is performed via the crate's own
production encoder, which mirrors the decoder's state machines.
Tests exercise:

- Per-spec hand-verifications transcribed from `spec/02..05` §7
  worked-step examples (Stage-A samples 0..2, Stage-B positive +
  negative `prev`, Rice step 0).
- Full encode-decode roundtrips on mono / stereo / six-channel
  fixtures, 16-bit and 24-bit, with sine / silence / pseudo-noise /
  DC+impulse content.
- Multi-frame roundtrip (2.5 s at 44.1 kHz spans three TTA frames)
  exercising the per-frame state-reset discipline.
- Negative-path: corrupted frame CRC and unsupported header values
  are rejected with the correct `Error` variants.

Bit-exact lockstep against libtta-encoded fixtures is deferred to a
future Auditor round once a sanctioned reference fixture lands in the
clean-room workspace.

## Fuzzing

Four [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) harnesses
live under `fuzz/fuzz_targets/`:

- **`decode`** (round 124) — feeds arbitrary bytes to both
  [`decode`](src/lib.rs) (format=1) and `decode_with_password`
  (format=2) and asserts the call always returns a `Result` rather
  than panicking, overflowing, indexing out of bounds, or OOMing.
  Seed corpus under `fuzz/corpus/decode/` is five real streams
  emitted by the crate's own encoder (mono/stereo, 16/24-bit,
  format=1/2, plus a tiny silent frame).
- **`scan_trailers`** (round 175) — drives the public
  [`scan_trailers`](src/lib.rs) entry point with arbitrary bytes,
  exercising the ID3v2 prefix skip, the seek-table sum arithmetic
  that computes the end-of-stream offset, and the ID3v1 / APEv2
  footer scanner (`spec/01` §7) against attacker-chosen header
  fields. Same panic-free contract as `decode`; seeded from the same
  five real-stream fixtures. 500K iters clean (cov 132, ft 133).
- **`encode_roundtrip`** (round 175) — drives the public encoder
  across the `(channels × bps × sample_rate × format × samples)`
  parameter cube and asserts (i) the encoder either rejects with a
  typed `Error::Unsupported…` / `InvalidSampleBuffer`, or returns
  `Ok(bytes)`; (ii) every `Ok(bytes)` decodes back via the matching
  `decode` / `decode_with_password` call; (iii) the recovered `i32`
  samples equal the input bit-exactly. Three hand-crafted seeds
  cover format=1 mono16, format=2 stereo24-pw, and quad16. 500K
  iters clean (cov 688, ft 3221, ~18.5K exec/s).
- **`streaming_decode`** (round 190) — drives the round-187
  streaming + random-access decode surface on [`Decoder`](src/decoder.rs)
  (`frame_iter`, `decode_frame_at`, `seek_to_sample`,
  `frame_iter_from`). Asserts cross-API agreement on every fuzz-
  constructed input: whenever the eager `decode_all` succeeds, the
  lazy `frame_iter` must concatenate to the same PCM bit-exactly;
  `decode_frame_at(target_frame_index)` must match the
  corresponding eager slice; `seek_to_sample(target_sample_index)`
  must return an in-range `(frame_index, sample_offset_in_frame)`
  pair; and `frame_iter_from(start_index)` must equal the eager
  suffix from the matching sample boundary. The fuzz input's first
  ten bytes seed the random-access targets so attacker-chosen
  frame / sample indices are driven against attacker-chosen byte
  streams. Seed corpus under `fuzz/corpus/streaming_decode/` is the
  five real-stream fixtures plus four crate-encoded multi-frame
  streams (mono16/stereo16/stereo24 at 2.5-3 s + a format=2 stereo
  3 s).
- **`sample_range`** (round 226) — drives the round-209 / r215 /
  r219 player-API sugar on [`Decoder`](src/decoder.rs):
  `decode_from_sample`, `frame_iter_from_sample`,
  `decode_sample_range(start, end)`, `frame_iter_sample_range`,
  `decode_time_range`, and `frame_iter_time_range`. Folds
  attacker-chosen `(start, end)` `u64` seeds against
  `total_samples + 1` (so `start == total` and `end == total` are
  both reachable per the half-open contract), then routes the pair
  through a 4-mode bias byte covering the canonical
  `start ≤ end` agreement branch, the swapped `start > end`
  rejection branch, and the empty-range boundary cases
  `(0, 0)` / `(total, total)`. Asserts (i) `decode_from_sample(s)`
  equals `decode_all()[s * channels..]`; (ii)
  `decode_sample_range(s, e)` equals
  `decode_all()[s * channels .. e * channels]`; (iii) the lazy
  `frame_iter_*` surfaces concatenate to their eager siblings on
  both the sample- and duration-keyed pairs; (iv) the boundary
  collapses `(0, total) ⇔ decode_all`,
  `(s, total) ⇔ decode_from_sample(s)`,
  `(s, s) ⇔ Ok(vec![])` for `s ∈ [0, total]`; (v) `start > end`
  and `end > total_samples` surface
  `Error::SampleIndexOutOfRange` (panic-free typed rejection) on
  both the sample- and duration-keyed surfaces. The duration-keyed
  `decode_time_range(Duration::ZERO, total_duration())` is *not*
  asserted equal to `decode_all`: the duration round-trip is lossy
  by one sample when `total_samples * 1e9 / sample_rate` doesn't
  have an exact integer-nanosecond representation, and the
  pre-existing `roundtrip_tests::decode_time_range_full_duration_equals_decode_all`
  hand-fixture only covers the rate-aligned case (44 100 samples
  at 44 100 Hz). Seed corpus under `fuzz/corpus/sample_range/` is
  derived from the `streaming_decode` 9-stream real-fixture pool:
  nine small seeds (mono16 ramp / mono16 pw / mono24 / stereo16 /
  tiny-silent) are replicated across the four range-mode prefixes
  for 20 small entries, plus four canonical-mode multi-frame
  streams (mono16-3s, stereo16-3s, stereo24-2.5s, stereo16-pw-3s)
  for the cross-API agreement path. 200K iters clean from a cold
  start, 7K+ iters per 60 s with the seeded corpus (per-iteration
  cost is heavy because every input forces multiple eager
  `decode_all` passes for the agreement check).

The harness body is clean-room (no reference-implementation
oracle). Run locally with `cargo +nightly fuzz run <target>`; the
`.github/workflows/fuzz.yml` shim points at the org reusable
workflow which auto-discovers every `[[bin]]` block in
`fuzz/Cargo.toml` and splits the daily 30-minute budget across
them.

The `decode` harness found one bug (round 124): a corrupt high-mode
bitstream could chain enough Rice escapes to drive the adaptive
parameter `k` past 31, after which the next binary-tail read requested
more than 32 bits and tripped the bit reader's `k <= 32` invariant.
The Rice decoder now caps `k` at 31 on increment — matching the
`[0, 31]` range the reference encoder stays within per `spec/05`
§5.3 — so the cap never alters the decode of any valid stream.

## Property tests

`tests/malformed_props.rs` is a round-156 addition that complements
the r124 fuzzer with a *semantic* fault-injection layer: structurally-
valid TTA1 streams with a single, deliberately-chosen corruption,
verifying that the documented `Error` variants surface (and never a
panic). The nine tests are driven by a deterministic xorshift64* PRNG
(same convention as `oxideav-scene/tests/transform_props.rs`), so any
failure reproduces from the literal seed in the source. Coverage:

- exhaustive `22 × 8` single-bit-flip walk of the stream header;
- exhaustive byte-prefix truncation walk (format=1 and format=2);
- **seek-table re-CRC bait** — flip one seek-table entry, then
  recompute the seek-table CRC32 so the decoder cannot rely on the
  seek-table CRC as the rejection signal; the disagreement must
  surface at the per-frame CRC32 instead;
- oversize `total_samples` with recomputed header CRC;
- wrong-password format=2 (decode must not panic; if it returns
  `Ok` the PCM shape must match the header);
- randomised ID3v2 prefix sweep — every variant must decode to the
  same PCM as the un-prefixed stream;
- randomised trailer-region junk — `scan_trailers` must never claim
  a trailer that overlaps the in-stream frame region;
- pseudo-noise round-trip at randomised `(channels, bits_per_sample)`
  shapes (1..=6 channels, 16- or 24-bit).

Run locally with `cargo test -p oxideav-tta --test malformed_props`.

## Benchmarks

`benches/{decode,encode,roundtrip}.rs` are
[Criterion](https://github.com/bheisler/criterion.rs) harnesses added
in round 127 for the "saturated codec gets fuzz + bench + profile"
follow-through to r124's fuzzer. Each binary covers five scenarios —
mono / stereo / six-channel, 16-bit and 24-bit, plus a format=2
(password-derived qm priming) variant — driven by a deterministic
xorshift-synthesised PCM workload. No checked-in fixture files: every
input is built in-bench, then the production [`encode`](src/lib.rs)
turns it into a TTA1 byte stream for the decoder benches to consume.

Round 193 adds `benches/streaming.rs`, a fourth Criterion harness
covering the round-187 streaming + random-access decode surface on
[`Decoder`](src/decoder.rs) — `frame_iter`, `decode_frame_at`,
`seek_to_sample`, and `frame_iter_from`. All four scenarios run
against the same 3 s stereo 16-bit 44.1 kHz stream (three frames
under `regular_frame_samples = 46_073`), so the lazy `frame_iter`
cost is directly comparable to the eager `decode` baseline,
`decode_frame_at(1)` measures a single mid-stream random-access
decode, `seek_to_sample` is the constant-time `spec/01` §4.1 sample →
frame arithmetic, and `frame_iter_from(1)` is the resume-from-seek
cost. Reference numbers on the development machine: `frame_iter` 3 s
≈ 3.72 ms (~135 MiB/s), `decode_frame_at(1)` ≈ 1.24 ms (~142 MiB/s),
`seek_to_sample` ≈ 1.07 ns (constant-time sentinel),
`frame_iter_from(1)` ≈ 2.74 ms — the resume cost lands at roughly
2/3 of the full-stream cost as expected (= 2 of 3 frames decoded).

Round 198 extends `benches/streaming.rs` with two parameter-cube
groups so the streaming surface has an A/B baseline at every shape
the eager `decode.rs` baseline covers, not just the original
stereo16-44k1 anchor:

- `streaming_frame_iter_cube` — walks `frame_iter` across three
  format=1 cells (`mono16_44k1_1s`, `stereo24_48k_500ms`,
  `ch6_16bit_48k_250ms`).
- `streaming_decode_frame_at_cube` — picks the middle frame of each
  same-shape cell (or frame 0 for single-frame cells) and measures
  random-access decode cost per shape.

Reference numbers on the development machine (`--quick`, 1 s
warmup): `streaming_frame_iter_cube/mono16_44k1_1s` ≈ 566 µs
(~148 MiB/s), `…/stereo24_48k_500ms` ≈ 626 µs (~219 MiB/s),
`…/ch6_16bit_48k_250ms` ≈ 974 µs (~141 MiB/s);
`streaming_decode_frame_at_cube/mono16_44k1_1s` ≈ 561 µs
(~150 MiB/s), `…/stereo24_48k_500ms` ≈ 600 µs (~229 MiB/s),
`…/ch6_16bit_48k_250ms` ≈ 960 µs (~143 MiB/s).

Round 204 closes the r198 cube's format=2 omission by adding a
`stereo16_44k1_1s_format2` cell to both `streaming_frame_iter_cube`
and `streaming_decode_frame_at_cube`. The cell uses the new
`Decoder::new_with_password` constructor at the same parameter point
as the format=1 anchor (`stereo16` × `16-bit` × `44.1 kHz` × `1 s`),
so the marginal cost of the per-frame qm re-prime
(`spec/07` §3.5–§3.6) is directly comparable against the format=1
baseline. Reference numbers on the development machine (`--bench
--quick`): `streaming_frame_iter_cube/stereo16_44k1_1s_format2`
≈ 1.22 ms (~138 MiB/s),
`streaming_decode_frame_at_cube/stereo16_44k1_1s_format2`
≈ 1.20 ms (~140 MiB/s) — the format=2 cells land essentially within
noise of the format=1 sibling at the same shape, confirming the
per-frame qm priming write is negligible against the Rice + Stage-A
LMS + Stage-B + decorr cascade cost.

Round 234 adds `benches/range.rs`, a fifth Criterion harness
covering the round-209 / round-215 / round-219 player-API range
surface on [`Decoder`](src/decoder.rs) — `decode_from_sample` /
`frame_iter_from_sample` (r209), `decode_from_time` / `seek_to_time`
/ `total_duration` (r215), and the half-open `[start, end)` range
quartet `decode_sample_range` / `frame_iter_sample_range` /
`decode_time_range` / `frame_iter_time_range` (r219). Eleven
scenarios run against the same 3 s stereo 16-bit 44.1 kHz anchor
the `streaming.rs` harness uses, so the per-API cost diffs against
the existing baselines: `range_decode_from_sample_mid` /
`range_frame_iter_from_sample_mid` measure tail-from-mid-stream cost
(eager vs lazy); `range_decode_from_time_mid` adds the duration →
sample-index conversion on top; `range_seek_to_time_mid` is the
duration-keyed sentinel for the constant-time `spec/01` §4.1
arithmetic; `range_decode_sample_range_middle_half` /
`range_frame_iter_sample_range_middle_half` /
`range_decode_time_range_middle_half` exercise the half-open range
quartet across the middle 50 % of the stream;
`range_decode_sample_range_full` measures the full-stream boundary
case `[0, total_samples)` against the eager `decode_all` baseline;
`range_decode_sample_range_empty` is a sentinel against accidentally
routing the empty-range short-circuit through the seek path; and
`range_decode_sample_range_format2_middle_half` adds the format=2
reach at the same parameter point so the per-frame qm re-prime
(`spec/07` §3.5–§3.6) cost is comparable against the format=1
anchor. `range_total_duration` rounds out the file as a
sub-nanosecond sentinel against accidentally promoting the integer-
arithmetic primitive to a heavier computation.

Run locally with `cargo bench -p oxideav-tta --bench <decode|encode|roundtrip|streaming|range>`.

## What round 240 adds on top

- **Typed sub-field accessors** for the four constrained
  `StreamHeader` fields per `spec/01` §3.1 / §3.2 / §3.3. The raw
  `format: u16` / `bits_per_sample: u16` / `channels: u16` /
  `sample_rate: u32` fields are kept as the on-wire data model, but
  consumers that want to branch on the spec's documented invariants
  can now lift each field into a validated typed accessor:
  - [`Format`](src/header.rs) — non-exhaustive enum with
    `Simple` (= 1) and `Encrypted` (= 2) variants; carries
    `from_raw`, `as_raw`, and a `requires_password()` convenience
    over the format=2 password-priming discipline of `spec/07` §3.
  - [`BitsPerSample`](src/header.rs) — newtype validated against the
    in-scope `16..=24` range per `spec/01` §3.2; carries `bits()`
    and `byte_depth()` (= `(bits + 7) / 8`, always 2 or 3).
  - [`ChannelCount`](src/header.rs) — newtype validated against the
    in-scope `1..=6` range per `spec/01` §3; carries `count()` and
    `is_multichannel()` (the gate for the inverse decorrelation
    cascade of `spec/04` §3).
  - [`SampleRate`](src/header.rs) — newtype validated against the
    workspace-policy `1..=0x7FFFFF` range per `spec/01` §3.3;
    carries `hz()` and `regular_frame_samples()` (the canonical
    `floor(rate * 256 / 245)` per-frame sample count of `spec/01`
    §4.1, computed with a 64-bit-wide intermediate per the same
    section's overflow rule).
  `StreamHeader` gains four matching `Result`-returning lifting
  accessors (`format_typed` / `bits_per_sample_typed` /
  `channel_count_typed` / `sample_rate_typed`). The `Result` shape
  matters for the ad-hoc construction path — a caller that mints a
  `StreamHeader` literal with an out-of-range field gets the same
  `Error::Unsupported*` variant the parser would have surfaced,
  rather than silently propagating an out-of-band value into the
  pipeline. Five new unit tests pin the boundary cases at each end
  of every range plus the parser-to-typed-accessor agreement on a
  44.1 kHz stereo 16-bit fixture. Lib tests: 140 (default features)
  / 145 (all-features) / 131 (no-default-features).

## What round 243 adds on top

- **Typed accessor for `StreamHeader::total_samples`** per
  `spec/01` §3.4 — the remaining raw `u32` on `StreamHeader` after
  the round-240 four-sub-field lift. New public newtype
  [`TotalSamples`](src/header.rs) (`from_raw` is infallible because
  every `u32` is structurally legal per spec §3.4 — zero is the
  documented empty-stream marker), carrying `count()`, `is_empty()`,
  and a `duration_at(sample_rate)` projection that returns the
  playback length in `core::time::Duration` using nanosecond-grain
  integer arithmetic identical to `Decoder::total_duration`
  (`floor(remainder * 1e9 / sample_rate)` with a `u128`-widened
  intermediate to stay overflow-free across the full
  `(total_samples = u32::MAX, sample_rate = 0x7FFFFF)` envelope).
  `StreamHeader` gains `total_samples_typed()` (the infallible
  projection) and a `total_duration()` convenience that threads
  through it — the header-side mirror of `Decoder::total_duration`,
  reachable without constructing a full `Decoder` (e.g. for a player
  UI that wants to display the stream duration before committing to
  a decode). Five new unit tests pin the boundary cases
  (`TotalSamples` at `0` / `44_100` / `u32::MAX`; `duration_at` at
  exact 1 s / zero samples / zero rate / 0.5 s sub-second precision;
  the upper-bound envelope `(u32::MAX, MAX_SAMPLE_RATE)` against a
  future regression that drops the `u128` widening; the
  parsed-header round-trip on `(1, 2, 16, 48_000, 96_000)` confirming
  the typed accessor matches the raw field and the convenience
  `total_duration` matches the typed `duration_at(sample_rate)` call;
  the zero-payload header at `total_samples = 0` confirming both the
  typed accessor's `is_empty` flag and the zero-duration round-trip).
  One new integration test in `roundtrip_tests` confirms cross-API
  agreement: for every shape in a six-case parameter grid (exact 1 s
  / 2.5 s / 3 s at the typical rates, a single sample at 192 kHz, 1 s
  plus one sample at 44.1 kHz, and an empty-stream literal), the
  header-level `StreamHeader::total_duration`, the typed
  `TotalSamples::duration_at(sample_rate)`, and the decoder-level
  `Decoder::total_duration` agree bit-for-bit. The raw `u32` field on
  `StreamHeader` is kept for backward compatibility; the typed
  accessor is purely additive. Lib tests: 146 (default features) /
  151 (all-features) / 137 (no-default-features).

## What round 246 adds on top

- **Typed sub-field accessors** for the two constrained
  [`FrameDescriptor`](src/header.rs) fields per `spec/01` §4.2 / §5.1
  / §5.5. The raw `disk_size: u32` and `sample_count: u32` fields are
  kept as the on-wire data model, but consumers that want to branch on
  the spec's documented frame-layout invariants can now lift each
  field into a validated typed accessor:
  - [`FrameByteLength`](src/header.rs) — newtype validated against the
    `>= 4` minimum per `spec/01` §5.1 (the smallest legal on-disk frame
    block is an empty body followed by the four trailing CRC bytes).
    Carries `total_size()` (the seek-table-entry value verbatim) and
    `body_size()` (= `total_size - 4`, safe subtraction by construction
    rather than `saturating_sub` as on the raw
    `FrameDescriptor::body_size`).
  - [`FrameSampleCount`](src/header.rs) — newtype validated against
    the `>= 1` minimum per `spec/01` §4.1 / §5.5 (every
    parser-produced descriptor describes at least one sample; the
    empty-stream case produces zero descriptors instead). Carries
    `count()` and `is_within_regular_bound(regular)`, the
    `<= floor(sample_rate * 256 / 245)` regular-frame ceiling gate of
    `spec/01` §4.1 / §5.5.
  `FrameDescriptor` gains two matching `Result`-returning lifting
  accessors (`disk_size_typed` / `sample_count_typed`). Two new
  `Error` variants — `InvalidFrameByteLength(u32)` and
  `InvalidFrameSampleCount(u32)` — surface the rejection at lift time
  so an ad-hoc `FrameDescriptor` literal (e.g. an encode-side fixture)
  gets the same discipline the per-frame decoder hot path enforces.
  Five new unit tests pin the boundary cases plus a three-frame parsed
  seek-table cross-check; one new integration test in
  `roundtrip_tests` confirms cross-API agreement on three independent
  encoded multi-frame stream shapes — mono 16-bit @ 44.1k / 2.5 s
  (three frames: `regular`, `regular`, shorter-last), stereo 16-bit @
  48k / 2 s (two regular via the exact-multiple case), mono 24-bit @
  44.1k / 1 s (single shorter-last) — every descriptor's typed lift
  agrees bit-for-bit with its raw field and every frame's sample count
  satisfies the regular-bound gate with the expected per-frame split
  from `header.frame_geometry()`. Lib tests: 152 (default features) /
  157 (all-features) / 143 (no-default-features). Integration tests
  unchanged at 9.

## What round 251 adds on top

- **Typed projection of the per-stream frame geometry** per
  `spec/01` §4.1 — the `(frame_count, regular_frame_samples,
  last_frame_samples)` triple that `StreamHeader::frame_geometry`
  has been returning as a bare `(u32, u32)` tuple since round 1.
  The new [`FrameGeometry`](src/header.rs) newtype threads the
  triple together, so callers do not have to re-derive
  `regular_frame_samples` separately when they already have the
  geometry in hand:
  - `frame_count()` — number of frames in the stream
    (`ceil(total_samples / regular_frame_samples)` per spec §4.1, or
    `0` for the empty-stream case `total_samples == 0` per spec §3.4).
  - `regular_frame_samples()` — `floor(sample_rate * 256 / 245)`
    per spec §4.1.
  - `last_frame_samples()` — `<= regular_frame_samples` per spec §4.1;
    equals `regular_frame_samples` when `total_samples` is an exact
    multiple of the regular count.
  - `is_empty()` — short-circuit for the empty-stream case.
  - `is_exact_multiple()` — predicate matching spec §4.1's exact-
    multiple branch (`last == regular` for non-empty streams).
  - `frame_samples_at(frame_index)` — per-frame sample-count lookup
    (`regular_frame_samples` for every non-last frame, `last_frame_samples`
    for the trailing one) matching the per-`FrameDescriptor.sample_count`
    assignment made by `parse_seek_table`.
  - `seek_table_size_bytes()` — `4 * frame_count + 4` per spec §4.2
    (entries + trailing CRC; `4` for an empty stream per spec §4.4).
  - `total_samples()` — round-trips the geometry back to the source
    `StreamHeader::total_samples` field per spec §3.4 in `u64`
    arithmetic so the back-derivation stays overflow-free across the
    full `(total_samples = u32::MAX, sample_rate = MAX_SAMPLE_RATE)`
    envelope.
  `StreamHeader` gains a new `frame_geometry_typed()` accessor that
  projects the existing bare-tuple `frame_geometry()` return into
  the typed newtype. The bare tuple is kept for backward
  compatibility — every existing caller in `src/` and `benches/`
  continues to destructure `(frame_count, last_samples)` verbatim;
  the typed projection is purely additive. Five new unit tests in
  `header::tests` pin the surface: the three-shape round-trip
  (`(1, 44_100)` single-frame, `(3, 18_090)` three-frame, `(2,
  46_080)` exact-multiple) walking every accessor; the empty-stream
  case at `total_samples = 0` confirming `is_empty`, the `4`-byte
  seek-table size from `spec/01` §4.4, and the `None` past-end
  `frame_samples_at`; the bare-tuple-vs-typed-projection agreement
  across a six-shape parameter grid (including the empty stream
  and the 24-bit / multi-channel cases) confirming the typed
  accessor is sugar over the existing `frame_geometry` return; the
  `(total_samples = u32::MAX, sample_rate = MAX_SAMPLE_RATE)`
  envelope canary against a future regression that drops the `u64`
  back-derivation widening; and an end-to-end parsed-header
  round-trip confirming the typed projection's
  `seek_table_size_bytes` matches the `spec/01` §4.2 closed form
  and `frame_samples_at` matches the parser's per-frame `is_last`
  discrimination. One new integration test in `roundtrip_tests`
  confirms cross-API agreement on a real encoded multi-frame stream:
  the same three independent shapes from the round-246 cross-check
  (mono 16-bit @ 44.1k / 2.5 s — three frames, stereo 16-bit @ 48k /
  2 s — exact-multiple two frames, mono 24-bit @ 44.1k / 1 s — one
  frame), with the typed projection's `frame_count` / `last_frame_samples`
  agreeing with the bare-tuple return, the projection's `regular_frame_samples`
  agreeing with `StreamHeader::regular_frame_samples`, the projection's
  `total_samples` round-tripping back to the header field, the projection's
  `seek_table_size_bytes` matching `4 * frame_count + 4`, the
  `is_exact_multiple` predicate matching the `total_samples mod
  regular == 0` source-side gate, and the per-frame `frame_samples_at`
  agreeing with every parsed `FrameDescriptor::sample_count`.
  Lib tests: 158 (default features). Integration tests unchanged
  at 9.

## What round 254 adds on top

- **Typed sub-field accessors for `SeekPoint`** per `spec/01` §4.1 /
  §4.2 — the two `pub` fields on the existing round-187 `SeekPoint`
  (`frame_index: usize` and `sample_offset_in_frame: u32`) are now
  lifted into validated newtypes so a caller that hand-constructs a
  seek point against an ad-hoc seek table gets the same window
  discipline `Decoder::seek_to_sample` enforces at construction:
  - [`FrameIndex`](src/decoder.rs) — `usize` newtype validated
    against the stream's `frame_count`. Carries `index()` and
    `is_last(frame_count)` (the same last-frame discrimination the
    parser uses in `parse_seek_table` to assign
    `FrameDescriptor::is_last` per `spec/01` §4.1).
  - [`InFrameSampleOffset`](src/decoder.rs) — `u32` newtype
    validated against the regular per-frame sample count derived
    per `spec/01` §4.1 (`floor(sample_rate * 256 / 245)`). Carries
    `offset()`, `is_frame_boundary()` (true when the offset is
    zero — the player-API "no prefix skip needed" predicate), and
    `interleaved_skip(channels)` (the prefix-entry count
    `Decoder::frame_iter_from_sample` discards from the
    `frame_index` frame's PCM buffer per `spec/01` §4.1 / §3.2).
  - Two new `Error` variants — `InvalidFrameIndex(usize)` and
    `InvalidInFrameSampleOffset(u32)` — surface the rejection at
    lift time so an ad-hoc literal gets the same discipline the
    random-access path enforces; both are slotted into the
    `tests/malformed_props.rs` exhaustive panic-when-leaked match
    because they surface only from typed-accessor invocation, never
    from `decode()`.
  - `SeekPoint::frame_index_typed(frame_count)` /
    `SeekPoint::sample_offset_typed(regular_frame_samples)` —
    `Result`-returning lifting accessors. The raw fields stay public;
    the typed accessors are purely additive.
  Five new unit tests in `decoder::seek_point_typed_tests` pin the
  boundary cases (`FrameIndex` empty-stream / single-frame /
  three-frame / upper-end `usize`, `InFrameSampleOffset`
  zero-regular / 44.1k-derived `46_080` boundary including the
  rejection at the regular ceiling, the `interleaved_skip` projection
  across `mono` / `stereo` / `6ch` and the `0` defensive channel
  case, the ad-hoc-literal cross-check between `SeekPoint`'s typed
  accessors and the validated newtypes, and the frame-boundary
  predicate). One new integration test in `roundtrip_tests`
  (`seek_point_typed_accessors_match_parsed_stream`) walks the same
  three-shape encoded-stream grid the round-246 / round-251 tests
  use and confirms cross-API agreement on every `Decoder::seek_to_sample`
  probe (first sample, last sample, every frame boundary, every
  mid-frame offset): the typed `frame_index` lift agrees with the
  raw field and reports `is_last` consistent with the seek table's
  last-frame discrimination, the typed `sample_offset` lift agrees
  with the raw field and `is_frame_boundary` matches the
  `sample_offset_in_frame == 0` source-side gate, and the
  `interleaved_skip` projection equals the `(offset * channels)`
  arithmetic `frame_iter_from_sample` uses internally — plus
  out-of-window literal rejection on both accessors. Lib tests: 164
  (default features) / 169 (all-features) / 155 (no-default-features).
  Integration tests unchanged at 9.

## What round 262 adds on top

- **Aggregate `TypedStreamHeader` validated view** per `spec/01` §3 —
  the capstone of the round-240 → round-261 typed-accessor arc. The
  per-field accessors require a caller that wants the complete
  `spec/01` §3.1–§3.4 invariant set to check four separate `Result`s
  (plus the infallible `total_samples` projection); the new
  `StreamHeader::typed()` performs the same five lifts behind one
  `Result`, returning a [`TypedStreamHeader`](src/header.rs) whose
  fields are the existing validated newtypes (`Format` /
  `ChannelCount` / `BitsPerSample` / `SampleRate` / `TotalSamples`).
  Validation runs in the header's on-wire field order (`format`,
  `channels`, `bits_per_sample`, `sample_rate` per the §3 table) —
  the same order the byte-level parser checks — so an ad-hoc
  `StreamHeader` literal with several out-of-range fields surfaces
  the same first error variant `parse_stream_header_with_crc` would
  have produced for the same raw values. Because every field is
  in-range by construction, the derived projections are total (no
  defensive zero-handling): `requires_password()` (`spec/07` §3
  gate), `byte_depth()` (§3.2), `regular_frame_samples()` (§4.1),
  `frame_geometry()` (typed, §4.1 — delegates to the round-251
  projection on the round-tripped header so there is a single source
  of arithmetic), `total_duration()` (§3.3/§3.4 nanosecond-grain
  integer arithmetic), and the new `pcm_byte_len()` — the
  `total_samples × channels × byte_depth` raw-PCM-buffer size of
  `spec/01` §3.4's product rule, computed in `u64` because the
  product overflows `u32` at the `(u32::MAX, 6ch, 3B)` envelope.
  `to_header()` round-trips losslessly back to the raw on-wire data
  model; no new `Error` variants (the aggregate reuses the per-field
  rejection variants). Five new unit tests in `header::tests` pin the
  field-by-field agreement with the individual lifts + every derived
  projection, the rejection-order chain against the byte-level parser
  on a five-case multi-invalid grid, the `pcm_byte_len` product rule
  at the §8.1/§8.2 fixture shapes + empty stream + the u64-widening
  envelope canary, the format=2 `requires_password` gate on a parsed
  header, and the empty-stream degradation. One new integration test
  in `roundtrip_tests` (`typed_stream_header_matches_parsed_stream`)
  walks the same three-shape encoded-stream grid as the round-246 /
  251 / 254 cross-checks and confirms the aggregate view agrees with
  the raw fields, the decoder's own `total_duration` / frame-table
  walk, and the §3.4 product rule against the actual interleaved
  input PCM length. Lib tests: 188 (default features) / 193
  (all-features) / 179 (no-default-features). Integration tests
  unchanged at 9.
