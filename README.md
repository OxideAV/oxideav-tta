# oxideav-tta

Pure-Rust True Audio (TTA) lossless audio codec for the
[oxideav](https://github.com/OxideAV/oxideav-workspace) framework.

## Status

**Round 5 — clean-room encoder + decoder + framework integration +
trace tape + format=2 + ID3v1/APEv2 trailer detection + multi-frame
format=2 trace coverage.** Both encodes and decodes TTA1 format=1
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

The harness body is clean-room (no `libtta` oracle). Run locally with
`cargo +nightly fuzz run <target>`; the `.github/workflows/fuzz.yml`
shim points at the org reusable workflow which auto-discovers every
`[[bin]]` block in `fuzz/Cargo.toml` and splits the daily 30-minute
budget across them.

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

Run locally with `cargo bench -p oxideav-tta --bench <decode|encode|roundtrip>`.
