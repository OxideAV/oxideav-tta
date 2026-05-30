# Changelog

All notable changes to this crate are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Round-190: `streaming_decode` cargo-fuzz target under
  `fuzz/fuzz_targets/streaming_decode.rs`. Drives the round-187
  streaming + random-access decode surface on `Decoder`
  (`frame_iter`, `decode_frame_at`, `seek_to_sample`,
  `frame_iter_from`) with attacker-chosen byte streams paired with
  attacker-chosen `target_frame_index` / `target_sample_index` /
  `start_index` seeds packed into the first ten bytes of the fuzz
  input. Asserts cross-API agreement against the eager `decode_all`
  baseline on every constructed input: the lazy `frame_iter` must
  concatenate to the eager output bit-exactly, `decode_frame_at`
  must match the corresponding eager slice, `seek_to_sample` must
  return an in-range `(frame_index, sample_offset_in_frame)` pair,
  and `frame_iter_from(start_index)` must equal the eager suffix
  from the matching sample boundary. Contract is the standard
  panic-free / no-integer-overflow / no-OOB / no-unbounded-alloc
  shape the existing three fuzz targets share. Seed corpus under
  `fuzz/corpus/streaming_decode/` includes the five real-stream
  fixtures plus four crate-encoded multi-frame seeds (mono16 /
  stereo16 / stereo24 spanning 2.5-3 s + a 3 s format=2 stereo
  stream), each pre-prefixed with the ten-byte seed header so the
  random-access branches are driven from the first fuzz iteration.
  The fuzz workflow's auto-discovery picks up the new `[[bin]]`
  block from `fuzz/Cargo.toml`; the daily 30-minute budget is now
  split four-way across `decode`, `scan_trailers`,
  `encode_roundtrip`, and `streaming_decode`. The cross-API
  agreement assertion is gated on `frame_count <= 4096` so the
  fuzzer's per-iteration budget stays on the streaming-state-
  machine surface rather than the `total_samples * channels`
  eager allocation.

- Round-187: streaming + random-access decode API on `Decoder`.
  The new surface exposes:
  - `Decoder::frame_iter(&self) -> FrameIter` — lazy iterator that
    yields one `Result<Vec<i32>>` per frame. Memory usage is
    bounded by `O(samples_per_frame * channels)` regardless of
    stream length. The eager `Decoder::decode_all` path is
    unchanged; new code that wants to start producing PCM before
    the whole file is decoded can use this instead.
  - `Decoder::decode_frame_at(&self, index)` — random-access
    decode of a single frame addressed by its seek-table index.
    Bit-identical to the slice of `decode_all` covering that
    frame (verified by the new
    `decode_frame_at_matches_decode_all_mono_24bit` test): the
    spec/01 §5.1 + spec/02..05 §3.1 per-frame state-reset
    discipline is what makes this safe.
  - `Decoder::seek_to_sample(&self, sample_index)` — locate the
    frame containing a given per-channel sample, returning a
    `SeekPoint { frame_index, sample_offset_in_frame }` driven by
    the spec §4.1 `regular_frame_samples = floor(sample_rate *
    256 / 245)` arithmetic.
  - `Decoder::frame_iter_from(&self, start_index)` — start a lazy
    iterator at the given frame instead of zero, so the
    seek-and-resume path does not decode the skipped prefix.
  - `Decoder::total_samples(&self)` — accessor for the
    per-channel total sample count (mirrors the header field but
    avoids the consumer having to reach into `header`).
  - `FrameIter` (re-exported at crate root) and `SeekPoint` —
    `ExactSizeIterator` shape with a correct `size_hint`.
  Adds `Error::FrameIndexOutOfRange` and
  `Error::SampleIndexOutOfRange` for the two new failure modes.
  Six new lib tests in `roundtrip_tests` lock the bit-exact
  agreement with `decode_all`, the seek-and-resume integration
  property, and the out-of-range rejection behaviour. Lib-test
  count: 78 → 85 (+7 incl. the new past-end test); integration
  tests unchanged at 9 (the existing `malformed_props.rs`
  exhaustive-`match Error` block was widened with the two new
  variants as panic arms — they are decoder-API misuse, not
  outcomes a header bit-flip can produce).
