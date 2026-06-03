# oxideav-tta

Pure-Rust True Audio (TTA) lossless audio codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Round 5 (+ r187 streaming surface, r204 format=2 streaming reach,
r209 sample-keyed player-API sugar, r215 duration-keyed player-API
sugar, r219 half-open sample/time range quartet) — clean-room encoder
+ decoder + framework integration + trace tape + format=2 +
ID3v1/APEv2 trailer detection + multi-frame format=2 trace coverage +
format=2 streaming/random-access surface + `decode_from_sample` /
`frame_iter_from_sample` + `decode_from_time` / `frame_iter_from_time` /
`seek_to_time` / `total_duration` + `decode_sample_range` /
`frame_iter_sample_range` / `decode_time_range` /
`frame_iter_time_range`.** Both encodes and decodes TTA1 format=1
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
  lockstep diff against a libtta-instrumented tape via
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
  error, mirroring libtta's silent passthrough behaviour for
  trailers).

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

Run locally with `cargo bench -p oxideav-tta --bench <decode|encode|roundtrip|streaming>`.