- Round-175: two additional cargo-fuzz targets under
  `fuzz/fuzz_targets/`:
  - `scan_trailers` — drives the public `scan_trailers` entry point
    with arbitrary bytes. Exercises the ID3v2 prefix skip, TTA1
    header parse, seek-table sum arithmetic, and the
    `detect_trailers` ID3v1 / APEv2 footer scanner (`spec/01` §7).
    Contract: every call returns a `Result`, never panicking,
    integer-overflowing, indexing out of bounds, or allocating
    proportional to an attacker-controlled header field. 500K
    iterations clean (cov 132, ft 133).
  - `encode_roundtrip` — drives the public `encode` /
    `encode_with_password` over a typed `(channels × bps ×
    sample_rate × format × samples)` parameter cube and asserts (i)
    the encoder either rejects with a typed
    `Error::Unsupported…` / `InvalidSampleBuffer`, or returns
    `Ok(bytes)`; (ii) every `Ok(bytes)` decodes back via the matching
    `decode` / `decode_with_password` call; (iii) the recovered
    `i32` samples equal the input bit-exactly. 500K iterations clean
    (cov 688, ft 3221, ~18.5K exec/s).
  Both targets seeded under `fuzz/corpus/{scan_trailers,encode_roundtrip}/`.
  The reusable `OxideAV/.github` fuzz workflow auto-discovers
  cargo-fuzz `[[bin]]` blocks from `fuzz/Cargo.toml`, so the 30-min
  daily budget is now split three-way across `decode`,
  `scan_trailers`, and `encode_roundtrip`.

- Round-156: malformed-input property tests under `tests/malformed_props.rs`.
  Nine integration tests, all driven by a deterministic xorshift64*
  PRNG so failures reproduce from the literal seed in the source
  (matching `oxideav-scene/tests/transform_props.rs`'s convention).
  Coverage classes — exhaustive 22×8 bit-flip walk of the stream
  header, exhaustive prefix-truncation walk (format=1 and format=2),
  seek-table re-CRC bait (recompute the seek-table CRC after
  corrupting an entry, forcing the rejection signal onto the
  per-frame CRC), oversize `total_samples` with recomputed header
  CRC, wrong-password format=2 (must not panic; if `Ok` the PCM
  shape must match the header), randomised ID3v2-prefix sweep
  (every prefix must yield identical PCM to the un-prefixed
  decode), randomised trailer-region junk (`scan_trailers` must
  never claim a trailer that overlaps the in-stream frame region),
  and pseudo-noise round-trip at randomised channel-count /
  bit-depth shapes. Targets the *semantic* fault classes that
  random-bytes fuzzing typically misses (a corrupt seek-table
  with a valid CRC is structurally indistinguishable from a real
  one to a panic-only oracle). All nine pass against the r124
  Rice-cap fix, the r127 baseline encoder, and the r5 audit/07
  closures. Tests use only the public crate API (`decode`,
  `decode_with_password`, `encode`, `encode_with_password`,
  `scan_trailers`); a local IEEE-802.3 CRC32 helper duplicates
  `spec/01` §6 instead of reaching into the crate's private
  `crc32` module.

- Round-127: Criterion benchmark harnesses under `benches/`. Three
  self-contained binaries — `decode`, `encode`, and `roundtrip` —
  drive the production decoder + encoder over a deterministic
  xorshift-synthesised PCM workload (mono / stereo / 6-channel,
  16-bit and 24-bit, plus a format=2 password-derived qm-priming
  variant). No checked-in fixtures: each scenario builds its input
  in-bench so future optimisation rounds (SIMD Rice emit, faster
  qm-priming, etc.) have a stable A/B baseline. Run with
  `cargo bench -p oxideav-tta --bench <name>`. Pairs with the
  r124 fuzz harness as the "saturated → fuzz/bench/profile"
  follow-through.

- Round-124: cargo-fuzz harness. `fuzz/fuzz_targets/decode.rs` is a
  decode-only libfuzzer target driving both `decode` (format=1) and
  `decode_with_password` (format=2) over arbitrary bytes; the contract
  is that the call always returns a `Result` rather than panicking,
  overflowing, indexing out of bounds, or OOMing. Seed corpus under
  `fuzz/corpus/decode/` is five real streams from the crate's own
  encoder. `.github/workflows/fuzz.yml` gives it a daily 30-minute
  budget via the org reusable `crate-fuzz.yml`. The harness body is
  clean-room (no `libtta` oracle). Two regression unit tests in `rice`
  pin the cap behaviour found by the fuzzer.

### Fixed

- Round-124: Rice decoder could drive the adaptive parameter `k` past
  31 on a corrupt high-mode bitstream that chained enough escapes,
  after which the next binary-tail read requested more than 32 bits and
  tripped `BitReader::read_bits`'s `k <= 32` invariant (a debug-build
  panic; a garbage shift in release). `k0`/`k1` are now capped at 31 on
  increment — matching the `[0, 31]` range the reference encoder stays
  within per `spec/05` §5.3 — so the cap never alters the decode of any
  valid stream. Found by the new `fuzz/fuzz_targets/decode.rs` harness.

- Round-5: multi-frame format=2 (password-derived qm priming) round-trip
  + trace-tape coverage closing `docs/audio/tta-cleanroom/audit/07`
  §6.2-5. New tests in `roundtrip_tests` exercise format=2 across 3+
  frames in both mono (2.5 s @ 44.1 kHz) and stereo configurations and
  verify (a) sample-exact decoder/encoder round-trip across every
  frame boundary, and (b) under the `trace` feature, that the
  `LMS_PRE` event's `qm_pre[]` carries the ECMA-182 CRC-64 digest
  bytes at step_idx ∈ {0..nch-1} of **every** frame — proving the
  spec/07 §3.5 / §3.6 "qm priming reapplied at every frame init,
  shared across all `nch` channels" rule on the wire (single-frame
  trace coverage couldn't distinguish "prime once at frame 0" from
  "prime at every frame").

### Changed

- `decoder::Decoder` now stores the freshly-computed IEEE-802.3 CRC32
  of the 18 stream-header body bytes (`header_crc`) alongside the
  parsed header. The `trace` feature's `HEADER_CRC` event surfaces
  the real value instead of the prior placeholder zero, closing
  `audit/07` §6.2-3. The header parser exposes the value through a
  new `parse_stream_header_with_crc` entry; the existing
  `parse_stream_header_any_format` is a thin wrapper that drops it.
- `decode_with_password` no longer re-parses the header and seek
  table when the on-disk `format` field is `1` (audit/07 §6.2-2).
  A new crate-internal `Decoder::clear_priming` method drops the
  computed digest in place on the already-constructed decoder so
  format=1's spec/02 §3.1 zero-init invariant is preserved without
  the redundant second parse.

## [0.0.1] — round 1–4

### Added

- Round-4: ID3v1 / APEv2 trailer detection per `spec/01` §7. New
  public entry points `scan_trailers(bytes) -> Result<TrailerInfo>`
  (parses the optional ID3v2 prefix + stream header + seek table to
  compute the end-of-stream byte boundary, then signature-scans the
  post-stream region) and `detect_trailers(bytes, eos_off) ->
  TrailerInfo` (signature-scans a region given an explicit
  end-of-stream offset; never reads bytes inside the TTA1 frame
  region). `TrailerInfo` exposes `id3v1` / `apev2` as absolute
  `(start, len)` byte ranges. ID3v1 detection follows spec §7's
  "last 128 bytes start with `'T','A','G'`" rule; APEv2 detection
  reads the 32-byte footer's `tag_size` field (LE u32 at footer
  offset 12) plus the "has-header" flag (footer offset 20, bit 31)
  to recover the full APE region span. Bogus `tag_size` values that
  would overrun the post-stream region are silently rejected (the
  trailer is reported as "not present"). Out-of-stream metadata
  parsing itself remains host-application territory per spec §7.
- Round-3: production TTA1 encoder. New public entry points
  `encode(samples, channels, bits_per_sample, sample_rate)` and
  `encode_with_password(.., password)` produce complete TTA1 byte
  streams (header + seek table + frame blobs) from interleaved `i32`
  PCM. The encoder is the symmetric inverse of the existing decoder:
  forward channel decorrelation (`spec/04` §3.1), Stage-B prediction
  subtraction (`spec/03` §4.3), Stage-A LMS step with residual
  feedback (`spec/02` §4.2), zigzag + adaptive-Rice with the
  decoder's lock-stepped `(k0, k1, sum0, sum1)` trackers (`spec/05`
  §5.2 / §5.3), per-frame byte alignment + IEEE-802.3 CRC32
  (`spec/01` §5.3 / §5.4), then header + seek table assembly
  (`spec/01` §3 / §4). Output is bit-exactly round-trippable through
  the existing `decode` / `decode_with_password` entry points across
  every fixture in the existing test suite (16-bit / 24-bit,
  1..=6 channels, silence / sine / pseudo-noise / DC+impulse / multi-
  frame; format=1 + format=2). Replaces the previous `#[cfg(test)]`
  internal encoder.
- Round-3: framework `Encoder` impl wired through the existing
  `registry` feature. The same `CodecInfo::new("tta")` registration
  that already carried `decoder(make_decoder)` now also carries
  `encoder(make_encoder)`, so `CodecRegistry::first_encoder(&params)`
  returns a working TTA encoder. The adapter accepts interleaved
  S16/S24 audio frames, buffers the PCM, and emits one self-contained
  TTA1 file as a keyframe packet on `flush()`.
- Round-3: new `Error::InvalidSampleBuffer` variant raised when the
  encoder is handed an interleaved PCM buffer whose length is not a
  multiple of the requested channel count.
- Frame-boundary streaming demuxer + O(1) seek (`Demuxer::seek_to`)
  built on the TTA1 in-file seek table. Each demuxer packet is a
  self-contained mini-TTA1 file carrying exactly one audio frame
  (re-prefixed header + 1-entry seek table + that frame's body),
  emitted with monotonically increasing pts in samples per
  `time_base = 1/sample_rate`. `seek_to(pts)` is a constant-time
  lookup: `target_frame = min(pts.max(0) / regular_frame_samples,
  n_frames - 1)` per `spec/01-bitstream-framing.md` §4.1, with
  `regular_frame_samples = floor(sample_rate * 256 / 245)`.
  Sub-frame pts requests snap to the containing frame's first
  sample, negative pts clamp to 0, past-end pts clamp to the last
  frame. Decoder per-channel state (LMS / Stage-B / Rice) resets at
  every frame boundary by construction (`spec/02..05`), so the
  demuxer does not coordinate decoder reset — the next mini-file
  packet starts a fresh decoder run. Covered by five tests in
  `src/seek_tests.rs`: `seek_to_zero_resets_to_first_frame`,
  `seek_at_frame_boundary_lands_exact`,
  `seek_mid_frame_lands_at_containing_frame_start`,
  `seek_past_end_clamps_to_last_frame`, and
  `seek_pts_matches_decoder_output_after_seek` (encode → seek →
  decode → byte-identical PCM round-trip).
- Round-2: `spec/06-trace-contract.md` debug trace emitter behind
  the new `trace` Cargo feature (off by default). With the feature
  on AND `OXIDEAV_TTA_TRACE_FILE=<path>` set, the decoder writes
  one TSV event line per state transition to that path,
  implementing all 18 events (`FILE_HEADER`, `HEADER_CRC`,
  `SEEK_TABLE_*`, `LMS_INIT`, `RICE_K_INIT`, `FRAME_BEGIN`/`_END`,
  per-step `RICE_DECODE` / `RICE_K_UPDATE` / `LMS_PRE` /
  `STAGE_A_PREDICT` / `LMS_POST` / `STAGE_B_PREDICT`, per-sample
  `DECORR_PRE` / `DECORR_POST` / `PCM_OUT`) with the field schemas
  from spec/06 §5. Zero overhead at runtime when the feature is
  off.
- Round-2: `oxideav-core` framework integration behind the
  default-on `registry` feature: a `Decoder` impl (codec id
  `"tta"`, capability flags `with_lossless / with_intra_only`,
  S16/S24 output), a raw `.tta` `Demuxer` (`tta` extension +
  TTA1-magic probe, ID3v2 prefix tolerated), and the
  `register(ctx)` entry point that `oxideav-meta::register_all`
  reaches via `oxideav_core::register!("oxideav-tta", register)`.
  Standalone (no-`oxideav-core`) consumers can opt out with
  `default-features = false`.
- Round-2: format=2 (password-derived qm priming) per `spec/07`.
  New `decode_with_password(bytes, password)` entry point computes
  an ECMA-182 CRC-64 digest of the password (forward / unreflected
  polynomial `0x42F0E1EBA9EA3693`, init / output XOR
  `0xFFFFFFFFFFFFFFFF`), unpacks the digest into eight signed-int8
  bytes per spec/07 §3.4, and primes Stage-A's `qm[0..7]` (sign-
  extended to int32) at every per-channel frame init. Plain
  `decode()` surfaces `Error::PasswordRequired` for format=2
  streams. Empty-password edge case (spec/07 §9 item 2) produces
  an all-zero priming, bit-identical to format=1.
- Round-1: TTA1 format=1 (integer PCM) decoder built against the
  clean-room workspace at `docs/audio/tta-cleanroom/`. Covers framing
  (`spec/01` header + seek table + per-frame CRC32), adaptive Rice
  entropy decoder (`spec/05`), 8-tap sign-LMS Stage-A predictor
  (`spec/02`), fixed-order Stage-B predictor (`spec/03`), and
  pairwise inverse channel decorrelation (`spec/04`) for all
  in-scope channel counts (1..=6) and bit depths (16, 24).
- Public surface: `decode`, `decode_with_password`, `pack_pcm`,
  `Decoder`, `decode_frame`, `StreamHeader` / `StreamInfo`,
  `FrameDescriptor`, and the crate's `Error` / `Result` types.
- `tables/lms-shift.csv` and `tables/lms-dx-magnitudes.csv` are
  loaded via `include_str!` and parsed once at startup, per the
  workspace's "no retyping numeric tables" policy.
- Crate-internal test-only encoder (`#[cfg(test)] mod encoder`) that
  manufactures self-consistent TTA1 streams (format=1 and format=2)
  for roundtrip testing, since no reference TTA fixtures are
  checked in.

### Changed

- Clean-room rebuild from a fresh orphan `master`. The previous
  implementation was retired by the OxideAV docs audit dated
  2026-05-06 (FFmpeg source cited as the writeup's basis, not merely
  as the trace-instrumentation host); the prior history is preserved
  on the `old` branch.
- The new code is being written against the strict-isolation
  clean-room workspace at `docs/audio/tta-cleanroom/` (Specifier /
  Extractor / Implementer / Auditor roles, with explicit allow-list
  and forbidden-input list per role). The Implementer reads only
  `spec/` + `tables/` + `reference/docs/`; libtta and
  FFmpeg `libavcodec/tta*` are forbidden inputs.
